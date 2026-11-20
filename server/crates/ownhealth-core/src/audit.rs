//! Append-only audit log writer (`core/audit.py`). Identifiers only: never values, document text or
//! free-text arguments.
//!
//! Write the event in the same transaction as the read it records. Free-text arguments go through
//! `hash_arg` so the log never says what was searched.

use std::net::IpAddr;

use futures::TryStreamExt;
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use sqlx::PgConnection;
use uuid::Uuid;

use crate::error::Result;

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
    let ip = parse_ip(e.ip.as_deref()).map(|a| a.to_string());
    Ok(sqlx::query_scalar(
        "insert into audit_event (actor_kind, actor_id, token_id, client, via, action, tool, profile_id, \
         resource_type, resource_ids, src_ids, args_summary, result_kind, rows_out, bytes_out, withheld, \
         protocol_version, ip, user_agent, not_assessed_n, arg_bytes) values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, \
         $11, $12, $13, $14, $15, $16, $17, cast($18 as inet), $19, $20, $21) returning id",
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

/// Recompute the audit chain (migrations/1250_audit_chain.sql) from seq 1 and name the first link
/// that does not hold. A rewrite of the whole chain from some row on still verifies here: only the
/// head kept outside the database (backups) shows that.
pub async fn verify_chain(conn: &mut PgConnection) -> Result<ChainCheck> {
    type Link = (i64, i64, Vec<u8>, Vec<u8>, Option<Vec<u8>>);
    let mut prev = vec![0u8; 32];
    let mut head: Option<(i64, Vec<u8>)> = None;
    let mut verified = 0_i64;
    let mut links = sqlx::query_as::<_, Link>(
        "select c.seq, c.event_id, c.prev_hash, c.hash, case when e.id is not null then oh_audit_row_v1(e) end \
         from audit_chain c left join audit_event e on e.id = c.event_id order by c.seq",
    )
    .fetch(&mut *conn);
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
        "select min(e.id) from audit_event e where not exists (select 1 from audit_chain c where c.event_id = e.id)",
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
}
