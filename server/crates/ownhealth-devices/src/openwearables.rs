//! Pulls the Open Wearables 0.11 API through the internal Compose address. The provider policy
//! applies at the wire boundary, before any bundle, measurement or score is stored.
//!
//! A poll reads each allowlisted vendor with an active connection on its own (`provider=`), only for
//! the series OwnHealth maps (`types=`), one day at a time. Each day is stored and committed before
//! the next one is read, and it moves that vendor's own cursor. So a long window or a later failure
//! keeps what already arrived, and a vendor whose connection ended or whose answers do not read never
//! holds the others back; a row that does not read is counted and left out. A poll reads the 7 days
//! before a vendor's cursor again for late uploads and corrections. `source_item` archives each
//! distinct payload once. The sample and score stores compare incoming rows with their current
//! values. A failed re-read is named and skipped so the vendor's newer days can still be stored.

use std::collections::{BTreeMap, HashMap, HashSet};

use chrono::{DateTime, Duration, FixedOffset, Utc};
use ownhealth_core::{
    arrive::store_bytes,
    blobs::{BlobOwner, sha256_hex_to_bytes},
    jobs::{self, EnqueueOptions, Job, JobContext, JobError},
    net::{self, Egress, Fetched},
    providers,
    secrets::{SecretBox, SecretString, read_secret, store_secret},
    sources::{self, NewSource, RunOutcome, RunProblem},
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::{PgConnection, PgPool};
use url::Url;
use uuid::Uuid;

use crate::{scores, store};

#[path = "gaps.rs"]
mod gaps;
use gaps::{ConnectionHealth, Connections, Status};

const KIND: &str = "openwearables";
const PURPOSE: &str = "openwearables_api_key";
const RECONCILE: &str = "openwearables.reconcile";
const INTERNAL: &str = "http://ow-app:8000";
const POLL_SECONDS: i32 = 900;
const PAGE: usize = 500;
/// Pages of one series resolution for one vendor and one day.
const MAX_PAGES: usize = 300;
/// Bytes read for one vendor and one day.
const MAX_BYTES_PER_DAY: usize = 48 * 1024 * 1024;
const OVERLAP_DAYS: i64 = 7;
const BACKFILL_DAYS: i64 = 30;
/// Days one poll reads per vendor at most; the next poll goes on from the vendor's cursor. Above
/// OVERLAP_DAYS + 1, so a cursor that is behind always moves.
const MAX_DAYS_PER_POLL: usize = 31;
const API_KEY: &str = "x-open-wearables-api-key";
/// The problem a poll records when an allowlisted vendor's connection ended or was revoked in Open
/// Wearables. Its message names the vendors, and the Connections screen reads it.
pub const CONNECTION_ENDED: &str = "ow_connection_ended";
/// The problem a poll records for a vendor whose reads failed: `<vendor>: <error>`. The other vendors
/// go on; the failed vendor keeps its cursor and is read from there on the next poll. A failed re-read
/// of the days before the cursor is recorded the same way, and the vendor goes on after its cursor.
pub const VENDOR_FAILED: &str = "ow_vendor_failed";

/// The vendors OwnHealth takes through Open Wearables. Each one also needs `ingest: allow` in
/// data/connectors/providers.yaml; setup switches every other provider off in the sidecar.
pub const PROVIDERS: [&str; 4] = ["oura", "polar", "whoop", "withings"];

/// The series OwnHealth stores: the units it takes, the sample key, and what to divide the value by.
const METRICS: [(&str, &[&str], &str, f64); 14] = [
    ("heart_rate", &["bpm", "/min"], "hr", 1.0),
    ("resting_heart_rate", &["bpm", "/min"], "hr_resting", 1.0),
    ("heart_rate_variability_rmssd", &["ms"], "hrv_rmssd", 1.0),
    ("heart_rate_variability_sdnn", &["ms"], "hrv_sdnn", 1.0),
    ("steps", &["count", "steps"], "steps", 1.0),
    ("oxygen_saturation", &["percent", "%"], "spo2", 1.0),
    ("respiratory_rate", &["brpm", "breaths/min", "/min"], "resp_rate", 1.0),
    ("weight", &["kg"], "weight", 1.0),
    ("blood_pressure_systolic", &["mmHg", "mm[Hg]"], "bp_systolic", 1.0),
    ("blood_pressure_diastolic", &["mmHg", "mm[Hg]"], "bp_diastolic", 1.0),
    ("active_energy", &["kcal"], "active_energy", 1.0),
    ("distance_walking_running", &["meters", "m"], "distance", 1000.0),
    ("distance_walking_running", &["km"], "distance", 1.0),
    ("vo2_max", &["ml_kg_min", "mL/kg/min", "ml/kg/min"], "vo2max", 1.0),
];

/// Vendor scores that come as a time series (WHOOP's workout effort). They go to `vendor_score`.
const SCORE_SERIES: [&str; 1] = ["workout_effort_score"];

type Result<T> = std::result::Result<T, JobError>;

fn not_http(base: &Url) -> JobError {
    JobError::permanent(format!(
        "Open Wearables address {base} is not an HTTP origin. / La dirección de Open Wearables {base} no es un origen HTTP."
    ))
}

fn internal() -> Result<Url> {
    Url::parse(INTERNAL).map_err(|_| {
        JobError::permanent(format!(
            "Invalid Open Wearables address {INTERNAL}. / Dirección de Open Wearables no válida: {INTERNAL}."
        ))
    })
}

fn allowed(provider: &str) -> bool {
    PROVIDERS.contains(&provider) && providers::ingest_allowed(provider)
}

/// The series a pull asks for: every mapped one and the score series, never a workout stream such as
/// latitude or power, which OwnHealth would drop.
fn series_types() -> Vec<&'static str> {
    let mut types: Vec<&str> = METRICS.iter().map(|m| m.0).collect();
    types.dedup();
    types.extend(SCORE_SERIES);
    types
}

fn endpoint(base: &Url, path: &[&str], query: &[(&str, String)]) -> Result<Url> {
    let mut url = base.clone();
    url.path_segments_mut()
        .map_err(|()| not_http(base))?
        .clear()
        .extend(["api", "v1"])
        .extend(path.iter().copied());
    if !query.is_empty() {
        url.query_pairs_mut().extend_pairs(query);
    }
    Ok(url)
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

/// Name the setup step an error comes from, in English and Spanish, so the operator knows which
/// credential or request to check.
fn during(step: &str) -> impl FnOnce(JobError) -> JobError + '_ {
    move |mut e| {
        e.message = format!("{step}: {}", e.message);
        e
    }
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
    let login = endpoint(base, &["auth", "login"], &[])?;
    let sign_in = "Developer sign-in with the email and password / Inicio de sesión de desarrollador con el correo y la contraseña";
    let response = net::post_form(
        login.as_str(),
        &egress(),
        &[],
        &[("username", email), ("password", password.expose())],
    )
    .await
    .map_err(network)
    .and_then(status)
    .map_err(during(sign_in))?;
    let login: Login = parse(&response.body).map_err(during(sign_in))?;
    let auth = SecretString::new(format!("Bearer {}", login.access_token));

    let provider_list = "Provider list / Lista de proveedores";
    let listed = endpoint(base, &["oauth", "providers"], &[])?;
    let listed = get(&listed, None).await.map_err(during(provider_list))?;
    for provider in parse::<Vec<Provider>>(&listed.body).map_err(during(provider_list))? {
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
            return Err(JobError::permanent(
                "Open Wearables returned an invalid provider id. / Open Wearables devolvió un id de proveedor no válido.",
            ));
        }
        let url = endpoint(base, &["oauth", "providers", &provider.provider], &[])?;
        let step = format!("Switching off {0} / Desactivación de {0}", provider.provider);
        let response = net::put_json(
            url.as_str(),
            &egress(),
            &[("authorization", auth.expose())],
            &json!({"is_enabled": false}),
        )
        .await
        .map_err(network)
        .and_then(status)
        .map_err(during(&step))?;
        let confirmed: Provider = parse(&response.body).map_err(during(&step))?;
        if confirmed.provider != provider.provider || confirmed.is_enabled {
            return Err(JobError::permanent(format!(
                "Open Wearables did not switch off {0}. / Open Wearables no desactivó {0}.",
                provider.provider
            )));
        }
    }
    Ok(())
}

/// The profile the operator names: its id, or its short name (`alex`) when one household has it.
pub async fn profile_id(pool: &PgPool, given: &str) -> Result<Uuid> {
    if let Ok(id) = given.parse::<Uuid>() {
        return Ok(id);
    }
    let found: Vec<Uuid> = sqlx::query_scalar("select id from profile where slug = $1 order by id limit 2")
        .bind(given)
        .fetch_all(pool)
        .await?;
    match found[..] {
        [id] => Ok(id),
        [] => Err(JobError::permanent(format!(
            "No profile is named {given}. / Ningún perfil se llama {given}."
        ))),
        _ => Err(JobError::permanent(format!(
            "Several households have a profile named {given}: pass the profile id. / Varios hogares tienen un perfil llamado {given}: indica el id del perfil."
        ))),
    }
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
    setup(ctx, &internal()?, profile, user, email, password, key).await
}

/// The profile's household, and the source and secret of the user's current binding, if any. Refuses a
/// user that waits for deletion or that is linked to another profile.
async fn check_binding(
    conn: &mut PgConnection,
    profile: Uuid,
    user: Uuid,
    lock: bool,
) -> Result<(Uuid, Option<(Uuid, Uuid)>)> {
    let household: Uuid = sqlx::query_scalar(if lock {
        "select household_id from profile where id = $1 for key share"
    } else {
        "select household_id from profile where id = $1"
    })
    .bind(profile)
    .fetch_optional(&mut *conn)
    .await?
    .ok_or_else(|| JobError::permanent("Unknown OwnHealth profile. / Perfil de OwnHealth desconocido."))?;
    let binding: Option<(Uuid, Option<Uuid>, bool)> = sqlx::query_as(
        "select secret_id, source_id, delete_refused_at is not null from openwearables_user where user_id = $1",
    )
    .bind(user)
    .fetch_optional(&mut *conn)
    .await?;
    let Some((secret, source, refused)) = binding else {
        return Ok((household, None));
    };
    let source = source.ok_or_else(|| {
        JobError::permanent(if refused {
            refused_text(user)
        } else {
            "This Open Wearables user waits for deletion. / Este usuario de Open Wearables está pendiente de borrado."
                .into()
        })
    })?;
    let found: Option<Uuid> = sqlx::query_scalar(if lock {
        "select profile_id from source where id = $1 and kind = 'openwearables' for update"
    } else {
        "select profile_id from source where id = $1 and kind = 'openwearables'"
    })
    .bind(source)
    .fetch_optional(&mut *conn)
    .await?
    .flatten();
    if found != Some(profile) {
        return Err(JobError::permanent(
            "This Open Wearables user is linked to another profile. / Este usuario de Open Wearables está vinculado a otro perfil.",
        ));
    }
    Ok((household, Some((source, secret))))
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
    // Refuse what no request can change before talking to the sidecar, then make the requests with
    // no lock held: a profile delete never waits behind them. The checks run again under the locks.
    check_binding(&mut *ctx.pool.acquire().await?, profile, user, false).await?;
    let url = endpoint(base, &["users", &user.to_string()], &[])?;
    let user_check = "User check with the API key / Comprobación del usuario con la clave de API";
    let answer = get(&url, Some(key)).await.map_err(during(user_check))?;
    let confirmed: User = parse(&answer.body).map_err(during(user_check))?;
    if confirmed.id != user {
        return Err(JobError::permanent(
            "Open Wearables returned a different user. / Open Wearables devolvió otro usuario.",
        ));
    }
    enforce_provider_settings(base, email, password).await?;

    let mut tx = ctx.pool.begin().await?;
    // Serialize setups and deletion retries for this remote user, even before there is a local row.
    lock_user(&mut tx, user).await?;
    let (household, previous) = check_binding(&mut tx, profile, user, true).await?;
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

#[derive(Deserialize)]
struct Connection {
    provider: String,
    #[serde(flatten)]
    health: ConnectionHealth,
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

/// Each row of a page that reads as `T`. A row that does not (a value that is not a number, a missing
/// field) is counted as `unreadable_row` and left out, so it never stops the page, the day or the vendor.
fn readable<T: DeserializeOwned>(rows: Vec<Value>, out: &mut RunOutcome) -> Vec<T> {
    out.seen += i32::try_from(rows.len()).unwrap_or(i32::MAX);
    rows.into_iter()
        .filter_map(|row| {
            let parsed = serde_json::from_value(row).ok();
            if parsed.is_none() {
                *out.skipped.entry("unreadable_row".into()).or_default() += 1;
            }
            parsed
        })
        .collect()
}

/// One vendor's reads for one day.
struct Pull<'a> {
    base: &'a Url,
    user: String,
    key: &'a SecretString,
    provider: &'a str,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
    remaining: usize,
}

impl Pull<'_> {
    async fn read<T: DeserializeOwned>(&mut self, part: &str, query: &[(&str, String)]) -> Result<T> {
        let url = endpoint(self.base, &["users", &self.user, part], query)?;
        let response = get(&url, Some(self.key)).await?;
        self.remaining = self.remaining.checked_sub(response.body.len()).ok_or_else(|| {
            JobError::permanent("too_large: Open Wearables answers for one day passed the size limit")
        })?;
        parse(&response.body)
    }

    /// A row is taken only from the vendor this pull asked for. Any other row is counted: a provider
    /// outside the allowlist by name, as the policy names it.
    fn accepts(&self, provider: &str, out: &mut RunOutcome) -> bool {
        if provider == self.provider {
            return true;
        }
        let key = if allowed(provider) {
            "provider_mismatch".into()
        } else if providers::policy()
            .providers
            .get(provider)
            .is_some_and(|p| p.ingest == providers::Ingest::Drop)
        {
            format!("provider_{provider}")
        } else {
            "provider_other".into()
        };
        *out.skipped.entry(key).or_default() += 1;
        false
    }

    /// The vendor's day: its daily and raw series and its scores.
    async fn bundle(&mut self, out: &mut RunOutcome) -> Result<Bundle> {
        Ok(Bundle {
            daily: self.series("1day", out).await?,
            raw: self.series("raw", out).await?,
            scores: self.scores(out).await?,
        })
    }

    async fn series(&mut self, resolution: &str, out: &mut RunOutcome) -> Result<Vec<Sample>> {
        let mut rows = Vec::new();
        let mut query = vec![
            ("start_time", self.from.to_rfc3339()),
            ("end_time", self.to.to_rfc3339()),
            ("resolution", resolution.into()),
            ("provider", self.provider.into()),
            ("limit", PAGE.to_string()),
        ];
        query.extend(series_types().into_iter().map(|t| ("types", t.to_string())));
        let fixed = query.len();
        let mut cursors = HashSet::new();
        for _ in 0..MAX_PAGES {
            let page: Page<Value> = self.read("timeseries", &query).await?;
            rows.extend(readable::<Sample>(page.data, out).into_iter().filter(|row| {
                let provider = row.source.as_ref().map_or("", |s| s.provider.as_str());
                if !self.accepts(provider, out) {
                    return false;
                }
                // A daily value Open Wearables computed itself is not the vendor's.
                if resolution == "1day" && row.computed_by.as_deref() != Some("provider") {
                    *out.skipped.entry("computed_by_open_wearables".into()).or_default() += 1;
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
                .ok_or_else(|| {
                    JobError::permanent("repeated_page: Open Wearables sent a time-series cursor it had sent before")
                })?;
            query.truncate(fixed);
            query.push(("cursor", next));
        }
        Err(JobError::permanent(format!(
            "too_many_pages: Open Wearables time series stopped after {MAX_PAGES} pages"
        )))
    }

    async fn scores(&mut self, out: &mut RunOutcome) -> Result<Vec<Score>> {
        let mut rows = Vec::new();
        let mut offset = 0;
        for _ in 0..MAX_PAGES {
            let page: Page<Value> = self
                .read(
                    "health-scores",
                    &[
                        ("start_date", self.from.to_rfc3339()),
                        ("end_date", self.to.to_rfc3339()),
                        ("provider", self.provider.into()),
                        ("limit", PAGE.to_string()),
                        ("offset", offset.to_string()),
                    ],
                )
                .await?;
            let count = page.data.len();
            rows.extend(readable::<Score>(page.data, out).into_iter().filter(|row| {
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
                return Err(JobError::permanent(
                    "repeated_page: Open Wearables did not move its health-score offset",
                ));
            }
            offset += count;
        }
        Err(JobError::permanent(format!(
            "too_many_pages: Open Wearables health scores stopped after {MAX_PAGES} pages"
        )))
    }
}

#[derive(Serialize)]
struct Bundle<S = Sample, V = Score> {
    daily: Vec<S>,
    raw: Vec<S>,
    scores: Vec<V>,
}

impl<S, V> Bundle<S, V> {
    fn is_empty(&self) -> bool {
        self.daily.is_empty() && self.raw.is_empty() && self.scores.is_empty()
    }
}

/// Where a vendor's last stored day ended.
fn mark(cursor: Option<&Value>, provider: &str) -> Option<DateTime<Utc>> {
    cursor?.get("providers")?.get(provider)?.as_str()?.parse().ok()
}

async fn poll(ctx: &JobContext, base: &Url, source: Uuid) -> Result<RunOutcome> {
    let Some(binding) = binding(&ctx.pool, source).await? else {
        return Ok(RunOutcome::default());
    };
    let mut conn = ctx.pool.acquire().await?;
    let sb = SecretBox::from_settings(&ctx.settings)?;
    let key = read_secret(&mut conn, &sb, binding.household, binding.secret).await?;
    drop(conn);
    let now = Utc::now();
    let user = binding.user.to_string();
    let mut pull = Pull {
        base,
        user: user.clone(),
        key: &key,
        provider: "",
        from: now,
        to: now,
        remaining: MAX_BYTES_PER_DAY,
    };
    let mut connections = Connections::new();
    for row in pull.read::<Vec<Connection>>("connections", &[]).await? {
        if !allowed(&row.provider) {
            continue;
        }
        let health = row.health;
        connections
            .entry(row.provider)
            .and_modify(|known| {
                // A working connection supersedes an ended one for the same vendor.
                // If both are active, keep the latest reported sync.
                if health.status == Status::Active {
                    let last_synced_at = if known.status == Status::Active {
                        health.last_synced_at.max(known.last_synced_at)
                    } else {
                        health.last_synced_at
                    };
                    *known = ConnectionHealth {
                        last_synced_at,
                        ..health
                    };
                } else if known.status == Status::Unknown {
                    *known = health;
                }
            })
            .or_insert(health);
    }
    let active: Vec<_> = connections
        .iter()
        .filter_map(|(provider, health)| (health.status == Status::Active).then_some(provider.as_str()))
        .collect();
    let ended: Vec<_> = connections
        .iter()
        .filter_map(|(provider, health)| {
            matches!(health.status, Status::Revoked | Status::Expired).then_some(provider.as_str())
        })
        .collect();
    let mut out = RunOutcome::default();
    let mut cursor = binding.cursor.clone();
    // A vendor whose reads fail stops at its last stored day; the vendors after it are still read.
    let mut failed = Vec::new();
    // A failed re-read of days a vendor already stored: named, and the vendor goes on after its cursor.
    let mut skipped = Vec::new();
    'vendors: for &provider in &active {
        let stored_until = mark(cursor.as_ref(), provider).map(|t| t.min(now));
        let mut from = stored_until.map_or(now - Duration::days(BACKFILL_DAYS), |t| {
            t - Duration::days(OVERLAP_DAYS)
        });
        for _ in 0..MAX_DAYS_PER_POLL {
            if from >= now {
                break;
            }
            let to = (from + Duration::days(1)).min(now);
            let mut pull = Pull {
                base,
                user: user.clone(),
                key: &key,
                provider,
                from,
                to,
                remaining: MAX_BYTES_PER_DAY,
            };
            let bundle = match pull.bundle(&mut out).await {
                Ok(bundle) => bundle,
                Err(err) => {
                    // Days the vendor already stored: go on after them. The next poll's window starts
                    // at the same place, so it tries these days again.
                    if let Some(until) = stored_until.filter(|until| to <= *until) {
                        skipped.push((provider, err));
                        from = until;
                        continue;
                    }
                    failed.push((provider, err));
                    continue 'vendors;
                }
            };
            let Some(next) = store_day(ctx, source, &binding, cursor.as_ref(), provider, to, bundle, &mut out).await?
            else {
                // The binding changed while the day was read (rotated, deleted, switched off). This run
                // read with the old one, so it must not count as a sync of the new one.
                return Err(binding_changed());
            };
            cursor = Some(next);
            from = to;
        }
    }
    // The connection report is useful even if every data endpoint failed. Save it under the
    // same binding guard as the data, without changing any vendor's data cursor.
    let mut tx = ctx.pool.begin().await?;
    if !lock_live_binding(&mut tx, source, &binding, cursor.as_ref()).await? {
        return Err(binding_changed());
    }
    let previous = gaps::observed_connections(binding.cursor.as_ref());
    let mut observed: BTreeMap<String, ConnectionHealth> = connections
        .iter()
        .map(|(vendor, report)| {
            let known = previous.get(vendor);
            // OwnHealth keeps the clocks of its own; a report never sets them.
            let mut health = ConnectionHealth {
                seen_since: None,
                failing_since: None,
                observed_at: Some(now),
                ..*report
            };
            match health.status {
                // Keep the last known health if the sidecar reports a status we do not recognize.
                // An unknown connection is still excluded from this run's data reads.
                Status::Unknown => health = known.copied().unwrap_or(health),
                Status::Active => {
                    if health.last_synced_at.is_none() {
                        health.last_synced_at = known.and_then(|p| p.last_synced_at);
                        if health.last_synced_at.is_none() {
                            health.seen_since = Some(known.and_then(|p| p.seen_since).unwrap_or(now));
                        }
                    }
                    // A vendor whose new days do not read counts from its first such poll, until a
                    // poll stores newer days for it. Another vendor's success does not clear it.
                    if failed.iter().any(|(stopped, _)| *stopped == vendor.as_str()) {
                        let moved = mark(cursor.as_ref(), vendor) != mark(binding.cursor.as_ref(), vendor);
                        health.failing_since =
                            Some(known.and_then(|p| p.failing_since).filter(|_| !moved).unwrap_or(now));
                    }
                }
                Status::Expired | Status::Revoked => health.failing_since = known.and_then(|p| p.failing_since),
            }
            (vendor.clone(), health)
        })
        .collect();
    // A vendor that drops out of the report (its connection deleted in the sidecar) sends no more data.
    // If it was in a gap when a report last included it, it has not recovered: keep its last health, so
    // its alert stays open. A vendor that was syncing then has nothing to report and is dropped, even if
    // its window has passed since.
    for (vendor, health) in previous {
        if gaps::in_gap_when_observed(&vendor, health) {
            observed.entry(vendor).or_insert(health);
        }
    }
    let mut next = cursor.unwrap_or_else(|| json!({}));
    next[gaps::CURSOR_KEY] = json!(observed);
    sqlx::query(
        "update source set cursor = $3,
           reconsent_needed_at = case when $2 then coalesce(reconsent_needed_at, now()) else null end
         where id = $1",
    )
    .bind(source)
    .bind(!ended.is_empty())
    .bind(next)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    let any_ended = !ended.is_empty();
    // The ended vendors come first: the Connections screen reads the first problem.
    if any_ended {
        out.problems.push(RunProblem {
            code: CONNECTION_ENDED.into(),
            message: ended.join(", "),
        });
    }
    // When every vendor failed, the run failed: nothing of this poll is new, and the job says why.
    let first_error = (!failed.is_empty() && failed.len() == active.len()).then(|| failed[0].1.clone());
    // A vendor that then failed after its cursor too is named once, for the error that stopped it.
    skipped.retain(|(provider, _)| !failed.iter().any(|(stopped, _)| stopped == provider));
    out.problems
        .extend(skipped.into_iter().chain(failed).map(|(provider, err)| RunProblem {
            code: VENDOR_FAILED.into(),
            message: format!("{provider}: {}", err.text()),
        }));
    match first_error {
        // The run record keeps the ended vendors ahead of the error, so the screen still names them.
        Some(err) if any_ended => {
            out.error = Some(err.text());
            Ok(out)
        }
        Some(err) => Err(err),
        None => Ok(out),
    }
}

fn binding_changed() -> JobError {
    JobError::typed(
        "ow_binding_changed",
        "The Open Wearables link changed while this poll ran; the next poll uses the new one. / El vínculo con Open Wearables cambió durante esta sincronización; la siguiente usa el nuevo.",
    )
}

async fn lock_live_binding(
    conn: &mut PgConnection,
    source: Uuid,
    binding: &Binding,
    cursor: Option<&Value>,
) -> Result<bool> {
    // Serialize with setup and with a reconcile job for this user. Check the binding
    // after grabbing the lock, so a rotation that just committed invalidates this pull.
    lock_user(conn, binding.user).await?;
    Ok(sqlx::query_scalar(
        "select true from source s join openwearables_user w on w.source_id = s.id
         where s.id = $1 and s.enabled and s.profile_id = $2 and w.secret_id = $3 and s.cursor is not distinct from $4
         for update of s",
    )
    .bind(source)
    .bind(binding.profile)
    .bind(binding.secret)
    .bind(cursor)
    .fetch_optional(conn)
    .await?
    .unwrap_or(false))
}

/// Store one vendor's day and move its cursor to `to`, in one transaction. None when the binding
/// changed since the poll read it: nothing is written then.
#[allow(clippy::too_many_arguments)]
async fn store_day(
    ctx: &JobContext,
    source: Uuid,
    binding: &Binding,
    cursor: Option<&Value>,
    provider: &str,
    to: DateTime<Utc>,
    bundle: Bundle,
    out: &mut RunOutcome,
) -> Result<Option<Value>> {
    let mut tx = ctx.pool.begin().await?;
    if !lock_live_binding(&mut tx, source, binding, cursor).await? {
        return Ok(None);
    }
    archive_new_rows(ctx, &mut tx, source, binding.profile, &bundle).await?;
    write(&mut tx, source, binding.profile, bundle, out).await?;
    let mut next = cursor.cloned().unwrap_or_else(|| json!({}));
    if mark(cursor, provider).is_none_or(|t| t < to) {
        next["providers"][provider] = json!(to);
    }
    sqlx::query("update source set cursor = $2 where id = $1")
        .bind(source)
        .bind(&next)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Some(next))
}

/// A row's archive key: its kind and the first 128 bits of the SHA-256 of its content.
fn row_key<T: Serialize>(kind: &str, row: &T) -> Result<String> {
    let bytes = serde_json::to_vec(row).map_err(ownhealth_core::Error::from)?;
    let digest: [u8; 32] = Sha256::digest(&bytes).into();
    let digest = hex::encode(digest);
    Ok(format!("ow{kind}:{}", &digest[..32]))
}

/// Archive each distinct payload once, before projecting the day's samples and scores.
async fn archive_new_rows(
    ctx: &JobContext,
    conn: &mut PgConnection,
    source: Uuid,
    profile: Uuid,
    bundle: &Bundle,
) -> Result<()> {
    fn keyed<'a, T: Serialize>(kind: &str, rows: &'a [T]) -> Result<Vec<(String, &'a T)>> {
        rows.iter().map(|row| Ok((row_key(kind, row)?, row))).collect()
    }

    fn unseen<T>(rows: Vec<(String, T)>, seen: &mut HashSet<String>, new_keys: &mut Vec<String>) -> Vec<T> {
        let mut kept = Vec::new();
        for (key, row) in rows {
            if seen.insert(key.clone()) {
                new_keys.push(key);
                kept.push(row);
            }
        }
        kept
    }

    if bundle.is_empty() {
        return Ok(());
    }
    let daily = keyed("d", &bundle.daily)?;
    let raw = keyed("r", &bundle.raw)?;
    let scores = keyed("s", &bundle.scores)?;
    let keys: Vec<&str> = daily
        .iter()
        .chain(raw.iter())
        .map(|r| r.0.as_str())
        .chain(scores.iter().map(|r| r.0.as_str()))
        .collect();
    let mut seen: HashSet<String> =
        sqlx::query_scalar("select external_id from source_item where source_id = $1 and external_id = any($2)")
            .bind(source)
            .bind(&keys)
            .fetch_all(&mut *conn)
            .await?
            .into_iter()
            .collect();
    let mut new_keys = Vec::new();
    let fresh = Bundle {
        daily: unseen(daily, &mut seen, &mut new_keys),
        raw: unseen(raw, &mut seen, &mut new_keys),
        scores: unseen(scores, &mut seen, &mut new_keys),
    };
    if fresh.is_empty() {
        return Ok(());
    }
    let bytes = serde_json::to_vec(&fresh).map_err(ownhealth_core::Error::from)?;
    let (stored, _) = store_bytes(&ctx.blobs, BlobOwner::Profile(profile), bytes).await?;
    sqlx::query(
        "insert into source_item (source_id, external_id, blob_sha256, profile_id)
         select $1, k, $3, $4 from unnest($2::text[]) k on conflict do nothing",
    )
    .bind(source)
    .bind(&new_keys)
    .bind(sha256_hex_to_bytes(&stored).map_err(ownhealth_core::Error::from)?)
    .bind(profile)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

fn metric(series: &str, unit: &str, value: f64) -> Option<(&'static str, f64)> {
    METRICS
        .iter()
        .find(|(name, units, ..)| *name == series && units.contains(&unit))
        .map(|(_, _, key, divisor)| (*key, value / divisor))
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

/// What the Wearables page calls a vendor's device in each context: the brand for readings, `· 24 h`
/// for the vendor's daily values, and Polar's own name for its nightly HRV. Each name is unique per
/// vendor, so the page never shows an internal id to tell two apart.
fn device_name(provider: &str, context: &str) -> String {
    let brand = match provider {
        "oura" => "Oura Ring",
        "polar" => "Polar",
        "whoop" => "WHOOP",
        "withings" => "Withings",
        other => other,
    };
    match context {
        "nightly" => "Polar Nightly Recharge".into(),
        "daily" => format!("{brand} · 24 h"),
        _ => brand.into(),
    }
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
        // Keep a provider's daily or overnight HRV apart from its spot readings: even when both
        // start at the same instant, neither may replace the other in the sample store. Daily values
        // and readings are apart too, so a day's steps are never counted twice.
        let window = daily || t_end.is_some_and(|end| end - t >= Duration::hours(1));
        let context = match (key.starts_with("hrv_") && window, provider.as_str(), daily) {
            (true, "polar", _) => "nightly",
            (true, _, _) | (false, _, true) => "daily",
            (false, _, false) => "raw",
        };
        let cache_key = (provider.clone(), context);
        let wsid = if let Some(id) = device_ids.get(&cache_key) {
            *id
        } else {
            let id = store::device_source(
                conn,
                profile,
                provider,
                Some(&device_name(provider, context)),
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
    out.new += (written.new + score_written.written as i64) as i32;
    out.duplicate +=
        (written.duplicate() + score_count - (score_written.written + score_written.dropped) as i64) as i32;
    Ok(())
}

/// The text of a cleanup the sidecar refused: which user, and what the operator runs.
fn refused_text(user: Uuid) -> String {
    format!(
        "Open Wearables refused to delete user {user} with the stored API key. Run `ownhealth openwearables-delete --user {user}` with a key that works (deploy/README.md, Wearables). / Open Wearables rechazó borrar el usuario {user} con la clave de API guardada. Ejecuta `ownhealth openwearables-delete --user {user}` con una clave que funcione (deploy/README.md, Wearables)."
    )
}

/// The sidecar users whose profile or source is gone and whose deletion the scheduler retries, with
/// the household their key belongs to. A user whose deletion the sidecar refused waits for the operator.
async fn tombstones(pool: &PgPool) -> ownhealth_core::Result<Vec<(Uuid, Uuid)>> {
    Ok(sqlx::query_as(
        "select w.user_id, k.household_id from openwearables_user w join secret k on k.id = w.secret_id
         where w.source_id is null and w.delete_refused_at is null order by w.user_id",
    )
    .fetch_all(pool)
    .await?)
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
    let url = endpoint(base, &["users", &user.to_string()], &[])?;
    let resp = net::delete(url.as_str(), &egress(), &[(API_KEY, key.expose())])
        .await
        .map_err(network)?;
    // The sidecar no longer takes this key (revoked, or a new sidecar after a restore): a retry with
    // it cannot help. The scheduler stops queueing the cleanup, and the dead job names the command.
    if matches!(resp.status, 401 | 403) {
        sqlx::query("update openwearables_user set delete_refused_at = now() where user_id = $1")
            .bind(user)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        return Err(JobError::permanent(refused_text(user)));
    }
    // The response could have been lost after the remote delete committed.
    if resp.status != 404 {
        status(resp)?;
    }
    // The secret owns the tombstone. Once the sidecar confirms deletion, discard both.
    ownhealth_core::secrets::delete_secret(&mut tx, household, secret).await?;
    tx.commit().await?;
    Ok(true)
}

/// Delete an Open Wearables user from the operator CLI, with an API key the sidecar takes now. A user
/// still linked to a profile loses its OwnHealth source first (the readings it brought stay). The key
/// replaces the stored one, and a 404 counts as deleted. This ends a cleanup the sidecar refused, and
/// retires a link that went stale when Open Wearables was set up again (after a restore).
pub async fn delete(ctx: &JobContext, user: Uuid, key: &SecretString) -> Result<()> {
    delete_with_key(ctx, &internal()?, user, key).await
}

async fn delete_with_key(ctx: &JobContext, base: &Url, user: Uuid, key: &SecretString) -> Result<()> {
    let mut tx = ctx.pool.begin().await?;
    lock_user(&mut tx, user).await?;
    let found: Option<(Uuid, Option<Uuid>, Uuid)> = sqlx::query_as(
        "select w.secret_id, w.source_id, k.household_id from openwearables_user w join secret k on k.id = w.secret_id
         where w.user_id = $1",
    )
    .bind(user)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((old, source, household)) = found else {
        return Err(JobError::permanent(format!(
            "OwnHealth knows no Open Wearables user {user}. / OwnHealth no conoce ningún usuario {user} de Open Wearables."
        )));
    };
    if let Some(source) = source {
        sources::delete_source(&mut tx, source).await?;
    }
    let sb = SecretBox::from_settings(&ctx.settings)?;
    let secret = store_secret(&mut tx, &sb, household, PURPOSE, None, key).await?;
    sqlx::query("update openwearables_user set secret_id = $2, delete_refused_at = null where user_id = $1")
        .bind(user)
        .bind(secret)
        .execute(&mut *tx)
        .await?;
    ownhealth_core::secrets::delete_secret(&mut tx, household, old).await?;
    tx.commit().await?;
    delete_user(ctx, base, user).await?;
    Ok(())
}

async fn schedule_reconcile(ctx: &JobContext) -> ownhealth_core::Result<u64> {
    let pending = tombstones(&ctx.pool).await?;
    let mut conn = ctx.pool.acquire().await?;
    for (user, household) in pending {
        let key = format!("{RECONCILE}:{user}");
        // The household, so a cleanup that ends dead files its problem where the household sees it.
        jobs::enqueue(
            &mut conn,
            RECONCILE,
            &json!({"user_id": user, "household_id": household}),
            EnqueueOptions::dedupe(&key),
        )
        .await?;
    }
    // A default-sources hook reports sources added. This one only queues cleanup.
    Ok(0)
}

/// The registered reconcile job: delete one tombstoned sidecar user.
async fn reconcile_job(ctx: &JobContext, base: &Url, job: &Job) -> Result<Option<Value>> {
    let deleted = delete_user(ctx, base, job.uuid("user_id")?).await?;
    Ok(Some(json!({"deleted": deleted})))
}

pub fn register() {
    gaps::register();
    sources::register_poller(KIND, |ctx, source| {
        Box::pin(async move { poll(&ctx, &internal()?, source.id).await })
    });
    sources::register_default_sources(KIND, |ctx| Box::pin(async move { schedule_reconcile(&ctx).await }));
    jobs::register_handler(RECONCILE, |ctx, job| {
        Box::pin(async move { reconcile_job(&ctx, &internal()?, &job).await })
    });
}

#[cfg(test)]
#[path = "openwearables/tests.rs"]
mod tests;
