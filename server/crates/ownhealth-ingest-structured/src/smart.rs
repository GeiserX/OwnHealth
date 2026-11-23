//! SMART on FHIR, standalone patient launch (connectors.md 6.2 and section 7): the poller of source
//! kind `smart`. The launch, callback, reconnect and disconnect are the shared connector routes
//! (`connector_oauth`, `routes/connectors.rs`); this module reads the person's record once a day.
//!
//! - **Searches.** For the patient the grant names (`source.config.patient`, the token answer's
//!   `patient`): a read of the Patient, then the US Core searches of AllergyIntolerance, Condition,
//!   DiagnosticReport, DocumentReference, Encounter, Immunization, MedicationRequest (intent order or
//!   plan), Observation (one search per category: laboratory, social-history, vital-signs; US Core
//!   servers require one) and Procedure. A Medication that a MedicationRequest names by reference is
//!   read on its own. A type the grant's `scope` leaves out is not asked for. Each search follows its
//!   `next` links, which must stay on a host the connection declared at connect time.
//! - **One bundle per run.** Every resource the searches returned becomes one `collection` Bundle,
//!   each resource once and in a fixed order, so a day when nothing changed gives the same bytes and
//!   the same document. The bytes are stored first, as a blob of the source's profile and a document
//!   of channel `smart`; then `fhir::import_pull` imports them, in one transaction that holds this
//!   source's pull lock, so its `source_item` ledger skips what the server sent before.
//! - **Tokens.** `connector_oauth::access_token` gives the access token, and refreshes and rotates it
//!   under the source row lock. A token the FHIR server refuses (401) is dropped and refreshed once. A
//!   refused refresh marks the connection for a new sign-in, and the run stops before any search.
//! - **Health.** A search that fails is a problem of the run (`partial`), and the other searches
//!   still import. The run fails when the Patient cannot be read or no search answers. A problem names
//!   the resource type and the status, never a URL, a patient id or a token.

use std::collections::{BTreeMap, HashSet};
use std::time::Duration;

use chrono::{DateTime, Utc};
use ownhealth_core::arrive::store_bytes;
use ownhealth_core::blobs::BlobOwner;
use ownhealth_core::connector_oauth::{self as oauth, OAuthError, SourceHttp};
use ownhealth_core::documents::{NewDocument, create_document};
use ownhealth_core::jobs::{JobContext, JobError};
use ownhealth_core::net::{Egress, Fetched, NetError};
use ownhealth_core::secrets::{SecretBox, SecretString};
use ownhealth_core::sources::{RunOutcome, RunProblem, Source};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use uuid::Uuid;

use crate::fhir;

pub const KIND: &str = "smart";

/// The US Core types a pull reads, each asked for at launch as a `patient/<type>.rs` scope
/// (`connector_oauth::SMART_SCOPES`, with Medication).
pub const US_CORE_TYPES: [&str; 10] = [
    "Patient",
    "AllergyIntolerance",
    "Condition",
    "DiagnosticReport",
    "DocumentReference",
    "Encounter",
    "Immunization",
    "MedicationRequest",
    "Observation",
    "Procedure",
];

/// The searches after the Patient read: a type and the parameters besides `patient`.
const SEARCHES: [(&str, &[(&str, &str)]); 11] = [
    ("AllergyIntolerance", &[]),
    ("Condition", &[]),
    ("DiagnosticReport", &[]),
    ("DocumentReference", &[]),
    ("Encounter", &[]),
    ("Immunization", &[]),
    ("MedicationRequest", &[("intent", "order,plan")]),
    ("Observation", &[("category", "laboratory")]),
    ("Observation", &[("category", "social-history")]),
    ("Observation", &[("category", "vital-signs")]),
    ("Procedure", &[]),
];

/// One search page: a year of labs and vitals of a busy record is a few MB.
pub const PAGE_BYTES: usize = 32 * 1024 * 1024;
/// Pages per search: a server that keeps sending `next` stops here.
pub const MAX_PAGES: usize = 200;
/// All the answers of one run together.
pub const RUN_BYTES: usize = 256 * 1024 * 1024;
/// Medications read one by one per run.
pub const MAX_MEDICATION_READS: usize = 200;
/// A FHIR search can take a while on a big record.
const PAGE_TIMEOUT: Duration = Duration::from_secs(60);
const FHIR_JSON: &str = "application/fhir+json";

/// What a run says when it stops before any search. Information only: the Connections screen shows it.
const STOP_RECONSENT: &str =
    "reconsent_needed: the vendor no longer accepts this connection, so it waits for a new sign-in";
const STOP_PATIENT: &str = "no_patient: the grant names no patient, so there is nothing to search";
const STOP_SERVER: &str = "no_server: the connection names no FHIR server";
const STOP_PROFILE: &str = "no_profile: the connection belongs to no profile";
const STOP_NO_ANSWER: &str = "no_answer: no US Core search answered";

/// The texts a person may read on the Connections screen, for the wording lint.
pub fn phrases() -> Value {
    json!({
        "stop": [STOP_RECONSENT, STOP_PATIENT, STOP_SERVER, STOP_PROFILE, STOP_NO_ANSWER,
                 "patient_unread: the server answered 404 for the patient"],
        "problem": [
            "Observation: the server answered 503",
            "MedicationRequest: a next page on a host this connection did not declare was not read",
            "Observation: the answer is not a FHIR Bundle",
            "Observation: stopped after 200 pages",
            "Observation: the answers of this run passed 268435456 bytes, so the rest was not read",
            "Medication: the server answered 404",
        ],
    })
}

/// A FHIR id ([A-Za-z0-9\-\.]{1,64}): only such an id goes into a URL.
fn fhir_id(s: &str) -> bool {
    !s.is_empty() && s.len() <= 64 && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'.')
}

/// Whether the grant's `scope` lets this run read `rtype`. A grant that names no `patient/` scope
/// (a server that does not echo the scope) is asked for everything; the server refuses what it must.
/// SMART v2 (`patient/Observation.rs`, `patient/*.rs`, granular `?category=`) and v1
/// (`patient/Observation.read`, `patient/*.*`) both count.
pub fn granted(scope: Option<&str>, rtype: &str) -> bool {
    let Some(scope) = scope else { return true };
    let mut any = false;
    for s in scope.split_whitespace() {
        let Some(rest) = s.strip_prefix("patient/") else {
            continue;
        };
        any = true;
        let rest = rest.split('?').next().unwrap_or(rest);
        let Some((t, perm)) = rest.split_once('.') else {
            continue;
        };
        let reads = perm == "read"
            || perm == "*"
            || (!perm.is_empty()
                && perm.chars().all(|c| "cruds".contains(c))
                && (perm.contains('r') || perm.contains('s')));
        if (t == "*" || t == rtype) && reads {
            return true;
        }
    }
    !any
}

/// The words a failed request leaves in a problem: the host at most, never the URL.
fn net_text(e: &NetError) -> String {
    match e {
        NetError::Http { host, .. } => format!("the request to {host} failed"),
        other => other.to_string(),
    }
}

/// The run stops on a token it cannot get; the texts carry no token (`OAuthError` never does).
fn token_stop(e: OAuthError) -> JobError {
    match e {
        OAuthError::ReconsentNeeded => JobError::permanent(STOP_RECONSENT),
        OAuthError::Refused { code, message } => JobError::permanent(format!("{code}: {message}")),
        OAuthError::Core(e) => e.into(),
        OAuthError::Vendor { status, code } => {
            JobError::retry(format!("token_refused: the token endpoint answered {status} ({code})"))
        }
        OAuthError::UndeclaredHost(h) => {
            JobError::permanent(format!("undeclared_host: {h} is not a host of this connection"))
        }
        OAuthError::Net(e) => JobError::retry(format!("token_unreachable: {}", net_text(&e))),
    }
}

/// Why one request gave nothing.
enum Miss {
    /// The run stops: no token.
    Run(JobError),
    /// This search stops: a problem code and its words.
    Search(&'static str, String),
}

/// The FHIR client of one run: the source's declared hosts, its access token, one refresh on a 401.
struct Reader {
    http: SourceHttp,
    pool: PgPool,
    sb: SecretBox,
    source_id: Uuid,
    /// The token endpoint's egress (the connector profile); the searches use `http`'s.
    egress: Egress,
    now: DateTime<Utc>,
    token: Option<SecretString>,
    refreshed: bool,
    bytes: usize,
}

impl Reader {
    async fn get(&mut self, url: &str) -> Result<Fetched, Miss> {
        loop {
            let token = match &self.token {
                Some(t) => t.clone(),
                None => {
                    let t = oauth::access_token(&self.pool, &self.sb, self.source_id, &self.egress, self.now)
                        .await
                        .map_err(|e| Miss::Run(token_stop(e)))?;
                    self.token = Some(t.clone());
                    t
                }
            };
            let bearer = format!("Bearer {}", token.expose());
            let f = match self
                .http
                .get(url, &[("accept", FHIR_JSON), ("authorization", &bearer)])
                .await
            {
                Ok(f) => f,
                Err(OAuthError::UndeclaredHost(_)) => {
                    return Err(Miss::Search(
                        "undeclared_host",
                        "a next page on a host this connection did not declare was not read".into(),
                    ));
                }
                Err(OAuthError::Net(e)) => return Err(Miss::Search("unreachable", net_text(&e))),
                Err(e) => return Err(Miss::Run(token_stop(e))),
            };
            // the server dropped the token before its time: one refresh, then the answer stands
            if f.status == 401 && !self.refreshed {
                self.refreshed = true;
                self.token = None;
                oauth::forget(self.source_id);
                continue;
            }
            self.bytes += f.body.len();
            return Ok(f);
        }
    }
}

/// What the searches of one run gathered.
#[derive(Default)]
struct Gathered {
    /// (fullUrl, resource), in the order the server sent them
    entries: Vec<(Option<String>, Value)>,
    problems: Vec<RunProblem>,
    skipped: BTreeMap<String, i64>,
    /// searches that answered 200 at least once
    answered: usize,
}

impl Gathered {
    fn problem(&mut self, code: &str, message: String) {
        self.problems.push(RunProblem {
            code: code.into(),
            message,
        });
    }

    fn skip(&mut self, what: &str, n: i64) {
        if n > 0 {
            *self.skipped.entry(what.into()).or_default() += n;
        }
    }
}

/// `base/rtype?patient=..&params`, form-encoded.
fn search_url(base: &str, rtype: &str, patient: &str, params: &[(&str, &str)]) -> String {
    let mut q = url::form_urlencoded::Serializer::new(String::new());
    q.append_pair("patient", patient);
    for (k, v) in params {
        q.append_pair(k, v);
    }
    format!("{base}/{rtype}?{}", q.finish())
}

/// One search, every page, into `g`. Only a token the run cannot get stops more than this search.
async fn search(r: &mut Reader, g: &mut Gathered, rtype: &str, first: String) -> Result<(), JobError> {
    let mut url = first;
    let mut visited: HashSet<String> = HashSet::new();
    let mut answered = false;
    for page in 1..=MAX_PAGES {
        if !visited.insert(url.clone()) {
            break;
        }
        if r.bytes > RUN_BYTES {
            g.problem(
                "too_large",
                format!("{rtype}: the answers of this run passed {RUN_BYTES} bytes, so the rest was not read"),
            );
            break;
        }
        let f = match r.get(&url).await {
            Ok(f) => f,
            Err(Miss::Run(e)) => return Err(e),
            Err(Miss::Search(code, words)) => {
                g.problem(code, format!("{rtype}: {words}"));
                break;
            }
        };
        if f.status != 200 {
            g.problem("search_failed", format!("{rtype}: the server answered {}", f.status));
            break;
        }
        let bundle: Value = serde_json::from_slice(&f.body).unwrap_or(Value::Null);
        if bundle["resourceType"] != "Bundle" {
            g.problem("not_fhir", format!("{rtype}: the answer is not a FHIR Bundle"));
            break;
        }
        answered = true;
        for e in bundle["entry"].as_array().map(Vec::as_slice).unwrap_or_default() {
            let res = &e["resource"];
            let t = res["resourceType"].as_str().unwrap_or_default();
            if t.is_empty() {
                g.skip("unreadable", 1);
            } else if e["search"]["mode"] == "outcome" || t == "OperationOutcome" {
                // a server's note about the search (a warning, a restriction), not part of the record
                g.skip("outcome", 1);
            } else {
                let full = e["fullUrl"].as_str().filter(|u| !u.is_empty()).map(String::from);
                g.entries.push((full, res.clone()));
            }
        }
        let next = bundle["link"].as_array().and_then(|links| {
            links
                .iter()
                .find(|l| l["relation"] == "next")
                .and_then(|l| l["url"].as_str())
                .filter(|u| !u.is_empty())
                .map(String::from)
        });
        match next {
            None => break,
            Some(_) if page == MAX_PAGES => {
                g.problem("too_many_pages", format!("{rtype}: stopped after {MAX_PAGES} pages"));
            }
            Some(n) => url = n,
        }
    }
    if answered {
        g.answered += 1;
    }
    Ok(())
}

/// The Medications that MedicationRequests name by a reference this bundle does not hold: id only,
/// relative (`Medication/<id>`) or on the FHIR server itself.
fn missing_medications(base: &str, entries: &[(Option<String>, Value)]) -> Vec<String> {
    let held: HashSet<String> = entries
        .iter()
        .filter(|(_, r)| r["resourceType"] == "Medication")
        .filter_map(|(_, r)| r["id"].as_str().map(String::from))
        .collect();
    let mut out: Vec<String> = Vec::new();
    for (_, r) in entries.iter().filter(|(_, r)| r["resourceType"] == "MedicationRequest") {
        let Some(reference) = r["medicationReference"]["reference"].as_str() else {
            continue;
        };
        let local = reference
            .strip_prefix(base)
            .map(|s| s.trim_start_matches('/'))
            .unwrap_or(reference);
        if let Some(id) = local.strip_prefix("Medication/")
            && fhir_id(id)
            && !held.contains(id)
            && !out.iter().any(|o| o == id)
        {
            out.push(id.to_string());
        }
    }
    out
}

/// The run's bundle: each resource once (by its ledger key), sorted, under an id made from what it
/// holds, so the same record gives the same bytes. Returns the bytes and how many repeats it left out.
fn bundle(base: &str, entries: Vec<(Option<String>, Value)>) -> (Vec<u8>, i64) {
    let mut by_key: BTreeMap<String, Value> = BTreeMap::new();
    let mut repeated = 0;
    for (full, r) in entries {
        let key = fhir::pull_key(&r);
        if by_key.contains_key(&key) {
            repeated += 1;
            continue;
        }
        let url = full.unwrap_or_else(|| {
            format!(
                "{base}/{}/{}",
                r["resourceType"].as_str().unwrap_or_default(),
                r["id"].as_str().unwrap_or_default()
            )
        });
        by_key.insert(key, json!({"fullUrl": url, "resource": r}));
    }
    let entries: Vec<Value> = by_key.into_values().collect();
    let digest = Sha256::digest(serde_json::to_vec(&entries).unwrap_or_default());
    let id = format!("smart-{}", &hex::encode(digest)[..16]);
    let doc = json!({"resourceType": "Bundle", "id": id, "type": "collection", "entry": entries});
    (serde_json::to_vec(&doc).unwrap_or_default(), repeated)
}

/// One pull of a `smart` source at `now` (the clock its token expiry is counted from), with `egress`
/// for the token endpoint and, with a bigger cap and timeout, for the searches. The `source.poll` job
/// records the run around it ([`poll_smart`]); tests call it with a fake server and a moving clock.
pub async fn pull(
    ctx: &JobContext,
    source: &Source,
    egress: &Egress,
    now: DateTime<Utc>,
) -> Result<RunOutcome, JobError> {
    let profile_id = source.profile_id.ok_or_else(|| JobError::permanent(STOP_PROFILE))?;
    let base = source.config["endpoints"]["issuer"]
        .as_str()
        .map(|b| b.trim().trim_end_matches('/').to_string())
        .filter(|b| !b.is_empty())
        .ok_or_else(|| JobError::permanent(STOP_SERVER))?;
    let patient = source.config["patient"]
        .as_str()
        .filter(|p| fhir_id(p))
        .map(String::from)
        .ok_or_else(|| JobError::permanent(STOP_PATIENT))?;
    let scope = source.config["scope"].as_str();
    let mut r = Reader {
        http: SourceHttp::for_source(
            source,
            Egress {
                max_bytes: PAGE_BYTES,
                timeout: PAGE_TIMEOUT,
                ..egress.clone()
            },
        ),
        pool: ctx.pool.clone(),
        sb: SecretBox::from_settings(&ctx.settings)?.with_keys(ctx.blobs.keyring()),
        source_id: source.id,
        egress: egress.clone(),
        now,
        token: None,
        refreshed: false,
        bytes: 0,
    };
    let mut g = Gathered::default();

    // the Patient first: the importer matches it against the profile
    if granted(scope, "Patient") {
        let f = match r.get(&format!("{base}/Patient/{patient}")).await {
            Ok(f) => f,
            Err(Miss::Run(e)) => return Err(e),
            Err(Miss::Search(code, words)) => return Err(JobError::retry(format!("{code}: {words}"))),
        };
        let p: Value = serde_json::from_slice(&f.body).unwrap_or(Value::Null);
        if f.status != 200 || p["resourceType"] != "Patient" {
            return Err(JobError::retry(format!(
                "patient_unread: the server answered {} for the patient",
                f.status
            )));
        }
        g.entries.push((Some(format!("{base}/Patient/{patient}")), p));
    } else {
        g.skip("not_granted", 1);
    }
    let mut left_out: HashSet<&str> = HashSet::new();
    for (rtype, params) in SEARCHES {
        if !granted(scope, rtype) {
            left_out.insert(rtype);
            continue;
        }
        search(&mut r, &mut g, rtype, search_url(&base, rtype, &patient, params)).await?;
    }
    g.skip("not_granted", left_out.len() as i64);
    if g.answered == 0 {
        return Err(JobError::retry(STOP_NO_ANSWER));
    }
    if granted(scope, "Medication") {
        for id in missing_medications(&base, &g.entries)
            .into_iter()
            .take(MAX_MEDICATION_READS)
        {
            match r.get(&format!("{base}/Medication/{id}")).await {
                Ok(f) if f.status == 200 => {
                    let m: Value = serde_json::from_slice(&f.body).unwrap_or(Value::Null);
                    if m["resourceType"] == "Medication" {
                        g.entries.push((Some(format!("{base}/Medication/{id}")), m));
                    } else {
                        g.problem("not_fhir", "Medication: the answer is not a FHIR Medication".into());
                    }
                }
                Ok(f) => g.problem("search_failed", format!("Medication: the server answered {}", f.status)),
                Err(Miss::Run(e)) => return Err(e),
                Err(Miss::Search(code, words)) => g.problem(code, format!("Medication: {words}")),
            }
        }
    }

    let (bytes, repeated) = bundle(&base, std::mem::take(&mut g.entries));
    g.skip("repeated", repeated);
    let (sha, size) = store_bytes(&ctx.blobs, BlobOwner::Profile(profile_id), bytes).await?;
    let filename = format!("{} · {}.json", source.label, now.format("%Y-%m-%d"));
    let mut tx = ctx.pool.begin().await?;
    // one pull of this source imports at a time, even when a lease ran out and the job runs twice
    sqlx::query("select pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(format!("source.pull:{}", source.id))
        .execute(&mut *tx)
        .await?;
    let doc = create_document(
        &mut tx,
        &NewDocument {
            filename: Some(&filename),
            channel: KIND,
            source_id: Some(source.id),
            ..NewDocument::upload(source.household_id, Some(profile_id), &sha, FHIR_JSON, size as i64)
        },
    )
    .await?;
    let got = fhir::import_pull(&mut tx, &ctx.blobs, source.id, doc.id).await?;
    tx.commit().await?;
    g.skip("context", got.context);
    let n = |x: i64| i32::try_from(x).unwrap_or(i32::MAX);
    Ok(RunOutcome {
        seen: n(got.new + got.duplicate),
        new: n(got.new),
        duplicate: n(got.duplicate),
        problems: g.problems,
        skipped: g.skipped,
        ..RunOutcome::default()
    })
}

/// The `source.poll` of a `smart` source: now, through the connector egress.
pub async fn poll_smart(ctx: JobContext, source: Source) -> Result<RunOutcome, JobError> {
    let egress = oauth::connector_egress(ctx.settings.dev_auth);
    pull(&ctx, &source, &egress, Utc::now()).await
}

pub fn register() {
    ownhealth_core::sources::register_poller(KIND, |ctx, source| Box::pin(poll_smart(ctx, source)));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_grant_covers_a_type_by_name_wildcard_or_v1_read() {
        let v2 = Some("launch/patient offline_access patient/Observation.rs patient/Condition.r");
        assert!(granted(v2, "Observation"));
        assert!(granted(v2, "Condition"));
        assert!(!granted(v2, "Procedure"));
        assert!(granted(Some("patient/*.rs"), "Procedure"));
        assert!(granted(Some("patient/*.read"), "Procedure"));
        assert!(granted(Some("patient/*.*"), "Procedure"));
        assert!(granted(
            Some("patient/Observation.rs?category=laboratory"),
            "Observation"
        ));
        // write-only and user-level scopes read nothing here
        assert!(!granted(
            Some("patient/Observation.write patient/Condition.cu"),
            "Observation"
        ));
        assert!(!granted(Some("patient/Observation.write"), "Observation"));
        assert!(!granted(Some("user/*.rs patient/Condition.rs"), "Observation"));
        // a grant that names no patient scope at all: ask, the server refuses what it must
        assert!(granted(Some("openid fhirUser"), "Observation"));
        assert!(granted(None, "Observation"));
    }

    #[test]
    fn every_type_a_pull_reads_is_one_the_launch_asks_for() {
        let asked: HashSet<&str> = oauth::SMART_SCOPES.split_whitespace().collect();
        let read: HashSet<&str> = SEARCHES
            .iter()
            .map(|(t, _)| *t)
            .chain(["Patient", "Medication"])
            .collect();
        for t in &read {
            assert!(asked.contains(format!("patient/{t}.rs").as_str()), "{t}");
            assert!(US_CORE_TYPES.contains(t) || *t == "Medication", "{t}");
        }
        for t in US_CORE_TYPES {
            assert!(read.contains(t), "{t} is listed but never read");
        }
    }

    #[test]
    fn only_a_fhir_id_goes_into_a_url() {
        assert!(fhir_id("sam-rivera.1"));
        assert!(fhir_id(&"a".repeat(64)));
        assert!(!fhir_id(&"a".repeat(65)));
        assert!(!fhir_id(""));
        assert!(!fhir_id("sam/../../Patient"));
        assert!(!fhir_id("sam?x=1"));
        assert!(!fhir_id("sam rivera"));
    }

    fn obs(id: &str, v: f64) -> Value {
        json!({"resourceType": "Observation", "id": id, "meta": {"versionId": "1"}, "valueQuantity": {"value": v}})
    }

    #[test]
    fn the_same_resources_in_another_order_give_the_same_bundle() {
        let base = "https://fhir.labnorte.example/r4";
        let a = vec![(None, obs("a", 1.0)), (None, obs("b", 2.0))];
        let b = vec![
            (None, obs("b", 2.0)),
            (Some(format!("{base}/Observation/a")), obs("a", 1.0)),
            (None, obs("a", 1.0)),
        ];
        let (one, none) = bundle(base, a);
        let (two, repeated) = bundle(base, b);
        assert_eq!((none, repeated), (0, 1));
        assert_eq!(one, two);
        let v: Value = serde_json::from_slice(&one).expect("json");
        assert_eq!(v["type"], "collection");
        assert!(v["id"].as_str().is_some_and(|i| i.starts_with("smart-")));
        assert_eq!(v["entry"][0]["fullUrl"], format!("{base}/Observation/a"));
        // a changed value is another bundle
        let (three, _) = bundle(base, vec![(None, obs("a", 1.5)), (None, obs("b", 2.0))]);
        assert_ne!(one, three);
    }

    #[test]
    fn a_medicine_named_by_reference_is_read_once() {
        let base = "https://fhir.labnorte.example/r4";
        let order = |id: &str, med: &str| json!({"resourceType": "MedicationRequest", "id": id, "medicationReference": {"reference": med}});
        let entries = vec![
            (None, order("1", "Medication/metf")),
            (None, order("2", "Medication/metf")),
            (None, order("3", &format!("{base}/Medication/levo"))),
            (None, order("4", "https://other.example/r4/Medication/x")),
            (None, order("5", "#contained")),
            (None, order("6", "Medication/held")),
            (None, json!({"resourceType": "Medication", "id": "held"})),
        ];
        assert_eq!(missing_medications(base, &entries), ["metf", "levo"]);
    }

    #[test]
    fn a_failed_request_names_its_host_only() {
        let e = NetError::Http {
            host: "fhir.labnorte.example".into(),
            message: "error sending request for url (https://fhir.labnorte.example/r4/Patient/sam?x)".into(),
        };
        assert_eq!(net_text(&e), "the request to fhir.labnorte.example failed");
    }
}
