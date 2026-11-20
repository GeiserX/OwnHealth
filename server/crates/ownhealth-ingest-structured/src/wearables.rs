//! Wearable samples, rollups and the source-priority rule (`wearables/__init__.py`; connectors.md 4,
//! architecture.md 6.4).
//!
//! Samples keep UTC (`t`) plus the original offset (`tz_offset_min`). Rollups are stored per source:
//! hourly buckets on the UTC hour, daily buckets on the local calendar date of the sample (stored as
//! that date at 00:00 UTC). Sleep counts on the date of waking: each stage is dated by the end of its
//! night.
//!
//! Source priority applies at query time, never at ingest: every raw sample is kept, and a daily
//! series takes, for each day, the highest-priority source that has data that day. That stops an
//! iPhone and an Apple Watch both counting the same steps. A `source_priority` row overrides the
//! default order (Watch, then iPhone, then anything else).
//!
//! HRV is the exception: each vendor measures it its own way, so an HRV series takes one source for
//! the whole window (the best one with data there) and names the sources it left out.

use std::cmp::Reverse;
use std::collections::{BTreeMap, HashMap};

use chrono::{DateTime, NaiveDate, Utc};
use ownhealth_core::Result;
use ownhealth_core::pyjson::{f32_json, py_isoformat_utc, pydantic_datetime_utc};
use serde_json::{Value, json};
use sqlx::{AssertSqlSafe, PgConnection};
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Metric {
    pub key: String,
    pub unit: &'static str,
    /// `sum`: the day value is the total; `mean`: the day value is the average.
    pub agg: &'static str,
    /// One reading per day or night: coverage is 1 when present.
    pub daily: bool,
    pub label_en: String,
    pub label_es: String,
}

type Spec = (
    &'static str,
    &'static str,
    &'static str,
    bool,
    &'static str,
    &'static str,
);

pub const METRICS: [Spec; 16] = [
    ("steps", "count", "sum", false, "Steps", "Pasos"),
    ("hr", "/min", "mean", false, "Heart rate", "Frecuencia cardiaca"),
    ("hr_resting", "/min", "mean", true, "Resting heart rate", "FC en reposo"),
    ("hrv_sdnn", "ms", "mean", true, "HRV (SDNN)", "VFC (SDNN)"),
    ("hrv_rmssd", "ms", "mean", true, "HRV (RMSSD)", "VFC (RMSSD)"),
    (
        "vo2max",
        "mL/kg/min",
        "mean",
        true,
        "VO₂max estimate",
        "VO₂máx estimado",
    ),
    ("spo2", "%", "mean", false, "Blood oxygen", "Oxígeno en sangre"),
    (
        "resp_rate",
        "/min",
        "mean",
        false,
        "Respiratory rate",
        "Frecuencia respiratoria",
    ),
    ("weight", "kg", "mean", true, "Weight", "Peso"),
    (
        "bp_systolic",
        "mm[Hg]",
        "mean",
        true,
        "Blood pressure, systolic",
        "Presión arterial sistólica",
    ),
    (
        "bp_diastolic",
        "mm[Hg]",
        "mean",
        true,
        "Blood pressure, diastolic",
        "Presión arterial diastólica",
    ),
    ("active_energy", "kcal", "sum", false, "Active energy", "Energía activa"),
    (
        "distance",
        "km",
        "sum",
        false,
        "Walking + running distance",
        "Distancia caminando y corriendo",
    ),
    (
        "sleep_asleep",
        "min",
        "sum",
        true,
        "Sleep duration",
        "Duración del sueño",
    ),
    ("sleep_in_bed", "min", "sum", true, "Time in bed", "Tiempo en cama"),
    ("glucose_cgm", "mg/dL", "mean", false, "Glucose (CGM)", "Glucosa (MCG)"),
];

pub const SLEEP_METRICS: [&str; 2] = ["sleep_asleep", "sleep_in_bed"];

pub fn daily_metrics() -> Vec<String> {
    METRICS.iter().filter(|m| m.3).map(|m| m.0.to_string()).collect()
}

/// Known metric, or a neutral description for one another importer added.
pub fn metric(key: &str) -> Metric {
    match METRICS.iter().find(|m| m.0 == key) {
        Some(&(key, unit, agg, daily, en, es)) => Metric {
            key: key.into(),
            unit,
            agg,
            daily,
            label_en: en.into(),
            label_es: es.into(),
        },
        None => Metric {
            key: key.into(),
            unit: "",
            agg: "mean",
            daily: false,
            label_en: key.into(),
            label_es: key.into(),
        },
    }
}

/// The metric as JSON (`key`, `unit`, `agg`, `daily`, `label_en`, `label_es`); None when unknown.
pub fn metric_info(key: &str) -> Option<Value> {
    METRICS.iter().find(|m| m.0 == key).map(|&(key, unit, agg, daily, en, es)| {
        json!({"key": key, "unit": unit, "agg": agg, "daily": daily, "label_en": en, "label_es": es})
    })
}

// ------------------------------------------------------------------- sources ----

/// Get or create the wearable_source row for one device of one profile.
pub async fn source_id(
    conn: &mut PgConnection,
    profile_id: Uuid,
    provider: &str,
    device: Option<&str>,
    external_id: Option<&str>,
) -> Result<i16> {
    loop {
        let sid: Option<i16> = sqlx::query_scalar(
            "select id from wearable_source where profile_id = $1 and provider = $2 \
             and coalesce(device, '') = coalesce(cast($3 as text), '') \
             and coalesce(external_id, '') = coalesce(cast($4 as text), '')",
        )
        .bind(profile_id)
        .bind(provider)
        .bind(device)
        .bind(external_id)
        .fetch_optional(&mut *conn)
        .await?;
        if let Some(id) = sid {
            return Ok(id);
        }
        let created: Option<i16> = sqlx::query_scalar(
            "insert into wearable_source (profile_id, provider, device, external_id) values ($1, $2, $3, $4) \
             on conflict do nothing returning id",
        )
        .bind(profile_id)
        .bind(provider)
        .bind(device)
        .bind(external_id)
        .fetch_optional(&mut *conn)
        .await?;
        if let Some(id) = created {
            return Ok(id);
        }
        // created by a concurrent import: read it again
    }
}

pub fn default_rank(device: Option<&str>) -> i32 {
    let d = device.unwrap_or_default().to_lowercase();
    if d.contains("watch") {
        0
    } else if d.contains("iphone") {
        1
    } else {
        2
    }
}

/// Source ids for one metric, best first: the profile's source_priority row, then the default order.
pub async fn priority(conn: &mut PgConnection, profile_id: Uuid, metric: &str) -> Result<Vec<i16>> {
    Ok(order(conn, profile_id, metric)
        .await?
        .into_iter()
        .map(|s| s.0)
        .collect())
}

/// `priority`, each source with its default rank, or None when the source_priority row places it.
async fn order(conn: &mut PgConnection, profile_id: Uuid, metric: &str) -> Result<Vec<(i16, Option<i32>)>> {
    let sources: Vec<(i16, Option<String>)> =
        sqlx::query_as("select id, device from wearable_source where profile_id = $1 order by id")
            .bind(profile_id)
            .fetch_all(&mut *conn)
            .await?;
    let explicit: Vec<i16> =
        sqlx::query_scalar("select source_ids from source_priority where profile_id = $1 and metric = $2")
            .bind(profile_id)
            .bind(metric)
            .fetch_optional(&mut *conn)
            .await?
            .unwrap_or_default();
    let mut rest: Vec<&(i16, Option<String>)> = sources.iter().filter(|s| !explicit.contains(&s.0)).collect();
    rest.sort_by_key(|s| (default_rank(s.1.as_deref()), s.0));
    Ok(explicit
        .iter()
        .map(|s| (*s, None))
        .chain(rest.into_iter().map(|s| (s.0, Some(default_rank(s.1.as_deref())))))
        .collect())
}

pub async fn set_priority(conn: &mut PgConnection, profile_id: Uuid, metric: &str, source_ids: &[i16]) -> Result<()> {
    sqlx::query(
        "insert into source_priority (profile_id, metric, source_ids) values ($1, $2, $3) \
         on conflict (profile_id, metric) do update set source_ids = excluded.source_ids",
    )
    .bind(profile_id)
    .bind(metric)
    .bind(source_ids)
    .execute(conn)
    .await?;
    Ok(())
}

// ------------------------------------------------------------------- rollups ----

// The instant a sample counts at (`s.at`, with its offset `s.off`): sleep on waking, everything else
// at its start. A Watch writes a night as many short stages, so a sleep stage counts at the end of its
// session: stages less than SLEEP_GAP apart are one session (one night), dated by the local date of
// its last stage's end. Parameters: $1 profile, $2 metrics, $3 sources, $4 sleep metrics, $5 daily.
const SLEEP_GAP: &str = "2 hours";
const LOCAL_DAY: &str =
    "(date_trunc('day', (s.at at time zone 'UTC') + make_interval(mins => coalesce(s.off, 0))) at time zone 'UTC')";
const HOUR: &str = "(date_trunc('hour', s.at at time zone 'UTC') at time zone 'UTC')";

fn cols(a: &str) -> String {
    format!("{a}.profile_id, {a}.metric, {a}.t, {a}.t_end, {a}.v, {a}.tz_offset_min, {a}.source_id")
}

/// wearable_sample rows matching `sel` (on alias w), with `at` and `off` added. `plain` narrows the
/// metrics that are not sleep only (sleep sessions are always read whole).
fn samples(sel: &str, plain: &str) -> String {
    let part = |a: &str| format!("partition by {a}.profile_id, {a}.metric, {a}.source_id");
    format!(
        "(select {w}, w.t as at, w.tz_offset_min as off from wearable_sample w where {sel} \
         and not w.metric = any(cast($4 as text[])) and {plain} \
         union all \
         select {z}, max(coalesce(z.t_end, z.t)) over ({pz}, z.sess), \
         first_value(z.tz_offset_min) over ({pz}, z.sess order by coalesce(z.t_end, z.t) desc) \
         from (select y.*, sum(y.brk) over ({py} order by y.t rows unbounded preceding) \
         as sess from (select {w}, case when w.t <= max(coalesce(w.t_end, w.t)) over \
         ({pw} order by w.t rows between unbounded preceding and 1 preceding) \
         + interval '{SLEEP_GAP}' then 0 else 1 end as brk from wearable_sample w where {sel} \
         and w.metric = any(cast($4 as text[]))) y) z) s",
        w = cols("w"),
        z = cols("z"),
        pz = part("z"),
        py = part("y"),
        pw = part("w"),
    )
}

const STATS: &str = "count(*), min(s.v), max(s.v), avg(s.v), sum(s.v), percentile_cont(0.1) within group (order by s.v), \
     percentile_cont(0.5) within group (order by s.v), percentile_cont(0.9) within group (order by s.v)";

/// Rebuild the hour and day rollups of one profile from wearable_sample (all metrics, or these).
/// Returns the number of rollup rows written. Whole metrics are rebuilt (per source when `source_ids`
/// is given), so a bucket is never computed from part of its samples.
pub async fn recompute_rollups(
    conn: &mut PgConnection,
    profile_id: Uuid,
    metrics: Option<&[String]>,
    source_ids: Option<&[i16]>,
) -> Result<i64> {
    recompute_rollups_between(conn, profile_id, metrics, source_ids, None).await
}

/// `recompute_rollups` after a write whose samples all lie in `span` (first and last `t`): metrics
/// other than sleep rebuild only the hour and day buckets those samples can reach, from three days
/// before the first sample's UTC day to three days after the last, reading the samples two days wider
/// each side so every rebuilt bucket sees all of its own (a local day reaches at most a day either side
/// of its UTC date). Sleep metrics are rebuilt whole: a new stage can join a session that began
/// earlier and move the date of all of it. `None` rebuilds everything.
pub async fn recompute_rollups_between(
    conn: &mut PgConnection,
    profile_id: Uuid,
    metrics: Option<&[String]>,
    source_ids: Option<&[i16]>,
    span: Option<(DateTime<Utc>, DateTime<Utc>)>,
) -> Result<i64> {
    let day = |t: DateTime<Utc>| t.date_naive().and_time(chrono::NaiveTime::MIN).and_utc();
    let (lo, hi) = match span {
        Some((a, b)) => (
            Some(day(a.min(b)) - chrono::Duration::days(3)),
            Some(day(a.max(b)) + chrono::Duration::days(4)),
        ),
        None => (None, None),
    };
    let sel = "w.profile_id = $1 and (cast($2 as text[]) is null or w.metric = any(cast($2 as text[]))) \
               and (cast($3 as smallint[]) is null or w.source_id = any(cast($3 as smallint[])))";
    let plain = "(cast($6 as timestamptz) is null or (w.t >= cast($6 as timestamptz) - interval '2 days' \
                 and w.t < cast($7 as timestamptz) + interval '2 days'))";
    let from = samples(sel, plain);
    let sleep: Vec<String> = SLEEP_METRICS.iter().map(|s| s.to_string()).collect();
    let daily = daily_metrics();
    sqlx::query(
        "delete from wearable_rollup where profile_id = $1 \
         and (cast($2 as text[]) is null or metric = any(cast($2 as text[]))) \
         and (cast($3 as smallint[]) is null or source_id = any(cast($3 as smallint[]))) \
         and (cast($5 as timestamptz) is null or metric = any(cast($4 as text[])) \
              or (t >= cast($5 as timestamptz) and t < cast($6 as timestamptz)))",
    )
    .bind(profile_id)
    .bind(metrics)
    .bind(source_ids)
    .bind(&sleep)
    .bind(lo)
    .bind(hi)
    .execute(&mut *conn)
    .await?;
    let insert = |bucket: &str, t: &str, coverage: String| {
        format!(
            "insert into wearable_rollup (profile_id, metric, source_id, bucket, t, n, v_min, v_max, v_avg, v_sum, \
             v_p10, v_p50, v_p90, coverage) select s.profile_id, s.metric, s.source_id, '{bucket}', {t}, {STATS}, \
             case when s.metric = any(cast($5 as text[])) then 1.0 else {coverage} end \
             from {from} group by 1, 2, 3, 5 having cast($6 as timestamptz) is null \
             or s.metric = any(cast($4 as text[])) or ({t} >= cast($6 as timestamptz) and {t} < cast($7 as timestamptz))"
        )
    };
    let hour_sql = insert(
        "hour",
        HOUR,
        "least(1.0, count(distinct floor(extract(minute from s.t at time zone 'UTC') / 5)) / 12.0)".into(),
    );
    let day_sql = insert("day", LOCAL_DAY, format!("least(1.0, count(distinct {HOUR}) / 24.0)"));
    let mut written = 0i64;
    for sql in [hour_sql, day_sql] {
        let done = sqlx::query(AssertSqlSafe(sql.as_str()))
            .bind(profile_id)
            .bind(metrics)
            .bind(source_ids)
            .bind(&sleep)
            .bind(&daily)
            .bind(lo)
            .bind(hi)
            .execute(&mut *conn)
            .await?;
        written += done.rows_affected() as i64;
    }
    Ok(written)
}

// ------------------------------------------------------------------- queries ----

/// One point of a series: the rollup of the best source for that bucket.
#[derive(Debug, Clone, PartialEq)]
pub struct Point {
    pub t: DateTime<Utc>,
    /// The sum or the mean, by metric.
    pub value: Option<f64>,
    pub min: Option<f64>,
    pub max: Option<f64>,
    pub mean: Option<f64>,
    pub median: Option<f64>,
    pub p10: Option<f64>,
    pub p90: Option<f64>,
    pub n: i32,
    pub coverage: f64,
    pub source_id: i16,
    pub device: Option<String>,
}

type RollupRow = (
    DateTime<Utc>,
    i16,
    i32,
    Option<f32>,
    Option<f32>,
    Option<f32>,
    Option<f32>,
    Option<f32>,
    Option<f32>,
    Option<f32>,
    f32,
    Option<String>,
);

/// A real as Python read it: the float of the text Postgres printed.
fn real(v: Option<f32>) -> Option<f64> {
    v.and_then(|x| f32_json(x).as_f64())
}

/// Metrics whose value depends on the vendor's own method: a series uses one source, never a mix.
pub fn one_source(metric_key: &str) -> bool {
    metric_key.starts_with("hrv_")
}

/// A series and the sources with data in the window that it left out (HRV only), best first.
#[derive(Debug, Clone, PartialEq)]
pub struct Series {
    pub points: Vec<Point>,
    /// The device name, else the provider, of each source left out.
    pub left_out: Vec<String>,
    /// The same name for the one source an HRV series shows; None for every other metric.
    pub shown: Option<String>,
}

/// One point per bucket with data, taken from the best source that has that bucket. Buckets without
/// data are absent; callers fill gaps if they draw them. An HRV series takes every point from one
/// source: the best one with data in the window.
pub async fn points(
    conn: &mut PgConnection,
    profile_id: Uuid,
    metric_key: &str,
    since: DateTime<Utc>,
    until: DateTime<Utc>,
    resolution: &str,
) -> Result<Vec<Point>> {
    Ok(
        points_and_left_out(conn, profile_id, metric_key, since, until, resolution)
            .await?
            .points,
    )
}

/// `points`, plus the sources an HRV series left out.
pub async fn points_and_left_out(
    conn: &mut PgConnection,
    profile_id: Uuid,
    metric_key: &str,
    since: DateTime<Utc>,
    until: DateTime<Utc>,
    resolution: &str,
) -> Result<Series> {
    let info = metric(metric_key);
    let order = order(&mut *conn, profile_id, metric_key).await?;
    let rank: HashMap<i16, usize> = order.iter().enumerate().map(|(i, s)| (s.0, i)).collect();
    // an explicit place is its own; a default rank is shared by every source that has it
    let place: HashMap<i16, (u8, i64)> = order
        .iter()
        .enumerate()
        .map(|(i, s)| {
            (
                s.0,
                s.1.map_or((0, i64::try_from(i).unwrap_or(i64::MAX)), |r| (1, i64::from(r))),
            )
        })
        .collect();
    let mut rows: Vec<RollupRow> = sqlx::query_as(
        "select r.t, r.source_id, r.n, r.v_min, r.v_max, r.v_avg, r.v_sum, r.v_p10, r.v_p50, r.v_p90, r.coverage, \
         ws.device from wearable_rollup r join wearable_source ws on ws.id = r.source_id \
         where r.profile_id = $1 and r.metric = $2 and r.bucket = $3 and r.t >= $4 and r.t <= $5 order by r.t",
    )
    .bind(profile_id)
    .bind(metric_key)
    .bind(resolution)
    .bind(since)
    .bind(until)
    .fetch_all(&mut *conn)
    .await?;
    let rank_of = |s: i16| rank.get(&s).copied().unwrap_or(99);
    // each source's points in the window and its newest one
    let mut seen: HashMap<i16, (usize, DateTime<Utc>)> = HashMap::new();
    for r in &rows {
        let e = seen.entry(r.1).or_insert((0, r.0));
        e.0 += 1;
        e.1 = e.1.max(r.0);
    }
    // HRV source order: its place, then more points in the window, then the newest point, then id
    let key = |s: i16| {
        let (n, newest) = seen.get(&s).copied().unwrap_or((0, DateTime::<Utc>::MIN_UTC));
        (place.get(&s).copied().unwrap_or((2, 0)), Reverse(n), Reverse(newest), s)
    };
    let mut left_out: Vec<i16> = Vec::new();
    let chosen = seen
        .keys()
        .copied()
        .min_by_key(|s| key(*s))
        .filter(|_| one_source(metric_key));
    if let Some(chosen) = chosen {
        left_out = seen.keys().copied().filter(|s| *s != chosen).collect();
        left_out.sort_by_key(|s| key(*s));
        rows.retain(|r| r.1 == chosen);
    }
    let mut best: BTreeMap<DateTime<Utc>, RollupRow> = BTreeMap::new();
    for r in rows {
        match best.get(&r.0) {
            Some(cur) if rank_of(r.1) >= rank_of(cur.1) => {}
            _ => {
                best.insert(r.0, r);
            }
        }
    }
    let names: Vec<(i16, String)> = match chosen {
        None => Vec::new(),
        Some(c) => {
            sqlx::query_as("select id, coalesce(device, provider) from wearable_source where id = any($1)")
                .bind(left_out.iter().copied().chain([c]).collect::<Vec<i16>>())
                .fetch_all(&mut *conn)
                .await?
        }
    };
    let name = |id: i16| names.iter().find(|n| n.0 == id).map(|n| n.1.clone());
    let shown = chosen.and_then(name);
    let left_out = left_out.iter().filter_map(|id| name(*id)).collect();
    let points = best
        .into_values()
        .map(|r| Point {
            t: r.0,
            value: if info.agg == "sum" { real(r.6) } else { real(r.5) },
            min: real(r.3),
            max: real(r.4),
            mean: real(r.5),
            median: real(r.8),
            p10: real(r.7),
            p90: real(r.9),
            n: r.2,
            coverage: real(Some(r.10)).unwrap_or(0.0),
            source_id: r.1,
            device: r.11,
        })
        .collect();
    Ok(Series {
        points,
        left_out,
        shown,
    })
}

/// The source a one-source series shows and the other names it left out, when it left one out. A
/// left-out name equal to the shown one (two sources of one device) is not repeated.
pub fn left_out_names(series: &Series) -> Option<(&str, Vec<&str>)> {
    let shown = series.shown.as_deref().filter(|_| !series.left_out.is_empty())?;
    let mut names: Vec<&str> = Vec::new();
    for n in &series.left_out {
        if n != shown && !names.contains(&n.as_str()) {
            names.push(n);
        }
    }
    Some((shown, names))
}

/// The same limitation without the names, for MCP: device names come from imports, so the agent gets
/// them beside this sentence as untrusted text.
pub const LEFT_OUT_SENTENCE: &str =
    "HRV from one source only (shown). Not mixed with HRV from other sources in this window (left_out).";

/// The limitation line of a one-source series that left a source out: which it shows and which not.
pub fn left_out_note(series: &Series, lang: &str) -> Option<String> {
    let (shown, names) = left_out_names(series)?;
    let rest = if names.is_empty() {
        String::new()
    } else {
        format!(": {}", names.join(", "))
    };
    Some(if lang == "es" {
        format!("VFC de una sola fuente: {shown}. No se mezcla con la VFC de otras fuentes en este periodo{rest}.")
    } else {
        format!("HRV from one source only: {shown}. Not mixed with HRV from other sources in this window{rest}.")
    })
}

/// Every text this module writes for a person, for the wording lint.
pub fn phrases() -> Value {
    let series = Series {
        points: Vec::new(),
        left_out: vec!["Whoop 4.0".into()],
        shown: Some("Oura Ring".into()),
    };
    let same = Series {
        left_out: vec!["Oura Ring".into()],
        ..series.clone()
    };
    json!({
        "en": [left_out_note(&series, "en"), left_out_note(&same, "en"), Some(LEFT_OUT_SENTENCE.to_string())],
        "es": [left_out_note(&series, "es"), left_out_note(&same, "es")],
    })
}

/// Python's `wearables.series` as JSON: `t`, `value`, `min`, `max`, `mean`, `median`, `p10`, `p90`,
/// `n`, `coverage`, `source_id`, `device` per bucket. `since` and `until` default to everything.
pub async fn series(
    conn: &mut PgConnection,
    profile_id: Uuid,
    metric_key: &str,
    since: Option<DateTime<Utc>>,
    until: Option<DateTime<Utc>>,
    resolution: &str,
) -> Result<Value> {
    let since = since.unwrap_or_else(|| day_start(NaiveDate::from_ymd_opt(1, 1, 1).unwrap_or_default()));
    let until = until.unwrap_or_else(|| day_start(NaiveDate::from_ymd_opt(9999, 12, 31).unwrap_or_default()));
    let pts = points(conn, profile_id, metric_key, since, until, resolution).await?;
    Ok(Value::Array(
        pts.into_iter()
            .map(|p| {
                json!({
                    "t": py_isoformat_utc(&p.t),
                    "value": p.value,
                    "min": p.min,
                    "max": p.max,
                    "mean": p.mean,
                    "median": p.median,
                    "p10": p.p10,
                    "p90": p.p90,
                    "n": p.n,
                    "coverage": p.coverage,
                    "source_id": p.source_id,
                    "device": p.device,
                })
            })
            .collect(),
    ))
}

pub async fn metrics_present(conn: &mut PgConnection, profile_id: Uuid) -> Result<Vec<String>> {
    Ok(sqlx::query_scalar(
        "select distinct metric from wearable_rollup where profile_id = $1 and bucket = 'day' order by 1",
    )
    .bind(profile_id)
    .fetch_all(conn)
    .await?)
}

/// Every source of the profile with its newest sample and its mean daily coverage in the window.
pub async fn devices(
    conn: &mut PgConnection,
    profile_id: Uuid,
    since: DateTime<Utc>,
    until: DateTime<Utc>,
) -> Result<Vec<Value>> {
    let days = ((until.date_naive() - since.date_naive()).num_days() + 1).max(1);
    type DeviceRow = (i16, String, Option<String>, Option<DateTime<Utc>>, f32);
    let rows: Vec<DeviceRow> = sqlx::query_as(
        "select ws.id, ws.provider, ws.device, \
         (select max(coalesce(t_end, t)) from wearable_sample where source_id = ws.id) as last_sample, \
         (select coalesce(sum(c), 0) from (select max(coverage) as c from wearable_rollup where source_id = ws.id \
           and bucket = 'day' and t >= $2 and t <= $3 group by t) d) as cov \
         from wearable_source ws where ws.profile_id = $1 order by ws.id",
    )
    .bind(profile_id)
    .bind(since)
    .bind(until)
    .fetch_all(conn)
    .await?;
    Ok(rows
        .into_iter()
        // a source whose samples were all retracted is not a device the person still has data from
        .filter(|row| row.3.is_some())
        .map(|(id, provider, device, last, cov)| {
            let cov = real(Some(cov)).unwrap_or(0.0);
            json!({
                "source_id": id,
                "provider": provider,
                "device": device,
                // sent by the annotated REST summary: pydantic writes UTC as `Z`
                "last_sample": last.map(|l| pydantic_datetime_utc(&l)),
                "coverage_pct": crate::py::round(100.0 * cov / days as f64, 1),
            })
        })
        .collect())
}

pub fn day_start(d: NaiveDate) -> DateTime<Utc> {
    d.and_hms_opt(0, 0, 0).map_or(DateTime::<Utc>::MIN_UTC, |n| n.and_utc())
}

#[cfg(test)]
mod note_tests {
    use super::*;

    fn series(shown: &str, left_out: &[&str]) -> Series {
        Series {
            points: Vec::new(),
            left_out: left_out.iter().map(|s| (*s).to_string()).collect(),
            shown: Some(shown.into()),
        }
    }

    /// Two sources of one vendor, or of one name: the note never blames a vendor nor repeats the name.
    #[test]
    fn the_note_names_only_other_sources() {
        for lang in ["en", "es"] {
            let same = left_out_note(&series("Apple Watch", &["Apple Watch"]), lang).unwrap_or_default();
            assert_eq!(same.matches("Apple Watch").count(), 1, "{same}");
            assert!(!same.contains("vendor") && !same.contains("fabricante"), "{same}");
            let two =
                left_out_note(&series("Oura Ring", &["oura", "Whoop 4.0", "Whoop 4.0"]), lang).unwrap_or_default();
            assert!(two.ends_with(": oura, Whoop 4.0."), "{two}");
        }
        assert!(left_out_note(&series("Oura Ring", &[]), "en").is_none());
    }
}
