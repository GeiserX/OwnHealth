//! Pulls the Open Wearables 0.11 API through the internal Compose address. The provider policy
//! applies at the wire boundary, before any bundle, measurement or score is stored.

use std::collections::{BTreeMap, HashMap, HashSet};

use chrono::{DateTime, Duration, FixedOffset, Utc};
use ownhealth_core::{
    arrive::store_bytes,
    blobs::BlobOwner,
    dedup::record_item,
    jobs::{self, EnqueueOptions, JobContext, JobError},
    net::{self, Egress, Fetched},
    providers,
    secrets::{SecretBox, SecretString, read_secret, store_secret},
    sources::{self, NewSource, RunOutcome},
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use sqlx::{PgConnection, PgPool};
use url::Url;
use uuid::Uuid;

use crate::{scores, store};

const KIND: &str = "openwearables";
const PURPOSE: &str = "openwearables_api_key";
const RECONCILE: &str = "openwearables.reconcile";
const INTERNAL: &str = "http://ow-app:8000";
const POLL_SECONDS: i32 = 900;
const PAGE: usize = 500;
const MAX_PAGES: usize = 300;
const MAX_BYTES_PER_PULL: usize = 48 * 1024 * 1024;
const OVERLAP_DAYS: i64 = 7;
const BACKFILL_DAYS: i64 = 30;
const API_KEY: &str = "x-open-wearables-api-key";

type Result<T> = std::result::Result<T, JobError>;

fn internal() -> Url {
    Url::parse(INTERNAL).expect("fixed Compose URL")
}

fn allowed(provider: &str) -> bool {
    providers::ingest_allowed(provider)
        && providers::policy()
            .providers
            .get(provider)
            .is_some_and(|p| p.path == "Open Wearables")
}

fn endpoint(base: &Url, path: &[&str], query: &[(&str, String)]) -> Url {
    let mut url = base.clone();
    url.path_segments_mut()
        .expect("HTTP origin")
        .clear()
        .extend(["api", "v1"])
        .extend(path.iter().copied());
    if !query.is_empty() {
        url.query_pairs_mut().extend_pairs(query);
    }
    url
}

fn egress() -> Egress {
    Egress {
        allow_private: true,
        https_only: false,
        ..Egress::connector()
    }
}

fn status(response: Fetched) -> Result<Fetched> {
    if (200..300).contains(&response.status) {
        Ok(response)
    } else {
        Err(JobError::typed(
            "ow_http",
            format!("Open Wearables HTTP {}", response.status),
        ))
    }
}

fn parse<T: DeserializeOwned>(body: &[u8]) -> Result<T> {
    serde_json::from_slice(body).map_err(|_| {
        JobError::typed(
            "ow_response",
            "Invalid Open Wearables response. / Respuesta no válida de Open Wearables.",
        )
    })
}

fn network(err: net::NetError) -> JobError {
    JobError::typed("unreachable", err.to_string())
}

async fn get(url: &Url, key: Option<&SecretString>) -> Result<Fetched> {
    let headers = key.map(|key| [(API_KEY, key.expose())]);
    status(
        net::get(url.as_str(), &egress(), headers.as_ref().map_or(&[], |h| &h[..]))
            .await
            .map_err(network)?,
    )
}

#[derive(Deserialize)]
struct Provider {
    provider: String,
    is_enabled: bool,
}

#[derive(Deserialize)]
struct Login {
    access_token: String,
}

#[derive(Deserialize)]
struct User {
    id: Uuid,
}

async fn enforce_provider_settings(base: &Url, email: &str, password: &SecretString) -> Result<()> {
    let login = endpoint(base, &["auth", "login"], &[]);
    let response = net::post_form(
        login.as_str(),
        &egress(),
        &[],
        &[("username", email), ("password", password.expose())],
    )
    .await
    .map_err(network)?;
    let login: Login = parse(&status(response)?.body)?;
    let auth = SecretString::new(format!("Bearer {}", login.access_token));

    let listed = endpoint(base, &["oauth", "providers"], &[]);
    for provider in parse::<Vec<Provider>>(&get(&listed, None).await?.body)? {
        if allowed(&provider.provider) {
            continue;
        }
        // Provider ids are path segments, never URLs supplied by the sidecar.
        if provider.provider.is_empty()
            || !provider
                .provider
                .bytes()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, b'_' | b'-'))
        {
            return Err(JobError::permanent("Open Wearables returned an invalid provider id"));
        }
        let url = endpoint(base, &["oauth", "providers", &provider.provider], &[]);
        let response = net::put_json(
            url.as_str(),
            &egress(),
            &[("authorization", auth.expose())],
            &json!({"is_enabled": false}),
        )
        .await
        .map_err(network)?;
        let confirmed: Provider = parse(&status(response)?.body)?;
        if confirmed.provider != provider.provider || confirmed.is_enabled {
            return Err(JobError::permanent("Open Wearables did not disable a provider"));
        }
    }
    Ok(())
}

/// Bind an existing sidecar user to a profile, from the operator CLI. A rerun rotates the API key.
/// The operator supplies a developer password only for the provider-settings requests.
pub async fn connect(
    ctx: &JobContext,
    profile: Uuid,
    user: Uuid,
    email: &str,
    password: &SecretString,
    key: &SecretString,
) -> Result<Uuid> {
    setup(ctx, &internal(), profile, user, email, password, key).await
}

async fn setup(
    ctx: &JobContext,
    base: &Url,
    profile: Uuid,
    user: Uuid,
    email: &str,
    password: &SecretString,
    key: &SecretString,
) -> Result<Uuid> {
    let user_text = user.to_string();
    let mut tx = ctx.pool.begin().await?;
    let household: Uuid = sqlx::query_scalar("select household_id from profile where id = $1 for key share")
        .bind(profile)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(|| JobError::permanent("Unknown OwnHealth profile"))?;
    // Serialize setups and deletion retries for this remote user, even before there is a local row.
    lock_user(&mut tx, user).await?;
    let binding: Option<(Uuid, Option<Uuid>)> =
        sqlx::query_as("select secret_id, source_id from openwearables_user where user_id = $1")
            .bind(user)
            .fetch_optional(&mut *tx)
            .await?;
    let previous = if let Some((secret, source)) = binding {
        let source = source.ok_or_else(|| JobError::permanent("Open Wearables user is pending deletion"))?;
        let found: Option<Uuid> =
            sqlx::query_scalar("select profile_id from source where id = $1 and kind = 'openwearables' for update")
                .bind(source)
                .fetch_optional(&mut *tx)
                .await?
                .flatten();
        if found != Some(profile) {
            return Err(JobError::permanent("Open Wearables user is already linked"));
        }
        Some((source, secret))
    } else {
        None
    };

    let url = endpoint(base, &["users", &user_text], &[]);
    let confirmed: User = parse(&get(&url, Some(key)).await?.body)?;
    if confirmed.id != user {
        return Err(JobError::permanent("Open Wearables returned a different user"));
    }
    enforce_provider_settings(base, email, password).await?;
    let sb = SecretBox::from_settings(&ctx.settings)?;
    let secret = store_secret(&mut tx, &sb, household, PURPOSE, None, key).await?;
    let source = if let Some((source, old_secret)) = previous {
        sqlx::query("update openwearables_user set secret_id = $2 where user_id = $1")
            .bind(user)
            .bind(secret)
            .execute(&mut *tx)
            .await?;
        ownhealth_core::secrets::delete_secret(&mut tx, household, old_secret).await?;
        source
    } else {
        let source = sources::create_source(
            &mut tx,
            &NewSource {
                profile_id: Some(profile),
                poll_seconds: Some(POLL_SECONDS),
                ..NewSource::new(household, KIND, "Open Wearables")
            },
        )
        .await?;
        sqlx::query("insert into openwearables_user (user_id, source_id, secret_id) values ($1, $2, $3)")
            .bind(user)
            .bind(source)
            .bind(secret)
            .execute(&mut *tx)
            .await?;
        source
    };
    sources::enqueue_grant_poll(&mut tx, source).await?;
    tx.commit().await?;
    Ok(source)
}

async fn lock_user(conn: &mut PgConnection, user: Uuid) -> Result<()> {
    sqlx::query("select pg_advisory_xact_lock(hashtextextended('openwearables:' || $1::uuid::text, 0))")
        .bind(user)
        .execute(conn)
        .await?;
    Ok(())
}

struct Binding {
    user: Uuid,
    secret: Uuid,
    household: Uuid,
    profile: Uuid,
    cursor: Option<Value>,
}

async fn binding(pool: &PgPool, source: Uuid) -> Result<Option<Binding>> {
    let row: Option<(Uuid, Uuid, Uuid, Uuid, Option<Value>)> = sqlx::query_as(
        "select w.user_id, w.secret_id, s.household_id, s.profile_id, s.cursor
         from openwearables_user w join source s on s.id = w.source_id
         where s.id = $1 and s.kind = 'openwearables' and s.enabled and s.profile_id is not null",
    )
    .bind(source)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|(user, secret, household, profile, cursor)| Binding {
        user,
        secret,
        household,
        profile,
        cursor,
    }))
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum ConnectionStatus {
    Active,
    Expired,
    Revoked,
    #[serde(other)]
    Unknown,
}

#[derive(Deserialize)]
struct Connection {
    provider: String,
    status: ConnectionStatus,
}

#[derive(Deserialize, Serialize)]
struct Origin {
    provider: String,
}

#[derive(Deserialize, Serialize)]
struct Sample {
    #[serde(rename = "type")]
    series: String,
    timestamp: DateTime<FixedOffset>,
    value: f64,
    unit: String,
    source: Option<Origin>,
    zone_offset: Option<String>,
    window_seconds: Option<i64>,
    period_end: Option<DateTime<Utc>>,
    computed_by: Option<String>,
}

#[derive(Deserialize, Serialize)]
struct Score {
    id: String,
    provider: Option<String>,
    source: Option<Origin>,
    category: String,
    recorded_at: DateTime<Utc>,
    value: Option<f64>,
    qualifier: Option<String>,
    components: Option<Value>,
    zone_offset: Option<String>,
}

#[derive(Deserialize)]
struct Page<T> {
    data: Vec<T>,
    pagination: Pagination,
}

#[derive(Deserialize)]
struct Pagination {
    has_more: bool,
    next_cursor: Option<String>,
}

struct Pull<'a> {
    base: &'a Url,
    user: String,
    key: &'a SecretString,
    since: DateTime<Utc>,
    until: DateTime<Utc>,
    remaining: usize,
    connections: HashMap<String, ConnectionStatus>,
}

impl Pull<'_> {
    async fn read<T: DeserializeOwned>(&mut self, part: &str, query: &[(&str, String)]) -> Result<T> {
        let url = endpoint(self.base, &["users", &self.user, part], query);
        let response = get(&url, Some(self.key)).await?;
        self.remaining = self
            .remaining
            .checked_sub(response.body.len())
            .ok_or_else(|| JobError::permanent("Open Wearables pull exceeded its byte limit"))?;
        parse(&response.body)
    }

    fn accepts(&self, provider: &str, out: &mut RunOutcome) -> bool {
        if !allowed(provider) {
            let key = if providers::policy()
                .providers
                .get(provider)
                .is_some_and(|p| p.ingest == providers::Ingest::Drop)
            {
                format!("provider_{provider}")
            } else {
                "provider_other".into()
            };
            *out.skipped.entry(key).or_default() += 1;
            return false;
        }
        let skipped = match self.connections.get(provider) {
            Some(ConnectionStatus::Active) => return true,
            Some(ConnectionStatus::Expired) => "connection_expired",
            Some(ConnectionStatus::Revoked) => "connection_revoked",
            _ => "connection_inactive",
        };
        *out.skipped.entry(skipped.into()).or_default() += 1;
        false
    }

    async fn series(&mut self, resolution: &str, out: &mut RunOutcome) -> Result<Vec<Sample>> {
        let mut rows = Vec::new();
        let mut query = vec![
            ("start_time", self.since.to_rfc3339()),
            ("end_time", self.until.to_rfc3339()),
            ("resolution", resolution.into()),
            ("limit", PAGE.to_string()),
        ];
        let mut cursors = HashSet::new();
        for _ in 0..MAX_PAGES {
            let page: Page<Sample> = self.read("timeseries", &query).await?;
            rows.extend(page.data.into_iter().filter(|row| {
                out.seen += 1;
                let provider = row.source.as_ref().map_or("", |s| s.provider.as_str());
                if !self.accepts(provider, out)
                    || (resolution == "1day" && row.computed_by.as_deref() != Some("provider"))
                {
                    return false;
                }
                if !scores::is_score_key(&row.series) && metric(&row.series, &row.unit, row.value).is_none() {
                    *out.skipped.entry("unsupported_series".into()).or_default() += 1;
                    return false;
                }
                true
            }));
            if !page.pagination.has_more {
                return Ok(rows);
            }
            let next = page
                .pagination
                .next_cursor
                .filter(|next| !next.is_empty() && cursors.insert(next.clone()))
                .ok_or_else(|| JobError::permanent("Open Wearables did not advance its time-series cursor"))?;
            query.truncate(4);
            query.push(("cursor", next));
        }
        Err(JobError::permanent(
            "Open Wearables time series exceeded its page limit",
        ))
    }

    async fn scores(&mut self, out: &mut RunOutcome) -> Result<Vec<Score>> {
        let mut rows = Vec::new();
        let mut offset = 0;
        for _ in 0..MAX_PAGES {
            let page: Page<Score> = self
                .read(
                    "health-scores",
                    &[
                        ("start_date", self.since.to_rfc3339()),
                        ("end_date", self.until.to_rfc3339()),
                        ("limit", PAGE.to_string()),
                        ("offset", offset.to_string()),
                    ],
                )
                .await?;
            let count = page.data.len();
            rows.extend(page.data.into_iter().filter(|row| {
                out.seen += 1;
                let provider = row.provider.as_deref().unwrap_or("");
                if !self.accepts(provider, out) {
                    return false;
                }
                if row.source.as_ref().is_some_and(|s| s.provider != provider) {
                    *out.skipped.entry("provider_other".into()).or_default() += 1;
                    return false;
                }
                true
            }));
            if !page.pagination.has_more {
                return Ok(rows);
            }
            if count == 0 {
                return Err(JobError::permanent("Open Wearables did not advance its score offset"));
            }
            offset += count;
        }
        Err(JobError::permanent(
            "Open Wearables health scores exceeded their page limit",
        ))
    }
}

#[derive(Serialize)]
struct Bundle {
    daily: Vec<Sample>,
    raw: Vec<Sample>,
    scores: Vec<Score>,
}

async fn poll(ctx: &JobContext, base: &Url, source: Uuid) -> Result<RunOutcome> {
    let Some(binding) = binding(&ctx.pool, source).await? else {
        return Ok(RunOutcome::default());
    };
    let mut conn = ctx.pool.acquire().await?;
    let sb = SecretBox::from_settings(&ctx.settings)?;
    let key = read_secret(&mut conn, &sb, binding.household, binding.secret).await?;
    drop(conn);
    let until = Utc::now();
    let since = binding
        .cursor
        .as_ref()
        .and_then(|v| v.get("until"))
        .and_then(Value::as_str)
        .and_then(|v| v.parse::<DateTime<Utc>>().ok())
        .map_or(until - Duration::days(BACKFILL_DAYS), |t| {
            t.min(until) - Duration::days(OVERLAP_DAYS)
        });
    let mut pull = Pull {
        base,
        user: binding.user.to_string(),
        key: &key,
        since,
        until,
        remaining: MAX_BYTES_PER_PULL,
        connections: HashMap::new(),
    };
    pull.connections = pull
        .read::<Vec<Connection>>("connections", &[])
        .await?
        .into_iter()
        .filter(|c| allowed(&c.provider))
        .map(|c| (c.provider, c.status))
        .collect();
    let needs_consent = pull
        .connections
        .values()
        .any(|s| matches!(s, ConnectionStatus::Expired | ConnectionStatus::Revoked));
    let mut out = RunOutcome::default();
    let daily = pull.series("1day", &mut out).await?;
    let raw = pull.series("raw", &mut out).await?;
    let scores = pull.scores(&mut out).await?;
    let bundle = Bundle { daily, raw, scores };

    let mut tx = ctx.pool.begin().await?;
    // Serialize with setup and with a reconcile job for this user. Check the binding
    // after grabbing the lock, so a rotation that just committed invalidates this pull.
    lock_user(&mut tx, binding.user).await?;
    let live: bool = sqlx::query_scalar(
        "select true from source s join openwearables_user w on w.source_id = s.id
         where s.id = $1 and s.enabled and s.profile_id = $2 and w.secret_id = $3 and s.cursor is not distinct from $4
         for update of s",
    )
    .bind(source)
    .bind(binding.profile)
    .bind(binding.secret)
    .bind(&binding.cursor)
    .fetch_optional(&mut *tx)
    .await?
    .unwrap_or(false);
    if !live {
        return Ok(RunOutcome::default());
    }
    keep_bundle(ctx, &mut tx, source, binding.profile, &bundle).await?;
    write(&mut tx, source, binding.profile, bundle, &mut out).await?;
    sqlx::query(
        "update source set reconsent_needed_at = case when $2 then coalesce(reconsent_needed_at, now()) else null end
         where id = $1",
    )
    .bind(source)
    .bind(needs_consent)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    if !needs_consent {
        out.cursor = Some(json!({"until": until}));
    }
    Ok(out)
}

async fn keep_bundle(
    ctx: &JobContext,
    conn: &mut PgConnection,
    source: Uuid,
    profile: Uuid,
    bundle: &Bundle,
) -> Result<()> {
    if bundle.daily.is_empty() && bundle.raw.is_empty() && bundle.scores.is_empty() {
        return Ok(());
    }
    let bytes = serde_json::to_vec(bundle).map_err(ownhealth_core::Error::from)?;
    let (stored, _) = store_bytes(&ctx.blobs, BlobOwner::Profile(profile), bytes).await?;
    record_item(
        conn,
        source,
        &format!("bundle:{stored}"),
        Some(&stored),
        None,
        None,
        Some(profile),
    )
    .await?;
    Ok(())
}

fn metric(series: &str, unit: &str, value: f64) -> Option<(&'static str, f64)> {
    Some(match (series, unit) {
        ("heart_rate", "bpm" | "/min") => ("hr", value),
        ("resting_heart_rate", "bpm" | "/min") => ("hr_resting", value),
        ("heart_rate_variability_rmssd", "ms") => ("hrv_rmssd", value),
        ("heart_rate_variability_sdnn", "ms") => ("hrv_sdnn", value),
        ("steps", "count" | "steps") => ("steps", value),
        ("oxygen_saturation", "percent" | "%") => ("spo2", value),
        ("respiratory_rate", "brpm" | "breaths/min" | "/min") => ("resp_rate", value),
        ("weight", "kg") => ("weight", value),
        ("blood_pressure_systolic", "mmHg" | "mm[Hg]") => ("bp_systolic", value),
        ("blood_pressure_diastolic", "mmHg" | "mm[Hg]") => ("bp_diastolic", value),
        ("active_energy", "kcal") => ("active_energy", value),
        ("distance_walking_running", "meters" | "m") => ("distance", value / 1000.0),
        ("distance_walking_running", "km") => ("distance", value),
        ("vo2_max", "ml_kg_min" | "mL/kg/min" | "ml/kg/min") => ("vo2max", value),
        _ => return None,
    })
}

fn score_category(series: &str) -> &'static str {
    let name = series.to_ascii_lowercase().replace(['-', ' ', '_'], "");
    ["body_battery", "readiness", "recovery", "sleep", "stress", "strain"]
        .into_iter()
        .find(|category| name.contains(&category.replace('_', "")))
        .unwrap_or_else(|| {
            if name.contains("charge") {
                "recovery"
            } else {
                "activity"
            }
        })
}

fn zone_offset(sample: &Sample) -> Option<i16> {
    let Some(offset) = &sample.zone_offset else {
        return i16::try_from(sample.timestamp.offset().local_minus_utc() / 60)
            .ok()
            .filter(|off| *off != 0);
    };
    let (hours, minutes) = offset.get(1..)?.split_once(':')?;
    let hours: i16 = hours.parse().ok()?;
    let minutes: i16 = minutes.parse().ok()?;
    if !(0..24).contains(&hours) || !(0..60).contains(&minutes) {
        return None;
    }
    match offset.as_bytes()[0] {
        b'+' => Some(hours * 60 + minutes),
        b'-' => Some(-hours * 60 - minutes),
        _ => None,
    }
}

fn window_end(sample: &Sample) -> Option<DateTime<Utc>> {
    let start = sample.timestamp.to_utc();
    sample.period_end.filter(|t| *t > start).or_else(|| {
        let window = sample.window_seconds.filter(|s| *s > 0)?;
        start.checked_add_signed(Duration::try_seconds(window)?)
    })
}

async fn write(
    conn: &mut PgConnection,
    source: Uuid,
    profile: Uuid,
    bundle: Bundle,
    out: &mut RunOutcome,
) -> Result<()> {
    let mut device_ids = HashMap::new();
    let mut values = Vec::new();
    let mut vendor = BTreeMap::new();
    for (daily, row) in bundle
        .daily
        .into_iter()
        .map(|r| (true, r))
        .chain(bundle.raw.into_iter().map(|r| (false, r)))
    {
        let Some(origin) = &row.source else { continue };
        let provider = &origin.provider;
        let t = row.timestamp.to_utc();
        if scores::is_score_key(&row.series) {
            let category = score_category(&row.series).to_string();
            vendor.insert(
                (provider.clone(), category.clone(), t),
                scores::Score {
                    provider: provider.clone(),
                    category,
                    t,
                    value: row.value,
                    qualifier: None,
                    components: Some(json!({"series": row.series, "unit": row.unit})),
                },
            );
            continue;
        }
        let Some((key, value)) = metric(&row.series, &row.unit, row.value) else {
            continue;
        };
        let t_end = window_end(&row);
        // Keep a provider's daily/overnight HRV separate from spot readings. Even when
        // both start at the same instant, neither may replace the other in the sample store.
        let context = if key.starts_with("hrv_") {
            if daily || t_end.is_some_and(|end| end - t >= Duration::hours(1)) {
                "hrv_window"
            } else {
                "hrv_spot"
            }
        } else if daily {
            "daily"
        } else {
            "raw"
        };
        let cache_key = (provider.clone(), context);
        let wsid = if let Some(id) = device_ids.get(&cache_key) {
            *id
        } else {
            let device = match (provider.as_str(), context) {
                ("polar", "hrv_window") => Some("Polar Nightly Recharge"),
                ("polar", "hrv_spot") => Some("Polar"),
                _ => None,
            };
            let id = store::device_source(
                conn,
                profile,
                provider,
                device,
                Some(&format!("{source}:{context}")),
                Some(source),
            )
            .await?;
            device_ids.insert(cache_key, id);
            id
        };
        values.push(store::Sample {
            metric: key,
            t,
            t_end,
            v: value,
            tz: zone_offset(&row),
            source: wsid,
        });
    }
    for row in bundle.scores {
        let (Some(provider), Some(value)) = (row.provider, row.value) else {
            *out.skipped.entry("score_no_value".into()).or_default() += 1;
            continue;
        };
        if !scores::CATEGORIES.contains(&row.category.as_str()) {
            *out.skipped.entry("unsupported_score".into()).or_default() += 1;
            continue;
        }
        // The dedicated endpoint has the vendor's qualifiers and components.
        vendor.insert(
            (provider.clone(), row.category.clone(), row.recorded_at),
            scores::Score {
                provider,
                category: row.category,
                t: row.recorded_at,
                value,
                qualifier: row.qualifier,
                components: row.components,
            },
        );
    }
    let written = store::write(conn, profile, None, values, store::Conflict::Refresh).await?;
    let score_count = vendor.len() as i64;
    let score_written = if vendor.is_empty() {
        scores::Written::default()
    } else {
        scores::write(conn, profile, source, &vendor.into_values().collect::<Vec<_>>()).await?
    };
    if score_written.dropped > 0 {
        *out.skipped.entry("invalid_score".into()).or_default() += score_written.dropped as i64;
    }
    out.new = (written.new + score_written.written as i64) as i32;
    out.duplicate = (written.duplicate() + score_count - (score_written.written + score_written.dropped) as i64) as i32;
    Ok(())
}

async fn tombstones(pool: &PgPool) -> Result<Vec<Uuid>> {
    Ok(
        sqlx::query_scalar("select user_id from openwearables_user where source_id is null order by user_id")
            .fetch_all(pool)
            .await?,
    )
}

async fn delete_user(ctx: &JobContext, base: &Url, user: Uuid) -> Result<bool> {
    let mut tx = ctx.pool.begin().await?;
    lock_user(&mut tx, user).await?;
    let tombstone: Option<(Uuid, Uuid)> = sqlx::query_as(
        "select k.id, k.household_id from openwearables_user w join secret k on k.id = w.secret_id
         where w.user_id = $1 and w.source_id is null and k.purpose = 'openwearables_api_key'",
    )
    .bind(user)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((secret, household)) = tombstone else {
        return Ok(false);
    };
    let sb = SecretBox::from_settings(&ctx.settings)?;
    let key = read_secret(&mut tx, &sb, household, secret).await?;
    let url = endpoint(base, &["users", &user.to_string()], &[]);
    let resp = net::delete(url.as_str(), &egress(), &[(API_KEY, key.expose())])
        .await
        .map_err(network)?;
    // The response could have been lost after the remote delete committed.
    if resp.status != 404 {
        status(resp)?;
    }
    // The secret owns the tombstone. Once the sidecar confirms deletion, discard both.
    ownhealth_core::secrets::delete_secret(&mut tx, household, secret).await?;
    tx.commit().await?;
    Ok(true)
}

async fn schedule_reconcile(ctx: &JobContext) -> ownhealth_core::Result<u64> {
    let pending = tombstones(&ctx.pool)
        .await
        .map_err(|e| ownhealth_core::Error::Other(anyhow::anyhow!(e.text())))?;
    let mut conn = ctx.pool.acquire().await?;
    for user in pending {
        let key = format!("{RECONCILE}:{user}");
        jobs::enqueue(
            &mut conn,
            RECONCILE,
            &json!({"user_id": user}),
            EnqueueOptions::dedupe(&key),
        )
        .await?;
    }
    // A default-sources hook reports sources added. This one only queues cleanup.
    Ok(0)
}

pub fn register() {
    sources::register_poller(KIND, |ctx, source| {
        Box::pin(async move { poll(&ctx, &internal(), source.id).await })
    });
    sources::register_default_sources(KIND, |ctx| Box::pin(async move { schedule_reconcile(&ctx).await }));
    jobs::register_handler(RECONCILE, |ctx, job| {
        Box::pin(async move {
            let deleted = delete_user(&ctx, &internal(), job.uuid("user_id")?).await?;
            Ok(Some(json!({"deleted": deleted})))
        })
    });
}

#[cfg(test)]
async fn reconcile(ctx: &JobContext, base: &Url) -> Result<u64> {
    let mut count = 0;
    for user in tombstones(&ctx.pool).await? {
        count += u64::from(delete_user(ctx, base, user).await?);
    }
    Ok(count)
}

#[cfg(test)]
#[path = "openwearables/tests.rs"]
mod tests;
