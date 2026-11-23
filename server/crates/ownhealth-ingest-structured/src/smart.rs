//! SMART on FHIR, standalone patient launch (connectors.md 6.2 and section 7): the poller of source
//! kind `smart`. The launch, callback, reconnect and disconnect are the shared connector routes
//! (`connector_oauth`, `routes/connectors.rs`); this module reads the person's record once a day.
//!
//! - **Searches.** For the patient the grant names (`source.config.patient`, the token answer's
//!   `patient`): a read of the Patient, then the US Core searches of AllergyIntolerance, Condition,
//!   DiagnosticReport, DocumentReference, Encounter, Immunization, MedicationRequest (intent order or
//!   plan, with `_include` of the Medications it names when the grant covers Medication reads),
//!   Observation (one search per category:
//!   laboratory, social-history, vital-signs; US Core servers require one) and Procedure. A
//!   Medication that a MedicationRequest names by reference and the answer did not include is read
//!   on its own. A read needs `r` in the grant and a search `s` (v1 `.read` gives both); a type the
//!   grant's `scope` leaves out is not asked for, and a granular scope (`?category=laboratory`)
//!   covers only the searches inside it. A server that answers a search with 400 because it wants
//!   `status` is asked again with every status, first as a list and then one at a time if the
//!   server rejects the list. Each search follows its `next` links on declared hosts and stops at
//!   a repeated page.
//! - **The patient.** The Patient the grant names must confirm the profile's name and full birth
//!   date before any clinical search. The pull stops if the identity differs or cannot be verified.
//!   The run is partial if a result does not identify the verified patient, or if the search
//!   returns a resource type outside the requested match and authorized includes.
//! - **One bundle per run.** The resources gathered by a pull form a `collection` Bundle, each
//!   resource once and in a fixed order. The grant or a failed search can leave the snapshot
//!   incomplete. The same snapshot gives the same bytes and document. A changed snapshot creates
//!   a new document, and the ledger imports only new resources. The import locks the source and
//!   checks the grant, profile and patient before storing the snapshot as a profile-owned blob
//!   and a document of channel `smart`. A disconnect, reconnect or new sign-in demand stops
//!   the import. A pull the person retracted comes back on the next day's run when the server
//!   still sends it (slice 14: the same bytes reactivate the document). Disconnect stops that.
//! - **Tokens.** `connector_oauth::access_token` gives the access token, and refreshes and rotates it
//!   under the source row lock. A token the FHIR server refuses (401) is dropped and refreshed once. A
//!   refused refresh marks the connection for a new sign-in, and the run stops before any search.
//! - **Health.** A search that fails is a problem of the run (`partial`), and the other searches
//!   still import. Identical problems are one line with a count, and failed searches come first. The
//!   run fails when the Patient cannot be read or no search answers; a grant that covers no search
//!   is a problem and reads nothing. A problem names the resource type and the status, never a URL,
//!   a patient id or a token.

use std::collections::{BTreeMap, BTreeSet, HashSet, VecDeque};
use std::time::Duration;

use chrono::{DateTime, NaiveDate, Utc};
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
use sqlx::{PgConnection, PgPool};
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

const MEDICATION_INTENT: (&str, &str) = ("intent", "order,plan");
const MEDICATION_INCLUDE: (&str, &str) = ("_include", "MedicationRequest:medication");
const MEDICATION_REQUEST_WITH_INCLUDE: [(&str, &str); 2] = [MEDICATION_INTENT, MEDICATION_INCLUDE];
const OBSERVATION_CATEGORY: &str = "http://terminology.hl7.org/CodeSystem/observation-category";

/// The searches after the Patient read: a type and the parameters besides `patient`.
const SEARCHES: [(&str, &[(&str, &str)]); 11] = [
    ("AllergyIntolerance", &[]),
    ("Condition", &[]),
    ("DiagnosticReport", &[]),
    ("DocumentReference", &[]),
    ("Encounter", &[]),
    ("Immunization", &[]),
    ("MedicationRequest", &[MEDICATION_INTENT]),
    ("Observation", &[("category", "laboratory")]),
    ("Observation", &[("category", "social-history")]),
    ("Observation", &[("category", "vital-signs")]),
    ("Procedure", &[]),
];

/// The status search parameter of each type and every value it takes (FHIR R4), for a server that
/// requires one (US Core general requirements: it answers 400 with an OperationOutcome).
fn every_status(rtype: &str) -> Option<(&'static str, &'static str)> {
    Some(match rtype {
        "AllergyIntolerance" => ("clinical-status", "active,inactive,resolved"),
        "Condition" => (
            "clinical-status",
            "active,recurrence,relapse,inactive,remission,resolved",
        ),
        "DiagnosticReport" => (
            "status",
            "registered,partial,preliminary,final,amended,corrected,appended,cancelled,entered-in-error,unknown",
        ),
        "DocumentReference" => ("status", "current,superseded,entered-in-error"),
        "Encounter" => (
            "status",
            "planned,arrived,triaged,in-progress,onleave,finished,cancelled,entered-in-error,unknown",
        ),
        "Immunization" => ("status", "completed,entered-in-error,not-done"),
        "MedicationRequest" => (
            "status",
            "active,on-hold,cancelled,completed,entered-in-error,stopped,draft,unknown",
        ),
        "Observation" => (
            "status",
            "registered,preliminary,final,amended,corrected,cancelled,entered-in-error,unknown",
        ),
        "Procedure" => (
            "status",
            "preparation,in-progress,not-done,on-hold,stopped,completed,entered-in-error,unknown",
        ),
        _ => return None,
    })
}

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

/// The limits of one run. The poller uses [`Caps::default`] (the constants above); a test narrows them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Caps {
    pub pages: usize,
    pub run_bytes: usize,
    pub medication_reads: usize,
}

impl Default for Caps {
    fn default() -> Self {
        Caps {
            pages: MAX_PAGES,
            run_bytes: RUN_BYTES,
            medication_reads: MAX_MEDICATION_READS,
        }
    }
}

/// What a run says when it stops. Information only: the Connections screen shows it, by its code.
const STOP_RECONSENT: &str =
    "reconsent_needed: the vendor no longer accepts this connection, so it waits for a new sign-in";
const STOP_PATIENT: &str = "no_patient: the grant names no patient, so there is nothing to search";
const STOP_SERVER: &str = "no_server: the connection names no FHIR server";
const STOP_PROFILE: &str = "no_profile: the connection belongs to no profile";
const STOP_NO_ANSWER: &str = "no_answer: no US Core search answered";
const STOP_ANOTHER_PATIENT: &str =
    "another_patient: the account at the portal is for another person than this profile, so nothing was imported";
const STOP_PATIENT_UNVERIFIED: &str =
    "patient_unverified: the Patient response does not verify the profile's identity, so nothing was imported";
const STOP_OFF: &str = "switched_off: the connection was switched off during the run, so nothing was imported";
const STOP_CHANGED: &str = "source_changed: the connection changed during the run, so nothing was imported";
const STOP_GRANT_CHANGED: &str = "grant_changed: the portal sign-in changed during the run, so nothing was imported";
const STOP_PATIENT_NOT_GRANTED: &str = "patient_not_granted: the grant does not cover reading the patient, so the record \
     cannot be matched to this profile and nothing was read";

/// The words of every problem and every stop that carries a value: the code uses these, and so does
/// [`phrases`], so the wording lint reads exactly what a person reads.
mod words {
    use super::NetError;

    pub fn answered(rtype: &str, status: u16) -> String {
        format!("{rtype}: the server answered {status}")
    }
    pub fn not_bundle(rtype: &str) -> String {
        format!("{rtype}: the answer is not a FHIR Bundle")
    }
    pub const NOT_MEDICATION: &str = "Medication: the answer is not a FHIR Medication";
    pub const MEDICATION_ID_MISMATCH: &str =
        "Medication: the server returned a different resource ID, so it was skipped";
    pub fn patient_reference(rtype: &str) -> String {
        format!("{rtype}: a resource did not identify the verified patient and was skipped")
    }
    pub fn unexpected_resource(rtype: &str) -> String {
        format!("{rtype}: the server returned a resource outside the search and it was skipped")
    }
    pub fn page_too_large(rtype: &str, bytes: usize) -> String {
        format!("{rtype}: the answer exceeded the {bytes}-byte limit for one page")
    }
    pub fn too_many_pages(rtype: &str, pages: usize) -> String {
        format!("{rtype}: stopped after {pages} pages")
    }
    pub fn repeated_page(rtype: &str) -> String {
        format!("{rtype}: the server sent a next page it had sent before, so the rest was not read")
    }
    pub fn too_large(rtype: &str, bytes: usize) -> String {
        format!("{rtype}: the answers of this run passed {bytes} bytes, so the rest was not read")
    }
    pub fn medications_left(n: usize) -> String {
        format!("Medication: {n} more medicines that orders name were not read")
    }
    pub fn undeclared(rtype: &str) -> String {
        format!("{rtype}: a next page on a host this connection did not declare was not read")
    }
    pub fn unreachable(rtype: &str, e: &NetError) -> String {
        format!("{rtype}: {}", net_text(e))
    }
    pub const NOTHING_GRANTED: &str = "the grant covers none of the US Core searches, so nothing was read";
    /// One line for a problem that happened `n` times.
    pub fn times(message: &str, n: usize) -> String {
        format!("{message} ({n} times)")
    }
    pub fn patient_unread(status: u16) -> String {
        format!("patient_unread: the server answered {status} for the patient")
    }
    pub fn token_refused(status: u16, code: &str) -> String {
        format!("token_refused: the token endpoint answered {status} ({code})")
    }
    pub fn token_unreachable(e: &NetError) -> String {
        format!("token_unreachable: {}", net_text(e))
    }
    pub fn undeclared_host(host: &str) -> String {
        format!("undeclared_host: {host} is not a host of this connection")
    }

    /// The words a failed request leaves: the host at most, never the URL.
    pub fn net_text(e: &NetError) -> String {
        match e {
            NetError::Http { host, .. } => format!("the request to {host} failed"),
            other => other.to_string(),
        }
    }
}

/// The texts a person may read on the Connections screen, for the wording lint: every stop, and
/// every problem made by the same functions as the run's, with sample values.
pub fn phrases() -> Value {
    let net = NetError::Http {
        host: "fhir.labnorte.example".into(),
        message: String::new(),
    };
    json!({
        "stop": [STOP_RECONSENT, STOP_PATIENT, STOP_SERVER, STOP_PROFILE, STOP_NO_ANSWER, STOP_ANOTHER_PATIENT,
                 STOP_PATIENT_UNVERIFIED, STOP_OFF, STOP_CHANGED, STOP_GRANT_CHANGED, STOP_PATIENT_NOT_GRANTED,
                 words::patient_unread(404), words::token_refused(500, "server_error"),
                 words::token_unreachable(&net), words::undeclared_host("fhir.elsewhere.example")],
        "problem": [
            words::answered("Observation", 503),
            words::answered("Medication", 404),
            words::times(&words::answered("Observation", 503), 3),
            words::undeclared("MedicationRequest"),
            words::unreachable("Condition", &net),
            words::not_bundle("Observation"),
            words::NOT_MEDICATION,
            words::MEDICATION_ID_MISMATCH,
            words::patient_reference("Observation"),
            words::unexpected_resource("Condition"),
            words::page_too_large("Observation", PAGE_BYTES),
            words::too_many_pages("Observation", MAX_PAGES),
            words::repeated_page("Condition"),
            words::too_large("Observation", RUN_BYTES),
            words::too_large("Medication", RUN_BYTES),
            words::medications_left(3),
            words::NOTHING_GRANTED,
        ],
    })
}

/// A FHIR id ([A-Za-z0-9\-\.]{1,64}): only such an id goes into a URL.
fn fhir_id(s: &str) -> bool {
    !s.is_empty() && s.len() <= 64 && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'.')
}

/// What a request does: a read of one resource by id, or a search.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    Read,
    Search,
}

/// Whether the grant's `scope` lets this run make a request of `op` on `rtype` with the search
/// `params`. A token answer without `scope` (a server that does not echo it) is asked for
/// everything, and the server refuses what it must; a `scope` that names no `patient/` scope (only
/// `openid fhirUser launch/patient`, say) grants no resource. SMART v2 letters count by operation (`r`
/// reads, `s` searches: `patient/Observation.rs`, `patient/*.rs`), v1 `.read` and `.*` give both. A
/// granular v2 scope covers only searches whose parameters hold each of its values. A category
/// grant that names a system requires the same system and code in the search.
pub fn granted(scope: Option<&str>, rtype: &str, op: Op, params: &[(&str, &str)]) -> bool {
    scope.is_none_or(|scope| {
        grant_filters(scope, rtype, op).any(|query| query.is_none_or(|q| op == Op::Search && within(q, params)))
    })
}

/// Filters from grants that cover this resource and operation. None means unrestricted.
fn grant_filters<'a>(scope: &'a str, rtype: &str, op: Op) -> impl Iterator<Item = Option<&'a str>> {
    let letter = match op {
        Op::Read => 'r',
        Op::Search => 's',
    };
    scope.split_whitespace().filter_map(move |s| {
        let rest = s.strip_prefix("patient/")?;
        let (resource, query) = rest.split_once('?').map_or((rest, None), |(r, q)| (r, Some(q)));
        let (rtype_in_scope, perm) = resource.split_once('.')?;
        let covers_type = rtype_in_scope == "*" || rtype_in_scope == rtype;
        let v1 = perm == "read" || perm == "*";
        let v2 = perm.chars().all(|c| "cruds".contains(c)) && perm.contains(letter);
        (covers_type && (v1 || v2)).then_some(query)
    })
}

/// Category-only Condition grants authorize one search per distinct category token.
fn condition_categories(scope: &str) -> BTreeSet<String> {
    grant_filters(scope, "Condition", Op::Search)
        .flatten()
        .filter_map(|query| {
            let mut params = url::form_urlencoded::parse(query.as_bytes());
            let (key, value) = params.next()?;
            (key == "category" && !value.is_empty() && params.next().is_none()).then(|| value.into_owned())
        })
        .collect()
}

/// Whether a search with `params` stays inside a granular scope's `k=v&..`.
fn within(query: &str, params: &[(&str, &str)]) -> bool {
    url::form_urlencoded::parse(query.as_bytes()).all(|(k, v)| {
        params.iter().any(|(pk, pv)| {
            *pk == k && (v == *pv || (k == "category" && pv.rsplit_once('|').is_some_and(|(_, code)| v == code)))
        })
    })
}

/// The run stops on a token it cannot get; the texts carry no token (`OAuthError` never does).
fn token_stop(e: OAuthError) -> JobError {
    match e {
        OAuthError::ReconsentNeeded => JobError::permanent(STOP_RECONSENT),
        OAuthError::Refused { code, message } => JobError::permanent(format!("{code}: {message}")),
        OAuthError::Core(e) => e.into(),
        OAuthError::Vendor { status, code } => JobError::retry(words::token_refused(status, &code)),
        OAuthError::UndeclaredHost(h) => JobError::permanent(words::undeclared_host(&h)),
        OAuthError::Net(e) => JobError::retry(words::token_unreachable(&e)),
    }
}

/// Why one request gave nothing.
enum Miss {
    /// The run stops: no token.
    Run(JobError),
    /// This search stops: a problem code and its words, without the type.
    Search(&'static str, Box<dyn Fn(&str) -> String + Send>),
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
    run_bytes: usize,
    remaining: usize,
    budget_hit: bool,
}

impl Reader {
    fn budget_problem(&mut self) -> Miss {
        self.budget_hit = true;
        let limit = self.run_bytes;
        Miss::Search("too_large", Box::new(move |t| words::too_large(t, limit)))
    }

    async fn get(&mut self, url: &str) -> Result<Fetched, Miss> {
        loop {
            if self.remaining == 0 {
                return Err(self.budget_problem());
            }
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
                .get_bounded(
                    url,
                    &[("accept", FHIR_JSON), ("authorization", &bearer)],
                    self.remaining,
                )
                .await
            {
                Ok(f) => f,
                Err((OAuthError::UndeclaredHost(_), _)) => {
                    return Err(Miss::Search("undeclared_host", Box::new(words::undeclared)));
                }
                Err((OAuthError::Net(NetError::TooLarge(limit)), bytes_read)) => {
                    // The last chunk can take the measured bytes past the page cap.
                    self.remaining = self.remaining.saturating_sub(bytes_read);
                    if self.remaining == 0 {
                        return Err(self.budget_problem());
                    }
                    return Err(Miss::Search(
                        "page_too_large",
                        Box::new(move |t| words::page_too_large(t, limit)),
                    ));
                }
                Err((OAuthError::Net(e), bytes_read)) => {
                    self.remaining -= bytes_read;
                    return Err(Miss::Search(
                        "unreachable",
                        Box::new(move |t: &str| words::unreachable(t, &e)),
                    ));
                }
                Err((e, _)) => return Err(Miss::Run(token_stop(e))),
            };
            self.remaining -= f.body.len();
            // the server dropped the token before its time: one refresh, then the answer stands
            if f.status == 401 && !self.refreshed {
                self.refreshed = true;
                self.token = None;
                oauth::forget(self.source_id);
                continue;
            }
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

/// The run's problems as the Connections screen shows them: each distinct one once, with how many
/// times it happened, and a search that failed before a note about a page (the screen shows the first).
fn tidy(problems: Vec<RunProblem>) -> Vec<RunProblem> {
    let mut out: Vec<(RunProblem, usize)> = Vec::new();
    for p in problems {
        match out.iter_mut().find(|(q, _)| q.code == p.code && q.message == p.message) {
            Some((_, n)) => *n += 1,
            None => out.push((p, 1)),
        }
    }
    let rank = |code: &str| match code {
        "search_failed" | "unreachable" | "not_fhir" | "medication_id_mismatch" => 0,
        "too_large" | "page_too_large" | "too_many_pages" | "repeated_page" | "too_many_medications" => 1,
        _ => 2,
    };
    out.sort_by_key(|(p, _)| rank(&p.code));
    out.into_iter()
        .map(|(p, n)| match n {
            1 => p,
            _ => RunProblem {
                message: words::times(&p.message, n),
                ..p
            },
        })
        .collect()
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

/// Whether a 400 answer is a server saying the search must name a status.
fn wants_status(body: &[u8]) -> bool {
    let o: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
    o["resourceType"] == "OperationOutcome" && o["issue"].to_string().to_lowercase().contains("status")
}

/// One request of a search: the answer, or `None` when the search stops (its problem is in `g`).
async fn fetch(r: &mut Reader, g: &mut Gathered, rtype: &str, url: &str) -> Result<Option<Fetched>, JobError> {
    match r.get(url).await {
        Ok(f) => Ok(Some(f)),
        Err(Miss::Run(e)) => Err(e),
        Err(Miss::Search(code, say)) => {
            g.problem(code, say(rtype));
            Ok(None)
        }
    }
}

/// The root of a RESTful fullUrl whose final segments identify this entry's resource.
fn entry_fhir_base(entry: &Value) -> Option<&str> {
    let full = entry["fullUrl"].as_str()?;
    let (before_id, id) = full.rsplit_once('/')?;
    let (base, kind) = before_id.rsplit_once('/')?;
    let resource = &entry["resource"];
    (resource["resourceType"] == kind && resource["id"] == id && fhir_id(id)).then_some(base)
}

/// Clinical results must name the verified patient. An included Medication describes the drug
/// and may omit the patient reference.
fn belongs_to_patient(entry: &Value, base: &str, patient: &str) -> bool {
    let resource = &entry["resource"];
    let needs_reference = resource["resourceType"] != "Medication";
    let mut references = ["patient", "subject"]
        .iter()
        .filter_map(|key| resource[*key]["reference"].as_str())
        .peekable();
    (!needs_reference || references.peek().is_some())
        && references.all(|reference| {
            let Some((root, id)) = (if let Some(id) = reference.strip_prefix("Patient/") {
                // A present fullUrl must supply the root. Without it, use the search server.
                if entry.get("fullUrl").is_none() {
                    Some((base, id))
                } else {
                    entry_fhir_base(entry).map(|root| (root, id))
                }
            } else {
                reference.rsplit_once("/Patient/")
            }) else {
                return false;
            };
            let same_patient = id == patient
                || id
                    .split_once("/_history/")
                    .is_some_and(|(id, version)| id == patient && fhir_id(version));
            same_patient
                && (root == base
                    || url::Url::parse(root).is_ok_and(|root| url::Url::parse(base).is_ok_and(|base| root == base)))
        })
}

#[derive(Clone, Copy)]
enum StatusRetry {
    Unfiltered,
    List(&'static str, &'static str),
    None,
}

/// Gather one search and its status fallbacks. An exhausted byte budget stops further searches.
async fn search(
    r: &mut Reader,
    g: &mut Gathered,
    caps: &Caps,
    (base, patient, scope): (&str, &str, Option<&str>),
    rtype: &str,
    params: &[(&str, &str)],
) -> Result<(), JobError> {
    let filtered_url = |key, value| {
        let mut with = params.to_vec();
        with.push((key, value));
        granted(scope, rtype, Op::Search, &with).then(|| search_url(base, rtype, patient, &with))
    };
    let mut pending = VecDeque::from([(search_url(base, rtype, patient, params), StatusRetry::Unfiltered)]);
    let mut visited = HashSet::new();
    let mut answered = false;
    while let Some((mut url, retry)) = pending.pop_front() {
        if r.budget_hit {
            break;
        }
        for page in 1..=caps.pages {
            if !visited.insert(url.clone()) {
                if page > 1 {
                    g.problem("repeated_page", words::repeated_page(rtype));
                }
                break;
            }
            let Some(f) = fetch(r, g, rtype, &url).await? else {
                break;
            };
            if f.status != 200 {
                if page == 1 {
                    match retry {
                        StatusRetry::Unfiltered if f.status == 400 && wants_status(&f.body) => {
                            if let Some((key, values)) = every_status(rtype)
                                && let Some(url) = filtered_url(key, values)
                            {
                                pending.push_back((url, StatusRetry::List(key, values)));
                                break;
                            }
                        }
                        // A server may require a status and accept only one value per search.
                        StatusRetry::List(key, values) if matches!(f.status, 400 | 422 | 501) => {
                            pending.extend(
                                values
                                    .split(',')
                                    .filter_map(|value| filtered_url(key, value).map(|url| (url, StatusRetry::None))),
                            );
                            if !pending.is_empty() {
                                break;
                            }
                        }
                        _ => {}
                    }
                }
                g.problem("search_failed", words::answered(rtype, f.status));
                break;
            }
            let bundle: Value = serde_json::from_slice(&f.body).unwrap_or(Value::Null);
            if bundle["resourceType"] != "Bundle" {
                g.problem("not_fhir", words::not_bundle(rtype));
                break;
            }
            answered = true;
            gather_entries(g, &bundle, (base, patient), rtype, params.contains(&MEDICATION_INCLUDE));
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
                Some(_) if page == caps.pages => {
                    g.problem("too_many_pages", words::too_many_pages(rtype, caps.pages));
                }
                Some(n) => url = n,
            }
        }
    }
    if answered {
        g.answered += 1;
    }
    Ok(())
}

fn gather_entries(
    g: &mut Gathered,
    bundle: &Value,
    (base, patient): (&str, &str),
    rtype: &str,
    include_medication: bool,
) {
    for e in bundle["entry"].as_array().map(Vec::as_slice).unwrap_or_default() {
        let res = &e["resource"];
        let t = res["resourceType"].as_str().unwrap_or_default();
        if t.is_empty() {
            g.skip("unreadable", 1);
        } else if t == "OperationOutcome" {
            g.skip("outcome", 1);
        } else if e["search"]["mode"] == "outcome" || !(t == rtype || (t == "Medication" && include_medication)) {
            g.skip("not_granted", i64::from(t == "Medication" && !include_medication));
            g.problem("unexpected_resource", words::unexpected_resource(rtype));
        } else if !belongs_to_patient(e, base, patient) {
            g.problem("patient_reference", words::patient_reference(rtype));
        } else {
            let full = e["fullUrl"].as_str().filter(|u| !u.is_empty()).map(String::from);
            g.entries.push((full, res.clone()));
        }
    }
}

/// The Medications that MedicationRequests name by a reference this bundle does not hold: id only,
/// relative (`Medication/<id>`) or on the FHIR server itself.
fn missing_medications(base: &str, entries: &[(Option<String>, Value)]) -> Vec<String> {
    let mut seen: HashSet<&str> = entries
        .iter()
        .filter(|(_, r)| r["resourceType"] == "Medication")
        .filter_map(|(_, r)| r["id"].as_str())
        .collect();
    let mut out = Vec::new();
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
            && seen.insert(id)
        {
            out.push(id.to_string());
        }
    }
    out
}

/// The run's bundle: each resource once (by its ledger key), sorted, under an id made from what it
/// holds, so the same record gives the same bytes. A resource sent twice keeps the URL the server
/// gave it (the smallest when it gave two), whatever the order. Returns the bytes and how many
/// repeats it left out.
fn bundle(base: &str, entries: Vec<(Option<String>, Value)>) -> (Vec<u8>, i64) {
    let mut by_key: BTreeMap<String, (Option<String>, Value)> = BTreeMap::new();
    let mut repeated = 0;
    for (full, r) in entries {
        let key = fhir::pull_key(&r);
        match by_key.get_mut(&key) {
            Some(held) => {
                repeated += 1;
                let better = match (&full, &held.0) {
                    (Some(_), None) => true,
                    (Some(a), Some(b)) => a < b,
                    _ => false,
                };
                if better {
                    held.0 = full;
                }
            }
            None => {
                by_key.insert(key, (full, r));
            }
        }
    }
    let entries: Vec<Value> = by_key
        .into_values()
        .map(|(full, r)| {
            let url = full.unwrap_or_else(|| {
                format!(
                    "{base}/{}/{}",
                    r["resourceType"].as_str().unwrap_or_default(),
                    r["id"].as_str().unwrap_or_default()
                )
            });
            json!({"fullUrl": url, "resource": r})
        })
        .collect();
    let digest = Sha256::digest(serde_json::to_vec(&entries).unwrap_or_default());
    let id = format!("smart-{}", &hex::encode(digest)[..16]);
    let doc = json!({"resourceType": "Bundle", "id": id, "type": "collection", "entry": entries});
    (serde_json::to_vec(&doc).unwrap_or_default(), repeated)
}

/// Why this run must import nothing now: the source row as it is (locked `for share` inside the
/// import transaction, so a disconnect waits for the import or the import sees it), or `None` when it
/// is still the connection the run started with.
async fn changed(conn: &mut PgConnection, source: &Source, patient: &str) -> Result<Option<&'static str>, JobError> {
    #[derive(sqlx::FromRow)]
    struct LockedSource {
        enabled: bool,
        reconsent_needed: bool,
        profile_id: Option<Uuid>,
        patient: Option<String>,
        oauth_grant_id: Option<Uuid>,
    }

    let row: Option<LockedSource> = sqlx::query_as(
        "select enabled, reconsent_needed_at is not null as reconsent_needed, profile_id, \
         config->>'patient' as patient, oauth_grant_id from source where id = $1 for share",
    )
    .bind(source.id)
    .fetch_optional(conn)
    .await?;
    Ok(match row {
        None | Some(LockedSource { enabled: false, .. }) => Some(STOP_OFF),
        Some(LockedSource {
            reconsent_needed: true, ..
        }) => Some(STOP_RECONSENT),
        Some(current) if current.profile_id != source.profile_id || current.patient.as_deref() != Some(patient) => {
            Some(STOP_CHANGED)
        }
        Some(current) if current.oauth_grant_id != source.oauth_grant_id => Some(STOP_GRANT_CHANGED),
        Some(_) => None,
    })
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
    pull_with(ctx, source, egress, now, &Caps::default()).await
}

/// [`pull`] with the run's limits given.
pub async fn pull_with(
    ctx: &JobContext,
    source: &Source,
    egress: &Egress,
    now: DateTime<Utc>,
    caps: &Caps,
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
    let mut g = Gathered::default();

    let mut left_out = HashSet::new();
    let mut planned = Vec::new();
    for (rtype, params) in SEARCHES {
        if granted(scope, rtype, Op::Search, params) {
            planned.push((rtype, params, None));
            continue;
        }
        if rtype == "Condition"
            && let Some(scope) = scope
        {
            let categories = condition_categories(scope);
            if !categories.is_empty() {
                planned.extend(categories.into_iter().map(|category| (rtype, params, Some(category))));
                continue;
            }
        }
        if let [("category", code)] = params {
            let token = format!("{OBSERVATION_CATEGORY}|{code}");
            if granted(scope, rtype, Op::Search, &[("category", &token)]) {
                planned.push((rtype, params, Some(token)));
                continue;
            }
        }
        left_out.insert(rtype);
    }
    let read_patient = granted(scope, "Patient", Op::Read, &[]);
    g.skip("not_granted", left_out.len() as i64 + i64::from(!read_patient));
    if planned.is_empty() {
        g.problem("not_granted", words::NOTHING_GRANTED.into());
        return Ok(RunOutcome {
            problems: g.problems,
            skipped: g.skipped,
            ..RunOutcome::default()
        });
    }
    // without the Patient nothing shows the record is this profile's person: search nothing
    if !read_patient {
        return Err(JobError::permanent(STOP_PATIENT_NOT_GRANTED));
    }

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
        run_bytes: caps.run_bytes,
        remaining: caps.run_bytes,
        budget_hit: false,
    };

    // Verify the Patient before requesting any clinical data.
    let f = match r.get(&format!("{base}/Patient/{patient}")).await {
        Ok(f) => f,
        Err(Miss::Run(e)) => return Err(e),
        Err(Miss::Search(code, say)) => {
            return Err(JobError::retry(format!("{code}: {}", say("Patient"))));
        }
    };
    let p: Value = serde_json::from_slice(&f.body).unwrap_or(Value::Null);
    if f.status != 200 || p["resourceType"] != "Patient" {
        return Err(JobError::retry(words::patient_unread(f.status)));
    }
    let who: Option<(String, Option<NaiveDate>)> =
        sqlx::query_as("select display_name, birth_date from profile where id = $1")
            .bind(profile_id)
            .fetch_optional(&ctx.pool)
            .await?;
    let Some((name, born)) = who else {
        return Err(JobError::permanent(STOP_CHANGED));
    };
    let full_birth_date = p["birthDate"]
        .as_str()
        .is_some_and(|date| date.len() == 10 && NaiveDate::parse_from_str(date, "%Y-%m-%d").is_ok());
    if p["id"] != patient || !full_birth_date {
        return Err(JobError::permanent(STOP_PATIENT_UNVERIFIED));
    }
    use fhir::mapping::PatientIdentity;
    match fhir::mapping::smart_patient_identity(&p, &name, born) {
        PatientIdentity::Verified => {}
        PatientIdentity::Mismatch => return Err(JobError::permanent(STOP_ANOTHER_PATIENT)),
        PatientIdentity::Unverified => return Err(JobError::permanent(STOP_PATIENT_UNVERIFIED)),
    }
    g.entries.push((Some(format!("{base}/Patient/{patient}")), p));
    let read_medication = granted(scope, "Medication", Op::Read, &[]);
    for (rtype, params, category) in planned {
        let qualified = category.as_deref().map(|token| [("category", token)]);
        let params: &[(&str, &str)] = match qualified.as_ref() {
            Some(category) => category,
            None if rtype == "MedicationRequest" && read_medication => &MEDICATION_REQUEST_WITH_INCLUDE,
            None => params,
        };
        search(&mut r, &mut g, caps, (&base, &patient, scope), rtype, params).await?;
        if r.budget_hit {
            break;
        }
    }
    if g.answered == 0 {
        let limit_code = if r.budget_hit { "too_large" } else { "page_too_large" };
        if let Some(problem) = g.problems.iter().find(|p| p.code == limit_code) {
            return Err(JobError::retry(format!("{}: {}", problem.code, problem.message)));
        }
        return Err(JobError::retry(STOP_NO_ANSWER));
    }
    if read_medication && !r.budget_hit {
        let missing = missing_medications(&base, &g.entries);
        let mut left = missing.len();
        for id in missing.iter().take(caps.medication_reads) {
            left -= 1;
            match r.get(&format!("{base}/Medication/{id}")).await {
                Ok(f) if f.status == 200 => {
                    let m: Value = serde_json::from_slice(&f.body).unwrap_or(Value::Null);
                    if m["resourceType"] != "Medication" {
                        g.problem("not_fhir", words::NOT_MEDICATION.into());
                    } else if m["id"].as_str() != Some(id.as_str()) {
                        g.problem("medication_id_mismatch", words::MEDICATION_ID_MISMATCH.into());
                    } else {
                        g.entries.push((Some(format!("{base}/Medication/{id}")), m));
                    }
                }
                Ok(f) => g.problem("search_failed", words::answered("Medication", f.status)),
                Err(Miss::Run(e)) => return Err(e),
                Err(Miss::Search(code, say)) => {
                    g.problem(code, say("Medication"));
                    if r.budget_hit {
                        left = 0;
                        break;
                    }
                }
            }
        }
        if left > 0 {
            g.problem("too_many_medications", words::medications_left(left));
        }
    }

    let (bytes, repeated) = bundle(&base, std::mem::take(&mut g.entries));
    g.skip("repeated", repeated);
    let filename = format!("{} · {}.json", source.label, now.format("%Y-%m-%d %H.%M UTC"));
    let mut tx = ctx.pool.begin().await?;
    // one pull of this source imports at a time, even when a lease ran out and the job runs twice
    sqlx::query("select pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(format!("source.pull:{}", source.id))
        .execute(&mut *tx)
        .await?;
    if let Some(stop) = changed(&mut tx, source, &patient).await? {
        tx.rollback().await?;
        return Err(JobError::permanent(stop));
    }
    let (sha, size) = store_bytes(&ctx.blobs, BlobOwner::Profile(profile_id), bytes).await?;
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
    let got = fhir::import_pull_with_identity(&mut tx, &ctx.blobs, source.id, doc.id, Some((&name, born))).await?;
    tx.commit().await?;
    g.skip("context", got.context);
    let n = |x: i64| i32::try_from(x).unwrap_or(i32::MAX);
    Ok(RunOutcome {
        seen: n(got.new + got.duplicate),
        new: n(got.new),
        duplicate: n(got.duplicate),
        problems: tidy(g.problems),
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

    const S: Op = Op::Search;
    const R: Op = Op::Read;

    #[test]
    fn a_grant_covers_a_type_by_name_wildcard_or_v1_read() {
        let v2 = Some("launch/patient offline_access patient/Observation.rs patient/Condition.rs");
        assert!(granted(v2, "Observation", S, &[]));
        assert!(granted(v2, "Condition", S, &[]));
        assert!(!granted(v2, "Procedure", S, &[]));
        assert!(granted(Some("patient/*.rs"), "Procedure", S, &[]));
        assert!(granted(Some("patient/*.read"), "Procedure", S, &[]));
        assert!(granted(Some("patient/*.*"), "Procedure", S, &[]));
        // write-only and user-level scopes read nothing here
        assert!(!granted(
            Some("patient/Observation.write patient/Condition.cu"),
            "Observation",
            S,
            &[]
        ));
        assert!(!granted(Some("patient/Observation.write"), "Observation", S, &[]));
        assert!(!granted(Some("user/*.rs patient/Condition.rs"), "Observation", S, &[]));
        // a token answer without scope: ask, the server refuses what it must
        assert!(granted(None, "Observation", S, &[]));
    }

    #[test]
    fn a_grant_of_identity_scopes_only_covers_no_resource() {
        let id_only = Some("openid fhirUser launch/patient offline_access");
        for t in US_CORE_TYPES {
            assert!(!granted(id_only, t, S, &[]), "{t}");
            assert!(!granted(id_only, t, R, &[]), "{t}");
        }
    }

    #[test]
    fn a_v2_letter_covers_only_its_own_operation_and_v1_read_covers_both() {
        let reads = Some("patient/Patient.r patient/Observation.s");
        assert!(granted(reads, "Patient", R, &[]));
        assert!(!granted(reads, "Patient", S, &[]));
        assert!(granted(reads, "Observation", S, &[]));
        assert!(!granted(reads, "Observation", R, &[]));
        assert!(granted(Some("patient/Patient.s"), "Patient", S, &[]));
        assert!(!granted(Some("patient/Patient.s"), "Patient", R, &[]));
        for op in [R, S] {
            assert!(granted(Some("patient/Medication.read"), "Medication", op, &[]));
            assert!(granted(Some("patient/Medication.rs"), "Medication", op, &[]));
        }
    }

    #[test]
    fn a_granular_scope_covers_only_the_searches_inside_it() {
        let labs = Some(
            "patient/Observation.rs?category=http://terminology.hl7.org/CodeSystem/observation-category|laboratory",
        );
        let qualified = "http://terminology.hl7.org/CodeSystem/observation-category|laboratory";
        assert!(granted(labs, "Observation", S, &[("category", qualified)]));
        assert!(!granted(labs, "Observation", S, &[("category", "laboratory")]));
        assert!(!granted(labs, "Observation", S, &[("category", "vital-signs")]));
        assert!(!granted(labs, "Observation", S, &[]));
        assert!(!granted(labs, "Observation", R, &[]));
        let bare = Some("patient/Observation.rs?category=laboratory");
        assert!(granted(bare, "Observation", S, &[("category", "laboratory")]));
        assert!(!granted(bare, "Observation", S, &[("category", "social-history")]));
        // an unrestricted scope beside it covers every category
        let both = Some("patient/Observation.rs?category=laboratory patient/Observation.rs");
        for c in ["laboratory", "social-history", "vital-signs"] {
            assert!(granted(both, "Observation", S, &[("category", c)]), "{c}");
        }
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
            assert!(
                *t == "Medication" || *t == "Patient" || every_status(t).is_some(),
                "{t}"
            );
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
    fn a_resource_sent_twice_under_two_urls_gives_the_same_bundle_in_either_order() {
        let base = "https://fhir.labnorte.example/r4";
        let alias = Some("https://fhir.labnorte.example/alias/Observation/a".to_string());
        let own = Some(format!("{base}/Observation/a"));
        let orders = [
            vec![
                (None, obs("a", 1.0)),
                (alias.clone(), obs("a", 1.0)),
                (own.clone(), obs("a", 1.0)),
            ],
            vec![
                (own.clone(), obs("a", 1.0)),
                (alias.clone(), obs("a", 1.0)),
                (None, obs("a", 1.0)),
            ],
            vec![
                (alias.clone(), obs("a", 1.0)),
                (None, obs("a", 1.0)),
                (own.clone(), obs("a", 1.0)),
            ],
        ];
        let made: Vec<Vec<u8>> = orders.into_iter().map(|o| bundle(base, o).0).collect();
        assert!(made.windows(2).all(|w| w[0] == w[1]));
        let v: Value = serde_json::from_slice(&made[0]).expect("json");
        assert_eq!(
            v["entry"][0]["fullUrl"],
            "https://fhir.labnorte.example/alias/Observation/a"
        );
        // a copy without a URL and one with: the server's URL wins in both orders
        let (x, _) = bundle(base, vec![(None, obs("a", 1.0)), (alias_of("x"), obs("a", 1.0))]);
        let (y, _) = bundle(base, vec![(alias_of("x"), obs("a", 1.0)), (None, obs("a", 1.0))]);
        assert_eq!(x, y);
    }

    fn alias_of(s: &str) -> Option<String> {
        Some(format!("https://fhir.labnorte.example/{s}/Observation/a"))
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
        assert_eq!(words::net_text(&e), "the request to fhir.labnorte.example failed");
    }

    #[test]
    fn identical_problems_are_one_line_and_failed_searches_come_first() {
        let p = |code: &str, message: String| RunProblem {
            code: code.into(),
            message,
        };
        let got = tidy(vec![
            p("undeclared_host", words::undeclared("MedicationRequest")),
            p("search_failed", words::answered("Observation", 503)),
            p("search_failed", words::answered("Observation", 503)),
            p("search_failed", words::answered("Observation", 503)),
            p("search_failed", words::answered("Procedure", 500)),
        ]);
        let codes: Vec<(&str, &str)> = got.iter().map(|p| (p.code.as_str(), p.message.as_str())).collect();
        assert_eq!(
            codes,
            [
                ("search_failed", "Observation: the server answered 503 (3 times)"),
                ("search_failed", "Procedure: the server answered 500"),
                (
                    "undeclared_host",
                    "MedicationRequest: a next page on a host this connection did not declare was not read"
                ),
            ]
        );
    }

    #[test]
    fn a_status_demand_is_an_outcome_that_names_status() {
        let oo = |text: &str| {
            serde_json::to_vec(
                &json!({"resourceType": "OperationOutcome", "issue": [{"severity": "error",
                "code": "required", "diagnostics": text}]}),
            )
            .unwrap_or_default()
        };
        assert!(wants_status(&oo("The status parameter is required")));
        assert!(!wants_status(&oo("category is required")));
        assert!(!wants_status(b"status"));
    }
}
