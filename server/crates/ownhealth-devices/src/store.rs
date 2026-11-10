//! Writing device samples (lane D): one path for Health Auto Export, Gadgetbridge and Nightscout into
//! `wearable_sample` (0.2.0 lane C's table), then the rollups of what changed.
//!
//! The unique key `(profile_id, metric, source_id, t)` makes every write idempotent. Two policies on a
//! sample that is already there:
//! - `Keep`: the first file to bring a sample owns it (Gadgetbridge exports, Nightscout readings);
//! - `Refresh`: a push may carry a newer value for the same bucket (Health Auto Export sums a minute or
//!   an hour that was still open at the previous push), so the value is replaced when it differs;
//! - `Adopt`: the newest file owns the samples it holds (Gadgetbridge exports are cumulative, so each
//!   holds the previous ones): a stored sample moves to the new document and takes its value, and
//!   retracting an older export leaves what a newer one still holds.

use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Utc};
use ownhealth_core::Result;
use ownhealth_ingest_structured::wearables;
use sqlx::PgConnection;
use uuid::Uuid;

/// Rows per INSERT statement.
const CHUNK: usize = 5_000;

#[derive(Debug, Clone, PartialEq)]
pub struct Sample {
    pub metric: &'static str,
    pub t: DateTime<Utc>,
    pub t_end: Option<DateTime<Utc>>,
    pub v: f64,
    /// The original UTC offset in minutes, when the source says it.
    pub tz: Option<i16>,
    /// `wearable_source.id`.
    pub source: i16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Conflict {
    Keep,
    Refresh,
    Adopt,
}

/// What a write did. `seen` counts distinct samples after merging repeats inside the batch.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Written {
    pub seen: i64,
    pub new: i64,
    pub refreshed: i64,
}

impl Written {
    /// Samples that were already stored (refreshed or not).
    pub fn duplicate(&self) -> i64 {
        self.seen - self.new
    }

    /// Add another batch's counts (`insert` per batch, then one `rebuild`).
    pub fn add(&mut self, o: Written) {
        self.seen += o.seen;
        self.new += o.new;
        self.refreshed += o.refreshed;
    }
}

/// What a series of `insert`s changed, for one rollup rebuild at the end (`rebuild`).
#[derive(Debug, Clone, Default)]
pub struct Touched {
    metrics: BTreeSet<String>,
    sources: BTreeSet<i16>,
}

/// The `wearable_source` of one device, linked to the connector `source` that brings it.
pub async fn device_source(
    conn: &mut PgConnection,
    profile_id: Uuid,
    provider: &str,
    device: Option<&str>,
    external_id: Option<&str>,
    source_id: Option<Uuid>,
) -> Result<i16> {
    let id = wearables::source_id(&mut *conn, profile_id, provider, device, external_id).await?;
    if let Some(s) = source_id {
        sqlx::query("update wearable_source set source_id = $2 where id = $1 and source_id is distinct from $2")
            .bind(id)
            .bind(s)
            .execute(conn)
            .await?;
    }
    Ok(id)
}

/// Insert `samples` for one profile, then rebuild the rollups of the metrics and sources that changed.
/// Repeats inside the batch (same metric, source and time) collapse to the last one.
pub async fn write(
    conn: &mut PgConnection,
    profile_id: Uuid,
    document_id: Option<Uuid>,
    samples: Vec<Sample>,
    conflict: Conflict,
) -> Result<Written> {
    let mut touched = Touched::default();
    let w = insert(&mut *conn, profile_id, document_id, samples, conflict, &mut touched).await?;
    rebuild(conn, profile_id, touched).await?;
    Ok(w)
}

/// Rebuild the rollups of what the inserts touched.
pub async fn rebuild(conn: &mut PgConnection, profile_id: Uuid, touched: Touched) -> Result<()> {
    if !touched.metrics.is_empty() {
        let metrics: Vec<String> = touched.metrics.into_iter().collect();
        let sources: Vec<i16> = touched.sources.into_iter().collect();
        wearables::recompute_rollups(&mut *conn, profile_id, Some(&metrics), Some(&sources)).await?;
    }
    Ok(())
}

/// Insert one batch without rebuilding rollups; what it touched is added to `touched`.
pub async fn insert(
    conn: &mut PgConnection,
    profile_id: Uuid,
    document_id: Option<Uuid>,
    samples: Vec<Sample>,
    conflict: Conflict,
    touched: &mut Touched,
) -> Result<Written> {
    let mut unique: BTreeMap<(&'static str, i16, DateTime<Utc>), Sample> = BTreeMap::new();
    for s in samples {
        if s.v.is_finite() {
            unique.insert((s.metric, s.source, s.t), s);
        }
    }
    let rows: Vec<Sample> = unique.into_values().collect();
    let mut out = Written {
        seen: rows.len() as i64,
        ..Written::default()
    };
    let on_conflict = match conflict {
        Conflict::Keep => "on conflict do nothing",
        Conflict::Refresh => {
            "on conflict (profile_id, metric, source_id, t) do update set v = excluded.v, t_end = excluded.t_end, \
             tz_offset_min = excluded.tz_offset_min where wearable_sample.v is distinct from excluded.v \
             or wearable_sample.t_end is distinct from excluded.t_end"
        }
        Conflict::Adopt => {
            "on conflict (profile_id, metric, source_id, t) do update set document_id = excluded.document_id, \
             v = excluded.v where wearable_sample.document_id is distinct from excluded.document_id \
             or wearable_sample.v is distinct from excluded.v"
        }
    };
    let sql = format!(
        "insert into wearable_sample (profile_id, metric, t, t_end, v, tz_offset_min, source_id, document_id) \
         select $1, x.m, x.t, x.te, x.v, x.tz, x.s, $2 from unnest(cast($3 as text[]), cast($4 as timestamptz[]), \
         cast($5 as timestamptz[]), cast($6 as real[]), cast($7 as smallint[]), cast($8 as smallint[])) \
         as x(m, t, te, v, tz, s) {on_conflict} returning (xmax = 0) as inserted"
    );
    for chunk in rows.chunks(CHUNK) {
        let metrics: Vec<&str> = chunk.iter().map(|s| s.metric).collect();
        let ts: Vec<DateTime<Utc>> = chunk.iter().map(|s| s.t).collect();
        let ends: Vec<Option<DateTime<Utc>>> = chunk.iter().map(|s| s.t_end).collect();
        let vs: Vec<f32> = chunk.iter().map(|s| s.v as f32).collect();
        let tzs: Vec<Option<i16>> = chunk.iter().map(|s| s.tz).collect();
        let srcs: Vec<i16> = chunk.iter().map(|s| s.source).collect();
        let flags: Vec<bool> = sqlx::query_scalar(sqlx::AssertSqlSafe(sql.as_str()))
            .bind(profile_id)
            .bind(document_id)
            .bind(&metrics)
            .bind(&ts)
            .bind(&ends)
            .bind(&vs)
            .bind(&tzs)
            .bind(&srcs)
            .fetch_all(&mut *conn)
            .await?;
        if flags.is_empty() {
            continue;
        }
        out.new += flags.iter().filter(|f| **f).count() as i64;
        out.refreshed += flags.iter().filter(|f| !**f).count() as i64;
        // a chunk that wrote anything may have touched any of its metrics and sources
        touched.metrics.extend(chunk.iter().map(|s| s.metric.to_string()));
        touched.sources.extend(chunk.iter().map(|s| s.source));
    }
    Ok(out)
}
