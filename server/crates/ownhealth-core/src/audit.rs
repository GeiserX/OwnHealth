//! Append-only audit log writer (`core/audit.py`). Identifiers only: never values, document text or
//! free-text arguments.
//!
//! Write the event in the same transaction as the read it records. Free-text arguments go through
//! `hash_arg` so the log never says what was searched.

use std::net::IpAddr;

use chrono::{DateTime, Utc};
use futures::TryStreamExt;
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use sqlx::{AssertSqlSafe, Connection, PgConnection};
use uuid::Uuid;

use crate::error::{Error, Result};

/// One audit row. `actor_kind`: user | token | share | system; `via`: ui | api | mcp | job | share |
/// export; `action`: read | search | propose | export | grant_change | login | purge | review | import ...
#[derive(Debug, Clone)]
pub struct AuditEvent {
    pub actor_kind: String,
    pub actor_id: Option<Uuid>,
    pub via: String,
    pub action: String,
    pub token_id: Option<Uuid>,
    pub client: Option<String>,
    pub tool: Option<String>,
    pub profile_id: Option<Uuid>,
    pub resource_type: Option<String>,
    pub resource_ids: Option<Vec<String>>,
    pub src_ids: Option<Vec<String>>,
    pub args_summary: Option<String>,
    pub result_kind: Option<String>,
    pub rows_out: Option<i32>,
    pub bytes_out: Option<i32>,
    pub withheld: Option<Value>,
    pub protocol_version: Option<String>,
    /// Kept only when it parses as an address: the column is inet, and a bad cast would abort the read.
    pub ip: Option<String>,
    pub user_agent: Option<String>,
    /// Session report (behind its consent switch): how many not_assessed entries the answer had.
    pub not_assessed_n: Option<i32>,
    /// Session report: the width of the call arguments in bytes, never the arguments themselves.
    pub arg_bytes: Option<i32>,
}

impl AuditEvent {
    /// An event with `result_kind = "ok"` and nothing else set, as the pydantic model defaults.
    pub fn new(actor_kind: &str, via: &str, action: &str) -> AuditEvent {
        AuditEvent {
            actor_kind: actor_kind.into(),
            actor_id: None,
            via: via.into(),
            action: action.into(),
            token_id: None,
            client: None,
            tool: None,
            profile_id: None,
            resource_type: None,
            resource_ids: None,
            src_ids: None,
            args_summary: None,
            result_kind: Some("ok".into()),
            rows_out: None,
            bytes_out: None,
            withheld: None,
            protocol_version: None,
            ip: None,
            user_agent: None,
            not_assessed_n: None,
            arg_bytes: None,
        }
    }
}

/// Python's `ipaddress.ip_address(str(v).strip())`, or None.
pub fn parse_ip(raw: Option<&str>) -> Option<IpAddr> {
    raw.map(str::trim)
        .filter(|s| !s.is_empty())
        .and_then(|s| s.parse().ok())
}

/// Stable short hash for a free-text argument, so the log keeps no search terms.
pub fn hash_arg(value: &str) -> String {
    format!("h:{}", &hex::encode(Sha256::digest(value.as_bytes()))[..12])
}

pub async fn write_audit(conn: &mut PgConnection, e: &AuditEvent) -> Result<i64> {
    write_audit_at(conn, e, None).await
}

/// `write_audit` with the row's time given: a read done in an earlier transaction is logged at the
/// moment that read began (`at`), not at the moment its row is written. `None` is now().
pub async fn write_audit_at(conn: &mut PgConnection, e: &AuditEvent, at: Option<DateTime<Utc>>) -> Result<i64> {
    let ip = parse_ip(e.ip.as_deref()).map(|a| a.to_string());
    Ok(sqlx::query_scalar(
        "insert into audit_event (actor_kind, actor_id, token_id, client, via, action, tool, profile_id, \
         resource_type, resource_ids, src_ids, args_summary, result_kind, rows_out, bytes_out, withheld, \
         protocol_version, ip, user_agent, not_assessed_n, arg_bytes, ts) values ($1, $2, $3, $4, $5, $6, $7, $8, \
         $9, $10, $11, $12, $13, $14, $15, $16, $17, cast($18 as inet), $19, $20, $21, coalesce($22, now())) \
         returning id",
    )
    .bind(&e.actor_kind)
    .bind(e.actor_id)
    .bind(e.token_id)
    .bind(&e.client)
    .bind(&e.via)
    .bind(&e.action)
    .bind(&e.tool)
    .bind(e.profile_id)
    .bind(&e.resource_type)
    .bind(&e.resource_ids)
    .bind(&e.src_ids)
    .bind(&e.args_summary)
    .bind(&e.result_kind)
    .bind(e.rows_out)
    .bind(e.bytes_out)
    .bind(&e.withheld)
    .bind(&e.protocol_version)
    .bind(ip)
    .bind(&e.user_agent)
    .bind(e.not_assessed_n)
    .bind(e.arg_bytes)
    .bind(at)
    .fetch_one(conn)
    .await?)
}

/// Newest first. Filter by the profiles the caller may see, or by actor.
pub async fn list_audit(
    conn: &mut PgConnection,
    profile_ids: Option<&[Uuid]>,
    actor_id: Option<Uuid>,
    limit: i64,
    before_id: Option<i64>,
) -> Result<Vec<Map<String, Value>>> {
    let rows = sqlx::query(
        "select * from audit_event where ($1::uuid[] is null or profile_id = any($1)) \
         and ($2::uuid is null or actor_id = $2) and ($3::bigint is null or id < $3) order by id desc limit $4",
    )
    .bind(profile_ids)
    .bind(actor_id)
    .bind(before_id)
    .bind(limit)
    .fetch_all(conn)
    .await?;
    rows.iter().map(crate::pyjson::row_to_json).collect()
}

// ------------------------------------------------------------------ hash chain ----

/// How a link fails: what `verify_chain` names at the first break.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChainBreakKind {
    /// No link has this seq: a link (and usually its event) was deleted.
    MissingLink,
    /// The link does not point at the hash of the link before it: links were reordered or swapped.
    PrevHash,
    /// The link names an event that is no longer there.
    MissingEvent,
    /// The event no longer hashes to its link: the row was changed.
    Hash,
    /// An event with no link at all: it was added behind the chain.
    Unlinked,
}

/// The first link that does not hold. `seq` is None only for an unlinked event, and `event_id` is
/// None only for a missing link.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChainBreak {
    pub seq: Option<i64>,
    pub event_id: Option<i64>,
    pub kind: ChainBreakKind,
}

/// What `verify_chain` found: `verified` links hold from seq 1, the last of them is `head`, and
/// `broken` names the first one that does not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChainCheck {
    pub verified: i64,
    pub head: Option<(i64, Vec<u8>)>,
    pub broken: Option<ChainBreak>,
}

impl ChainCheck {
    pub fn ok(&self) -> bool {
        self.broken.is_none()
    }
}

/// The bytes a link hashes, version 1, for the audit_event row `e`: the expression of
/// `oh_audit_row_v1` in migrations/1250_audit_chain.sql, written out again with every function taken
/// from pg_catalog. The database owner can replace `oh_audit_row_v1`, but not these, so the check
/// never trusts a function the owner controls. A test keeps the two copies equal.
const ROW_V1: &str = "pg_catalog.convert_to(pg_catalog.jsonb_build_array('v1', e.id, e.ts at time zone 'UTC', \
    e.actor_kind, e.actor_id, e.token_id, e.client, e.via, e.action, e.tool, e.profile_id, e.resource_type, \
    e.resource_ids::text, e.src_ids::text, e.args_summary, e.result_kind, e.rows_out, e.bytes_out, \
    e.withheld::text, e.protocol_version, e.ip::text, e.user_agent, e.not_assessed_n, e.arg_bytes)::text, 'UTF8')";

/// The check reads both tables by name, so it first makes sure each is still a plain table with no
/// row-level security, no rules and no generated column, and that every column has a built-in type
/// (one in pg_catalog, made at initdb: an oid below FirstNormalObjectId, 16384). Otherwise the owner
/// could put a view in its place that shows the check the old row and the app the edited one, or give
/// a column a type of its own whose cast to json shows the check a value the row does not hold. A
/// non-superuser owner cannot change pg_catalog.
const PLAIN_TABLES: &str = "select pg_catalog.count(*) from pg_catalog.pg_class c \
    join pg_catalog.pg_namespace n on n.oid = c.relnamespace \
    where n.nspname = 'public' and c.relname in ('audit_event', 'audit_chain') \
    and c.relkind = 'r' and not c.relrowsecurity and not c.relhasrules \
    and not exists (select 1 from pg_catalog.pg_attribute a join pg_catalog.pg_type t on t.oid = a.atttypid \
      where a.attrelid = c.oid and a.attnum > 0 and not a.attisdropped \
      and (t.oid >= 16384::pg_catalog.oid or t.typnamespace <> 'pg_catalog'::pg_catalog.regnamespace \
           or a.attgenerated <> ''))";

/// Recompute the audit chain (migrations/1250_audit_chain.sql) from seq 1 and name the first link
/// that does not hold: an edited, deleted or reordered row, or a row added behind the chain.
///
/// It cannot see what leaves a consistent chain behind: the newest rows removed together with their
/// links, both tables emptied, or the whole chain re-hashed from some row on. Only a head kept
/// outside the database (the backup anchor) shows those.
///
/// It runs as one transaction of its own (a savepoint when the caller already holds one) and rolls
/// it back at the end. The transaction first sets `search_path` to pg_catalog, so no operator or
/// function of the owner's takes part in a comparison. Then it locks both tables, so neither can be
/// renamed or swapped until the check ends (that needs ACCESS EXCLUSIVE). Then it runs the guard,
/// and it refuses to go on when either table is no longer a plain table (see `PLAIN_TABLES`):
/// `Error::AuditUncheckable`. Only then does it read.
///
/// So it holds against the app role, and against an owner who edits rows with plain SQL or with the
/// tricks we know. An owner can always change what the database answers, and a superuser can make
/// it answer any query however it likes; this check can only ask that same database. Only a head
/// kept outside the database proves the history against the owner.
pub async fn verify_chain(conn: &mut PgConnection) -> Result<ChainCheck> {
    let mut tx = conn.begin().await?;
    sqlx::raw_sql(
        "set local search_path = pg_catalog, pg_temp; \
         lock table public.audit_event, public.audit_chain in access share mode",
    )
    .execute(&mut *tx)
    .await?;
    let plain: i64 = sqlx::query_scalar(PLAIN_TABLES).fetch_one(&mut *tx).await?;
    let check = if plain == 2 {
        read_chain(&mut tx).await
    } else {
        Err(Error::AuditUncheckable(
            "the audit log cannot be checked: public.audit_event or public.audit_chain is no longer a plain table"
                .into(),
        ))
    };
    tx.rollback().await?;
    check
}

/// The reads of `verify_chain`, inside its transaction, after the guard.
async fn read_chain(conn: &mut PgConnection) -> Result<ChainCheck> {
    type Link = (i64, i64, Vec<u8>, Vec<u8>, Option<Vec<u8>>);
    let mut prev = vec![0u8; 32];
    let mut head: Option<(i64, Vec<u8>)> = None;
    let mut verified = 0_i64;
    let sql = format!(
        "select c.seq, c.event_id, c.prev_hash, c.hash, case when e.id is not null then {ROW_V1} end \
         from public.audit_chain c left join public.audit_event e on e.id = c.event_id order by c.seq"
    );
    let mut links = sqlx::query_as::<_, Link>(AssertSqlSafe(sql)).fetch(&mut *conn);
    while let Some((seq, event_id, prev_hash, hash, row)) = links.try_next().await? {
        let kind = if seq != verified + 1 {
            Some(ChainBreakKind::MissingLink)
        } else if prev_hash != prev {
            Some(ChainBreakKind::PrevHash)
        } else {
            match row {
                None => Some(ChainBreakKind::MissingEvent),
                Some(r) => (Sha256::new().chain_update(&prev).chain_update(&r).finalize().as_slice() != hash)
                    .then_some(ChainBreakKind::Hash),
            }
        };
        if let Some(kind) = kind {
            let event_id = (kind != ChainBreakKind::MissingLink).then_some(event_id);
            let broken = Some(ChainBreak {
                seq: Some(verified + 1),
                event_id,
                kind,
            });
            return Ok(ChainCheck { verified, head, broken });
        }
        verified = seq;
        prev.clone_from(&hash);
        head = Some((seq, hash));
    }
    drop(links);
    // an event and its link commit together, so a visible event with no link was added behind it
    let unlinked: Option<i64> = sqlx::query_scalar(
        "select min(e.id) from public.audit_event e \
         where not exists (select 1 from public.audit_chain c where c.event_id = e.id)",
    )
    .fetch_one(conn)
    .await?;
    let broken = unlinked.map(|id| ChainBreak {
        seq: None,
        event_id: Some(id),
        kind: ChainBreakKind::Unlinked,
    });
    Ok(ChainCheck { verified, head, broken })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_arg_hides_search_terms() {
        let h = hash_arg("HIV test");
        assert!(!h.contains("HIV") && h == hash_arg("HIV test") && h.len() == 14);
        // the Python value: "h:" + sha256("HIV test").hexdigest()[:12]
        assert_eq!(h, format!("h:{}", &hex::encode(Sha256::digest(b"HIV test"))[..12]));
    }

    #[test]
    fn only_real_addresses_are_kept() {
        assert_eq!(parse_ip(Some("testclient")), None);
        assert_eq!(parse_ip(Some("1.2.3.4, 5.6.7.8")), None);
        assert_eq!(
            parse_ip(Some(" 10.0.0.1 ")).map(|a| a.to_string()),
            Some("10.0.0.1".into())
        );
        assert_eq!(parse_ip(Some("::1")).map(|a| a.to_string()), Some("::1".into()));
    }

    // ------------------------------------------------------------ hash chain tests ----

    use crate::testing::TestDb;
    use sqlx::{Connection, PgPool};

    /// `n` audit rows, each committed on its own, as the app writes them. Synthetic values only.
    async fn chained(n: usize) -> anyhow::Result<(TestDb, Vec<i64>)> {
        let db = TestDb::new().await?;
        let mut ids = Vec::new();
        for i in 0..n {
            let mut tx = db.pool.begin().await?;
            let mut e = AuditEvent::new("token", "mcp", "read");
            e.client = Some("Claude Code".into());
            e.tool = Some("get_health_status".into());
            e.resource_ids = Some(vec![format!("obs-{i}")]);
            e.rows_out = Some(i32::try_from(i)?);
            e.ip = Some("10.0.0.7".into());
            ids.push(write_audit(&mut tx, &e).await?);
            tx.commit().await?;
        }
        Ok((db, ids))
    }

    async fn check(pool: &PgPool) -> anyhow::Result<ChainCheck> {
        Ok(verify_chain(&mut *pool.acquire().await?).await?)
    }

    /// Run `sql` as the database owner with every trigger off (foreign keys and append-only too),
    /// the way someone with the owner's password would rewrite history.
    async fn tamper(db: &TestDb, sql: &str) -> anyhow::Result<()> {
        let mut c = sqlx::PgConnection::connect(&db.url).await?;
        sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
            "set session_replication_role = replica; {sql}"
        )))
        .execute(&mut c)
        .await?;
        c.close().await?;
        Ok(())
    }

    fn broke(seq: Option<i64>, event_id: Option<i64>, kind: ChainBreakKind) -> Option<ChainBreak> {
        Some(ChainBreak { seq, event_id, kind })
    }

    #[tokio::test]
    async fn an_untouched_chain_verifies_up_to_its_head() -> anyhow::Result<()> {
        let (db, ids) = chained(5).await?;
        let c = check(&db.pool).await?;
        let (last_seq, last_hash): (i64, Vec<u8>) =
            sqlx::query_as("select seq, hash from audit_chain order by seq desc limit 1")
                .fetch_one(&db.pool)
                .await?;
        assert!(c.ok(), "{c:?}");
        assert_eq!((c.verified, c.head), (5, Some((last_seq, last_hash))));
        let linked: Vec<i64> = sqlx::query_scalar("select event_id from audit_chain order by seq")
            .fetch_all(&db.pool)
            .await?;
        assert_eq!(linked, ids);
        Ok(())
    }

    #[tokio::test]
    async fn an_edited_row_is_named() -> anyhow::Result<()> {
        let (db, ids) = chained(5).await?;
        // the owner role can turn the append-only trigger off on its own table
        let mut c = sqlx::PgConnection::connect(&db.url).await?;
        sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
            "alter table audit_event disable trigger audit_event_no_update; \
             update audit_event set rows_out = 0 where id = {}; \
             alter table audit_event enable trigger audit_event_no_update",
            ids[2]
        )))
        .execute(&mut c)
        .await?;
        c.close().await?;
        let c = check(&db.pool).await?;
        assert_eq!(c.broken, broke(Some(3), Some(ids[2]), ChainBreakKind::Hash));
        assert_eq!(c.verified, 2);
        Ok(())
    }

    #[tokio::test]
    async fn a_deleted_row_is_named() -> anyhow::Result<()> {
        // the event alone: its link still names it
        let (db, ids) = chained(5).await?;
        tamper(&db, &format!("delete from audit_event where id = {}", ids[2])).await?;
        let c = check(&db.pool).await?;
        assert_eq!(c.broken, broke(Some(3), Some(ids[2]), ChainBreakKind::MissingEvent));
        // the event and its link: seq 3 is gone
        let (db, ids) = chained(5).await?;
        tamper(
            &db,
            &format!(
                "delete from audit_chain where event_id = {0}; delete from audit_event where id = {0}",
                ids[2]
            ),
        )
        .await?;
        let c = check(&db.pool).await?;
        assert_eq!(c.broken, broke(Some(3), None, ChainBreakKind::MissingLink));
        assert_eq!(c.verified, 2);
        Ok(())
    }

    #[tokio::test]
    async fn a_reordered_row_is_named() -> anyhow::Result<()> {
        let (db, ids) = chained(5).await?;
        tamper(
            &db,
            "update audit_chain set seq = -2 where seq = 2; update audit_chain set seq = 2 where seq = 3; \
             update audit_chain set seq = 3 where seq = -2",
        )
        .await?;
        let c = check(&db.pool).await?;
        assert_eq!(c.broken, broke(Some(2), Some(ids[2]), ChainBreakKind::PrevHash));
        assert_eq!(c.verified, 1);
        Ok(())
    }

    #[tokio::test]
    async fn a_row_added_behind_the_chain_is_named() -> anyhow::Result<()> {
        let (db, _) = chained(3).await?;
        tamper(
            &db,
            "insert into audit_event (actor_kind, via, action) values ('system', 'job', 'read')",
        )
        .await?;
        let added: i64 = sqlx::query_scalar("select max(id) from audit_event")
            .fetch_one(&db.pool)
            .await?;
        let c = check(&db.pool).await?;
        assert_eq!(c.broken, broke(None, Some(added), ChainBreakKind::Unlinked));
        assert_eq!(c.verified, 3);
        Ok(())
    }

    #[tokio::test]
    async fn the_session_report_columns_are_stored_and_hashed() -> anyhow::Result<()> {
        let (db, ids) = chained(1).await?;
        let mut tx = db.pool.begin().await?;
        let mut e = AuditEvent::new("token", "mcp", "read");
        e.not_assessed_n = Some(2);
        e.arg_bytes = Some(41);
        let id = write_audit(&mut tx, &e).await?;
        tx.commit().await?;
        let rows: Vec<(Option<i32>, Option<i32>)> =
            sqlx::query_as("select not_assessed_n, arg_bytes from audit_event where id = any($1) order by id")
                .bind([ids[0], id])
                .fetch_all(&db.pool)
                .await?;
        assert_eq!(rows, [(None, None), (Some(2), Some(41))]);
        assert!(check(&db.pool).await?.ok());
        tamper(&db, &format!("update audit_event set arg_bytes = 40 where id = {id}")).await?;
        assert_eq!(
            check(&db.pool).await?.broken,
            broke(Some(2), Some(id), ChainBreakKind::Hash)
        );
        Ok(())
    }

    #[tokio::test]
    async fn concurrent_audit_writers_do_not_deadlock() -> anyhow::Result<()> {
        let db = TestDb::new().await?;
        // half the writers audit and then lock a shared row, half lock it and then audit: a chain lock
        // taken before commit would deadlock the two orders
        sqlx::raw_sql("create table oh_test_counter (id int primary key, n int not null); insert into oh_test_counter values (0, 0)")
            .execute(&db.pool)
            .await?;
        let mut tasks = Vec::new();
        for w in 0..8_i32 {
            let pool = db.pool.clone();
            tasks.push(tokio::spawn(async move {
                let bump = "update oh_test_counter set n = n + 1 where id = 0";
                for _ in 0..10 {
                    let mut tx = pool.begin().await?;
                    if w % 2 == 0 {
                        write_audit(&mut tx, &AuditEvent::new("system", "job", "read")).await?;
                        sqlx::query(bump).execute(&mut *tx).await?;
                    } else {
                        sqlx::query(bump).execute(&mut *tx).await?;
                        write_audit(&mut tx, &AuditEvent::new("system", "job", "read")).await?;
                    }
                    write_audit(&mut tx, &AuditEvent::new("system", "job", "read")).await?;
                    tx.commit().await?;
                }
                anyhow::Ok(())
            }));
        }
        let all = tokio::time::timeout(std::time::Duration::from_secs(60), futures::future::join_all(tasks)).await?;
        for t in all {
            t??;
        }
        let c = check(&db.pool).await?;
        assert!(c.ok(), "{c:?}");
        assert_eq!(c.verified, 160);
        Ok(())
    }

    #[tokio::test]
    async fn a_repeatable_read_audit_is_refused_every_time() -> anyhow::Result<()> {
        let db = TestDb::new().await?;
        let mut tx = db.pool.begin().await?;
        sqlx::query("set transaction isolation level repeatable read")
            .execute(&mut *tx)
            .await?;
        write_audit(&mut tx, &AuditEvent::new("user", "export", "export")).await?;
        let err = tx
            .commit()
            .await
            .expect_err("a repeatable read audit row must not commit");
        assert!(err.to_string().contains("read committed"), "{err}");
        let n: i64 = sqlx::query_scalar("select count(*) from audit_event")
            .fetch_one(&db.pool)
            .await?;
        assert_eq!(n, 0);
        Ok(())
    }

    #[tokio::test]
    async fn rows_written_before_the_upgrade_are_linked_in_id_order() -> anyhow::Result<()> {
        let db = TestDb::empty().await?;
        let before: Vec<_> = crate::migrate::embedded()?
            .into_iter()
            .filter(|m| m.version < 1250)
            .collect();
        crate::migrate::migrate(&db.pool, &before).await?;
        for action in ["login", "read", "export"] {
            sqlx::query("insert into audit_event (actor_kind, via, action) values ('user', 'ui', $1)")
                .bind(action)
                .execute(&db.pool)
                .await?;
        }
        crate::migrate::migrate_embedded(&db.pool).await?;
        let c = check(&db.pool).await?;
        assert!(c.ok(), "{c:?}");
        assert_eq!(c.verified, 3);
        let order: Vec<(i64, i64)> = sqlx::query_as("select seq, event_id from audit_chain order by seq")
            .fetch_all(&db.pool)
            .await?;
        let ids: Vec<i64> = sqlx::query_scalar("select id from audit_event order by id")
            .fetch_all(&db.pool)
            .await?;
        assert_eq!(
            order,
            ids.iter()
                .enumerate()
                .map(|(i, id)| (i64::try_from(i).unwrap_or(0) + 1, *id))
                .collect::<Vec<_>>()
        );
        // and the next row links onto the backfilled head
        let mut tx = db.pool.begin().await?;
        write_audit(&mut tx, &AuditEvent::new("user", "ui", "read")).await?;
        tx.commit().await?;
        assert_eq!(check(&db.pool).await?.verified, 4);
        Ok(())
    }

    // ------------------------------------------------------- round 1 review tests ----

    /// Run `sql` as the database owner with the triggers on.
    async fn owner_sql(db: &TestDb, sql: &str) -> anyhow::Result<()> {
        let mut c = sqlx::PgConnection::connect(&db.url).await?;
        sqlx::raw_sql(sqlx::AssertSqlSafe(sql.to_string()))
            .execute(&mut c)
            .await?;
        c.close().await?;
        Ok(())
    }

    #[tokio::test]
    async fn a_swapped_row_function_does_not_hide_an_old_edit() -> anyhow::Result<()> {
        let (db, ids) = chained(5).await?;
        let x = ids[2];
        // the owner edits a row, then makes the row function return the bytes the row had before
        owner_sql(
            &db,
            &format!(
                "create table oh_probe as select e.id, oh_audit_row_v1(e) as b from audit_event e where e.id = {x}; \
                 set session_replication_role = replica; update audit_event set rows_out = 999 where id = {x}; \
                 set session_replication_role = origin; \
                 alter function oh_audit_row_v1(audit_event) rename to oh_audit_row_v1_real; \
                 create function oh_audit_row_v1(e audit_event) returns bytea language sql stable as $f$ \
                   select coalesce((select b from oh_probe where id = e.id), oh_audit_row_v1_real(e)) $f$"
            ),
        )
        .await?;
        let c = check(&db.pool).await?;
        assert_eq!(c.broken, broke(Some(3), Some(x), ChainBreakKind::Hash), "{c:?}");
        Ok(())
    }

    #[tokio::test]
    async fn rows_linked_after_a_function_swap_are_still_protected() -> anyhow::Result<()> {
        let (db, ids) = chained(2).await?;
        let last = ids[1];
        // the owner makes every later row hash to nothing, then edits one of those rows
        owner_sql(
            &db,
            &format!(
                "alter function oh_audit_row_v1(audit_event) rename to oh_audit_row_v1_real; \
                 create function oh_audit_row_v1(e audit_event) returns bytea language sql stable as $f$ \
                   select case when e.id <= {last} then public.oh_audit_row_v1_real(e) else ''::bytea end $f$"
            ),
        )
        .await?;
        let mut tx = db.pool.begin().await?;
        let id = write_audit(&mut tx, &AuditEvent::new("user", "export", "export")).await?;
        tx.commit().await?;
        tamper(&db, &format!("update audit_event set action = 'read' where id = {id}")).await?;
        let c = check(&db.pool).await?;
        assert_eq!(c.broken, broke(Some(3), Some(id), ChainBreakKind::Hash), "{c:?}");
        Ok(())
    }

    #[tokio::test]
    async fn every_v1_column_is_hashed() -> anyhow::Result<()> {
        for set in [
            "ts = ts + interval '1 microsecond'",
            "actor_kind = 'system'",
            "actor_id = gen_random_uuid()",
            "token_id = gen_random_uuid()",
            "client = 'x'",
            "via = 'job'",
            "action = 'export'",
            "tool = 'x'",
            "profile_id = gen_random_uuid()",
            "resource_type = 'x'",
            "resource_ids = array['x']",
            "src_ids = array['x']",
            "args_summary = 'x'",
            "result_kind = 'x'",
            "rows_out = 77",
            "bytes_out = 77",
            "withheld = '{\"x\": 1}'",
            "protocol_version = 'x'",
            "ip = '10.0.0.8'",
            "user_agent = 'x'",
            "not_assessed_n = 3",
            "arg_bytes = 3",
        ] {
            let (db, ids) = chained(2).await?;
            tamper(&db, &format!("update audit_event set {set} where id = {}", ids[1])).await?;
            assert_eq!(
                check(&db.pool).await?.broken,
                broke(Some(2), Some(ids[1]), ChainBreakKind::Hash),
                "{set}"
            );
        }
        Ok(())
    }

    #[tokio::test]
    async fn the_hash_does_not_depend_on_the_session_time_zone() -> anyhow::Result<()> {
        let db = TestDb::new().await?;
        let mut c = sqlx::PgConnection::connect(&db.url).await?;
        sqlx::query("set timezone = 'America/New_York'").execute(&mut c).await?;
        let mut tx = c.begin().await?;
        write_audit(&mut tx, &AuditEvent::new("system", "job", "read")).await?;
        tx.commit().await?;
        sqlx::query("set timezone = 'Asia/Kolkata'").execute(&mut c).await?;
        let r = verify_chain(&mut c).await?;
        assert!(r.ok() && r.verified == 1, "{r:?}");
        c.close().await?;
        Ok(())
    }

    /// Edit row 2, then patch only its own link hash and the next link's prev_hash.
    async fn patch_one_link(formula: &str) -> anyhow::Result<ChainCheck> {
        let (db, ids) = chained(4).await?;
        tamper(
            &db,
            &format!(
                "update audit_event set rows_out = 999 where id = {x}; \
                 update audit_chain c set hash = {formula} from audit_event e where e.id = c.event_id and c.seq = 2; \
                 update audit_chain set prev_hash = (select hash from audit_chain where seq = 2) where seq = 3",
                x = ids[1]
            ),
        )
        .await?;
        check(&db.pool).await
    }

    #[tokio::test]
    async fn a_patched_link_breaks_the_rest_of_the_chain() -> anyhow::Result<()> {
        for formula in [
            "sha256(c.prev_hash || oh_audit_row_v1(e))",
            "sha256(oh_audit_row_v1(e))",
        ] {
            let c = patch_one_link(formula).await?;
            assert!(!c.ok(), "{formula}: {c:?}");
        }
        Ok(())
    }

    #[tokio::test]
    async fn a_serializable_audit_is_refused() -> anyhow::Result<()> {
        let db = TestDb::new().await?;
        let mut tx = db.pool.begin().await?;
        sqlx::query("set transaction isolation level serializable")
            .execute(&mut *tx)
            .await?;
        write_audit(&mut tx, &AuditEvent::new("user", "export", "export")).await?;
        let err = tx.commit().await.expect_err("a serializable audit row must not commit");
        assert!(err.to_string().contains("read committed"), "{err}");
        Ok(())
    }

    #[tokio::test]
    async fn audit_rows_committed_at_the_same_moment_all_get_a_link() -> anyhow::Result<()> {
        // no shared row: only the chain lock keeps two commits from taking the same head
        const WRITERS: usize = 16;
        const ROUNDS: usize = 5;
        let db = TestDb::new().await?;
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(u32::try_from(WRITERS)?)
            .connect(&db.url)
            .await?;
        for _ in 0..ROUNDS {
            let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(WRITERS));
            let mut tasks = Vec::new();
            for _ in 0..WRITERS {
                let (pool, barrier) = (pool.clone(), barrier.clone());
                tasks.push(tokio::spawn(async move {
                    let mut tx = pool.begin().await?;
                    write_audit(&mut tx, &AuditEvent::new("system", "job", "read")).await?;
                    barrier.wait().await;
                    tx.commit().await?;
                    anyhow::Ok(())
                }));
            }
            for t in futures::future::join_all(tasks).await {
                t??;
            }
        }
        pool.close().await;
        let c = check(&db.pool).await?;
        assert!(c.ok(), "{c:?}");
        assert_eq!(c.verified, i64::try_from(WRITERS * ROUNDS)?);
        Ok(())
    }

    #[tokio::test]
    async fn the_check_hashes_the_same_bytes_as_the_row_function() -> anyhow::Result<()> {
        let db = TestDb::new().await?;
        // nulls, odd text, an IPv6 address and a network, microsecond and whole-second times, nested jsonb,
        // arrays with another lower bound or two dimensions, and a JSON null
        owner_sql(
            &db,
            r#"insert into audit_event (ts, actor_kind, actor_id, token_id, client, via, action, tool, profile_id,
               resource_type, resource_ids, src_ids, args_summary, result_kind, rows_out, bytes_out, withheld,
               protocol_version, ip, user_agent, not_assessed_n, arg_bytes) values
               ('2026-11-20 21:13:46.123456+00', 'token', gen_random_uuid(), gen_random_uuid(), 'Claude "Code"',
                'mcp', 'read', 'get_trend', gen_random_uuid(), 'observation', array['a"b', 'c\d', E'line\nbreak'],
                array[]::text[], E'tab\there \u0001 ñ ¿ 漢字 😀', 'ok', -1, 2147483647,
                '{"z": [1.50, null, true], "a": {"é": "x"}, "bb": 1e3}', '2026-07-28', '2001:db8::1',
                E'agent/1.0 \\ \x7f', 2, 41),
               ('2026-01-01 00:00:00.000001+05:30', 'system', null, null, null, 'job', 'purge', null, null, null,
                null, null, null, null, null, null, null, null, '10.0.0.0/24', null, null, null),
               (default, 'user', null, null, '', 'ui', 'read', null, null, null, null, null, '', null, 0, 0, '[]',
                null, '::1', '', 0, 0),
               (default, 'user', null, null, null, 'ui', 'read', null, null, null, '[0:1]={a,"NULL"}',
                '{{a,b},{c,NULL}}', null, null, null, null, 'null', null, null, null, null, null)"#,
        )
        .await?;
        let rows: Vec<(i64, Vec<u8>, Vec<u8>)> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "select e.id, oh_audit_row_v1(e), {ROW_V1} from audit_event e order by e.id"
        )))
        .fetch_all(&db.pool)
        .await?;
        assert_eq!(rows.len(), 4);
        for (id, function, check) in rows {
            assert_eq!(
                String::from_utf8_lossy(&function),
                String::from_utf8_lossy(&check),
                "event {id}"
            );
            assert_eq!(function, check, "event {id}");
        }
        let c = check(&db.pool).await?;
        assert!(c.ok() && c.verified == 4, "{c:?}");
        Ok(())
    }

    /// The owner swaps audit_event for a view that shows the check the old row and the app the edit.
    #[tokio::test]
    async fn a_view_in_place_of_the_audit_table_stops_the_check() -> anyhow::Result<()> {
        let (db, ids) = chained(5).await?;
        let x = ids[2];
        owner_sql(
            &db,
            &format!(
                "create table oh_shadow as select * from audit_event where id = {x}; \
                 set session_replication_role = replica; update audit_event set rows_out = 999 where id = {x}; \
                 set session_replication_role = origin; \
                 alter table audit_event rename to audit_event_t; \
                 create view audit_event as \
                   select t.* from audit_event_t t \
                    where not (t.id = {x} and pg_catalog.current_query() like '%audit_chain%') \
                   union all select s.* from oh_shadow s where pg_catalog.current_query() like '%audit_chain%'; \
                 create function oh_ins() returns trigger language plpgsql as $f$ begin \
                   new.id := coalesce(new.id, nextval(pg_get_serial_sequence('audit_event_t', 'id')::regclass)); \
                   insert into audit_event_t overriding system value select new.*; return new; end $f$; \
                 create trigger oh_ins instead of insert on audit_event for each row execute function oh_ins();"
            ),
        )
        .await?;
        // an audit write through the view fails at commit: the chain function links
        // public.audit_event only, and the table under the view is no longer that table
        let mut tx = db.pool.begin().await?;
        write_audit(&mut tx, &AuditEvent::new("user", "ui", "read")).await?;
        let err = tx
            .commit()
            .await
            .expect_err("the chain function must refuse the renamed table");
        assert!(err.to_string().contains("links public.audit_event only"), "{err}");
        let err = check(&db.pool)
            .await
            .expect_err("a view in place of audit_event must stop the check");
        assert!(err.to_string().contains("no longer a plain table"), "{err}");
        Ok(())
    }

    /// Row-level security on a chain table could hide rows from the check, so it stops the check too.
    #[tokio::test]
    async fn row_security_on_the_chain_stops_the_check() -> anyhow::Result<()> {
        let (db, _) = chained(2).await?;
        assert!(check(&db.pool).await?.ok());
        owner_sql(&db, "alter table audit_chain enable row level security").await?;
        let err = check(&db.pool)
            .await
            .expect_err("row security on audit_chain must stop the check");
        assert!(err.to_string().contains("no longer a plain table"), "{err}");
        Ok(())
    }

    // ------------------------------------------------------- round 3 review tests ----

    /// The check on a new connection, so it starts with the database's own settings.
    async fn check_new(db: &TestDb) -> anyhow::Result<ChainCheck> {
        let mut c = sqlx::PgConnection::connect(&db.url).await?;
        let r = verify_chain(&mut c).await;
        c.close().await?;
        Ok(r?)
    }

    /// The chain function links rows of public.audit_event and nothing else. Attached to another
    /// table, it raises before it reads the row, so no cast of that row ever runs as its owner.
    #[tokio::test]
    async fn the_chain_function_refuses_any_other_table() -> anyhow::Result<()> {
        let (db, _) = chained(2).await?;
        let err = owner_sql(
            &db,
            "create temp table t (like public.audit_event); \
             create trigger x after insert on t for each row execute function public.oh_audit_chain_link(); \
             insert into t (id, ts, actor_kind, via, action) values (424242, pg_catalog.now(), 'user', 'ui', 'read');",
        )
        .await
        .expect_err("the chain function must refuse a table other than audit_event");
        assert!(err.to_string().contains("links public.audit_event only"), "{err}");
        let links: i64 = sqlx::query_scalar("select count(*) from audit_chain")
            .fetch_one(&db.pool)
            .await?;
        assert_eq!(links, 2);
        Ok(())
    }

    /// Every SECURITY DEFINER function the migrations make: no role but its owner may call it, and it
    /// looks names up in pg_catalog only (Postgres never searches pg_temp for functions or operators).
    #[tokio::test]
    async fn security_definer_functions_are_closed_to_other_roles() -> anyhow::Result<()> {
        let db = TestDb::new().await?;
        let rows: Vec<(String, Option<Vec<String>>, bool)> = sqlx::query_as(
            "select p.oid::regprocedure::text, p.proconfig, exists (select 1 \
               from aclexplode(coalesce(p.proacl, acldefault('f', p.proowner))) a \
              where a.grantee <> p.proowner and a.privilege_type = 'EXECUTE') \
             from pg_proc p join pg_namespace n on n.oid = p.pronamespace \
             where p.prosecdef and n.nspname not in ('pg_catalog', 'information_schema') order by 1",
        )
        .fetch_all(&db.pool)
        .await?;
        assert!(!rows.is_empty(), "no SECURITY DEFINER function found");
        for (f, config, others_execute) in rows {
            assert_eq!(config, Some(vec!["search_path=pg_catalog, pg_temp".to_string()]), "{f}");
            assert!(!others_execute, "{f}: a role other than its owner can call it");
        }
        Ok(())
    }

    /// The owner puts public ahead of pg_catalog for the database and adds its own `=` for bigint,
    /// true whenever the query is the check's search for unlinked rows. The row added behind the
    /// chain must still be named.
    #[tokio::test]
    async fn an_owner_operator_does_not_hide_a_row_added_behind_the_chain() -> anyhow::Result<()> {
        let (db, _) = chained(5).await?;
        tamper(
            &db,
            "insert into audit_event (actor_kind, via, action, rows_out) values ('system', 'job', 'purge', 77)",
        )
        .await?;
        let added: i64 = sqlx::query_scalar("select max(id) from audit_event")
            .fetch_one(&db.pool)
            .await?;
        assert_eq!(
            check_new(&db).await?.broken,
            broke(None, Some(added), ChainBreakKind::Unlinked)
        );
        owner_sql(
            &db,
            &format!(
                "create function public.oh_eq(bigint, bigint) returns boolean language sql volatile as $f$ \
                   select $1 operator(pg_catalog.=) $2 \
                       or pg_catalog.strpos(pg_catalog.current_query(), 'not exists') > 0 $f$; \
                 create operator public.= (leftarg = bigint, rightarg = bigint, function = public.oh_eq); \
                 alter database \"{}\" set search_path = public, pg_catalog",
                db.name
            ),
        )
        .await?;
        let c = check_new(&db).await?;
        assert_eq!(c.broken, broke(None, Some(added), ChainBreakKind::Unlinked), "{c:?}");
        Ok(())
    }

    /// The same path with an always-true `=` for "char" would let a partitioned table that holds the
    /// old row pass as a plain one.
    #[tokio::test]
    async fn an_owner_operator_does_not_pass_a_partitioned_table_as_plain() -> anyhow::Result<()> {
        let (db, ids) = chained(5).await?;
        let x = ids[2];
        owner_sql(
            &db,
            &format!(
                "create function public.oh_t(\"char\", \"char\") returns boolean language sql as $f$ select true $f$; \
                 create operator public.= (leftarg = \"char\", rightarg = \"char\", function = public.oh_t); \
                 alter database \"{n}\" set search_path = public, pg_catalog; \
                 create table oh_shadow as select * from audit_event where id = {x}; \
                 set session_replication_role = replica; update audit_event set rows_out = 999 where id = {x}; \
                 set session_replication_role = origin; \
                 create schema oh_p; \
                 alter table audit_event rename to audit_event_t; \
                 create table audit_event (like audit_event_t) partition by range (id); \
                 create table oh_p.part partition of audit_event default; \
                 insert into oh_p.part select * from audit_event_t where id <> {x} union all select * from oh_shadow;",
                n = db.name
            ),
        )
        .await?;
        let err = check_new(&db)
            .await
            .expect_err("a partitioned audit_event must stop the check");
        assert!(err.to_string().contains("no longer a plain table"), "{err}");
        Ok(())
    }

    /// The owner gives a hashed column a type of its own, with its own cast to json. The check would
    /// then hash a value the row does not hold, so such a column stops the check.
    #[tokio::test]
    async fn an_owner_column_type_stops_the_check() -> anyhow::Result<()> {
        let (db, _) = chained(5).await?;
        let mut tx = db.pool.begin().await?;
        let x = write_audit(&mut tx, &AuditEvent::new("user", "ui", "purge")).await?;
        tx.commit().await?;
        let fake = "re\u{430}d"; // looks like "read"
        owner_sql(
            &db,
            &format!(
                "create type public.\"TEXT\" as enum ('read', 'purge', '{fake}'); \
                 alter table audit_event alter column action type public.\"TEXT\" \
                   using action::pg_catalog.text::public.\"TEXT\"; \
                 create function public.oh_j(public.\"TEXT\") returns json language sql immutable as $f$ \
                   select pg_catalog.to_json(case when $1::pg_catalog.text = '{fake}' then 'purge' \
                     else $1::pg_catalog.text end) $f$; \
                 create cast (public.\"TEXT\" as json) with function public.oh_j(public.\"TEXT\"); \
                 create function public.oh_jb(public.\"TEXT\") returns jsonb language sql immutable as $f$ \
                   select pg_catalog.to_jsonb(case when $1::pg_catalog.text = '{fake}' then 'purge' \
                     else $1::pg_catalog.text end) $f$; \
                 create cast (public.\"TEXT\" as jsonb) with function public.oh_jb(public.\"TEXT\");"
            ),
        )
        .await?;
        tamper(&db, &format!("update audit_event set action = '{fake}' where id = {x}")).await?;
        let err = check_new(&db)
            .await
            .expect_err("an owner type on a hashed column must stop the check");
        assert!(err.to_string().contains("no longer a plain table"), "{err}");
        Ok(())
    }

    /// A generated column is computed by an expression the owner writes, so it stops the check too.
    #[tokio::test]
    async fn a_generated_column_stops_the_check() -> anyhow::Result<()> {
        let (db, _) = chained(2).await?;
        owner_sql(
            &db,
            "alter table audit_chain add column oh_g int generated always as (1) stored",
        )
        .await?;
        let err = check_new(&db)
            .await
            .expect_err("a generated column must stop the check");
        assert!(err.to_string().contains("no longer a plain table"), "{err}");
        Ok(())
    }

    /// The check runs in a transaction of its own, a savepoint inside the caller's, and leaves the
    /// caller's settings as they were.
    #[tokio::test]
    async fn the_check_leaves_the_callers_transaction_as_it_was() -> anyhow::Result<()> {
        let (db, _) = chained(2).await?;
        let mut tx = db.pool.begin().await?;
        sqlx::query("set local search_path = public").execute(&mut *tx).await?;
        assert!(verify_chain(&mut tx).await?.ok());
        let path: String = sqlx::query_scalar("select current_setting('search_path')")
            .fetch_one(&mut *tx)
            .await?;
        assert_eq!(path, "public");
        tx.rollback().await?;
        Ok(())
    }

    // ------------------------------------------------------- round 4 review tests ----

    /// SQL NULL and JSON null in withheld are different bytes, and so are two arrays that differ
    /// only in their lower bound.
    #[tokio::test]
    async fn null_kinds_and_array_bounds_are_hashed() -> anyhow::Result<()> {
        for (withheld, set) in [
            (None, "withheld = 'null'::jsonb"),
            (Some(Value::Null), "withheld = null"),
            (None, "resource_ids = '[0:0]={r}'"),
            (None, "src_ids = '[2:2]={s}'"),
        ] {
            let db = TestDb::new().await?;
            let mut e = AuditEvent::new("token", "mcp", "read");
            e.resource_ids = Some(vec!["r".into()]);
            e.src_ids = Some(vec!["s".into()]);
            e.withheld.clone_from(&withheld);
            let mut tx = db.pool.begin().await?;
            let id = write_audit(&mut tx, &e).await?;
            tx.commit().await?;
            let sql_null: bool = sqlx::query_scalar("select withheld is null from audit_event where id = $1")
                .bind(id)
                .fetch_one(&db.pool)
                .await?;
            assert_eq!(sql_null, withheld.is_none(), "{set}");
            assert!(check(&db.pool).await?.ok(), "{set}");
            tamper(&db, &format!("update audit_event set {set} where id = {id}")).await?;
            assert_eq!(
                check(&db.pool).await?.broken,
                broke(Some(1), Some(id), ChainBreakKind::Hash),
                "{set}"
            );
        }
        Ok(())
    }

    /// The guard's refusal is an error of its own, apart from a database failure.
    #[tokio::test]
    async fn a_table_the_check_cannot_trust_is_its_own_error() -> anyhow::Result<()> {
        let (db, _) = chained(2).await?;
        owner_sql(&db, "alter table audit_chain enable row level security").await?;
        let err = verify_chain(&mut *db.pool.acquire().await?)
            .await
            .expect_err("row security on audit_chain must stop the check");
        assert!(matches!(err, Error::AuditUncheckable(_)), "{err:?}");
        Ok(())
    }
}
