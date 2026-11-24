//! The US endpoint directory (connectors.md 7, "Endpoint discovery"), source kind `brands`. A person
//! searches for their hospital or practice by name, and the US connection wizard gets the FHIR base URL
//! from here instead of asking the person to paste one.
//!
//! Each vendor publishes who answers on which FHIR server: Epic and MEDITECH as a SMART User-access
//! Brands bundle, Oracle Health and healow as a Bundle of Endpoint and Organization pairs. NextGen and
//! the VA each run one shared server, so they are two fixed entries and are never fetched. A run
//! downloads each vendor's file to a temporary file, parses the whole file with typed serde, cuts every
//! organisation to (name, endpoint, state), and replaces that vendor's entries in one transaction, only
//! when the file gave at least the vendor's minimum. A truncated or spliced body, an error answer or a
//! file far smaller than usual keeps the old copy, and the run names the vendor as a problem. A facility
//! linked to its parent by `partOf` carries the parent's endpoint. The file is deleted however the run
//! ends: we keep no vendor file (Epic allows distribution by link only, Oracle's list has no licence).
//!
//! Nothing here runs on its own. There is no default-sources hook: the source exists only after a
//! person first opens the US connection wizard ([`ensure_source`]), so an install that never connects
//! a US portal sends no request to a US vendor. From then on it runs weekly, like any polled source,
//! and a source switched off stays off. The requests are plain GETs of public files: no personal
//! data, no token, no cookie.
//!
//! Source `config`: `{urls?: {<vendor>: http(s) URL}, allow_private?: bool}`. `urls` points a vendor at
//! a mirror; `allow_private` allows a mirror on the person's network (plain http only there).

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use ownhealth_core::jobs::{JobContext, JobError};
use ownhealth_core::net::{self, Egress};
use ownhealth_core::sources::{self, NewSource, RunOutcome, RunProblem, STALE_INTERVALS, Source};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::PgConnection;
use uuid::Uuid;

pub const KIND: &str = "brands";
pub const LABEL: &str = "US provider directory";
/// The vendors refresh their files about weekly.
pub const POLL_SECONDS: i32 = 7 * 24 * 3600;
/// A vendor's copy older than this many seconds reads stale: three missed weekly runs.
pub const STALE_SECONDS: i64 = POLL_SECONDS as i64 * STALE_INTERVALS as i64;
/// The biggest file we take (Epic's bundle was about 100 MB in November 2026).
pub const MAX_BYTES: usize = 512 * 1024 * 1024;
const NAME_MAX: usize = 200;
const STATE_MAX: usize = 40;
const ENDPOINT_MAX: usize = 2048;
/// How far a facility's `partOf` chain is followed to find an endpoint.
const PART_OF_DEPTH: usize = 8;
const INSERT_CHUNK: usize = 5000;

/// A vendor file: where it is published and the fewest entries a complete copy gives. The minimums
/// sit far under what each file gave in November 2026 (Epic about 99,000 entries, healow 17,000,
/// Oracle 1,300, MEDITECH 450), so normal churn passes and a cut or emptied file does not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Bundle {
    pub vendor: &'static str,
    pub url: &'static str,
    pub min_entries: usize,
}

pub const BUNDLES: [Bundle; 4] = [
    Bundle {
        vendor: "epic",
        url: "https://open.epic.com/Endpoints/Brands",
        min_entries: 500,
    },
    Bundle {
        vendor: "oracle",
        url: "https://raw.githubusercontent.com/oracle-samples/ignite-endpoints/main/oracle_health_fhir_endpoints/millennium_patient_r4_endpoints.json",
        min_entries: 200,
    },
    Bundle {
        vendor: "healow",
        url: "https://connect4.healow.com/apps/api/v1/fhir/activated_clinical_endpoints",
        min_entries: 500,
    },
    Bundle {
        vendor: "meditech",
        url: "https://fhir-apis.meditech.com/v1/brands",
        min_entries: 100,
    },
];

/// Vendors with one shared patient-access server: a fixed entry each, never fetched.
pub const SINGLES: [(&str, &str, &str); 2] = [
    (
        "nextgen",
        "NextGen Enterprise (practices on NextGen)",
        "https://fhir.nextgen.com/nge/prod/fhir-api-r4/fhir/r4/",
    ),
    (
        "va",
        "U.S. Department of Veterans Affairs (VA)",
        "https://api.va.gov/services/fhir/v0/r4",
    ),
];

// ------------------------------------------------------------------ config ----

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub urls: HashMap<String, String>,
    #[serde(default)]
    pub allow_private: bool,
}

fn parse_config(config: &Value) -> Result<Config, String> {
    serde_json::from_value(config.clone())
        .map_err(|_| "the provider directory takes only urls and allow_private".into())
}

/// Checks a `brands` source config before it is saved: `urls` names only known vendors, each with an
/// http(s) URL, and plain http only for a mirror on the person's network.
pub fn check_config(config: &Value) -> Result<(), String> {
    let cfg = parse_config(config)?;
    for (vendor, url) in &cfg.urls {
        if !BUNDLES.iter().any(|b| b.vendor == vendor) {
            return Err(format!("config.urls names an unknown vendor {vendor:?}"));
        }
        let u = url.trim();
        let plain = u.strip_prefix("http://");
        let rest = u.strip_prefix("https://").or(plain);
        if rest.is_none_or(|r| r.split(['/', '?', '#']).next().unwrap_or_default().is_empty()) {
            return Err(format!("config.urls.{vendor} must be an http(s) URL"));
        }
        if plain.is_some() && !cfg.allow_private {
            return Err(format!(
                "config.urls.{vendor} uses plain http: use https, or allow_private for a mirror on your own network"
            ));
        }
    }
    Ok(())
}

/// The URL a run fetches for each vendor file: the vendor's own, or the mirror the config names.
pub fn bundle_urls(config: &Config) -> Vec<(Bundle, String)> {
    BUNDLES
        .iter()
        .map(|b| {
            let url = config
                .urls
                .get(b.vendor)
                .map(|u| u.trim().to_string())
                .filter(|u| !u.is_empty())
                .unwrap_or_else(|| b.url.to_string());
            (*b, url)
        })
        .collect()
}

// ------------------------------------------------------------- typed file ----

/// A vendor file, typed: a FHIR Bundle whose entries are Organization and Endpoint resources. Other
/// resource types are read and left out; any field we read that has the wrong shape fails the whole
/// file, so a damaged file never replaces a good copy.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct FhirBundle {
    resource_type: String,
    #[serde(default)]
    entry: Vec<Entry>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Entry {
    #[serde(default)]
    full_url: Option<String>,
    #[serde(default)]
    resource: Option<Resource>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "resourceType")]
enum Resource {
    Organization(Organization),
    Endpoint(Endpoint),
    #[serde(other)]
    Other,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Organization {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    active: Option<bool>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    address: Vec<Address>,
    #[serde(default)]
    endpoint: Vec<Reference>,
    #[serde(default)]
    part_of: Option<Reference>,
}

#[derive(Debug, Deserialize)]
struct Address {
    #[serde(default)]
    state: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Reference {
    #[serde(default)]
    reference: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Endpoint {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    address: Option<String>,
}

/// One directory entry: an organisation or facility, the FHIR base URL it answers on, and its US state
/// when the file gives one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Brand {
    pub name: String,
    pub endpoint: String,
    pub state: Option<String>,
}

/// Why a vendor file was not taken. The text names no body content.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FileError {
    #[error("the file is not a complete FHIR Bundle")]
    Unreadable,
    #[error("the file gave {got} entries, fewer than the {min} a complete copy gives")]
    TooFew { got: usize, min: usize },
}

/// Parses a whole vendor file and resolves it to directory entries (see [`resolve`]).
pub fn parse_file(path: &Path) -> Result<Vec<Brand>, FileError> {
    let file = std::fs::File::open(path).map_err(|_| FileError::Unreadable)?;
    let reader = std::io::BufReader::with_capacity(1 << 20, file);
    let bundle: FhirBundle = serde_json::from_reader(reader).map_err(|_| FileError::Unreadable)?;
    resolve(bundle)
}

/// [`parse_file`] over bytes already in memory.
pub fn parse_bytes(bytes: &[u8]) -> Result<Vec<Brand>, FileError> {
    let bundle: FhirBundle = serde_json::from_slice(bytes).map_err(|_| FileError::Unreadable)?;
    resolve(bundle)
}

/// The lookup keys of a resource: its full URL, and `<Type>/<id>`.
fn keys(full_url: &Option<String>, kind: &str, id: &Option<String>) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(u) = full_url.as_deref().map(str::trim).filter(|u| !u.is_empty()) {
        out.push(u.to_string());
    }
    if let Some(i) = id.as_deref().map(str::trim).filter(|i| !i.is_empty()) {
        out.push(format!("{kind}/{i}"));
    }
    out
}

/// A reference as written (`urn:uuid:…`, `Endpoint/x`, or an absolute URL), then its last two path
/// segments (`…/Endpoint/x` read as `Endpoint/x`).
fn lookup(map: &HashMap<String, usize>, reference: Option<&Reference>) -> Option<usize> {
    let r = reference?.reference.as_deref()?.trim();
    if let Some(i) = map.get(r) {
        return Some(*i);
    }
    let mut parts = r.trim_end_matches('/').rsplit('/');
    let (id, kind) = (parts.next()?, parts.next()?);
    map.get(&format!("{kind}/{id}")).copied()
}

/// An endpoint address we would hand to the wizard: an https URL with a host and no user info, as
/// the URL parser writes it back (`HTTPS://X` and `https:x` read as `https://x/`), so what we store
/// always starts with `https://`.
fn usable_endpoint(e: &Endpoint) -> Option<String> {
    if e.status.as_deref().is_some_and(|s| s != "active") {
        return None;
    }
    let address = e.address.as_deref()?.trim();
    if address.len() > ENDPOINT_MAX {
        return None;
    }
    let url = url::Url::parse(address).ok()?;
    if url.scheme() != "https" || url.host_str().is_none_or(str::is_empty) || !url.username().is_empty() {
        return None;
    }
    let url = String::from(url);
    (url.starts_with("https://") && url.len() <= ENDPOINT_MAX).then_some(url)
}

/// A name or state as we store it: control characters (a NUL included) and runs of white space read
/// as one space, cut to `max` characters. None when nothing is left.
fn clean(text: &str, max: usize) -> Option<String> {
    let joined = text
        .split(|c: char| c.is_whitespace() || c.is_control())
        .filter(|w| !w.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    let cut: String = joined.chars().take(max).collect();
    let cut = cut.trim().to_string();
    (!cut.is_empty()).then_some(cut)
}

fn state_of(o: &Organization) -> Option<String> {
    o.address
        .iter()
        .find_map(|a| a.state.as_deref().and_then(|s| clean(s, STATE_MAX)))
}

/// Every active, named organisation with the endpoints it answers on: its own, or else the nearest
/// ancestor's along `partOf` (at most [`PART_OF_DEPTH`] steps, a loop ends the walk). An organisation
/// with no endpoint on the way gives no entry. The state is the organisation's own, else the nearest
/// one on the way up to that ancestor. Entries are unique by (name, endpoint), the first one kept.
fn resolve(bundle: FhirBundle) -> Result<Vec<Brand>, FileError> {
    if bundle.resource_type != "Bundle" {
        return Err(FileError::Unreadable);
    }
    let mut endpoints: Vec<Option<String>> = Vec::new();
    let mut endpoint_keys: HashMap<String, usize> = HashMap::new();
    let mut orgs: Vec<Organization> = Vec::new();
    let mut org_keys: HashMap<String, usize> = HashMap::new();
    for entry in bundle.entry {
        match entry.resource {
            Some(Resource::Endpoint(e)) => {
                for k in keys(&entry.full_url, "Endpoint", &e.id) {
                    endpoint_keys.entry(k).or_insert(endpoints.len());
                }
                endpoints.push(usable_endpoint(&e));
            }
            Some(Resource::Organization(o)) => {
                for k in keys(&entry.full_url, "Organization", &o.id) {
                    org_keys.entry(k).or_insert(orgs.len());
                }
                orgs.push(o);
            }
            Some(Resource::Other) | None => {}
        }
    }
    let own = |o: &Organization| -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for r in &o.endpoint {
            if let Some(Some(address)) = lookup(&endpoint_keys, Some(r)).map(|i| &endpoints[i])
                && !out.contains(address)
            {
                out.push(address.clone());
            }
        }
        out
    };
    let mut seen: HashSet<(String, String)> = HashSet::new();
    let mut out = Vec::new();
    for (i, o) in orgs.iter().enumerate() {
        if o.active == Some(false) {
            continue;
        }
        let Some(name) = o.name.as_deref().and_then(|n| clean(n, NAME_MAX)) else {
            continue;
        };
        let mut at = i;
        let mut visited: HashSet<usize> = HashSet::from([i]);
        let mut found = own(o);
        let mut state = state_of(o);
        while found.is_empty() && visited.len() <= PART_OF_DEPTH {
            let Some(parent) = lookup(&org_keys, orgs[at].part_of.as_ref()) else {
                break;
            };
            if !visited.insert(parent) {
                break;
            }
            at = parent;
            found = own(&orgs[at]);
            if state.is_none() {
                state = state_of(&orgs[at]);
            }
        }
        for endpoint in found {
            if seen.insert((name.clone(), endpoint.clone())) {
                out.push(Brand {
                    name: name.clone(),
                    endpoint,
                    state: state.clone(),
                });
            }
        }
    }
    Ok(out)
}

/// The vendor's minimum, checked on what the file gave.
pub fn check_count(bundle: &Bundle, brands: &[Brand]) -> Result<(), FileError> {
    if brands.len() < bundle.min_entries {
        return Err(FileError::TooFew {
            got: brands.len(),
            min: bundle.min_entries,
        });
    }
    Ok(())
}

// ---------------------------------------------------------------- storage ----

/// Replaces one vendor's entries of this source and stamps its feed, in one transaction: a reader
/// sees the old copy or the new one, never a mix. Locks the source row, so two runs of one source
/// (a requeued job) replace one after the other.
pub async fn replace(
    conn: &mut PgConnection,
    source_id: Uuid,
    vendor: &str,
    brands: &[Brand],
) -> ownhealth_core::Result<()> {
    let mut tx = sqlx::Connection::begin(&mut *conn).await?;
    let exists: Option<Uuid> = sqlx::query_scalar("select id from source where id = $1 for update")
        .bind(source_id)
        .fetch_optional(&mut *tx)
        .await?;
    if exists.is_none() {
        return Err(ownhealth_core::Error::NotFound("Unknown source.".into()));
    }
    sqlx::query(
        "insert into fhir_brand_feed (source_id, vendor, fetched_at, entries) values ($1, $2, now(), $3) \
         on conflict (source_id, vendor) do update set fetched_at = now(), entries = excluded.entries",
    )
    .bind(source_id)
    .bind(vendor)
    .bind(i32::try_from(brands.len()).unwrap_or(i32::MAX))
    .execute(&mut *tx)
    .await?;
    sqlx::query("delete from fhir_brand where source_id = $1 and vendor = $2")
        .bind(source_id)
        .bind(vendor)
        .execute(&mut *tx)
        .await?;
    for chunk in brands.chunks(INSERT_CHUNK) {
        let names: Vec<&str> = chunk.iter().map(|b| b.name.as_str()).collect();
        let endpoints: Vec<&str> = chunk.iter().map(|b| b.endpoint.as_str()).collect();
        let states: Vec<Option<&str>> = chunk.iter().map(|b| b.state.as_deref()).collect();
        sqlx::query(
            "insert into fhir_brand (source_id, vendor, name, endpoint, state) \
             select $1, $2, n, e, s from unnest($3::text[], $4::text[], $5::text[]) as t(n, e, s)",
        )
        .bind(source_id)
        .bind(vendor)
        .bind(&names)
        .bind(&endpoints)
        .bind(&states)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(())
}

// ------------------------------------------------------------------ poller ----

/// Deletes the downloaded file however the run ends.
struct Downloaded(PathBuf);

impl Drop for Downloaded {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn problem(vendor: &str, code: &str, message: impl std::fmt::Display) -> RunProblem {
    RunProblem {
        code: code.into(),
        message: format!("{vendor}: {message}"),
    }
}

/// Fetches, parses and stores one vendor file; the number of entries it gave.
async fn refresh(
    ctx: &JobContext,
    source: &Source,
    cfg: &Config,
    bundle: Bundle,
    url: &str,
) -> Result<usize, RunProblem> {
    let vendor = bundle.vendor;
    let dest = ctx
        .blobs
        .staging_path(&format!("brands-{}-{vendor}.json", source.id))
        .map_err(|e| problem(vendor, "staging", e))?;
    let _guard = Downloaded(dest.clone());
    let egress = Egress {
        max_bytes: MAX_BYTES,
        ..Egress::reference_file(cfg.allow_private)
    };
    match net::download(url, &egress, &dest).await {
        Ok((200, _)) => {}
        Ok((status, _)) => return Err(problem(vendor, "http_status", format!("the server answered {status}"))),
        Err(e) => return Err(problem(vendor, "unreachable", e)),
    }
    let path = dest.clone();
    let brands = tokio::task::spawn_blocking(move || parse_file(&path))
        .await
        .map_err(|e| problem(vendor, "unreadable", e))?
        .map_err(|e| problem(vendor, "unreadable", e))?;
    check_count(&bundle, &brands).map_err(|e| problem(vendor, "too_few", e))?;
    let mut conn = ctx.pool.acquire().await.map_err(|e| problem(vendor, "store", e))?;
    replace(&mut conn, source.id, vendor, &brands)
        .await
        .map_err(|e| problem(vendor, "store", e))?;
    Ok(brands.len())
}

/// The `brands` poller. Each vendor file is one item: `new` when its entries were replaced, a problem
/// when the old copy stays. A run where every file failed is an error, so the scheduler tries again
/// within hours instead of a week. The two fixed entries are written on every run.
pub async fn poll_brands(ctx: JobContext, source: Source) -> Result<RunOutcome, JobError> {
    let cfg = parse_config(&source.config).map_err(JobError::permanent)?;
    let mut out = RunOutcome::default();
    for (bundle, url) in bundle_urls(&cfg) {
        out.seen += 1;
        match refresh(&ctx, &source, &cfg, bundle, &url).await {
            Ok(_) => out.new += 1,
            Err(p) => out.problems.push(p),
        }
    }
    let mut conn = ctx.pool.acquire().await?;
    for (vendor, name, endpoint) in SINGLES {
        let one = [Brand {
            name: name.into(),
            endpoint: endpoint.into(),
            state: None,
        }];
        replace(&mut conn, source.id, vendor, &one).await?;
    }
    Ok(out)
}

// ------------------------------------------------------------ entry points ----

/// Takes the lock every writer of a new directory source holds until its transaction ends, so two
/// people opening the wizard at once (or two replicas) never add a household's row twice.
async fn lock_new_source(conn: &mut PgConnection) -> ownhealth_core::Result<()> {
    sqlx::query("select pg_advisory_xact_lock(hashtext('ownhealth.brands_source'))")
        .execute(conn)
        .await?;
    Ok(())
}

/// What the wizard calls the first time a person of the household opens it: the household's
/// directory source, made on the first call (on, weekly) with its first pull queued at once. Later
/// calls return the same source and queue nothing, and a source a person switched off stays off. Call
/// it inside a transaction: the lock that keeps the household to one source lasts until it ends.
/// Returns the source id and whether this call made it.
pub async fn ensure_source(
    conn: &mut PgConnection,
    household_id: Uuid,
    created_by: Option<Uuid>,
) -> ownhealth_core::Result<(Uuid, bool)> {
    lock_new_source(&mut *conn).await?;
    let existing: Option<Uuid> = sqlx::query_scalar(
        "select id from source where household_id = $1 and kind = $2 order by created_at, id limit 1",
    )
    .bind(household_id)
    .bind(KIND)
    .fetch_optional(&mut *conn)
    .await?;
    if let Some(id) = existing {
        return Ok((id, false));
    }
    let id = sources::create_source(
        &mut *conn,
        &NewSource {
            poll_seconds: Some(POLL_SECONDS),
            created_by,
            ..NewSource::new(household_id, KIND, LABEL)
        },
    )
    .await?;
    sources::enqueue_poll(&mut *conn, id).await?;
    Ok((id, true))
}

/// One vendor's copy as the wizard shows it.
#[derive(Debug, Clone, PartialEq, Serialize, sqlx::FromRow)]
pub struct Feed {
    pub vendor: String,
    pub fetched_at: DateTime<Utc>,
    pub entries: i32,
    /// The copy is older than [`STALE_SECONDS`]: the vendor's newer file could not be taken.
    pub stale: bool,
}

/// The household's directory: its source, whether it is on, and each vendor's copy. `stale` when any
/// vendor's copy is stale; `ready` once any entry can be searched.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Directory {
    pub source_id: Uuid,
    pub enabled: bool,
    pub last_run_at: Option<DateTime<Utc>>,
    pub feeds: Vec<Feed>,
    pub stale: bool,
    pub ready: bool,
}

/// The household's directory, or None before anyone opened the wizard.
pub async fn directory(conn: &mut PgConnection, household_id: Uuid) -> ownhealth_core::Result<Option<Directory>> {
    let source: Option<(Uuid, bool, Option<DateTime<Utc>>)> = sqlx::query_as(
        "select id, enabled, last_run_at from source where household_id = $1 and kind = $2 \
         order by created_at, id limit 1",
    )
    .bind(household_id)
    .bind(KIND)
    .fetch_optional(&mut *conn)
    .await?;
    let Some((source_id, enabled, last_run_at)) = source else {
        return Ok(None);
    };
    let feeds: Vec<Feed> = sqlx::query_as(
        "select vendor, fetched_at, entries, fetched_at < now() - make_interval(secs => $2) as stale \
         from fhir_brand_feed where source_id = $1 order by vendor",
    )
    .bind(source_id)
    .bind(STALE_SECONDS as f64)
    .fetch_all(&mut *conn)
    .await?;
    Ok(Some(Directory {
        source_id,
        enabled,
        last_run_at,
        stale: feeds.iter().any(|f| f.stale),
        ready: feeds.iter().any(|f| f.entries > 0),
        feeds,
    }))
}

/// One search hit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, sqlx::FromRow)]
pub struct Hit {
    pub name: String,
    pub endpoint: String,
    pub state: Option<String>,
    pub vendor: String,
}

/// The longest query and the most words a search takes.
pub const QUERY_MAX: usize = 100;
pub const QUERY_WORDS: usize = 8;
pub const LIMIT_MAX: i64 = 50;

/// The words of a query as `ILIKE` patterns, `%`, `_` and `\` taken literally. None when the query
/// has fewer than 2 characters or more than [`QUERY_MAX`].
pub fn query_patterns(q: &str) -> Option<Vec<String>> {
    let q = q.trim();
    let n = q.chars().count();
    if !(2..=QUERY_MAX).contains(&n) {
        return None;
    }
    let words: Vec<String> = q
        .split_whitespace()
        .take(QUERY_WORDS)
        .map(|w| {
            let escaped = w.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_");
            format!("%{escaped}%")
        })
        .collect();
    (!words.is_empty()).then_some(words)
}

/// Entries of the household's directory whose name holds every word of `q`, case-insensitive. Names
/// that start with the query come first, then shorter names. Empty for a query [`query_patterns`]
/// refuses, or before the directory exists.
pub async fn search(
    conn: &mut PgConnection,
    household_id: Uuid,
    q: &str,
    limit: i64,
) -> ownhealth_core::Result<Vec<Hit>> {
    let Some(patterns) = query_patterns(q) else {
        return Ok(Vec::new());
    };
    let prefix = format!("{}%", patterns[0].trim_start_matches('%').trim_end_matches('%'));
    Ok(sqlx::query_as(
        "select b.name, b.endpoint, b.state, b.vendor from fhir_brand b join source s on s.id = b.source_id \
         where s.household_id = $1 and s.kind = $2 and b.name ilike all($3::text[]) \
         order by (b.name ilike $4) desc, char_length(b.name), b.name, b.endpoint, b.vendor limit $5",
    )
    .bind(household_id)
    .bind(KIND)
    .bind(&patterns)
    .bind(&prefix)
    .bind(limit.clamp(1, LIMIT_MAX))
    .fetch_all(conn)
    .await?)
}

pub fn register() {
    // A poller only: no default-sources hook (module docs).
    ownhealth_core::sources::register_poller(KIND, |ctx, source| Box::pin(poll_brands(ctx, source)));
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_config_names_known_vendors_with_http_urls() {
        assert!(check_config(&json!({})).is_ok());
        assert!(check_config(&json!({"urls": {"epic": "https://mirror.example/epic.json"}})).is_ok());
        assert!(check_config(&json!({"urls": {"epic": "http://mirror.lan/epic.json"}, "allow_private": true})).is_ok());
        for bad in [
            json!({"urls": {"cerner": "https://mirror.example/x.json"}}),
            json!({"urls": {"epic": "ftp://mirror.example/epic.json"}}),
            json!({"urls": {"epic": "http://mirror.example/epic.json"}}),
            json!({"urls": {"epic": "https://"}}),
            json!({"url": "https://mirror.example/epic.json"}),
            json!({"token": "not-a-real-token"}),
        ] {
            assert!(check_config(&bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn every_vendor_file_is_https_and_a_mirror_replaces_only_its_vendor() {
        let cfg = Config {
            urls: [("oracle".to_string(), " http://mirror.lan/oracle.json ".to_string())].into(),
            allow_private: true,
        };
        let urls = bundle_urls(&cfg);
        assert_eq!(urls.len(), BUNDLES.len());
        for (b, u) in &urls {
            if b.vendor == "oracle" {
                assert_eq!(u, "http://mirror.lan/oracle.json");
            } else {
                assert!(u.starts_with("https://"), "{u}");
                assert_eq!(u, b.url);
            }
        }
        for (_, _, endpoint) in SINGLES {
            assert!(endpoint.starts_with("https://"), "{endpoint}");
        }
    }

    #[test]
    fn a_query_is_two_to_a_hundred_characters_and_its_wildcards_are_literal() {
        assert_eq!(query_patterns(" a "), None);
        assert_eq!(query_patterns(&"x".repeat(QUERY_MAX + 1)), None);
        assert_eq!(
            query_patterns("LabNorte  50%_off"),
            Some(vec!["%LabNorte%".to_string(), "%50\\%\\_off%".to_string()])
        );
    }
}
