//! Partition `wearable_sample` by month once it holds more than `OWNHEALTH_PARTITION_ROWS` rows
//! (default 50 million): a job, never a startup migration. A household below the threshold keeps the
//! plain table the migrations made, and nothing here touches it.
//!
//! An Apple Watch at full resolution writes 5k to 30k samples a person a day (architecture.md 6.4).
//! Past the threshold the table becomes range partitions on `t`: the old table, as it is, holds
//! everything before a cutoff, one partition a month follows it, and a DEFAULT partition takes a
//! sample no month holds. The switch attaches the historical rows in place and moves at most 50,000
//! staged samples under its exclusive lock. Each step can stop and resume.
//!
//! 1. Build a BRIN index on `t` and a btree on `document_id` on the plain table, `concurrently`, so
//!    reads and writes go on. A build that a stop left invalid is dropped and built again. Their names
//!    are the ones they keep as indexes of the historical partition.
//! 2. Pick the cutoff: the first day of the month after the month that holds `now` plus 7 days
//!    ([`cutoff_for`]). In one short transaction, add `check (t < cutoff) not valid` (a catalog
//!    change), keep the cutoff in the constraint's comment, and move the samples already dated at or
//!    after the cutoff (a device with a wrong clock) aside to `wearable_sample_ahead`, so the check
//!    can hold. A cutoff that an earlier, stopped run left less than a day ahead is taken back first:
//!    its samples come back and its check goes.
//! 3. Validate the check: one scan, under a lock that lets reads and writes go on. From step 2 to the
//!    switch, a new sample dated at or after the cutoff is refused: its import job waits an hour and
//!    runs again without spending an attempt (`ownhealth_core::jobs::JobError::waits`). The samples
//!    set aside stay in the rollups while active. The cutoff is at least 7 days ahead, so a device
//!    with a correct clock sends none.
//! 4. The switch, one transaction: rename the table to `wearable_sample_hist` (and its indexes),
//!    create the partitioned `wearable_sample` like it (columns, NOT NULLs, defaults, CHECKs, indexes,
//!    then its foreign keys, grants and owner), ATTACH the old table for `t < cutoff`, which the valid
//!    check turns into a catalog change (no scan, no copy, the indexes and foreign keys are reused),
//!    add the DEFAULT partition and the months up to next month, and put the samples set aside back:
//!    those with no document, and those whose document is still active on the same profile (a
//!    document moved or retracted meanwhile took its samples along).
//!    It waits at most `lock_timeout` for the table, gives up and tries again a few times, then leaves
//!    it to the next run: it never stands in the lock queue holding later readers back for longer.
//! 5. Every later run makes next month's partition ahead of time, and any month between that is
//!    missing, moving the samples that landed in DEFAULT into their month in the same transaction.
//!
//! The job changes tables, so it needs the role that owns `wearable_sample`. In the compose files the
//! app connects as a role that owns nothing (deploy/db/app-role.sql): its hourly run only reports
//! `needs_owner`, and the operator runs `ownhealth partition` in the `admin` service
//! (deploy/README.md, "Large wearable histories"). An app that connects as the owner runs the whole
//! job itself, every hour. Only one run works at a time (an advisory lock).

use std::time::Duration;

use chrono::{DateTime, Datelike, Months, TimeZone, Utc};
use futures::future::BoxFuture;
use ownhealth_core::{Error, Result};
use ownhealth_ingest_structured::wearables::{self, STAGED_SAMPLE_IS_ACTIVE};
use serde_json::{Value, json};
use sqlx::{AssertSqlSafe, Connection, PgConnection, PgPool};

/// The table the job partitions, and the names it gives.
pub const TABLE: &str = "wearable_sample";
pub const HIST: &str = "wearable_sample_hist";
pub const DEFAULT_PART: &str = "wearable_sample_default";
/// Samples dated at or after the cutoff, kept here between steps 2 and 4.
pub const AHEAD: &str = wearables::STAGED_SAMPLES;
const AHEAD_PROFILE_FK: &str = "wearable_sample_ahead_profile_fkey";
/// The most samples a preparation or switch may move while readers wait.
const MAX_MOVE: i64 = 50_000;
/// The `check (t < cutoff)` that lets the ATTACH skip its scan. A job whose write it refuses waits.
const BOUND: &str = ownhealth_core::jobs::PARTITION_BOUND;
/// The indexes step 1 builds, named for the historical partition the table becomes.
const INDEXES: [(&str, &str); 2] = [
    ("wearable_sample_hist_t_idx", "using brin (t)"),
    ("wearable_sample_hist_document_id_idx", "(document_id)"),
];
/// The session advisory lock one run holds ("ohpart").
const RUN_LOCK: i64 = 0x6f68_7061_7274;
/// How far ahead of now the cutoff is at least.
const MARGIN_DAYS: i64 = 7;
/// A cutoff left closer to now than this is taken back and chosen again.
const STALE_HOURS: i64 = 24;
/// lock_not_available: a `lock_timeout` ran out.
const LOCK_TIMEOUT: &str = "55P03";

/// What one run may do.
#[derive(Debug, Clone)]
pub struct Options {
    /// Switch the table once it holds more rows than this (`OWNHEALTH_PARTITION_ROWS`).
    pub threshold: u64,
    /// A fixed time for callers that need a repeatable run. By default the job reads the live clock.
    pub now: Option<DateTime<Utc>>,
    /// How long a step waits for its exclusive lock before it gives up for this attempt.
    pub lock_timeout: Duration,
    /// Attempts per locked step, with a growing pause between them.
    pub attempts: u32,
}

impl Options {
    pub fn new(threshold: u64) -> Options {
        Options {
            threshold,
            now: None,
            lock_timeout: Duration::from_secs(2),
            attempts: 5,
        }
    }
}

/// The first instant of the UTC month that holds `t`.
pub fn month_start(t: DateTime<Utc>) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(t.year(), t.month(), 1, 0, 0, 0)
        .single()
        .unwrap_or(t)
}

/// `t` moved by `n` months (a month start stays a month start).
pub fn add_months(t: DateTime<Utc>, n: i32) -> DateTime<Utc> {
    let m = Months::new(n.unsigned_abs());
    let moved = if n >= 0 {
        t.checked_add_months(m)
    } else {
        t.checked_sub_months(m)
    };
    moved.unwrap_or(t)
}

/// The cutoff a run that starts at `now` picks: the first day of the month after the one that holds
/// `now + 7 days`, so between 7 and about 38 days ahead.
pub fn cutoff_for(now: DateTime<Utc>) -> DateTime<Utc> {
    add_months(month_start(now + chrono::Duration::days(MARGIN_DAYS)), 1)
}

/// The partition of the month that starts at `m`: `wearable_sample_y2027m01`.
pub fn month_name(m: DateTime<Utc>) -> String {
    format!("{TABLE}_y{:04}m{:02}", m.year(), m.month())
}

/// A timestamptz literal for DDL, which takes no parameters.
fn lit(t: DateTime<Utc>) -> String {
    format!("'{}'::timestamptz", t.format("%Y-%m-%d %H:%M:%S%.6f+00"))
}

fn quote(ident: &str) -> String {
    format!("\"{}\"", ident.replace('"', "\"\""))
}

fn lock_timed_out(e: &Error) -> bool {
    matches!(e, Error::Db(sqlx::Error::Database(d)) if d.code().as_deref() == Some(LOCK_TIMEOUT))
}

async fn exec(conn: &mut PgConnection, sql: impl Into<String>) -> Result<u64> {
    Ok(sqlx::query(AssertSqlSafe(sql.into()))
        .execute(conn)
        .await?
        .rows_affected())
}

async fn exists(conn: &mut PgConnection, name: &str) -> Result<bool> {
    Ok(sqlx::query_scalar("select to_regclass($1) is not null")
        .bind(name)
        .fetch_one(conn)
        .await?)
}

/// What one run did; `json` is the report the command prints.
#[derive(Debug, Default)]
struct Report {
    state: &'static str,
    rows: Option<i64>,
    cutoff: Option<DateTime<Utc>>,
    made: Vec<String>,
    moved_from_default: u64,
    held_aside: u64,
    held_back: u64,
    note: Option<String>,
}

impl Report {
    fn of(state: &'static str) -> Report {
        Report {
            state,
            ..Report::default()
        }
    }

    fn json(&self, opts: &Options) -> Value {
        json!({
            "ok": matches!(self.state, "below_threshold" | "partitioned" | "converted" | "prepared"),
            "state": self.state,
            "table": TABLE,
            "rows": self.rows,
            "threshold": opts.threshold,
            "cutoff": self.cutoff.map(|c| c.to_rfc3339()),
            "made": self.made,
            "moved_from_default": self.moved_from_default,
            "held_aside": self.held_aside,
            "held_back": self.held_back,
            "note": self.note,
        })
    }
}

/// One run of the job: switch the table past the threshold, or make next month's partition ahead
/// of time once it is switched. Returns the report; `ok` is false when work is left for a later run
/// (`busy`: another run or another session held the table; `needs_owner`: this role may not change
/// tables; `needs_maintenance`: a move exceeds the exclusive step's sample limit).
pub async fn run(pool: &PgPool, opts: &Options) -> Result<Value> {
    locked(pool, opts, false, || opts.now.unwrap_or_else(Utc::now)).await
}

/// Steps 1 to 3 only, and the checks before them: what a run leaves when it stops before the switch.
/// For tests and for an operator who wants the slow part done ahead of a quiet hour.
pub async fn prepare(pool: &PgPool, opts: &Options) -> Result<Value> {
    locked(pool, opts, true, || opts.now.unwrap_or_else(Utc::now)).await
}

async fn locked(pool: &PgPool, opts: &Options, only_prepare: bool, clock: impl Fn() -> DateTime<Utc>) -> Result<Value> {
    // A session of its own: closing it drops the advisory lock, also when a step fails.
    let mut conn = pool.acquire().await?.detach();
    let mine: bool = sqlx::query_scalar("select pg_try_advisory_lock($1)")
        .bind(RUN_LOCK)
        .fetch_one(&mut conn)
        .await?;
    let out = if mine {
        steps(&mut conn, opts, only_prepare, clock).await
    } else {
        Ok(Report {
            note: Some("another run is partitioning the table".into()),
            ..Report::of("busy")
        })
    };
    let _ = conn.close().await;
    out.map(|r| r.json(opts))
}

async fn steps(
    conn: &mut PgConnection,
    opts: &Options,
    only_prepare: bool,
    clock: impl Fn() -> DateTime<Utc>,
) -> Result<Report> {
    let started = clock();
    let kind: Option<String> = sqlx::query_scalar("select relkind::text from pg_class where oid = to_regclass($1)")
        .bind(TABLE)
        .fetch_optional(&mut *conn)
        .await?;
    match kind.as_deref() {
        Some("p") if only_prepare => return Ok(Report::of("partitioned")),
        Some("p") => return ahead(conn, opts, started).await,
        Some("r") => {}
        _ => {
            return Err(Error::PartitionState(format!(
                "{TABLE} is missing or not a table: run `ownhealth migrate` first"
            )));
        }
    }

    let bound = current_bound(conn).await?;
    let owner = may_change(conn).await?;
    let rows = row_count(conn, owner && bound.is_some()).await?;
    let past = u64::try_from(rows).unwrap_or(0) > opts.threshold;
    let mut report = Report {
        rows: Some(rows),
        ..Report::default()
    };
    if !owner {
        report.state = if past || bound.is_some() {
            "needs_owner"
        } else {
            "below_threshold"
        };
        if past {
            report.note = Some(format!(
                "{TABLE} holds more rows than the threshold, and this database role does not own it: run `ownhealth partition` as the database owner"
            ));
        }
        pending(conn, &mut report).await?;
        return Ok(report);
    }

    if bound.is_some() {
        let staged = staged_count(conn).await?;
        if staged > MAX_MOVE {
            return needs_maintenance(conn, report, staged).await;
        }
        if !upgrade_staging(conn, opts).await? {
            return busy(conn, report).await;
        }
    }
    if !past {
        if bound.is_some() {
            if !reconcile_before_restore(conn, opts).await? {
                return busy(conn, report).await;
            }
            match retrying(conn, opts, |c| Box::pin(take_back(c))).await? {
                Some(Move::Done(n)) => report.held_back = n,
                Some(Move::TooMany(n)) => return needs_maintenance(conn, report, n).await,
                None => return busy(conn, report).await,
            }
        }
        report.state = "below_threshold";
        return Ok(report);
    }

    // Refuse an already oversized group before even building the indexes. It is checked again
    // with writers held out when a bound is added.
    if reusable_cutoff(bound, started).is_none() {
        let late = future_count(conn, cutoff_for(started)).await?;
        if late > MAX_MOVE {
            return needs_maintenance(conn, report, late).await;
        }
    }

    // 1. the indexes the historical partition keeps
    for (name, using) in INDEXES {
        let valid: Option<bool> =
            sqlx::query_scalar("select indisvalid from pg_index where indexrelid = to_regclass($1)")
                .bind(name)
                .fetch_optional(&mut *conn)
                .await?;
        match valid {
            Some(true) => continue,
            Some(false) => {
                exec(conn, format!("drop index concurrently if exists {name}")).await?;
            }
            None => {}
        }
        exec(
            conn,
            format!("create index concurrently if not exists {name} on {TABLE} {using}"),
        )
        .await?;
    }

    // 2. An index rebuild can wait on other transactions. Read the clock again before deciding
    // whether to keep a cutoff, and use the same time to choose the months for the switch.
    let now = clock();
    let cutoff = match reusable_cutoff(bound, now) {
        Some(cutoff) => cutoff,
        None => {
            let cutoff = cutoff_for(now);
            if bound.is_some() {
                if !reconcile_before_restore(conn, opts).await? {
                    return busy(conn, report).await;
                }
                match retrying(conn, opts, |c| Box::pin(take_back(c))).await? {
                    Some(Move::Done(n)) => report.held_back += n,
                    Some(Move::TooMany(n)) => return needs_maintenance(conn, report, n).await,
                    None => return busy(conn, report).await,
                }
            }
            match retrying(conn, opts, move |c| Box::pin(add_bound(c, cutoff))).await? {
                Some(Move::Done(n)) => report.held_aside = n,
                Some(Move::TooMany(n)) => return needs_maintenance(conn, report, n).await,
                None => return busy(conn, report).await,
            }
            cutoff
        }
    };
    report.cutoff = Some(cutoff);

    // 3. one scan; reads and writes go on
    let valid: bool =
        sqlx::query_scalar("select convalidated from pg_constraint where conrelid = to_regclass($1) and conname = $2")
            .bind(TABLE)
            .bind(BOUND)
            .fetch_one(&mut *conn)
            .await?;
    if !valid {
        exec(conn, format!("alter table {TABLE} validate constraint {BOUND}")).await?;
    }
    if only_prepare {
        report.state = "prepared";
        return Ok(report);
    }

    // 4. Reconcile the rollups while the live table still accepts reads and writes. The rollup
    // query treats active staged rows as input, so the cutover leaves its result unchanged.
    if !reconcile_before_restore(conn, opts).await? {
        return busy(conn, report).await;
    }
    match retrying(conn, opts, move |c| Box::pin(switch(c, cutoff, now))).await? {
        Some(Move::Done(done)) => {
            report.made = done.made;
            report.moved_from_default = done.moved;
            report.held_back += done.held_back;
        }
        Some(Move::TooMany(n)) => return needs_maintenance(conn, report, n).await,
        None => return busy(conn, report).await,
    }
    // statistics for the new parent; the partitions keep their own
    exec(conn, format!("analyze {TABLE}")).await?;
    report.state = "converted";
    Ok(report)
}

/// Why a locked step gave up. Any session can hold the lock: a reader or a writer of the table, or of
/// a table its foreign keys reference.
const HELD: &str = "another session held the table, or a table it references, for longer than the lock timeout each time: the next run tries again";

/// A run that gave up on a locked step: what it did so far, and the check it leaves when it leaves one.
async fn busy(conn: &mut PgConnection, report: Report) -> Result<Report> {
    let mut report = Report {
        note: Some(HELD.into()),
        ..report
    };
    report.state = "busy";
    pending(conn, &mut report).await?;
    Ok(report)
}

async fn needs_maintenance(conn: &mut PgConnection, mut report: Report, count: i64) -> Result<Report> {
    report.state = "needs_maintenance";
    report.note = Some(format!(
        "the partition step would move {count} samples under an exclusive lock (limit {MAX_MOVE}). \
         Review the future-dated or staged samples during a maintenance window"
    ));
    pending(conn, &mut report).await?;
    Ok(report)
}

/// When a check of step 2 stands on the plain table, put its cutoff in the report and say in the note
/// what it means until a run finishes or takes it back.
async fn pending(conn: &mut PgConnection, report: &mut Report) -> Result<()> {
    let Some(cutoff) = current_bound(conn).await? else {
        report.cutoff = None;
        return Ok(());
    };
    report.cutoff = Some(cutoff);
    // the app's role may not read the table the owner's run made
    let readable: Option<bool> = sqlx::query_scalar("select has_table_privilege(to_regclass($1), 'select')")
        .bind(AHEAD)
        .fetch_one(&mut *conn)
        .await?;
    let aside = match readable {
        Some(true) => {
            let n: i64 = sqlx::query_scalar(AssertSqlSafe(format!("select count(*) from {AHEAD}")))
                .fetch_one(&mut *conn)
                .await?;
            format!(", and {n} samples are set aside from the raw table")
        }
        Some(false) => ", and the samples are set aside from the raw table".into(),
        None => String::new(),
    };
    let wait = format!(
        "Until a run as the table's owner finishes the switch, a sample dated on or after {} is refused and its import waits{aside}",
        cutoff.format("%Y-%m-%d")
    );
    report.note = Some(match report.note.take() {
        Some(n) => format!("{n}. {wait}"),
        None => wait,
    });
    Ok(())
}

/// Use the planner's estimate on the ordinary path. Once a cutoff is live, the owner counts
/// the live and staged rows in one snapshot to decide whether the threshold still applies.
async fn row_count(conn: &mut PgConnection, prepared: bool) -> Result<i64> {
    if prepared {
        let staged = if exists(conn, AHEAD).await? {
            format!(" + (select count(*) from {AHEAD} h where {STAGED_SAMPLE_IS_ACTIVE})")
        } else {
            String::new()
        };
        return Ok(
            sqlx::query_scalar(AssertSqlSafe(format!("select (select count(*) from {TABLE}){staged}")))
                .fetch_one(conn)
                .await?,
        );
    }
    let estimate: f64 = sqlx::query_scalar("select reltuples::float8 from pg_class where oid = to_regclass($1)")
        .bind(TABLE)
        .fetch_one(&mut *conn)
        .await?;
    if estimate >= 0.0 {
        return Ok(estimate as i64);
    }
    Ok(
        sqlx::query_scalar(AssertSqlSafe(format!("select count(*) from {TABLE}")))
            .fetch_one(conn)
            .await?,
    )
}

/// Whether this role may change the table: it is the owner (or a member of the owner's role) and may
/// create tables in its schema.
async fn may_change(conn: &mut PgConnection) -> Result<bool> {
    Ok(sqlx::query_scalar(
        "select pg_has_role(c.relowner, 'USAGE') and has_schema_privilege(c.relnamespace, 'CREATE') \
         from pg_class c where c.oid = to_regclass($1)",
    )
    .bind(TABLE)
    .fetch_one(conn)
    .await?)
}

/// The cutoff a stopped run left on the plain table, from the comment of its check.
async fn current_bound(conn: &mut PgConnection) -> Result<Option<DateTime<Utc>>> {
    let row: Option<Option<String>> = sqlx::query_scalar(
        "select obj_description(oid, 'pg_constraint') from pg_constraint where conrelid = to_regclass($1) \
         and conname = $2",
    )
    .bind(TABLE)
    .bind(BOUND)
    .fetch_optional(conn)
    .await?;
    // a check without a readable cutoff reads as stale, so it is taken back and made again
    Ok(row.map(|text| {
        text.and_then(|t| DateTime::parse_from_rfc3339(&t).ok())
            .map_or(DateTime::<Utc>::MIN_UTC, |t| t.with_timezone(&Utc))
    }))
}

fn reusable_cutoff(bound: Option<DateTime<Utc>>, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
    bound.filter(|c| *c > now + chrono::Duration::hours(STALE_HOURS))
}

/// Run `step` in a transaction that waits at most `lock_timeout` for each lock. When a wait runs out,
/// roll back, pause and try again, `attempts` times in all; then `None`: busy.
async fn retrying<T>(
    conn: &mut PgConnection,
    opts: &Options,
    mut step: impl for<'c> FnMut(&'c mut PgConnection) -> BoxFuture<'c, Result<T>>,
) -> Result<Option<T>> {
    for attempt in 0..opts.attempts.max(1) {
        if attempt > 0 {
            tokio::time::sleep(opts.lock_timeout * attempt).await;
        }
        let mut tx = conn.begin().await?;
        exec(
            &mut tx,
            format!("set local lock_timeout = {}", opts.lock_timeout.as_millis().max(1)),
        )
        .await?;
        match step(&mut tx).await {
            Ok(v) => {
                tx.commit().await?;
                return Ok(Some(v));
            }
            Err(e) if lock_timed_out(&e) => tx.rollback().await?,
            Err(e) => return Err(e),
        }
    }
    Ok(None)
}

async fn lock_table(conn: &mut PgConnection, table: &str) -> Result<()> {
    exec(conn, format!("lock table {table} in access exclusive mode")).await?;
    Ok(())
}

enum Move<T> {
    Done(T),
    TooMany(i64),
}

async fn future_count(conn: &mut PgConnection, cutoff: DateTime<Utc>) -> Result<i64> {
    Ok(
        sqlx::query_scalar(AssertSqlSafe(format!("select count(*) from {TABLE} where t >= $1")))
            .bind(cutoff)
            .fetch_one(conn)
            .await?,
    )
}

async fn staged_count(conn: &mut PgConnection) -> Result<i64> {
    if !exists(conn, AHEAD).await? {
        return Ok(0);
    }
    Ok(
        sqlx::query_scalar(AssertSqlSafe(format!("select count(*) from {AHEAD}")))
            .fetch_one(conn)
            .await?,
    )
}

/// Step 2: set the samples at or after the cutoff aside and add the check.
async fn add_bound(conn: &mut PgConnection, cutoff: DateTime<Utc>) -> Result<Move<u64>> {
    // Freeze the count against new inserts while still allowing readers.
    exec(conn, format!("lock table {TABLE} in share row exclusive mode")).await?;
    exec(conn, "set local enable_seqscan = off").await?;
    let late = future_count(conn, cutoff).await?;
    if late > MAX_MOVE {
        return Ok(Move::TooMany(late));
    }
    if late != 0 {
        // Adding the foreign key can wait for a writer on profile. Readers of the live
        // samples can continue while this transaction creates the staging table.
        exec(
            conn,
            format!(
                "create table {AHEAD} (like {TABLE} including defaults, \
                 constraint {AHEAD_PROFILE_FK} foreign key (profile_id) references profile(id) on delete cascade)"
            ),
        )
        .await?;
        remove_default_grants(conn, AHEAD).await?;
        copy_grants(conn, TABLE, AHEAD, &["SELECT", "DELETE"]).await?;
        same_owner(conn, AHEAD, TABLE).await?;
    }
    lock_table(conn, TABLE).await?;
    let aside = if late != 0 {
        sqlx::query(AssertSqlSafe(format!(
            "with gone as (delete from {TABLE} where t >= $1 returning *) insert into {AHEAD} select * from gone"
        )))
        .bind(cutoff)
        .execute(&mut *conn)
        .await?
        .rows_affected()
    } else {
        0
    };
    exec(
        conn,
        format!(
            "alter table {TABLE} add constraint {BOUND} check (t < {}) not valid",
            lit(cutoff)
        ),
    )
    .await?;
    exec(
        conn,
        format!("comment on constraint {BOUND} on {TABLE} is '{}'", cutoff.to_rfc3339()),
    )
    .await?;
    Ok(Move::Done(aside))
}

/// Add the deletion and access rules to a staging table left by an older preparation.
async fn upgrade_staging(conn: &mut PgConnection, opts: &Options) -> Result<bool> {
    let legacy: bool = sqlx::query_scalar(
        "select to_regclass($1) is not null and not exists \
         (select 1 from pg_constraint where conrelid = to_regclass($1) and conname = $2)",
    )
    .bind(AHEAD)
    .bind(AHEAD_PROFILE_FK)
    .fetch_one(&mut *conn)
    .await?;
    if !legacy {
        return Ok(true);
    }
    Ok(retrying(conn, opts, |c| Box::pin(upgrade_legacy_staging(c)))
        .await?
        .is_some())
}

async fn upgrade_legacy_staging(conn: &mut PgConnection) -> Result<()> {
    exec(conn, format!("lock table {AHEAD} in share row exclusive mode")).await?;
    exec(
        conn,
        format!("delete from {AHEAD} h where not exists (select 1 from profile p where p.id = h.profile_id)"),
    )
    .await?;
    exec(
        conn,
        format!(
            "alter table {AHEAD} add constraint {AHEAD_PROFILE_FK} \
             foreign key (profile_id) references profile(id) on delete cascade"
        ),
    )
    .await?;
    remove_default_grants(conn, AHEAD).await?;
    copy_grants(conn, TABLE, AHEAD, &["SELECT", "DELETE"]).await
}

/// Undo step 2 on the plain table: drop the check and put the samples set aside back.
async fn take_back(conn: &mut PgConnection) -> Result<Move<u64>> {
    if let Some(count) = lock_for_restore(conn).await? {
        return Ok(Move::TooMany(count));
    }
    exec(conn, format!("alter table {TABLE} drop constraint if exists {BOUND}")).await?;
    Ok(Move::Done(put_back(conn).await?))
}

async fn lock_for_restore(conn: &mut PgConnection) -> Result<Option<i64>> {
    if exists(conn, AHEAD).await? {
        // Match the app's lock order: the live table before staging.
        exec(conn, format!("lock table {TABLE} in share row exclusive mode")).await?;
        exec(conn, format!("lock table {AHEAD} in share row exclusive mode")).await?;
        let staged = staged_count(conn).await?;
        if staged > MAX_MOVE {
            return Ok(Some(staged));
        }
    }
    lock_table(conn, TABLE).await?;
    Ok(None)
}

type StagedSpan = (uuid::Uuid, String, i16, DateTime<Utc>, DateTime<Utc>);

async fn reconcile_before_restore(conn: &mut PgConnection, opts: &Options) -> Result<bool> {
    if !exists(conn, AHEAD).await? {
        return Ok(true);
    }
    Ok(retrying(conn, opts, |c| Box::pin(reconcile_staged_rollups(c)))
        .await?
        .is_some())
}

async fn reconcile_staged_rollups(conn: &mut PgConnection) -> Result<()> {
    exec(conn, format!("lock table only {TABLE} in access share mode")).await?;
    let spans: Vec<StagedSpan> = sqlx::query_as(AssertSqlSafe(format!(
        "select h.profile_id, h.metric, h.source_id, min(h.t), max(h.t) from {AHEAD} h \
         join profile p on p.id = h.profile_id join wearable_source ws on ws.id = h.source_id \
         group by h.profile_id, h.metric, h.source_id order by h.profile_id, h.metric, h.source_id"
    )))
    .fetch_all(&mut *conn)
    .await?;
    for (profile, metric, source, first, last) in spans {
        wearables::recompute_rollups_between(conn, profile, Some(&[metric]), Some(&[source]), Some((first, last)))
            .await?;
    }
    Ok(())
}

/// Restore active samples. Move and retraction withdraw staged samples on the same path as live
/// ones. The filter also covers staging created by an earlier version of the job.
async fn put_back(conn: &mut PgConnection) -> Result<u64> {
    if !exists(conn, AHEAD).await? {
        return Ok(0);
    }
    let n = exec(
        conn,
        format!("insert into {TABLE} select h.* from {AHEAD} h where {STAGED_SAMPLE_IS_ACTIVE} on conflict do nothing"),
    )
    .await?;
    exec(conn, format!("drop table {AHEAD}")).await?;
    Ok(n)
}

struct Switched {
    made: Vec<String>,
    moved: u64,
    held_back: u64,
}

/// Step 4, in the caller's transaction.
async fn switch(conn: &mut PgConnection, cutoff: DateTime<Utc>, now: DateTime<Utc>) -> Result<Move<Switched>> {
    if let Some(count) = lock_for_restore(conn).await? {
        return Ok(Move::TooMany(count));
    }
    let kind: String = sqlx::query_scalar("select relkind::text from pg_class where oid = to_regclass($1)")
        .bind(TABLE)
        .fetch_one(&mut *conn)
        .await?;
    if kind == "p" {
        return Ok(Move::Done(Switched {
            made: Vec::new(),
            moved: 0,
            held_back: 0,
        }));
    }
    // without a valid check the ATTACH would read every row while it holds the lock
    let valid: Option<bool> =
        sqlx::query_scalar("select convalidated from pg_constraint where conrelid = to_regclass($1) and conname = $2")
            .bind(TABLE)
            .bind(BOUND)
            .fetch_optional(&mut *conn)
            .await?;
    if valid != Some(true) {
        return Err(Error::PartitionState(format!(
            "{BOUND} is missing or not validated: the switch would scan {TABLE} under its lock"
        )));
    }
    exec(conn, format!("alter table {TABLE} rename to {HIST}")).await?;
    // the parent takes the plain names: wearable_sample_x becomes wearable_sample_hist_x
    let indexes: Vec<String> = sqlx::query_scalar(
        "select c.relname::text from pg_index i join pg_class c on c.oid = i.indexrelid \
         where i.indrelid = to_regclass($1) order by 1",
    )
    .bind(HIST)
    .fetch_all(&mut *conn)
    .await?;
    let plain = format!("{TABLE}_");
    let hist = format!("{HIST}_");
    for name in indexes {
        if let Some(rest) = name.strip_prefix(&plain)
            && !name.starts_with(&hist)
        {
            exec(
                conn,
                format!(
                    "alter index {} rename to {}",
                    quote(&name),
                    quote(&format!("{hist}{rest}"))
                ),
            )
            .await?;
        }
    }
    exec(
        conn,
        format!("create table {TABLE} (like {HIST} including all) partition by range (t)"),
    )
    .await?;
    exec(conn, format!("alter table {TABLE} drop constraint {BOUND}")).await?;
    let fks: Vec<(String, String)> = sqlx::query_as(
        "select conname::text, pg_get_constraintdef(oid) from pg_constraint where conrelid = to_regclass($1) \
         and contype = 'f' order by conname",
    )
    .bind(HIST)
    .fetch_all(&mut *conn)
    .await?;
    for (name, def) in fks {
        exec(
            conn,
            format!("alter table {TABLE} add constraint {} {def}", quote(&name)),
        )
        .await?;
    }
    // the valid check proves the partition constraint: no scan, and every index and foreign key of
    // the old table is attached as it is
    exec(
        conn,
        format!(
            "alter table {TABLE} attach partition {HIST} for values from (minvalue) to ({})",
            lit(cutoff)
        ),
    )
    .await?;
    exec(conn, format!("alter table {HIST} drop constraint {BOUND}")).await?;
    // Keep the old table's access policy, even when the creating role has default grants.
    remove_default_grants(conn, TABLE).await?;
    copy_grants(conn, HIST, TABLE, &[]).await?;
    same_owner(conn, TABLE, HIST).await?;
    exec(
        conn,
        format!("create table {DEFAULT_PART} partition of {TABLE} default"),
    )
    .await?;
    remove_default_grants(conn, DEFAULT_PART).await?;
    same_owner(conn, DEFAULT_PART, TABLE).await?;
    let (made, moved) = make_months(conn, now).await?;
    let held_back = put_back(conn).await?;
    Ok(Move::Done(Switched { made, moved, held_back }))
}

/// Revoke privileges that ALTER DEFAULT PRIVILEGES would otherwise add to a newly created table.
async fn remove_default_grants(conn: &mut PgConnection, table: &str) -> Result<()> {
    let revokes: Vec<String> = sqlx::query_scalar(
        "select distinct format('revoke all on %I from %s cascade', c.relname, \
         case when a.grantee = 0 then 'public' else quote_ident(r.rolname) end) \
         from pg_class c cross join lateral aclexplode(c.relacl) a left join pg_roles r on r.oid = a.grantee \
         where c.oid = to_regclass($1) and a.grantee <> c.relowner order by 1",
    )
    .bind(table)
    .fetch_all(&mut *conn)
    .await?;
    for revoke in revokes {
        exec(conn, revoke).await?;
    }
    Ok(())
}

async fn copy_grants(conn: &mut PgConnection, from: &str, to: &str, privileges: &[&str]) -> Result<()> {
    let grants: Vec<String> = sqlx::query_scalar(
        "select format('grant %s on %I to %s%s', a.privilege_type, $2::text, \
         case when a.grantee = 0 then 'public' else quote_ident(r.rolname) end, \
         case when a.is_grantable then ' with grant option' else '' end) \
         from pg_class c cross join lateral aclexplode(c.relacl) a left join pg_roles r on r.oid = a.grantee \
         where c.oid = to_regclass($1) and a.grantee <> c.relowner \
         and (cardinality($3::text[]) = 0 or a.privilege_type = any($3)) order by 1",
    )
    .bind(from)
    .bind(to)
    .bind(privileges)
    .fetch_all(&mut *conn)
    .await?;
    for grant in grants {
        exec(conn, grant).await?;
    }
    Ok(())
}

/// Give `table` the owner of `like` when they differ (a run by a member of the owner's role).
async fn same_owner(conn: &mut PgConnection, table: &str, like: &str) -> Result<()> {
    let owner: Option<String> = sqlx::query_scalar(
        "select quote_ident(pg_get_userbyid(o.relowner)) from pg_class t, pg_class o \
         where t.oid = to_regclass($1) and o.oid = to_regclass($2) and t.relowner <> o.relowner",
    )
    .bind(table)
    .bind(like)
    .fetch_optional(&mut *conn)
    .await?;
    if let Some(owner) = owner {
        exec(conn, format!("alter table {table} owner to {owner}")).await?;
    }
    Ok(())
}

/// The months that need a partition: from the end of the historical one through next month, and at
/// least the first month after it. Those that exist already are left out.
async fn missing_months(conn: &mut PgConnection, now: DateTime<Utc>) -> Result<Vec<DateTime<Utc>>> {
    let upper: Option<DateTime<Utc>> = sqlx::query_scalar(
        "select (regexp_match(pg_get_expr(c.relpartbound, c.oid), 'TO \\(''([^'']+)''\\)'))[1]::timestamptz \
         from pg_class c where c.oid = to_regclass($1)",
    )
    .bind(HIST)
    .fetch_one(&mut *conn)
    .await?;
    let first = month_start(upper.ok_or_else(|| Error::PartitionState(format!("{HIST} has no upper bound")))?);
    let last = add_months(month_start(now), 1).max(first);
    let mut out = Vec::new();
    let mut m = first;
    while m <= last {
        if !exists(conn, &month_name(m)).await? {
            out.push(m);
        }
        m = add_months(m, 1);
    }
    Ok(out)
}

/// Make the missing months, in the caller's transaction. Samples that wait in DEFAULT for a month
/// move into it. Returns the partitions made and the samples moved.
async fn make_months(conn: &mut PgConnection, now: DateTime<Utc>) -> Result<(Vec<String>, u64)> {
    let mut made = Vec::new();
    let mut moved = 0;
    for m in missing_months(conn, now).await? {
        let end = add_months(m, 1);
        let name = month_name(m);
        let waiting: bool = sqlx::query_scalar(AssertSqlSafe(format!(
            "select exists (select 1 from {DEFAULT_PART} where t >= $1 and t < $2)"
        )))
        .bind(m)
        .bind(end)
        .fetch_one(&mut *conn)
        .await?;
        if waiting {
            exec(
                conn,
                format!("create temp table oh_partition_move (like {TABLE}) on commit drop"),
            )
            .await?;
            moved += sqlx::query(AssertSqlSafe(format!(
                "with gone as (delete from {DEFAULT_PART} where t >= $1 and t < $2 returning *) \
                 insert into oh_partition_move select * from gone"
            )))
            .bind(m)
            .bind(end)
            .execute(&mut *conn)
            .await?
            .rows_affected();
        }
        exec(
            conn,
            format!(
                "create table {name} partition of {TABLE} for values from ({}) to ({})",
                lit(m),
                lit(end)
            ),
        )
        .await?;
        remove_default_grants(conn, &name).await?;
        same_owner(conn, &name, TABLE).await?;
        if waiting {
            exec(conn, format!("insert into {TABLE} select * from oh_partition_move")).await?;
            exec(conn, "drop table oh_partition_move").await?;
        }
        made.push(name);
    }
    Ok((made, moved))
}

/// Step 5, on a partitioned table: make next month (and any month missing before it) ahead of time.
async fn ahead(conn: &mut PgConnection, opts: &Options, now: DateTime<Utc>) -> Result<Report> {
    let mut report = Report::of("partitioned");
    if missing_months(conn, now).await?.is_empty() {
        return Ok(report);
    }
    if !may_change(conn).await? {
        report.state = "needs_owner";
        report.note = Some(format!(
            "next month's partition of {TABLE} is missing, and this database role does not own the table: run `ownhealth partition` as the database owner"
        ));
        return Ok(report);
    }
    match retrying(conn, opts, move |c| Box::pin(make_months_locked(c, now))).await? {
        Some((made, moved)) => {
            report.made = made;
            report.moved_from_default = moved;
            Ok(report)
        }
        None => Ok(Report {
            note: Some(HELD.into()),
            ..Report::of("busy")
        }),
    }
}

async fn make_months_locked(conn: &mut PgConnection, now: DateTime<Utc>) -> Result<(Vec<String>, u64)> {
    lock_table(conn, TABLE).await?;
    make_months(conn, now).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(y: i32, m: u32, d: u32, h: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(y, m, d, h, 0, 0)
            .single()
            .unwrap_or(DateTime::<Utc>::MIN_UTC)
    }

    #[test]
    fn months_are_utc_month_starts_with_stable_names() {
        assert_eq!(month_start(at(2026, 11, 24, 13)), at(2026, 11, 1, 0));
        assert_eq!(add_months(at(2026, 12, 1, 0), 1), at(2027, 1, 1, 0));
        assert_eq!(add_months(at(2027, 1, 1, 0), -1), at(2026, 12, 1, 0));
        assert_eq!(add_months(at(2026, 1, 1, 0), 14), at(2027, 3, 1, 0));
        assert_eq!(month_name(at(2027, 1, 1, 0)), "wearable_sample_y2027m01");
        assert_eq!(month_name(at(2026, 12, 1, 0)), "wearable_sample_y2026m12");
    }

    #[test]
    fn the_cutoff_is_a_month_start_at_least_seven_days_ahead() {
        let mut now = at(2026, 1, 1, 0);
        while now < at(2028, 1, 1, 0) {
            let c = cutoff_for(now);
            assert_eq!(c, month_start(c), "{now}");
            assert!(c - now >= chrono::Duration::days(MARGIN_DAYS), "{now} -> {c}");
            assert!(c - now <= chrono::Duration::days(39), "{now} -> {c}");
            now += chrono::Duration::hours(7);
        }
        assert_eq!(cutoff_for(at(2026, 11, 10, 0)), at(2026, 12, 1, 0));
        assert_eq!(cutoff_for(at(2026, 11, 24, 12)), at(2027, 1, 1, 0));
    }

    #[test]
    fn ddl_literals_are_utc_and_quoted() {
        assert_eq!(lit(at(2027, 1, 1, 0)), "'2027-01-01 00:00:00.000000+00'::timestamptz");
        assert_eq!(quote("a\"b"), "\"a\"\"b\"");
    }
    #[tokio::test]
    async fn a_cutoff_is_rechecked_with_the_clock_after_index_work() -> anyhow::Result<()> {
        let db = ownhealth_core::testing::TestDb::empty().await?;
        sqlx::query("create table wearable_sample(t timestamptz not null, document_id uuid)")
            .execute(&db.pool)
            .await?;
        sqlx::query("insert into wearable_sample(t) values ('2026-11-01')")
            .execute(&db.pool)
            .await?;
        let opts = Options::new(0);
        let then = at(2026, 11, 15, 0);
        let report = locked(&db.pool, &opts, true, || then).await?;
        assert_eq!(report["cutoff"], cutoff_for(then).to_rfc3339());
        sqlx::query("drop index wearable_sample_hist_t_idx")
            .execute(&db.pool)
            .await?;
        let before = at(2026, 11, 29, 0);
        let after = at(2026, 12, 1, 1);
        let time = std::cell::Cell::new(before);
        let report = locked(&db.pool, &opts, true, || time.replace(after)).await?;
        assert_eq!(report["cutoff"], cutoff_for(after).to_rfc3339());
        Ok(())
    }

    #[tokio::test]
    async fn an_invalid_partition_state_keeps_its_typed_error_and_operator_message() -> anyhow::Result<()> {
        let db = ownhealth_core::testing::TestDb::empty().await?;
        sqlx::query("create view wearable_sample as select 1 as id")
            .execute(&db.pool)
            .await?;
        let err = run(&db.pool, &Options::new(0))
            .await
            .expect_err("wearable_sample is a view");
        assert!(matches!(err, Error::PartitionState(_)), "{err:?}");
        assert_eq!(
            err.to_string(),
            "wearable_sample is missing or not a table: run `ownhealth migrate` first"
        );
        Ok(())
    }
}
