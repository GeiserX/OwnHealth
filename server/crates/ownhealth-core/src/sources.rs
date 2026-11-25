//! The source registry and its scheduler (connectors.md 6.1, ingestion.md 3): one row per configured
//! inbox or device stream, each run recorded, and the health a person (or an alert) reads: when did it
//! last work, and what was the last error.
//!
//! Polled kinds register a [`Poller`]; the scheduler enqueues one `source.poll` job per due source on
//! the existing job queue (dedupe key `source.poll:<id>`), and the job records the run around the
//! poller. Push kinds (Paperless webhook, share target, Shortcut, Health Auto Export) call
//! [`begin_run`] and [`finish_run`] themselves, so every source reports health the same way.

use std::collections::HashMap;
use std::sync::{LazyLock, RwLock};
use std::time::Duration;

use chrono::{DateTime, Utc};
use futures::future::BoxFuture;
use serde::Serialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::{PgConnection, Postgres, Transaction};
use tokio::sync::watch;
use uuid::Uuid;

use crate::error::{Error, Result};
use crate::jobs::{self, EnqueueOptions, Job, JobContext, JobError};

/// The `source.kind` check constraint.
pub const SOURCE_KINDS: [&str; 14] = [
    "email",
    "folder",
    "paperless",
    "share",
    "shortcut",
    "health_auto_export",
    "gadgetbridge",
    "nightscout",
    "clinvar",
    "smart",
    "bluebutton",
    "brands",
    "openwearables",
    "dexcom",
];
/// Kinds a connector makes through its own route (the OAuth launch, the brands directory, the Open
/// Wearables setup), never POST /api/inboxes.
pub const CONNECTOR_KINDS: [&str; 5] = ["smart", "bluebutton", "brands", "openwearables", "dexcom"];
pub const POLL_JOB: &str = "source.poll";
/// How often the scheduler looks for due sources.
pub const SCHEDULER_SECONDS: u64 = 30;
/// A source with no success for this many poll intervals is stale in the health report.
pub const STALE_INTERVALS: i32 = 3;
pub const TOKEN_PREFIX: &str = "oh_ing_";

const COLS: &str = "id, household_id, profile_id, kind, label, config, secret_id, token_prefix, enabled, poll_seconds, \
    cursor, last_run_at, last_success_at, last_error_at, last_error, consecutive_failures, \
    created_by, created_at, oauth_grant_id";

#[derive(Debug, Clone, PartialEq, Serialize, sqlx::FromRow)]
pub struct Source {
    pub id: Uuid,
    pub household_id: Uuid,
    pub profile_id: Option<Uuid>,
    pub kind: String,
    pub label: String,
    pub config: Value,
    pub secret_id: Option<Uuid>,
    pub token_prefix: Option<String>,
    pub enabled: bool,
    pub poll_seconds: Option<i32>,
    pub cursor: Option<Value>,
    pub last_run_at: Option<DateTime<Utc>>,
    pub last_success_at: Option<DateTime<Utc>>,
    pub last_error_at: Option<DateTime<Utc>>,
    pub last_error: Option<String>,
    pub consecutive_failures: i32,
    pub created_by: Option<Uuid>,
    pub created_at: DateTime<Utc>,
    /// The grant in effect when this source snapshot was loaded.
    #[serde(skip_serializing)]
    pub oauth_grant_id: Option<Uuid>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct NewSource<'a> {
    pub household_id: Uuid,
    pub profile_id: Option<Uuid>,
    pub kind: &'a str,
    pub label: &'a str,
    pub config: Value,
    pub secret_id: Option<Uuid>,
    pub poll_seconds: Option<i32>,
    pub created_by: Option<Uuid>,
}

impl<'a> NewSource<'a> {
    pub fn new(household_id: Uuid, kind: &'a str, label: &'a str) -> Self {
        NewSource {
            household_id,
            profile_id: None,
            kind,
            label,
            config: json!({}),
            secret_id: None,
            poll_seconds: None,
            created_by: None,
        }
    }
}

/// A change to a source; `None` leaves the field as it is.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SourcePatch {
    pub label: Option<String>,
    pub profile_id: Option<Option<Uuid>>,
    pub config: Option<Value>,
    pub secret_id: Option<Option<Uuid>>,
    pub enabled: Option<bool>,
    pub poll_seconds: Option<Option<i32>>,
}

fn check_kind(kind: &str) -> Result<()> {
    if SOURCE_KINDS.contains(&kind) {
        Ok(())
    } else {
        Err(Error::Invalid(format!("unknown source kind {kind:?}")))
    }
}

pub async fn create_source(conn: &mut PgConnection, s: &NewSource<'_>) -> Result<Uuid> {
    check_kind(s.kind)?;
    if s.poll_seconds.is_some_and(|p| p < 60) {
        return Err(Error::Invalid("poll_seconds must be at least 60".into()));
    }
    Ok(sqlx::query_scalar(
        "insert into source (household_id, profile_id, kind, label, config, secret_id, poll_seconds, created_by) \
         values ($1, $2, $3, $4, $5, $6, $7, $8) returning id",
    )
    .bind(s.household_id)
    .bind(s.profile_id)
    .bind(s.kind)
    .bind(s.label)
    .bind(&s.config)
    .bind(s.secret_id)
    .bind(s.poll_seconds)
    .bind(s.created_by)
    .fetch_one(conn)
    .await?)
}

pub async fn get_source(conn: &mut PgConnection, id: Uuid) -> Result<Option<Source>> {
    query_source(conn, id, false).await
}

async fn query_source(conn: &mut PgConnection, id: Uuid, for_update: bool) -> Result<Option<Source>> {
    let lock = if for_update { " for update" } else { "" };
    let sql = format!("select {COLS} from source where id = $1{lock}");
    Ok(sqlx::query_as(sqlx::AssertSqlSafe(sql))
        .bind(id)
        .fetch_optional(conn)
        .await?)
}

pub async fn list_sources(conn: &mut PgConnection, household_id: Uuid) -> Result<Vec<Source>> {
    let sql = format!("select {COLS} from source where household_id = $1 order by kind, label, created_at");
    Ok(sqlx::query_as(sqlx::AssertSqlSafe(sql))
        .bind(household_id)
        .fetch_all(conn)
        .await?)
}

pub async fn update_source(conn: &mut PgConnection, id: Uuid, p: &SourcePatch) -> Result<Source> {
    if p.poll_seconds.is_some_and(|ps| ps.is_some_and(|s| s < 60)) {
        return Err(Error::Invalid("poll_seconds must be at least 60".into()));
    }
    let sql = format!(
        "update source set label = coalesce($2, label), \
           profile_id = case when $3 then $4 else profile_id end, \
           config = coalesce($5, config), \
           secret_id = case when $6 then $7 else secret_id end, \
           enabled = coalesce($8, enabled), \
           poll_seconds = case when $9 then $10 else poll_seconds end \
         where id = $1 returning {COLS}"
    );
    let mut tx = sqlx::Connection::begin(conn).await?;
    let row: Option<Source> = sqlx::query_as(sqlx::AssertSqlSafe(sql))
        .bind(id)
        .bind(&p.label)
        .bind(p.profile_id.is_some())
        .bind(p.profile_id.flatten())
        .bind(&p.config)
        .bind(p.secret_id.is_some())
        .bind(p.secret_id.flatten())
        .bind(p.enabled)
        .bind(p.poll_seconds.is_some())
        .bind(p.poll_seconds.flatten())
        .fetch_optional(&mut *tx)
        .await?;
    let row = row.ok_or_else(|| Error::NotFound("Unknown source.".into()))?;
    if p.enabled == Some(false) {
        // The update holds the row lock a gap check takes, and this statement reads with a fresh snapshot,
        // so it also ends an alert that check filed while the update waited. Switching the source back on
        // before the next check then starts a new episode.
        end_gap_episode(&mut tx, id, row.household_id).await?;
    }
    tx.commit().await?;
    Ok(row)
}

/// End the episode of a switched-off source's sync alert: an open alert closes as unavailable, and a
/// decided one keeps the person's decision. Either way the next gap opens a new alert. The stamp comes
/// from the clock the gap checks read, not the database's, since a check compares it with its own.
async fn end_gap_episode(conn: &mut PgConnection, id: Uuid, household_id: Uuid) -> Result<()> {
    sqlx::query(
        "update review_item set payload = jsonb_set(payload, '{ended_at}', to_jsonb($3::timestamptz)), \
         decided_at = coalesce(decided_at, $3), \
         decision_reason = case when status = 'open' then 'source_unavailable' else decision_reason end, \
         status = case when status = 'open' then 'dismissed' else status end \
         where household_id = $2 and kind = 'source_gap' and ref_table = 'source' and ref_id = $1 \
         and payload->>'ended_at' is null and (status = 'open' or decided_by is not null)",
    )
    .bind(id)
    .bind(household_id)
    .bind(Utc::now())
    .execute(conn)
    .await?;
    Ok(())
}

pub async fn delete_source(conn: &mut PgConnection, id: Uuid) -> Result<bool> {
    let mut tx = sqlx::Connection::begin(conn).await?;
    // Take the row lock a gap check holds first. The next statement then reads with a fresh snapshot,
    // so it sees any alert that check filed while this delete waited.
    sqlx::query("select 1 from source where id = $1 for update")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    // A review reference is polymorphic. Close the alert in the same statement that removes
    // the source, so a disconnected device cannot leave an open review item behind.
    let removed = sqlx::query_scalar(
        "with removed as (delete from source where id = $1 returning id, household_id), \
         closed as (update review_item i set status = 'dismissed', decided_at = now(), \
           decision_reason = 'source_unavailable' from removed s where i.household_id = s.household_id \
           and i.kind = 'source_gap' and i.ref_table = 'source' and i.ref_id = s.id and i.status = 'open') \
         select exists (select 1 from removed)",
    )
    .bind(id)
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(removed)
}

/// Save the position a poller reached, in the caller's transaction (connectors.md 6.3 rule 2: in the
/// same transaction as the item it follows).
pub async fn save_cursor(conn: &mut PgConnection, id: Uuid, cursor: &Value) -> Result<()> {
    sqlx::query("update source set cursor = $2 where id = $1")
        .bind(id)
        .bind(cursor)
        .execute(conn)
        .await?;
    Ok(())
}

// --------------------------------------------------------- push tokens ----

fn random_id8() -> String {
    use rand::RngExt;
    const ALNUM: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
    let mut rng = rand::rng();
    (0..8)
        .map(|_| ALNUM[rng.random_range(0..ALNUM.len())] as char)
        .collect()
}

/// A new bearer token for a push source, `oh_ing_<id8>_<secret43>`, replacing any earlier one. The
/// plaintext is returned once; the row keeps the prefix and sha256 of the secret.
pub async fn issue_token(conn: &mut PgConnection, source_id: Uuid) -> Result<String> {
    use base64::Engine as _;
    let mut raw = [0u8; 32];
    rand::fill(&mut raw[..]);
    let secret = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(raw);
    let prefix = format!("{TOKEN_PREFIX}{}", random_id8());
    let n = sqlx::query("update source set token_prefix = $2, token_sha256 = $3 where id = $1")
        .bind(source_id)
        .bind(&prefix)
        .bind(Sha256::digest(secret.as_bytes()).to_vec())
        .execute(conn)
        .await?
        .rows_affected();
    if n != 1 {
        return Err(Error::NotFound("Unknown source.".into()));
    }
    Ok(format!("{prefix}_{secret}"))
}

/// The enabled source a push token belongs to, or None. Constant-time on the secret.
pub async fn source_for_token(conn: &mut PgConnection, token: &str) -> Result<Option<Source>> {
    use subtle::ConstantTimeEq;
    let Some(rest) = token.strip_prefix(TOKEN_PREFIX) else {
        return Ok(None);
    };
    let Some((id8, secret)) = rest.split_once('_') else {
        return Ok(None);
    };
    if id8.len() != 8 || secret.is_empty() {
        return Ok(None);
    }
    let sql = format!("select {COLS}, token_sha256 from source where token_prefix = $1 and enabled");
    let row: Option<sqlx::postgres::PgRow> = sqlx::query(sqlx::AssertSqlSafe(sql))
        .bind(format!("{TOKEN_PREFIX}{id8}"))
        .fetch_optional(&mut *conn)
        .await?;
    let Some(row) = row else { return Ok(None) };
    let stored: Vec<u8> = sqlx::Row::try_get(&row, "token_sha256")?;
    let given = Sha256::digest(secret.as_bytes());
    if !bool::from(given.as_slice().ct_eq(&stored)) {
        return Ok(None);
    }
    Ok(Some(sqlx::FromRow::from_row(&row)?))
}

// ---------------------------------------------------------------- runs ----

/// One problem inside a run, without content: `{code, message}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RunProblem {
    pub code: String,
    pub message: String,
}

/// What a run did. `cursor`, when set, is saved with the run's end.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct RunOutcome {
    pub seen: i32,
    pub new: i32,
    pub duplicate: i32,
    pub quarantined: i32,
    pub problems: Vec<RunProblem>,
    pub cursor: Option<Value>,
    /// Items left out on purpose, counted by name (a metric OwnHealth does not chart yet).
    pub skipped: std::collections::BTreeMap<String, i64>,
    /// The error that stopped a run after it found `problems`: the run is an `error` with this as its
    /// error, and its record keeps the problems (an Open Wearables poll whose active vendors all
    /// failed still names the vendors whose connection ended).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, sqlx::FromRow)]
pub struct SourceRun {
    pub id: i64,
    pub source_id: Uuid,
    pub trigger: String,
    pub job_id: Option<i64>,
    pub started_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
    pub status: String,
    pub items_seen: i32,
    pub items_new: i32,
    pub items_duplicate: i32,
    pub items_quarantined: i32,
    pub errors: Value,
    pub error: Option<String>,
    pub skipped: Value,
}

/// Open a run (poll, push or manual), capturing the grant that owns it and stamping last_run_at.
pub async fn begin_run(conn: &mut PgConnection, source_id: Uuid, trigger: &str, job_id: Option<i64>) -> Result<i64> {
    Ok(sqlx::query_scalar(
        "with active as (update source set last_run_at = now() where id = $1 returning oauth_grant_id) \
         insert into source_run (source_id, trigger, job_id, oauth_grant_id) \
         values ($1, $2, $3, (select oauth_grant_id from active)) returning id",
    )
    .bind(source_id)
    .bind(trigger)
    .bind(job_id)
    .fetch_one(conn)
    .await?)
}

/// Close a run with what it did, or with the error that stopped it (type and message only, cut at
/// 500 characters). Updates the source's health and cursor only if the grant still matches.
/// A run with problems where something still arrived is `partial` and counts as a success for "last successful sync"; a run where every item it
/// saw failed is an `error` (nothing arrived, and the cursor stays so the items are retried).
/// The run row and the source row commit together, so a health read never sees one without the other.
pub async fn finish_run(
    conn: &mut PgConnection,
    run_id: i64,
    result: std::result::Result<&RunOutcome, &str>,
) -> Result<()> {
    let first_problem = |o: &RunOutcome| {
        o.problems.first().map(|p| {
            format!("{}: {}", p.code, p.message)
                .chars()
                .take(500)
                .collect::<String>()
        })
    };
    let all_failed = |o: &RunOutcome| o.seen > 0 && o.new + o.duplicate + o.quarantined == 0;
    let (status, outcome, error) = match result {
        Ok(o) if o.error.is_some() => (
            "error",
            Some(o),
            o.error.as_deref().map(|e| e.chars().take(500).collect::<String>()),
        ),
        Ok(o) if o.problems.is_empty() => ("ok", Some(o), None),
        Ok(o) if all_failed(o) => ("error", Some(o), first_problem(o)),
        Ok(o) => ("partial", Some(o), None),
        Err(e) => ("error", None, Some(e.chars().take(500).collect::<String>())),
    };
    let d = RunOutcome::default();
    let o = outcome.unwrap_or(&d);
    let mut tx = sqlx::Connection::begin(&mut *conn).await?;
    // Lock the source before its run, in the order a source delete takes them (the source, then its
    // runs by cascade) and begin_run writes them, so a delete never waits on this run while it waits on
    // the source.
    sqlx::query("select 1 from source where id = (select source_id from source_run where id = $1) for no key update")
        .bind(run_id)
        .execute(&mut *tx)
        .await?;
    let (source_id, grant_id): (Uuid, Option<Uuid>) = sqlx::query_as(
        "update source_run set finished_at = now(), status = $2, items_seen = $3, items_new = $4, \
         items_duplicate = $5, items_quarantined = $6, errors = $7, error = $8, skipped = $9 where id = $1 \
         returning source_id, oauth_grant_id",
    )
    .bind(run_id)
    .bind(status)
    .bind(o.seen)
    .bind(o.new)
    .bind(o.duplicate)
    .bind(o.quarantined)
    .bind(serde_json::to_value(&o.problems)?)
    .bind(&error)
    .bind(serde_json::to_value(&o.skipped)?)
    .fetch_one(&mut *tx)
    .await?;
    if let Some(e) = &error {
        sqlx::query(
            "update source set last_error_at = now(), last_error = $2, consecutive_failures = consecutive_failures + 1 \
             where id = $1 and oauth_grant_id is not distinct from $3",
        )
        .bind(source_id)
        .bind(e)
        .bind(grant_id)
        .execute(&mut *tx)
        .await?;
    } else {
        sqlx::query(
            "update source set last_success_at = now(), consecutive_failures = 0, cursor = coalesce($2, cursor) \
             where id = $1 and oauth_grant_id is not distinct from $3",
        )
        .bind(source_id)
        .bind(&o.cursor)
        .bind(grant_id)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(())
}

pub async fn list_runs(conn: &mut PgConnection, source_id: Uuid, limit: i64) -> Result<Vec<SourceRun>> {
    Ok(sqlx::query_as(
        "select id, source_id, trigger, job_id, started_at, finished_at, status, items_seen, items_new, \
         items_duplicate, items_quarantined, errors, error, skipped from source_run where source_id = $1 \
         order by started_at desc, id desc limit $2",
    )
    .bind(source_id)
    .bind(limit.clamp(1, 500))
    .fetch_all(conn)
    .await?)
}

// -------------------------------------------------------------- health ----

/// What the health endpoint and the inbox screen show per source.
#[derive(Debug, Clone, PartialEq, Serialize, sqlx::FromRow)]
pub struct SourceHealth {
    pub id: Uuid,
    pub kind: String,
    pub label: String,
    pub profile_id: Option<Uuid>,
    pub enabled: bool,
    pub last_run_at: Option<DateTime<Utc>>,
    pub last_success_at: Option<DateTime<Utc>>,
    pub last_error_at: Option<DateTime<Utc>>,
    pub last_error: Option<String>,
    pub consecutive_failures: i32,
    /// Enabled, polled, and no success within STALE_INTERVALS poll intervals (or never).
    pub stale: bool,
    /// The newest finished run of the current grant: `ok`, `partial` or `error`; none before the first run ends.
    pub last_run_status: Option<String>,
    /// How many problems that run had, and the first one as `code: message` (no content), or the
    /// error that stopped it.
    pub last_run_problems: i32,
    pub last_run_problem: Option<String>,
    /// What only a person who manages the source is sent (routes::inboxes adds it per caller).
    #[sqlx(flatten)]
    #[serde(skip_serializing)]
    pub managed: ManagedHealth,
}

/// The part of a health row for the people who manage the source, for the Connections screen (v1.0,
/// connectors.md 6.2). A viewer of the profile, or the source's own token, is not sent it.
#[derive(Debug, Clone, PartialEq, Serialize, sqlx::FromRow)]
pub struct ManagedHealth {
    /// With `poll_seconds`: a source with no run yet that was made less than one interval ago has
    /// no data yet, it is not stale.
    pub created_at: DateTime<Utc>,
    pub poll_seconds: Option<i32>,
    /// The vendor refused the token and the person must sign in again; when the person's grant at the
    /// vendor ends; when the current access token ends. None for an inbox.
    pub reconsent_needed_at: Option<DateTime<Utc>>,
    pub grant_expires_at: Option<DateTime<Utc>>,
    pub access_expires_at: Option<DateTime<Utc>>,
    /// Open sync-gap alerts of this source on its own profile: `source_gap` review items with
    /// `ref_table = 'source'`.
    pub open_gaps: i32,
    /// The server a connection talks to (`config.endpoints.issuer`; SMART: the FHIR base URL).
    pub server: Option<String>,
    /// Distinct claims the latest Blue Button pull read; absent when that pull failed or its claim
    /// search did not answer.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub claim_count: Option<i32>,
}

pub async fn source_health(conn: &mut PgConnection, household_id: Uuid) -> Result<Vec<SourceHealth>> {
    Ok(sqlx::query_as(
        "select s.id, s.kind, s.label, s.profile_id, s.enabled, s.last_run_at, s.last_success_at, s.last_error_at, \
         s.last_error, s.consecutive_failures, \
         (s.enabled and s.poll_seconds is not null and (s.last_success_at is null or \
           s.last_success_at < now() - make_interval(secs => s.poll_seconds * $2))) as stale, \
         r.status as last_run_status, coalesce(jsonb_array_length(r.errors), 0)::int4 as last_run_problems, \
         left(coalesce((r.errors -> 0 ->> 'code') || ': ' || (r.errors -> 0 ->> 'message'), r.error), 500) \
           as last_run_problem, \
         s.created_at, s.poll_seconds, s.reconsent_needed_at, s.grant_expires_at, s.access_expires_at, \
         (select count(*)::int4 from review_item g where g.household_id = s.household_id and g.status = 'open' \
           and g.kind = 'source_gap' and g.ref_table = 'source' and g.ref_id = s.id \
           and g.profile_id is not distinct from s.profile_id) as open_gaps, \
         s.config -> 'endpoints' ->> 'issuer' as server, \
         case when s.kind = 'bluebutton' and r.status is distinct from 'error' \
           then (s.cursor->>'claim_count')::int4 end as claim_count \
         from source s left join lateral (select status, errors, error from source_run \
           where source_id = s.id and oauth_grant_id is not distinct from s.oauth_grant_id \
           and finished_at is not null order by started_at desc, id desc limit 1) r on true \
         where s.household_id = $1 order by s.kind, s.label",
    )
    .bind(household_id)
    .bind(STALE_INTERVALS)
    .fetch_all(conn)
    .await?)
}

// ------------------------------------------------------ pollers and jobs ----

/// Reads one source: fetches what is new, stores the bytes, hands each item to
/// `arrive::arrive` (or its own importer), and says what it did. The job records the run around it.
pub type Poller = fn(JobContext, Source) -> BoxFuture<'static, std::result::Result<RunOutcome, JobError>>;

static POLLERS: LazyLock<RwLock<HashMap<String, Poller>>> = LazyLock::new(|| RwLock::new(HashMap::new()));

pub fn register_poller(kind: &str, poller: Poller) {
    if let Ok(mut m) = POLLERS.write() {
        m.insert(kind.to_string(), poller);
    }
}

/// Adds the sources a kind turns on by itself (ClinVar for a household with a genome). The scheduler
/// runs every one before it looks for due sources. A hook only adds rows that do not exist yet, so a
/// source a person switched off stays off.
pub type DefaultSources = fn(JobContext) -> BoxFuture<'static, Result<u64>>;

static DEFAULTS: LazyLock<RwLock<HashMap<String, DefaultSources>>> = LazyLock::new(|| RwLock::new(HashMap::new()));

pub fn register_default_sources(kind: &str, hook: DefaultSources) {
    if let Ok(mut m) = DEFAULTS.write() {
        m.insert(kind.to_string(), hook);
    }
}

/// Run every kind's default-sources hook. Returns the rows added. A hook that fails is logged and
/// skipped, so it stops neither the other hooks nor the scheduler's polls of the existing sources.
pub async fn add_default_sources(ctx: &JobContext) -> Result<u64> {
    let hooks: Vec<(String, DefaultSources)> = DEFAULTS
        .read()
        .map(|m| m.iter().map(|(k, h)| (k.clone(), *h)).collect())
        .unwrap_or_default();
    let mut added = 0;
    for (kind, hook) in hooks {
        match hook(ctx.clone()).await {
            Ok(n) => added += n,
            Err(e) => tracing::warn!(kind, error = %e, "a default-sources hook failed"),
        }
    }
    Ok(added)
}

pub fn poller_for(kind: &str) -> Option<Poller> {
    POLLERS.read().ok().and_then(|m| m.get(kind).copied())
}

fn polled_kinds() -> Vec<String> {
    POLLERS.read().map(|m| m.keys().cloned().collect()).unwrap_or_default()
}

/// The `source.poll` job: run the kind's poller between `begin_run` and `finish_run`. A failed poll is
/// recorded on the source and the job completes: the next schedule is the retry, so a source that is
/// down does not fill the queue with dead jobs.
async fn poll_job(ctx: JobContext, job: Job) -> jobs::HandlerResult {
    let source_id = job.uuid("source_id")?;
    // Keep the source snapshot and the run's grant together if a reconnect lands here.
    let mut tx = ctx.pool.begin().await?;
    let Some(source) = query_source(&mut tx, source_id, true).await? else {
        return Ok(Some(json!({"skipped": "unknown_source"})));
    };
    if !source.enabled {
        return Ok(Some(json!({"skipped": "disabled"})));
    }
    let poller = poller_for(&source.kind)
        .ok_or_else(|| JobError::permanent(format!("no poller for source kind {}", source.kind)))?;
    let run_id = begin_run(&mut tx, source_id, "poll", Some(job.id)).await?;
    tx.commit().await?;
    let result = poller(ctx.clone(), source).await;
    let mut conn = ctx.pool.acquire().await?;
    match &result {
        Ok(o) => finish_run(&mut conn, run_id, Ok(o)).await?,
        Err(e) => finish_run(&mut conn, run_id, Err(&e.text())).await?,
    }
    Ok(Some(match result {
        Ok(o) => {
            json!({"run_id": run_id, "status": if o.error.is_some() { "error" } else { "ok" }, "new": o.new, "duplicate": o.duplicate,
                        "quarantined": o.quarantined, "problems": o.problems.len()})
        }
        Err(_) => json!({"run_id": run_id, "status": "error"}),
    }))
}

pub fn register() {
    jobs::register_handler(POLL_JOB, |ctx, job| Box::pin(poll_job(ctx, job)));
}

/// Enqueue a `source.poll` for every enabled source whose kind has a poller and whose interval has
/// passed. After a failed run the wait is shorter: 1 hour, doubling with each failure up to 64 hours,
/// and never longer than the interval, so a weekly source that hit one bad answer tries again the
/// same day. Returns the jobs added.
pub async fn schedule_due(conn: &mut PgConnection) -> Result<Vec<i64>> {
    let kinds = polled_kinds();
    if kinds.is_empty() {
        return Ok(Vec::new());
    }
    let due: Vec<Uuid> = sqlx::query_scalar(
        "select id from source where enabled and poll_seconds is not null and kind = any($1) \
         and (last_run_at is null or last_run_at <= now() - make_interval(secs => case when consecutive_failures > 0 \
         then least(poll_seconds, 3600 * power(2, least(consecutive_failures, 7) - 1)) else poll_seconds end)) \
         order by id",
    )
    .bind(&kinds)
    .fetch_all(&mut *conn)
    .await?;
    let mut added = Vec::new();
    for id in due {
        if let Some(j) = enqueue_poll(&mut *conn, id).await? {
            added.push(j);
        }
    }
    Ok(added)
}

/// Poll one source now (the "sync now" button, or a webhook that only says something changed).
pub async fn enqueue_poll(conn: &mut PgConnection, source_id: Uuid) -> Result<Option<i64>> {
    let key = format!("{POLL_JOB}:{source_id}");
    Ok(jobs::enqueue(
        conn,
        POLL_JOB,
        &json!({"source_id": source_id.to_string()}),
        EnqueueOptions::dedupe(&key),
    )
    .await?)
}

/// Queue the first pull of a grant in the same transaction that saves the grant. A queued poll
/// reads the new grant. If the poll is already running, move its dedupe key aside and queue one
/// successor under the original key. The worker waits for the previous job before claiming it.
pub async fn enqueue_grant_poll(tx: &mut Transaction<'_, Postgres>, source_id: Uuid) -> Result<()> {
    let key = format!("{POLL_JOB}:{source_id}");
    loop {
        if enqueue_poll(tx, source_id).await?.is_some() {
            return Ok(());
        }
        let active: Option<(i64, bool)> = sqlx::query_as(
            "select id, state = 'running' from job \
             where dedupe_key = $1 and state in ('queued', 'running') for update",
        )
        .bind(&key)
        .fetch_optional(&mut **tx)
        .await?;
        let Some((job_id, running)) = active else {
            // The existing job finished before we locked it. Try the now-free key again.
            continue;
        };
        if running {
            sqlx::query("update job set dedupe_key = $2 where id = $1")
                .bind(job_id)
                .bind(format!("{key}{}", jobs::PREVIOUS_POLL_SUFFIX))
                .execute(&mut **tx)
                .await?;
            enqueue_poll(tx, source_id).await?;
        }
        return Ok(());
    }
}

/// A connector interprets its cursor and returns localized review payload when a gap is present.
/// The core supplies whether a pull has been failing for longer than the source's interval, its retry
/// included.
pub type GapCheck = fn(Option<&Value>, bool, DateTime<Utc>) -> Option<Value>;

static GAP_CHECKS: LazyLock<RwLock<HashMap<String, GapCheck>>> = LazyLock::new(|| RwLock::new(HashMap::new()));

pub fn register_gap_check(kind: &str, check: GapCheck) {
    if let Ok(mut m) = GAP_CHECKS.write() {
        m.insert(kind.into(), check);
    }
}

#[derive(sqlx::FromRow)]
struct GapSource {
    household_id: Uuid,
    profile_id: Option<Uuid>,
    kind: String,
    enabled: bool,
    cursor: Option<Value>,
    consecutive_failures: i32,
    poll_seconds: Option<i32>,
    last_success_at: Option<DateTime<Utc>>,
    oauth_grant_id: Option<Uuid>,
}

async fn pull_failed_for_interval(
    conn: &mut PgConnection,
    id: Uuid,
    source: &GapSource,
    now: DateTime<Utc>,
) -> Result<bool> {
    let Some(interval) = source.poll_seconds.filter(|_| source.consecutive_failures > 0) else {
        return Ok(false);
    };
    // Measure from the first recorded failure of the current streak. Retries cannot postpone the
    // alert, and one late failure after a long interval does not immediately raise it. The scheduler
    // queues the retry of a failed poll and runs this check in the same pass, so the check always
    // comes before that retry ends: a gap needs the retry to have failed too.
    Ok(sqlx::query_scalar(
        "select count(*) >= 2 and min(finished_at) < $3 from source_run where source_id = $1 \
         and oauth_grant_id is not distinct from $2 and trigger in ('poll', 'manual') and status = 'error' \
         and finished_at is not null and ($4::timestamptz is null or finished_at > $4)",
    )
    .bind(id)
    .bind(source.oauth_grant_id)
    .bind(now - chrono::Duration::seconds(i64::from(interval)))
    .bind(source.last_success_at)
    .fetch_one(conn)
    .await?)
}

async fn reconcile_gap(
    conn: &mut PgConnection,
    id: Uuid,
    checks: &HashMap<String, GapCheck>,
    now: DateTime<Utc>,
) -> Result<()> {
    // The source row serializes schedulers and health writers. Read the run and review rows in
    // separate statements after acquiring the lock so they include any writer we waited for.
    let mut tx = sqlx::Connection::begin(conn).await?;
    let source: Option<GapSource> = sqlx::query_as(
        "select household_id, profile_id, kind, enabled, cursor, consecutive_failures, poll_seconds, \
         last_success_at, oauth_grant_id from source where id = $1 for no key update",
    )
    .bind(id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(source) = source else {
        return Ok(());
    };
    // Passes can reach the row out of order. Every evaluation stamps the alert it keeps, so a pass whose
    // clock is older than the newest evaluation of this source leaves the alerts to that one.
    let newest: Option<DateTime<Utc>> = sqlx::query_scalar(
        "select max(greatest(created_at, (payload->>'evaluated_at')::timestamptz, \
         (payload->>'ended_at')::timestamptz)) from review_item \
         where household_id = $2 and kind = 'source_gap' and ref_table = 'source' and ref_id = $1",
    )
    .bind(id)
    .bind(source.household_id)
    .fetch_one(&mut *tx)
    .await?;
    if newest.is_some_and(|newest| newest > now) {
        return Ok(());
    }
    let payload = match (checks.get(&source.kind), source.enabled, source.profile_id) {
        (Some(check), true, Some(_)) => {
            let failed = pull_failed_for_interval(&mut tx, id, &source, now).await?;
            check(source.cursor.as_ref(), failed, now)
        }
        _ => None,
    };
    // A dismissed or rejected alert still belongs to this gap until the source recovers.
    // Keep the person's decision and end the episode so a later gap can open a new item.
    sqlx::query(
        "update review_item set payload = jsonb_set(payload, '{ended_at}', to_jsonb($3::timestamptz)), \
         decided_at = coalesce(decided_at, $3), \
         decision_reason = case when status <> 'open' then decision_reason \
           when $4::uuid is distinct from profile_id or not $5 then 'source_unavailable' \
           else 'sync_recovered' end, \
         status = case when status = 'open' then 'dismissed' else status end \
         where household_id = $2 and kind = 'source_gap' and ref_table = 'source' and ref_id = $1 \
         and payload->>'ended_at' is null and (status = 'open' or decided_by is not null) \
         and created_at <= $3 and ($4::uuid is distinct from profile_id or not $6)",
    )
    .bind(id)
    .bind(source.household_id)
    .bind(now)
    .bind(source.profile_id)
    .bind(source.enabled)
    .bind(payload.is_some())
    .execute(&mut *tx)
    .await?;

    if let Some(payload) = payload {
        let current: Option<Uuid> = sqlx::query_scalar(
            "select id from review_item where household_id = $1 and profile_id = $2 \
             and kind = 'source_gap' and ref_table = 'source' and ref_id = $3 and payload->>'ended_at' is null \
             and (status = 'open' or decided_by is not null) order by created_at desc, id desc limit 1",
        )
        .bind(source.household_id)
        .bind(source.profile_id)
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?;
        let stamp = json!({ "evaluated_at": now });
        match current {
            // An open alert takes the current text; a decided one keeps the text the person saw.
            Some(item) => {
                sqlx::query(
                    "update review_item set payload = case when status = 'open' then $2 else payload end || $3 \
                     where id = $1",
                )
                .bind(item)
                .bind(payload)
                .bind(stamp)
                .execute(&mut *tx)
                .await?;
            }
            None => {
                sqlx::query(
                    "insert into review_item (household_id, profile_id, kind, ref_table, ref_id, payload, created_at) \
                     values ($1, $2, 'source_gap', 'source', $3, $4 || $5, $6)",
                )
                .bind(source.household_id)
                .bind(source.profile_id)
                .bind(id)
                .bind(payload)
                .bind(stamp)
                .bind(now)
                .execute(&mut *tx)
                .await?;
            }
        }
    }
    tx.commit().await?;
    Ok(())
}

async fn check_source_gaps(conn: &mut PgConnection, now: DateTime<Utc>) -> Result<()> {
    let checks = GAP_CHECKS.read().map(|m| m.clone()).unwrap_or_default();
    if checks.is_empty() {
        return Ok(());
    }
    let kinds: Vec<&str> = checks.keys().map(String::as_str).collect();
    let sources: Vec<Uuid> = sqlx::query_scalar("select id from source where kind = any($1) order by id")
        .bind(&kinds)
        .fetch_all(&mut *conn)
        .await?;
    for id in sources {
        reconcile_gap(conn, id, &checks, now).await?;
    }
    Ok(())
}

/// Queue due polls before reconciling sync-gap alerts and sweeping expired share files.
/// Returns the jobs added.
pub async fn scheduler_tick(ctx: &JobContext) -> Result<Vec<i64>> {
    scheduler_tick_at(ctx, Utc::now()).await
}

/// The scheduler pass with an explicit clock for sync-gap alerts.
pub async fn scheduler_tick_at(ctx: &JobContext, now: DateTime<Utc>) -> Result<Vec<i64>> {
    add_default_sources(ctx).await?;
    let mut conn = ctx.pool.acquire().await?;
    let added = schedule_due(&mut conn).await?;
    if let Err(error) = check_source_gaps(&mut conn, now).await {
        tracing::warn!(%error, "source sync-gap check failed");
    }
    sweep_link_files(&mut conn).await?;
    Ok(added)
}

/// Delete the sealed file of every SMART Health Link that can no longer open (migration 1270): past
/// its expiry, or out of passcode attempts. Revoke and the last wrong passcode delete theirs at once;
/// this catches expiry, which no request marks. It first revokes the links whose creator's grant ran
/// out by itself (`shl_end_unshareable`: a lock or a changed grant does that at the write). Returns the
/// files deleted.
pub async fn sweep_link_files(conn: &mut PgConnection) -> Result<u64> {
    sqlx::query("select shl_end_unshareable(null)")
        .execute(&mut *conn)
        .await?;
    let done = sqlx::query(
        "delete from shl_file f using shl s where s.id = f.shl_id \
         and (s.expires_at <= now() or s.attempts_left = 0)",
    )
    .execute(conn)
    .await?;
    Ok(done.rows_affected())
}

/// Every SCHEDULER_SECONDS: `scheduler_tick`. Stops when `stop` turns true.
pub async fn scheduler_loop(ctx: JobContext, mut stop: watch::Receiver<bool>) {
    let mut tick = tokio::time::interval(Duration::from_secs(SCHEDULER_SECONDS));
    loop {
        tokio::select! {
            _ = tick.tick() => {}
            _ = stop.changed() => {}
        }
        if *stop.borrow() {
            return;
        }
        if let Err(e) = scheduler_tick(&ctx).await {
            tracing::warn!(error = %e, "source scheduler failed");
        }
    }
}
