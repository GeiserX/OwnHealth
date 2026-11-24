//! Pulls the Open Wearables 0.11 API through the internal Compose address. The provider policy
//! applies at the wire boundary, before any bundle, measurement or score is stored.
//!
//! A poll reads each allowlisted vendor with an active connection on its own (`provider=`), only for
//! the series OwnHealth maps (`types=`), one day at a time. Each day is stored and committed before
//! the next one is read, and it moves that vendor's own cursor. So a long window or a later failure
//! keeps what already arrived, and a vendor whose connection ended never holds the others back. A poll
//! reads the 7 days before a vendor's cursor again, for late uploads, but its bundle keeps only the
//! rows this source never brought: `source_item` knows each row by its content.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

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

fn internal() -> Url {
    Url::parse(INTERNAL).expect("fixed Compose URL")
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
    let login = endpoint(base, &["auth", "login"], &[]);
    let response = net::post_form(
        login.as_str(),
        &egress(),
        &[],
        &[("username", email), ("password", password.expose())],
    )
    .await
    .map_err(network)
    .and_then(status)
    .map_err(during(
        "Developer sign-in with the email and password / Inicio de sesión de desarrollador con el correo y la contraseña",
    ))?;
    let login: Login = parse(&response.body)?;
    let auth = SecretString::new(format!("Bearer {}", login.access_token));

    let listed = endpoint(base, &["oauth", "providers"], &[]);
    let listed = get(&listed, None)
        .await
        .map_err(during("Provider list / Lista de proveedores"))?;
    for provider in parse::<Vec<Provider>>(&listed.body)? {
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
        let url = endpoint(base, &["oauth", "providers", &provider.provider], &[]);
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
        let confirmed: Provider = parse(&response.body)?;
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
    setup(ctx, &internal(), profile, user, email, password, key).await
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
    let binding: Option<(Uuid, Option<Uuid>)> =
        sqlx::query_as("select secret_id, source_id from openwearables_user where user_id = $1")
            .bind(user)
            .fetch_optional(&mut *conn)
            .await?;
    let Some((secret, source)) = binding else {
        return Ok((household, None));
    };
    let source = source.ok_or_else(|| {
        JobError::permanent(
            "This Open Wearables user waits for deletion. / Este usuario de Open Wearables está pendiente de borrado.",
        )
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
    let url = endpoint(base, &["users", &user.to_string()], &[]);
    let user_check = "User check with the API key / Comprobación del usuario con la clave de API";
    let confirmed: User = parse(&get(&url, Some(key)).await.map_err(during(user_check))?.body)?;
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
        let url = endpoint(self.base, &["users", &self.user, part], query);
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
            let page: Page<Sample> = self.read("timeseries", &query).await?;
            rows.extend(page.data.into_iter().filter(|row| {
                out.seen += 1;
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
            let page: Page<Score> = self
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

#[derive(Serialize, Default)]
struct Bundle {
    daily: Vec<Sample>,
    raw: Vec<Sample>,
    scores: Vec<Score>,
}

impl Bundle {
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
    let mut active = BTreeSet::new();
    let mut ended = BTreeSet::new();
    for c in pull.read::<Vec<Connection>>("connections", &[]).await? {
        if !allowed(&c.provider) {
            continue;
        }
        match c.status {
            ConnectionStatus::Active => active.insert(c.provider),
            ConnectionStatus::Expired | ConnectionStatus::Revoked => ended.insert(c.provider),
            ConnectionStatus::Unknown => false,
        };
    }
    let mut out = RunOutcome::default();
    let mut cursor = binding.cursor.clone();
    for provider in &active {
        let mut from = mark(cursor.as_ref(), provider).map_or(now - Duration::days(BACKFILL_DAYS), |t| {
            t.min(now) - Duration::days(OVERLAP_DAYS)
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
            let bundle = Bundle {
                daily: pull.series("1day", &mut out).await?,
                raw: pull.series("raw", &mut out).await?,
                scores: pull.scores(&mut out).await?,
            };
            let Some(next) = store_day(ctx, source, &binding, cursor.as_ref(), provider, to, bundle, &mut out).await?
            else {
                // The binding changed while the day was read (rotated, deleted, switched off).
                return Ok(out);
            };
            cursor = Some(next);
            from = to;
        }
    }
    // A vendor whose connection ended keeps its own cursor and is read from there once it is back.
    sqlx::query(
        "update source set reconsent_needed_at = case when $2 then coalesce(reconsent_needed_at, now()) else null end
         where id = $1",
    )
    .bind(source)
    .bind(!ended.is_empty())
    .execute(&ctx.pool)
    .await?;
    if !ended.is_empty() {
        out.problems.push(RunProblem {
            code: CONNECTION_ENDED.into(),
            message: ended.into_iter().collect::<Vec<_>>().join(", "),
        });
    }
    Ok(out)
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
    .bind(cursor)
    .fetch_optional(&mut *tx)
    .await?
    .unwrap_or(false);
    if !live {
        return Ok(None);
    }
    let fresh = keep_new_rows(ctx, &mut tx, source, binding.profile, bundle, out).await?;
    write(&mut tx, source, binding.profile, fresh, out).await?;
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

/// A row's `source_item` id: what it is and the sha256 of its content (128 bits), so a value the
/// vendor revises arrives again, and a row read again does not.
fn row_key<T: Serialize>(kind: &str, row: &T) -> Result<String> {
    let bytes = serde_json::to_vec(row).map_err(ownhealth_core::Error::from)?;
    let digest: [u8; 32] = Sha256::digest(&bytes).into();
    let digest = hex::encode(digest);
    Ok(format!("ow{kind}:{}", &digest[..32]))
}

/// Keep the rows this source never brought, store them as one bundle (bytes first), and remember
/// each row. Every other row counts as a duplicate.
async fn keep_new_rows(
    ctx: &JobContext,
    conn: &mut PgConnection,
    source: Uuid,
    profile: Uuid,
    bundle: Bundle,
    out: &mut RunOutcome,
) -> Result<Bundle> {
    fn keyed<T: Serialize>(kind: &str, rows: Vec<T>) -> Result<Vec<(String, T)>> {
        rows.into_iter().map(|row| Ok((row_key(kind, &row)?, row))).collect()
    }
    // A row is new when no earlier pull and no earlier row of this day had its key.
    fn split<T>(
        rows: Vec<(String, T)>,
        seen: &mut HashSet<String>,
        new_keys: &mut Vec<String>,
        out: &mut RunOutcome,
    ) -> Vec<T> {
        let mut kept = Vec::new();
        for (key, row) in rows {
            if seen.insert(key.clone()) {
                new_keys.push(key);
                kept.push(row);
            } else {
                out.duplicate += 1;
            }
        }
        kept
    }
    let daily = keyed("d", bundle.daily)?;
    let raw = keyed("r", bundle.raw)?;
    let scores = keyed("s", bundle.scores)?;
    let keys: Vec<&str> = daily
        .iter()
        .map(|r| r.0.as_str())
        .chain(raw.iter().map(|r| r.0.as_str()))
        .chain(scores.iter().map(|r| r.0.as_str()))
        .collect();
    if keys.is_empty() {
        return Ok(Bundle::default());
    }
    let mut seen: HashSet<String> =
        sqlx::query_scalar("select external_id from source_item where source_id = $1 and external_id = any($2)")
            .bind(source)
            .bind(&keys)
            .fetch_all(&mut *conn)
            .await?
            .into_iter()
            .collect();
    let mut new_keys = Vec::new();
    let daily = split(daily, &mut seen, &mut new_keys, out);
    let raw = split(raw, &mut seen, &mut new_keys, out);
    let scores = split(scores, &mut seen, &mut new_keys, out);
    let kept = Bundle { daily, raw, scores };
    if kept.is_empty() {
        return Ok(kept);
    }
    let bytes = serde_json::to_vec(&kept).map_err(ownhealth_core::Error::from)?;
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
    Ok(kept)
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

/// The registered reconcile job: delete one tombstoned sidecar user.
async fn reconcile_job(ctx: &JobContext, base: &Url, job: &Job) -> Result<Option<Value>> {
    let deleted = delete_user(ctx, base, job.uuid("user_id")?).await?;
    Ok(Some(json!({"deleted": deleted})))
}

pub fn register() {
    sources::register_poller(KIND, |ctx, source| {
        Box::pin(async move { poll(&ctx, &internal(), source.id).await })
    });
    sources::register_default_sources(KIND, |ctx| Box::pin(async move { schedule_reconcile(&ctx).await }));
    jobs::register_handler(RECONCILE, |ctx, job| {
        Box::pin(async move { reconcile_job(&ctx, &internal(), &job).await })
    });
}

#[cfg(test)]
#[path = "openwearables/tests.rs"]
mod tests;
