//! SMART standalone patient launch and the US Core poller (connectors.md 6.2, slice 15) against a
//! fake SMART server on loopback. The fake serves `fixtures/fhir/uscore-sam.json` through discovery,
//! a token endpoint (PKCE S256, refresh tokens that rotate and die once used) and the US Core
//! searches, one resource per page. Only the newest access token reads the FHIR API, so every pull
//! proves it used the token of its own refresh. Synthetic data only (the Rivera family, the
//! fictional LabNorte hospital); nothing here reaches the internet.

mod common;

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::{Arc, Mutex, MutexGuard};

use axum::body::Body;
use axum::extract::{Form, OriginalUri, Path, RawQuery, State};
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use base64::Engine as _;
use chrono::{DateTime, Duration, TimeZone, Utc};
use common::{R, family, fhir_file};
use ownhealth_core::blobs::{BlobOwner, BlobStore};
use ownhealth_core::connector_oauth::{self as oauth, ConnectorClient, Launch};
use ownhealth_core::demo::Family;
use ownhealth_core::jobs::JobContext;
use ownhealth_core::net::Egress;
use ownhealth_core::records::list_records;
use ownhealth_core::review::{self, ItemQuery, ReviewItem};
use ownhealth_core::secrets::SecretBox;
use ownhealth_core::sources::{self, RunOutcome, get_source};
use ownhealth_core::testing::TestDb;
use ownhealth_ingest_structured::smart;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use uuid::Uuid;

const CLIENT_ID: &str = "labnorte-ehr-client";
const REDIRECT: &str = "https://ownhealth.rivera.example/api/connectors/callback";
const PATIENT: &str = "sam-rivera";
const BINDER: &str = "binder-of-sam-browser-00001";
/// What the fake grants unless a test narrows it.
const ALL: &str = "openid fhirUser launch/patient offline_access patient/*.rs";
/// The grant lasts 90 days from each token answer.
const GRANT_DAYS: i64 = 90;

#[derive(Debug, Default)]
struct ProcedurePause {
    arrived: tokio::sync::Notify,
    release: tokio::sync::Notify,
}

/// The US Core searches a pull makes, as the fake records them: type and the parameters besides
/// `patient`.
fn us_core_searches() -> BTreeSet<String> {
    [
        "AllergyIntolerance",
        "Condition",
        "DiagnosticReport",
        "DocumentReference",
        "Encounter",
        "Immunization",
        "MedicationRequest?_include=MedicationRequest:medication&intent=order,plan",
        "Observation?category=laboratory",
        "Observation?category=social-history",
        "Observation?category=vital-signs",
        "Patient/sam-rivera",
        "Procedure",
    ]
    .map(String::from)
    .into()
}

fn t0() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 11, 2, 6, 0, 0)
        .single()
        .expect("a fixed instant")
}

/// The fake is plain http on loopback: only these two fields widen, the rest is the connector profile.
fn egress() -> Egress {
    Egress {
        allow_private: true,
        https_only: false,
        ..Egress::connector()
    }
}

// ------------------------------------------------------------------ the fake ----

#[derive(Debug, Clone)]
struct Behaviour {
    /// The `scope` of token answers that carry it.
    scope: String,
    omit_refresh_scope: bool,
    /// The `capabilities` discovery lists.
    capabilities: Vec<&'static str>,
    /// A search of this type (`*`: every type) answers this status.
    fail: Option<(&'static str, u16)>,
    /// The next-page links point at this origin instead of the fake's own.
    next_origin: Option<String>,
    /// Hold the first Procedure page until the test releases it.
    procedure_pause: Option<Arc<ProcedurePause>>,
    /// This search returns a response larger than the per-page limit.
    oversized_search: Option<&'static str>,
    /// Use chunked FHIR responses to exercise the byte cap while reading the body.
    stream_answers: bool,
    /// This type's search links its first page to itself as `next`.
    loop_next: Option<&'static str>,
    /// Every FHIR request is refused for its token, even with the newest one.
    refuse_all: bool,
    /// The Patient read answers this status.
    patient_status: Option<u16>,
    /// Demographics the vendor withholds from the Patient read.
    redact_patient: &'static [&'static str],
    /// Override the id in the Patient read while keeping the same demographics.
    returned_patient_id: Option<&'static str>,
    /// Replace the Patient's name using this given initial.
    given_initial: Option<&'static str>,
    patient_name: Option<&'static str>,
    /// `_include=MedicationRequest:medication` brings the named Medications (US Core); off, the
    /// server ignores it.
    include: bool,
    /// The server refuses requests with an include outside the grant.
    reject_include: bool,
    /// This type's search answers 400 "status is required" without its status parameter.
    require_status: Option<&'static str>,
    /// Condition only accepts one known clinical status per search in this mode.
    single_condition_status: bool,
    /// Bytes of filler in each Medication read answer.
    medication_padding: usize,
}

impl Default for Behaviour {
    fn default() -> Self {
        Behaviour {
            scope: ALL.into(),
            omit_refresh_scope: false,
            capabilities: vec![
                "launch-standalone",
                "client-public",
                "permission-offline",
                "permission-v2",
            ],
            fail: None,
            next_origin: None,
            procedure_pause: None,
            oversized_search: None,
            stream_answers: false,
            loop_next: None,
            refuse_all: false,
            patient_status: None,
            redact_patient: &[],
            returned_patient_id: None,
            given_initial: None,
            patient_name: None,
            include: true,
            reject_include: false,
            require_status: None,
            single_condition_status: false,
            medication_padding: 0,
        }
    }
}

#[derive(Debug, Default)]
struct Book {
    /// code -> the PKCE challenge it was issued for
    codes: HashMap<String, String>,
    /// refresh tokens the fake still accepts
    live: HashSet<String>,
    /// the one access token the FHIR API accepts now
    access: Option<String>,
    minted: usize,
    refreshes: usize,
    /// every FHIR request that got past the bearer check, as `Type?params` (no patient, no page)
    searches: Vec<String>,
    /// Every FHIR request path, including requests refused for their bearer.
    attempts: Vec<String>,
    /// FHIR requests refused for their bearer token
    unauthorized: usize,
    /// every token handed out, to look for them in the database
    handed_out: Vec<String>,
    /// resources the server gained after the launch
    extra: Vec<Value>,
    /// Extra entries a search returns on its first page, even if they do not match the query.
    extra_search_entries: Vec<(&'static str, Value)>,
    /// Fixed wire responses for individual searches, keyed as in searches.
    search_bodies: HashMap<String, (StatusCode, Vec<u8>)>,
    /// Responses that override individual Medication reads.
    medication_answers: HashMap<String, Value>,
    /// Bytes of successful Patient reads and search bundles, excluding Medication reads.
    served: usize,
}

#[derive(Clone)]
struct Shared {
    base: String,
    book: Arc<Mutex<Book>>,
    behaviour: Arc<Mutex<Behaviour>>,
    record: Arc<Vec<Value>>,
}

struct Fake {
    base: String,
    book: Arc<Mutex<Book>>,
    behaviour: Arc<Mutex<Behaviour>>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Fake {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn query_of(url: &str) -> Vec<(String, String)> {
    url::Url::parse(url)
        .map(|u| u.query_pairs().map(|(k, v)| (k.into_owned(), v.into_owned())).collect())
        .unwrap_or_default()
}

fn param(url: &str, key: &str) -> Option<String> {
    query_of(url).into_iter().find(|(k, _)| k == key).map(|(_, v)| v)
}

fn s256(verifier: &str) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

impl Fake {
    async fn start(b: Behaviour) -> anyhow::Result<Fake> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let base = format!("http://127.0.0.1:{}", listener.local_addr()?.port());
        let fixture: Value = serde_json::from_slice(&fhir_file("uscore-sam.json"))?;
        let record: Vec<Value> = fixture["entry"]
            .as_array()
            .map(|a| a.iter().map(|e| e["resource"].clone()).collect())
            .unwrap_or_default();
        let shared = Shared {
            base: base.clone(),
            book: Arc::default(),
            behaviour: Arc::new(Mutex::new(b)),
            record: Arc::new(record),
        };
        let app = Router::new()
            .route("/fhir/.well-known/smart-configuration", get(discovery))
            .route("/token", post(token))
            .route("/fhir/Patient/{id}", get(read_patient))
            .route("/fhir/Medication/{id}", get(read_medication))
            .route("/fhir/{rtype}", get(search))
            .with_state(shared.clone());
        let task = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        Ok(Fake {
            base,
            book: shared.book,
            behaviour: shared.behaviour,
            task,
        })
    }

    fn issuer(&self) -> String {
        format!("{}/fhir", self.base)
    }

    fn set(&self, f: impl FnOnce(&mut Behaviour)) {
        f(&mut lock(&self.behaviour));
    }

    fn book(&self) -> MutexGuard<'_, Book> {
        lock(&self.book)
    }

    /// Sam signs in at the portal and consents: a code bound to the URL's PKCE challenge.
    fn consent(&self, authorize_url: &str) -> String {
        let challenge = param(authorize_url, "code_challenge").unwrap_or_default();
        let code = format!("fake-code-{}", Uuid::new_v4().simple());
        self.book().codes.insert(code.clone(), challenge);
        code
    }
}

async fn discovery(State(s): State<Shared>) -> Json<Value> {
    let b = lock(&s.behaviour).clone();
    Json(json!({
        "authorization_endpoint": format!("{}/authorize", s.base),
        "token_endpoint": format!("{}/token", s.base),
        "code_challenge_methods_supported": ["S256"],
        "token_endpoint_auth_methods_supported": ["client_secret_basic"],
        "capabilities": b.capabilities,
    }))
}

fn oauth_error(status: StatusCode, code: &str) -> Response {
    (
        status,
        Json(json!({"error": code, "error_description": "refused by the fake"})),
    )
        .into_response()
}

fn mint(s: &Shared) -> Value {
    let scope = lock(&s.behaviour).scope.clone();
    let mut book = lock(&s.book);
    book.minted += 1;
    let n = book.minted;
    let tag = Uuid::new_v4().simple().to_string();
    let (access, refresh) = (format!("fake-access-{n}-{tag}"), format!("fake-refresh-{n}-{tag}"));
    book.access = Some(access.clone());
    book.live.insert(refresh.clone());
    book.handed_out.extend([access.clone(), refresh.clone()]);
    json!({
        "access_token": access, "token_type": "Bearer", "expires_in": 3600, "refresh_token": refresh,
        "refresh_token_expires_in": GRANT_DAYS * 86_400, "scope": scope, "patient": PATIENT,
    })
}

async fn token(State(s): State<Shared>, Form(form): Form<HashMap<String, String>>) -> Response {
    let field = |k: &str| form.get(k).cloned().unwrap_or_default();
    if field("client_id") != CLIENT_ID {
        return oauth_error(StatusCode::UNAUTHORIZED, "invalid_client");
    }
    match field("grant_type").as_str() {
        "authorization_code" => {
            let challenge = lock(&s.book).codes.remove(&field("code"));
            match challenge {
                Some(c) if field("redirect_uri") == REDIRECT && s256(&field("code_verifier")) == c => {
                    Json(mint(&s)).into_response()
                }
                _ => oauth_error(StatusCode::BAD_REQUEST, "invalid_grant"),
            }
        }
        "refresh_token" => {
            let alive = lock(&s.book).live.remove(&field("refresh_token"));
            if !alive {
                return oauth_error(StatusCode::BAD_REQUEST, "invalid_grant");
            }
            lock(&s.book).refreshes += 1;
            let mut answer = mint(&s);
            if lock(&s.behaviour).omit_refresh_scope {
                answer.as_object_mut().expect("token object").shift_remove("scope");
            }
            Json(answer).into_response()
        }
        _ => oauth_error(StatusCode::BAD_REQUEST, "unsupported_grant_type"),
    }
}

/// The bearer check of every FHIR request: only the newest access token reads.
fn authorized(s: &Shared, headers: &HeaderMap, uri: &Uri) -> bool {
    let given = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));
    let refuse_all = lock(&s.behaviour).refuse_all;
    let mut book = lock(&s.book);
    book.attempts.push(uri.path().into());
    let ok = !refuse_all && given.is_some() && given == book.access.as_deref();
    if !ok {
        book.unauthorized += 1;
    }
    ok
}

/// A FHIR answer; `count` adds its body to the bytes the run read before its Medication reads.
fn fhir_answer(s: &Shared, status: StatusCode, body: &Value, count: bool) -> Response {
    let bytes = serde_json::to_vec(body).unwrap_or_default();
    if count {
        lock(&s.book).served += bytes.len();
    }
    wire_answer(lock(&s.behaviour).stream_answers, status, bytes)
}

fn wire_answer(stream: bool, status: StatusCode, bytes: Vec<u8>) -> Response {
    let headers = [("content-type", "application/fhir+json")];
    if stream {
        let chunks = futures::stream::iter([Ok::<_, std::convert::Infallible>(bytes)]);
        (status, headers, Body::from_stream(chunks)).into_response()
    } else {
        (status, headers, bytes).into_response()
    }
}

fn outcome_body(text: &str) -> Value {
    json!({"resourceType": "OperationOutcome",
           "issue": [{"severity": "error", "code": "processing", "diagnostics": text}]})
}

fn outcome(status: StatusCode, text: &str) -> Response {
    let body = outcome_body(text);
    (status, [("content-type", "application/fhir+json")], Json(body)).into_response()
}

async fn read_patient(
    State(s): State<Shared>,
    Path(id): Path<String>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
) -> Response {
    if !authorized(&s, &headers, &uri) {
        return outcome(StatusCode::UNAUTHORIZED, "expired token");
    }
    lock(&s.book).searches.push(format!("Patient/{id}"));
    if let Some(status) = lock(&s.behaviour).patient_status {
        return outcome(
            StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY),
            "patient down",
        );
    }
    match s
        .record
        .iter()
        .find(|r| r["resourceType"] == "Patient" && r["id"] == id)
    {
        Some(p) => {
            let mut p = p.clone();
            if let Some(fields) = p.as_object_mut() {
                for field in lock(&s.behaviour).redact_patient {
                    fields.shift_remove(*field);
                }
            }
            if let Some(initial) = lock(&s.behaviour).given_initial {
                p["name"] = json!([{"given": [initial], "family": "Rivera"}]);
            }
            if let Some(name) = lock(&s.behaviour).patient_name {
                p["name"] = json!([{"text": name}]);
            }
            if let Some(id) = lock(&s.behaviour).returned_patient_id {
                p["id"] = json!(id);
            }
            fhir_answer(&s, StatusCode::OK, &p, true)
        }
        None => outcome(StatusCode::NOT_FOUND, "no such patient"),
    }
}

/// A Medication the server holds apart, named by a MedicationRequest's `medicationReference`.
async fn read_medication(
    State(s): State<Shared>,
    Path(id): Path<String>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
) -> Response {
    if !authorized(&s, &headers, &uri) {
        return outcome(StatusCode::UNAUTHORIZED, "expired token");
    }
    let answer = {
        let mut book = lock(&s.book);
        book.searches.push(format!("Medication/{id}"));
        book.medication_answers.get(&id).cloned()
    };
    if let Some(answer) = answer {
        return fhir_answer(&s, StatusCode::OK, &answer, false);
    }
    let extra = lock(&s.book).extra.clone();
    let padding = lock(&s.behaviour).medication_padding;
    match extra
        .iter()
        .find(|r| r["resourceType"] == "Medication" && r["id"] == id)
    {
        Some(m) => {
            let mut m = m.clone();
            if padding > 0 {
                m["text"] = json!({"status": "generated", "div": "x".repeat(padding)});
            }
            fhir_answer(&s, StatusCode::OK, &m, false)
        }
        None => outcome(StatusCode::NOT_FOUND, "no such medication"),
    }
}

fn has_code(r: &Value, field: &str, token: &str) -> bool {
    let (system, code) = token.split_once('|').map_or((None, token), |(s, c)| (Some(s), c));
    r[field].as_array().is_some_and(|cats| {
        cats.iter().any(|c| {
            c["coding"].as_array().is_some_and(|cs| {
                cs.iter()
                    .any(|x| x["code"] == code && system.is_none_or(|s| x["system"] == s))
            })
        })
    })
}

/// A US Core search: `patient` required, `category` required for Observation and `intent` for
/// MedicationRequest, one resource per page with a `next` link, a fresh envelope every time, and a
/// warning OperationOutcome in the Condition answer.
async fn search(
    State(s): State<Shared>,
    Path(rtype): Path<String>,
    RawQuery(raw): RawQuery,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
) -> Response {
    if !authorized(&s, &headers, &uri) {
        return outcome(StatusCode::UNAUTHORIZED, "expired token");
    }
    let raw = raw.unwrap_or_default();
    let pairs: Vec<(String, String)> = url::form_urlencoded::parse(raw.as_bytes())
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    let get = |k: &str| pairs.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());
    let page: usize = get("page").and_then(|p| p.parse().ok()).unwrap_or(1);
    let b = lock(&s.behaviour).clone();
    if page == 1 {
        let mut said: Vec<String> = pairs
            .iter()
            .filter(|(k, _)| k != "patient" && k != "page")
            .map(|(k, v)| format!("{k}={v}"))
            .collect();
        said.sort();
        let entry = if said.is_empty() {
            rtype.clone()
        } else {
            format!("{rtype}?{}", said.join("&"))
        };
        let fixed = {
            let mut book = lock(&s.book);
            let fixed = book.search_bodies.get(&entry).cloned();
            book.searches.push(entry);
            fixed
        };
        if let Some((status, bytes)) = fixed {
            return wire_answer(b.stream_answers, status, bytes);
        }
    }
    if b.reject_include && get("_include").is_some() {
        return outcome(StatusCode::FORBIDDEN, "include outside the grant");
    }
    if let Some((t, status)) = b.fail
        && (t == rtype || t == "*")
    {
        return outcome(StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY), "down");
    }
    if b.oversized_search == Some(rtype.as_str()) {
        return wire_answer(b.stream_answers, StatusCode::OK, vec![b' '; smart::PAGE_BYTES + 1]);
    }
    if rtype == "Procedure"
        && page == 1
        && let Some(pause) = b.procedure_pause
    {
        pause.arrived.notify_one();
        pause.release.notified().await;
    }
    if get("patient").as_deref() != Some(PATIENT) {
        return outcome(StatusCode::FORBIDDEN, "another patient");
    }
    let category = get("category");
    let intents: Vec<String> = get("intent")
        .map(|i| i.split(',').map(String::from).collect())
        .unwrap_or_default();
    match rtype.as_str() {
        "Observation" if category.is_none() => return outcome(StatusCode::BAD_REQUEST, "category is required"),
        "MedicationRequest" if intents.is_empty() => return outcome(StatusCode::BAD_REQUEST, "intent is required"),
        t if b.require_status == Some(t) && get("status").is_none() && get("clinical-status").is_none() => {
            return outcome(
                StatusCode::BAD_REQUEST,
                "the status parameter is required for this search",
            );
        }
        _ => {}
    }
    let clinical_status = get("clinical-status");
    if b.single_condition_status
        && rtype == "Condition"
        && !matches!(
            clinical_status.as_deref(),
            Some("active" | "recurrence" | "relapse" | "inactive" | "remission" | "resolved")
        )
    {
        return outcome(StatusCode::BAD_REQUEST, "one clinical-status value is required");
    }
    let extra = lock(&s.book).extra.clone();
    let hits: Vec<Value> = s
        .record
        .iter()
        .chain(extra.iter())
        .filter(|r| r["resourceType"] == rtype.as_str())
        .filter(|r| rtype != "Condition" || category.as_deref().is_none_or(|c| has_code(r, "category", c)))
        .filter(|r| match rtype.as_str() {
            "Observation" => has_code(r, "category", category.as_deref().unwrap_or_default()),
            "MedicationRequest" => intents.iter().any(|i| r["intent"] == i.as_str()),
            "Condition" if b.single_condition_status => {
                r["clinicalStatus"]["coding"][0]["code"].as_str() == clinical_status.as_deref()
            }
            _ => true,
        })
        .cloned()
        .collect();
    let mut entries: Vec<Value> = hits
        .iter()
        .skip(page - 1)
        .take(1)
        .map(|r| {
            let url = format!("{}/fhir/{rtype}/{}", s.base, r["id"].as_str().unwrap_or_default());
            json!({"fullUrl": url, "resource": r, "search": {"mode": "match"}})
        })
        .collect();
    if rtype == "MedicationRequest" && b.include && get("_include").as_deref() == Some("MedicationRequest:medication") {
        let named: Vec<String> = entries
            .iter()
            .filter_map(|e| e["resource"]["medicationReference"]["reference"].as_str())
            .filter_map(|r| r.strip_prefix("Medication/").map(String::from))
            .collect();
        for m in extra
            .iter()
            .filter(|m| m["resourceType"] == "Medication" && named.iter().any(|n| m["id"] == n.as_str()))
        {
            let url = format!("{}/fhir/Medication/{}", s.base, m["id"].as_str().unwrap_or_default());
            entries.push(json!({"fullUrl": url, "resource": m, "search": {"mode": "include"}}));
        }
    }
    if page == 1 {
        entries.extend(
            lock(&s.book)
                .extra_search_entries
                .iter()
                .filter(|(search, _)| *search == rtype)
                .map(|(_, entry)| entry.clone()),
        );
    }
    if rtype == "Condition" && page == 1 {
        entries.push(json!({"resource": {"resourceType": "OperationOutcome", "issue": [{"severity": "warning",
            "code": "informational", "diagnostics": "some conditions are restricted"}]}, "search": {"mode": "outcome"}}));
    }
    let own = format!("{}/fhir/{rtype}?{raw}", s.base);
    let mut links = vec![json!({"relation": "self", "url": own})];
    if b.loop_next == Some(rtype.as_str()) {
        links.push(json!({"relation": "next", "url": own}));
    } else if hits.len() > page {
        let kept: Vec<String> = pairs
            .iter()
            .filter(|(k, _)| k != "page")
            .map(|(k, v)| {
                format!(
                    "{k}={}",
                    url::form_urlencoded::byte_serialize(v.as_bytes()).collect::<String>()
                )
            })
            .collect();
        let origin = b.next_origin.clone().unwrap_or_else(|| s.base.clone());
        links.push(json!({"relation": "next",
            "url": format!("{origin}/fhir/{rtype}?{}&page={}", kept.join("&"), page + 1)}));
    }
    let body = json!({"resourceType": "Bundle", "id": Uuid::new_v4().to_string(), "type": "searchset",
        "meta": {"lastUpdated": Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)}, "total": hits.len(), "link": links, "entry": entries});
    fhir_answer(&s, StatusCode::OK, &body, true)
}

// ---------------------------------------------------------------- the world ----

struct World {
    db: TestDb,
    _dir: tempfile::TempDir,
    ctx: JobContext,
    sb: SecretBox,
    fam: Family,
    fake: Fake,
    client: ConnectorClient,
}

async fn world(b: Behaviour) -> anyhow::Result<World> {
    ownhealth_ingest_structured::register();
    let db = TestDb::new().await?;
    let dir = tempfile::tempdir()?;
    let settings = Arc::new(db.settings(dir.path())?);
    let blobs = BlobStore::for_settings(&settings)?;
    let sb = SecretBox::from_settings(&settings)?.with_keys(blobs.keyring());
    let ctx = JobContext::new(db.pool.clone(), settings, blobs);
    let mut c = db.pool.acquire().await?;
    let fam = family(&mut c).await?;
    let fake = Fake::start(b).await?;
    // a public client registered with the hospital's server (the wizard of slice 19 writes this row)
    let id: Uuid = sqlx::query_scalar(
        "insert into connector_client (household_id, vendor, issuer, client_id, redirect_uri) \
         values ($1, 'labnorte-ehr', $2, $3, $4) returning id",
    )
    .bind(fam.household_id)
    .bind(fake.issuer())
    .bind(CLIENT_ID)
    .bind(REDIRECT)
    .fetch_one(&mut *c)
    .await?;
    let client = oauth::load_client(&mut c, Some(&sb), fam.household_id, id)
        .await?
        .expect("the client");
    drop(c);
    Ok(World {
        db,
        _dir: dir,
        ctx,
        sb,
        fam,
        fake,
        client,
    })
}

impl World {
    fn sam(&self) -> Uuid {
        self.fam.profile("sam")
    }

    /// The launch the route starts: discovery, then the authorize URL.
    async fn launch(&self) -> anyhow::Result<oauth::Begun> {
        self.launch_for("sam").await
    }

    /// [`World::launch`] for another profile of the family, started by Alex.
    async fn launch_for(&self, slug: &str) -> anyhow::Result<oauth::Begun> {
        self.launch_for_source(slug, None).await
    }

    async fn launch_for_source(&self, slug: &str, source_id: Option<Uuid>) -> anyhow::Result<oauth::Begun> {
        let spec = oauth::kind_spec("smart").expect("smart is served");
        let endpoints = oauth::resolve(&spec, &self.fake.issuer(), &egress()).await?;
        let mut c = self.db.pool.acquire().await?;
        Ok(oauth::begin(
            &mut c,
            &Launch {
                kind: "smart",
                client: &self.client,
                endpoints: &endpoints,
                profile_id: self.fam.profile(slug),
                label: "Hospital LabNorte",
                source_id,
                user_id: Some(self.fam.user(if slug == "sam" { "sam" } else { "alex" })),
                session_id: None,
                binder: BINDER,
            },
        )
        .await?)
    }

    /// Launch, consent at the fake, call back and save, as the callback route does: a connected source.
    async fn connect(&self, now: DateTime<Utc>) -> anyhow::Result<Uuid> {
        self.connect_for("sam", now).await
    }

    /// [`World::connect`] of the portal account the fake serves (Sam's) to another profile.
    async fn connect_for(&self, slug: &str, now: DateTime<Utc>) -> anyhow::Result<Uuid> {
        let begun = self.launch_for(slug).await?;
        self.save_consent(begun, now).await
    }

    async fn save_consent(&self, begun: oauth::Begun, now: DateTime<Utc>) -> anyhow::Result<Uuid> {
        let code = self.fake.consent(&begun.url);
        let mut c = self.db.pool.acquire().await?;
        let pending = oauth::take_state(&mut c, &begun.state, BINDER).await?;
        drop(c);
        let tokens = oauth::exchange(&pending, &self.client, &code, &egress()).await?;
        let mut tx = self.db.pool.begin().await?;
        let id = oauth::save_connection(&mut tx, &self.sb, &pending, &self.client, &tokens, now).await?;
        tx.commit().await?;
        oauth::remember(id, &tokens, now);
        Ok(id)
    }

    /// One scheduled pull at `now`, recorded as the `source.poll` job records it.
    async fn day(&self, source: Uuid, now: DateTime<Utc>) -> anyhow::Result<Result<RunOutcome, String>> {
        self.day_with(source, now, &smart::Caps::default()).await
    }

    /// [`World::day`] with the run's limits given.
    async fn day_with(
        &self,
        source: Uuid,
        now: DateTime<Utc>,
        caps: &smart::Caps,
    ) -> anyhow::Result<Result<RunOutcome, String>> {
        let mut c = self.db.pool.acquire().await?;
        let s = get_source(&mut c, source).await?.expect("the source");
        let run = sources::begin_run(&mut c, source, "poll", None).await?;
        drop(c);
        let got = smart::pull_with(&self.ctx, &s, &egress(), now, caps).await;
        let mut c = self.db.pool.acquire().await?;
        match &got {
            Ok(o) => sources::finish_run(&mut c, run, Ok(o)).await?,
            Err(e) => sources::finish_run(&mut c, run, Err(&e.text())).await?,
        }
        Ok(got.map_err(|e| e.text()))
    }

    async fn searched_since(&self, from: usize) -> BTreeSet<String> {
        self.fake.book().searches[from..].iter().cloned().collect()
    }

    async fn reconsent(&self, source: Uuid) -> anyhow::Result<Option<DateTime<Utc>>> {
        Ok(
            sqlx::query_scalar("select reconsent_needed_at from source where id = $1")
                .bind(source)
                .fetch_one(&self.db.pool)
                .await?,
        )
    }

    async fn grant_ends(&self, source: Uuid) -> anyhow::Result<Option<DateTime<Utc>>> {
        Ok(sqlx::query_scalar("select grant_expires_at from source where id = $1")
            .bind(source)
            .fetch_one(&self.db.pool)
            .await?)
    }

    async fn documents(&self, source: Uuid) -> anyhow::Result<Vec<(Uuid, String)>> {
        Ok(sqlx::query_as(
            "select id, encode(blob_sha256, 'hex') from document where source_id = $1 order by created_at, id",
        )
        .bind(source)
        .fetch_all(&self.db.pool)
        .await?)
    }

    async fn stored_bundle(&self, source: Uuid) -> anyhow::Result<Value> {
        let docs = self.documents(source).await?;
        anyhow::ensure!(docs.len() == 1, "expected one stored bundle, found {}", docs.len());
        let bytes = self.ctx.blobs.get_bytes(BlobOwner::Profile(self.sam()), &docs[0].1)?;
        Ok(serde_json::from_slice(&bytes)?)
    }

    async fn open_items(&self) -> anyhow::Result<Vec<ReviewItem>> {
        self.open_items_of(self.sam()).await
    }

    async fn open_items_of(&self, profile: Uuid) -> anyhow::Result<Vec<ReviewItem>> {
        let mut c = self.db.pool.acquire().await?;
        let mut rows = review::list_items(
            &mut c,
            &ItemQuery {
                profile_ids: Some(vec![profile]),
                status: Some("open".into()),
                ..ItemQuery::default()
            },
        )
        .await?;
        rows.retain(|r| r.profile_id == Some(profile));
        Ok(rows)
    }

    fn medication_reads(&self) -> usize {
        self.fake
            .book()
            .searches
            .iter()
            .filter(|s| s.starts_with("Medication/"))
            .count()
    }

    async fn status_of_last_run(&self, source: Uuid) -> anyhow::Result<String> {
        Ok(
            sqlx::query_scalar("select status from source_run where source_id = $1 order by id desc limit 1")
                .bind(source)
                .fetch_one(&self.db.pool)
                .await?,
        )
    }

    /// Whether another backend of this test's database waits on a lock, within 20 s.
    async fn someone_waits(&self) -> anyhow::Result<bool> {
        for _ in 0..400 {
            let n: i64 = sqlx::query_scalar(
                "select count(*) from pg_locks l join pg_stat_activity a on a.pid = l.pid \
                 where not l.granted and a.datname = current_database()",
            )
            .fetch_one(&self.db.pool)
            .await?;
            if n > 0 {
                return Ok(true);
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        Ok(false)
    }

    async fn count(&self, table: &str) -> anyhow::Result<usize> {
        let mut c = self.db.pool.acquire().await?;
        Ok(list_records(&mut c, table, self.sam(), None, true).await?.len())
    }
}

fn kinds(items: &[ReviewItem]) -> BTreeSet<(String, String)> {
    items
        .iter()
        .map(|i| {
            (
                i.kind.clone(),
                i.payload["resource_type"].as_str().unwrap_or_default().to_string(),
            )
        })
        .collect()
}

fn pair(kind: &str, rtype: &str) -> (String, String) {
    (kind.into(), rtype.into())
}

// ----------------------------------------------------------------- the tests ----

#[tokio::test]
async fn the_launch_asks_for_a_standalone_patient_grant_with_the_us_core_scopes() -> R {
    let w = world(Behaviour::default()).await?;
    let url = w.launch().await?.url;
    let scope: BTreeSet<String> = param(&url, "scope")
        .unwrap_or_default()
        .split_whitespace()
        .map(String::from)
        .collect();
    for s in ["launch/patient", "offline_access", "openid", "fhirUser"] {
        assert!(scope.contains(s), "{s} missing from {scope:?}");
    }
    for t in smart::US_CORE_TYPES {
        assert!(scope.contains(&format!("patient/{t}.rs")), "{t} missing from {scope:?}");
    }
    assert!(scope.contains("patient/Medication.rs"), "{scope:?}");
    assert!(
        scope.iter().all(|s| !s.starts_with("patient/") || s.ends_with(".rs")),
        "read and search only: {scope:?}"
    );
    assert_eq!(
        param(&url, "aud"),
        Some(w.fake.issuer()),
        "the token is for this FHIR server"
    );

    // a server that speaks SMART v1 scopes only is asked for `.read`; so is a SMART 1.0 server,
    // which lists neither marker (they came with 2.0); one that lists both gets v2
    for (caps, v2) in [
        (vec!["launch-standalone", "permission-offline", "permission-v1"], false),
        (
            vec![
                "launch-standalone",
                "client-confidential-symmetric",
                "permission-offline",
                "permission-patient",
            ],
            false,
        ),
        (
            vec![
                "launch-standalone",
                "permission-offline",
                "permission-v1",
                "permission-v2",
            ],
            true,
        ),
    ] {
        w.fake.set(|b| b.capabilities = caps.clone());
        let got = param(&w.launch().await?.url, "scope").unwrap_or_default();
        assert_eq!(got.contains("patient/Observation.rs"), v2, "{caps:?}: {got}");
        assert_eq!(got.contains("patient/Observation.read"), !v2, "{caps:?}: {got}");
        assert_eq!(got.contains(".rs"), v2, "{caps:?}: {got}");
        assert!(
            got.contains("launch/patient") && got.contains("offline_access"),
            "{got}"
        );
    }
    Ok(())
}

#[tokio::test]
async fn a_launch_imports_the_record_and_thirty_daily_pulls_rotate_without_new_consent() -> R {
    let w = world(Behaviour::default()).await?;
    let source = w.connect(t0()).await?;
    assert!(sources::poller_for("smart").is_some(), "the smart kind has a poller");
    assert_eq!(
        w.grant_ends(source).await?,
        Some(t0() + Duration::days(GRANT_DAYS)),
        "the grant's end comes from the token answer"
    );

    // the first pull, minutes after the callback: the access token of the launch is still good
    let first = w
        .day(source, t0() + Duration::minutes(2))
        .await?
        .map_err(anyhow::Error::msg)?;
    assert_eq!(w.fake.book().refreshes, 0);
    assert_eq!(w.searched_since(0).await, us_core_searches());
    // weight, TSH, two medicines, one condition; the Patient is context; the warning is no resource
    assert_eq!((first.seen, first.new, first.duplicate), (5, 5, 0), "{first:?}");
    assert!(first.problems.is_empty(), "{first:?}");
    assert_eq!(first.skipped.get("context"), Some(&1), "{first:?}");
    assert_eq!(first.skipped.get("outcome"), Some(&1), "{first:?}");
    assert_eq!(w.count("medication").await?, 2);
    assert_eq!(w.count("condition").await?, 1);
    assert_eq!(
        kinds(&w.open_items().await?),
        [pair("candidate_row", "Observation")].into(),
        "the TSH without a UCUM unit waits in review"
    );
    // one bundle, stored first as the profile's own file and through the smart channel
    let docs = w.documents(source).await?;
    assert_eq!(docs.len(), 1, "{docs:?}");
    assert!(
        w.ctx.blobs.exists(BlobOwner::Profile(w.sam()), &docs[0].1),
        "the bundle is a blob of Sam's profile"
    );
    // a second file on one day reads apart from the first: the name carries the time
    let name: String = sqlx::query_scalar("select filename from document where id = $1")
        .bind(docs[0].0)
        .fetch_one(&w.db.pool)
        .await?;
    assert_eq!(name, "Hospital LabNorte · 2026-11-02 06.02 UTC.json");
    let channel: Vec<String> = sqlx::query_scalar("select channel from document_source where document_id = $1")
        .bind(docs[0].0)
        .fetch_all(&w.db.pool)
        .await?;
    assert!(
        channel.iter().all(|c| c == "smart") && !channel.is_empty(),
        "{channel:?}"
    );
    let bundle: Value = serde_json::from_slice(&w.ctx.blobs.get_bytes(BlobOwner::Profile(w.sam()), &docs[0].1)?)?;
    let types: BTreeMap<String, usize> = bundle["entry"]
        .as_array()
        .map(|a| {
            a.iter().fold(BTreeMap::new(), |mut m, e| {
                *m.entry(e["resource"]["resourceType"].as_str().unwrap_or("?").to_string())
                    .or_default() += 1;
                m
            })
        })
        .unwrap_or_default();
    assert_eq!(
        types,
        [
            ("Condition", 1),
            ("MedicationRequest", 2),
            ("Observation", 2),
            ("Patient", 1)
        ]
        .map(|(k, v)| (k.to_string(), v))
        .into(),
        "every page of every search, and no OperationOutcome"
    );

    // thirty days, one pull each: every access token has run out, so each pull refreshes and the
    // server rotates the refresh token. On day 15 the hospital adds a note and a new weight.
    let items_before = w.open_items().await?.len();
    for d in 1..=30 {
        let now = t0() + Duration::days(d) + Duration::minutes(2);
        if d == 15 {
            w.fake.book().extra.extend([
                json!({"resourceType": "Observation", "id": "sam-weight-2", "status": "final",
                       "category": [{"coding": [{"system": "http://terminology.hl7.org/CodeSystem/observation-category", "code": "vital-signs"}]}],
                       "code": {"coding": [{"system": "http://loinc.org", "code": "29463-7", "display": "Body weight"}]},
                       "subject": {"reference": "Patient/sam-rivera"}, "effectiveDateTime": "2026-11-16T07:30:00-05:00",
                       "valueQuantity": {"value": 61.0, "unit": "kg", "system": "http://unitsofmeasure.org", "code": "kg"}}),
                json!({"resourceType": "DocumentReference", "id": "sam-note-1", "status": "current",
                       "type": {"coding": [{"system": "http://loinc.org", "code": "11506-3", "display": "Progress note"}]},
                       "subject": {"reference": "Patient/sam-rivera"}, "date": "2026-11-16T09:00:00-05:00",
                       "content": [{"attachment": {"contentType": "text/plain", "data": "Tm90YSBkZSBzZWd1aW1pZW50by4="}}]}),
            ]);
        }
        let got = w.day(source, now).await?.map_err(anyhow::Error::msg)?;
        assert!(got.problems.is_empty(), "day {d}: {got:?}");
        match d {
            15 => assert_eq!((got.new, got.duplicate), (2, 5), "day {d}: {got:?}"),
            _ if d < 15 => assert_eq!((got.new, got.duplicate), (0, 5), "day {d}: {got:?}"),
            _ => assert_eq!((got.new, got.duplicate), (0, 7), "day {d}: {got:?}"),
        }
        assert_eq!(w.reconsent(source).await?, None, "day {d}");
        assert_eq!(
            w.grant_ends(source).await?,
            Some(now + Duration::days(GRANT_DAYS)),
            "day {d}: the rotated grant's end"
        );
    }
    let handed: Vec<String> = {
        let book = w.fake.book();
        assert_eq!(book.refreshes, 30, "one refresh a day");
        assert_eq!(book.live.len(), 1, "every older refresh token was used up once");
        assert_eq!(book.unauthorized, 0, "no request with an old access token");
        book.handed_out.clone()
    };
    // the same answers on 29 of 30 days: no new file for them, and one more for day 15
    assert_eq!(w.documents(source).await?.len(), 2);
    assert_eq!(
        kinds(&w.open_items().await?),
        [
            pair("candidate_row", "Observation"),
            pair("fhir_unmapped_summary", "DocumentReference")
        ]
        .into(),
    );
    assert_eq!(w.open_items().await?.len(), items_before + 1);
    let runs: Vec<String> = sqlx::query_scalar("select status from source_run where source_id = $1")
        .bind(source)
        .fetch_all(&w.db.pool)
        .await?;
    assert_eq!(runs.len(), 31);
    assert!(runs.iter().all(|s| s == "ok"), "{runs:?}");
    // no token in a run row, the source row or a document flag
    let stored: String = sqlx::query_scalar(
        "select coalesce((select string_agg(r::text, ' ') from source_run r where source_id = $1), '') || \
         (select s::text from source s where id = $1) || \
         coalesce((select string_agg(flags::text, ' ') from document where source_id = $1), '')",
    )
    .bind(source)
    .fetch_one(&w.db.pool)
    .await?;
    for t in handed {
        assert!(!stored.contains(&t), "a token reached the database in clear");
    }
    Ok(())
}

#[tokio::test]
async fn a_second_run_on_the_same_day_imports_nothing() -> R {
    let w = world(Behaviour::default()).await?;
    let source = w.connect(t0()).await?;
    let first = w
        .day(source, t0() + Duration::minutes(1))
        .await?
        .map_err(anyhow::Error::msg)?;
    let records = (w.count("medication").await?, w.count("condition").await?);
    let items = w.open_items().await?.len();
    let again = w
        .day(source, t0() + Duration::minutes(5))
        .await?
        .map_err(anyhow::Error::msg)?;
    assert_eq!((first.new, again.new, again.duplicate), (5, 0, 5), "{again:?}");
    assert_eq!((w.count("medication").await?, w.count("condition").await?), records);
    assert_eq!(w.open_items().await?.len(), items);
    assert_eq!(w.documents(source).await?.len(), 1, "the same bundle, the same file");
    Ok(())
}

#[tokio::test]
async fn an_access_token_the_server_refuses_is_refreshed_once() -> R {
    let w = world(Behaviour::default()).await?;
    let source = w.connect(t0()).await?;
    // the server forgets the launch's access token before it expires here
    w.fake.book().access = Some("nobody-holds-this".into());
    let got = w
        .day(source, t0() + Duration::minutes(1))
        .await?
        .map_err(anyhow::Error::msg)?;
    assert_eq!(got.new, 5, "{got:?}");
    let book = w.fake.book();
    assert_eq!((book.unauthorized, book.refreshes), (1, 1));
    Ok(())
}

#[tokio::test]
async fn a_refused_refresh_stops_the_pull_and_asks_for_a_new_sign_in() -> R {
    let w = world(Behaviour::default()).await?;
    let source = w.connect(t0()).await?;
    w.day(source, t0() + Duration::minutes(1))
        .await?
        .map_err(anyhow::Error::msg)?;
    let before = w.fake.book().attempts.len();
    // Sam removed the app at the portal: every refresh token is dead
    w.fake.book().live.clear();
    let now = t0() + Duration::days(1);
    let got = w.day(source, now).await?;
    let e = got.expect_err("the pull stops");
    assert!(e.contains("reconsent_needed"), "{e}");
    assert_eq!(w.reconsent(source).await?, Some(now));
    assert_eq!(w.fake.book().attempts.len(), before, "no request without a token");
    let health: (Option<String>, i32) =
        sqlx::query_as("select last_error, consecutive_failures from source where id = $1")
            .bind(source)
            .fetch_one(&w.db.pool)
            .await?;
    assert_eq!(health.1, 1);
    assert!(
        health.0.is_some_and(|m| m.contains("reconsent_needed")),
        "the Connections screen reads why"
    );
    Ok(())
}

#[tokio::test]
async fn the_pull_reads_only_the_types_the_grant_covers() -> R {
    // Sam left notes and procedures out at the consent screen
    let narrow = [
        "launch/patient",
        "offline_access",
        "patient/Patient.rs",
        "patient/Observation.rs",
        "patient/MedicationRequest.rs",
        "patient/Medication.rs",
        "patient/Condition.rs",
        "patient/AllergyIntolerance.rs",
        "patient/Immunization.rs",
        "patient/Encounter.rs",
        "patient/DiagnosticReport.rs",
    ]
    .join(" ");
    let w = world(Behaviour {
        scope: narrow,
        ..Behaviour::default()
    })
    .await?;
    let source = w.connect(t0()).await?;
    let got = w
        .day(source, t0() + Duration::minutes(1))
        .await?
        .map_err(anyhow::Error::msg)?;
    assert!(got.problems.is_empty(), "{got:?}");
    assert_eq!(got.skipped.get("not_granted"), Some(&2), "{got:?}");
    let searched = w.searched_since(0).await;
    assert!(
        !searched.contains("DocumentReference") && !searched.contains("Procedure"),
        "{searched:?}"
    );
    assert!(searched.contains("Condition"), "{searched:?}");
    Ok(())
}

#[tokio::test]
async fn a_failed_search_is_a_problem_and_the_rest_still_arrives() -> R {
    let w = world(Behaviour {
        fail: Some(("MedicationRequest", 503)),
        ..Behaviour::default()
    })
    .await?;
    let source = w.connect(t0()).await?;
    let got = w
        .day(source, t0() + Duration::minutes(1))
        .await?
        .map_err(anyhow::Error::msg)?;
    assert_eq!(got.new, 3, "weight, TSH and the condition: {got:?}");
    let codes: Vec<(&str, &str)> = got
        .problems
        .iter()
        .map(|p| (p.code.as_str(), p.message.as_str()))
        .collect();
    assert_eq!(codes.len(), 1, "{codes:?}");
    assert_eq!(codes[0].0, "search_failed");
    assert!(
        codes[0].1.contains("MedicationRequest") && codes[0].1.contains("503"),
        "{codes:?}"
    );
    assert_eq!(w.count("condition").await?, 1);
    let status: String = sqlx::query_scalar("select status from source_run where source_id = $1")
        .bind(source)
        .fetch_one(&w.db.pool)
        .await?;
    assert_eq!(status, "partial");
    // the next day the server is back: the medicines come in, nothing else again
    w.fake.set(|b| b.fail = None);
    let next = w
        .day(source, t0() + Duration::days(1))
        .await?
        .map_err(anyhow::Error::msg)?;
    assert_eq!((next.new, next.duplicate), (2, 3), "{next:?}");
    Ok(())
}

#[tokio::test]
async fn a_next_page_on_another_host_is_not_read() -> R {
    // the other host is a second local listener (another port is another host): it must hear nothing
    let elsewhere = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let origin = format!("http://127.0.0.1:{}", elsewhere.local_addr()?.port());
    let heard = Arc::new(Mutex::new(0usize));
    let ears = heard.clone();
    let listening = tokio::spawn(async move {
        while let Ok((socket, _)) = elsewhere.accept().await {
            *lock(&ears) += 1;
            drop(socket);
        }
    });
    let w = world(Behaviour {
        next_origin: Some(origin),
        ..Behaviour::default()
    })
    .await?;
    let source = w.connect(t0()).await?;
    let got = w
        .day(source, t0() + Duration::minutes(1))
        .await?
        .map_err(anyhow::Error::msg)?;
    // the second medicine sits on page 2, behind a link to a host the connection never declared
    assert_eq!(w.count("medication").await?, 1);
    let undeclared: Vec<&str> = got
        .problems
        .iter()
        .filter(|p| p.code == "undeclared_host")
        .map(|p| p.message.as_str())
        .collect();
    assert_eq!(undeclared.len(), 1, "{:?}", got.problems);
    assert!(undeclared[0].contains("MedicationRequest"), "{undeclared:?}");
    assert_eq!(*lock(&heard), 0, "the undeclared host was asked");
    listening.abort();
    Ok(())
}

#[tokio::test]
async fn a_medicine_named_by_reference_is_read_once_and_imported_with_its_order() -> R {
    // a server that ignores `_include`: the named medicine is read on its own
    let w = world(Behaviour {
        include: false,
        ..Behaviour::default()
    })
    .await?;
    w.fake.book().extra.extend([
        json!({"resourceType": "MedicationRequest", "id": "sam-metf", "status": "active", "intent": "plan",
               "medicationReference": {"reference": "Medication/metformin-850"},
               "subject": {"reference": "Patient/sam-rivera"}, "authoredOn": "2026-09-01",
               "dosageInstruction": [{"text": "1 tablet with dinner"}]}),
        // the Medication is not in the patient compartment, so no search returns it: it is read
        json!({"resourceType": "Medication", "id": "metformin-850", "meta": {"versionId": "3"},
               "code": {"coding": [{"system": "http://www.nlm.nih.gov/research/umls/rxnorm", "code": "861004",
                                    "display": "Metformin 850 mg"}]}}),
    ]);
    let source = w.connect(t0()).await?;
    let first = w
        .day(source, t0() + Duration::minutes(1))
        .await?
        .map_err(anyhow::Error::msg)?;
    assert!(first.problems.is_empty(), "{first:?}");
    assert!(
        w.searched_since(0).await.contains("Medication/metformin-850"),
        "the named medicine is read"
    );
    let mut c = w.db.pool.acquire().await?;
    let meds = list_records(&mut c, "medication", w.sam(), None, true).await?;
    drop(c);
    assert!(
        meds.iter().any(|m| m["name"] == "Metformin 850 mg"),
        "the order takes its medicine's name: {meds:?}"
    );
    let reads = |b: &Book| b.searches.iter().filter(|s| s.starts_with("Medication/")).count();
    assert_eq!(reads(&w.fake.book()), 1);
    let again = w
        .day(source, t0() + Duration::minutes(5))
        .await?
        .map_err(anyhow::Error::msg)?;
    assert_eq!(again.new, 0, "{again:?}");
    Ok(())
}

// ------------------------------------------------------ review round 1 ----

/// Two orders that name a Medication each, which the server keeps apart from the patient.
fn two_named_medicines() -> Vec<Value> {
    let order = |id: &str, med: &str| {
        json!({"resourceType": "MedicationRequest", "id": id, "status": "active", "intent": "plan",
               "medicationReference": {"reference": format!("Medication/{med}")},
               "subject": {"reference": "Patient/sam-rivera"}, "authoredOn": "2026-09-01"})
    };
    let med = |id: &str, code: &str, name: &str| {
        json!({"resourceType": "Medication", "id": id, "meta": {"versionId": "1"},
               "code": {"coding": [{"system": "http://www.nlm.nih.gov/research/umls/rxnorm", "code": code, "display": name}]}})
    };
    vec![
        order("sam-metf", "metformin-850"),
        order("sam-ator", "atorvastatin-20"),
        med("metformin-850", "861004", "Metformin 850 mg"),
        med("atorvastatin-20", "617310", "Atorvastatin 20 mg"),
    ]
}

#[tokio::test]
async fn a_pull_that_began_before_a_disconnect_imports_nothing() -> R {
    let pause = Arc::new(ProcedurePause::default());
    let w = world(Behaviour {
        procedure_pause: Some(pause.clone()),
        ..Behaviour::default()
    })
    .await?;
    let source = w.connect(t0()).await?;
    let pull = w.day(source, t0() + Duration::minutes(1));
    let cut = async {
        tokio::time::timeout(std::time::Duration::from_secs(30), pause.arrived.notified()).await?;
        assert_eq!(
            w.fake.book().searches.last().map(String::as_str),
            Some("Procedure"),
            "disconnect must land while the fake holds Procedure"
        );
        oauth::disconnect(&w.db.pool, &w.sb, source, &egress()).await?;
        pause.release.notify_one();
        anyhow::Ok(())
    };
    let (got, ()) = tokio::try_join!(pull, cut)?;
    let e = got.expect_err("the pull stops");
    assert!(e.contains("switched_off"), "{e}");
    assert_eq!(w.documents(source).await?.len(), 0);
    assert_eq!(w.count("condition").await?, 0);
    assert_eq!(w.open_items().await?.len(), 0);
    Ok(())
}

#[tokio::test]
async fn a_disconnect_that_lands_while_the_import_waits_for_the_source_is_seen_by_it() -> R {
    let owned = world(Behaviour::default()).await?;
    let w = &owned;
    let source = w.connect(t0()).await?;
    let mut c = w.db.pool.acquire().await?;
    let s = get_source(&mut c, source).await?.expect("the source");
    drop(c);
    // a disconnect holds the source row: the pull's first look passes, its import waits on the row
    // (the pull alone, without the run bookkeeping, which would wait on the row before it starts)
    let mut held = w.db.pool.begin().await?;
    sqlx::query("select 1 from source where id = $1 for update")
        .bind(source)
        .execute(&mut *held)
        .await?;
    let pull = async {
        smart::pull(&w.ctx, &s, &egress(), t0() + Duration::minutes(1))
            .await
            .map_err(|e| e.text())
    };
    let cut = async move {
        let waited = w.someone_waits().await?;
        sqlx::query("update source set enabled = false where id = $1")
            .bind(source)
            .execute(&mut *held)
            .await?;
        held.commit().await?;
        anyhow::Ok(waited)
    };
    let blobs_before = w.ctx.blobs.files()?;
    let (got, waited) =
        tokio::time::timeout(std::time::Duration::from_secs(30), async { tokio::join!(pull, cut) }).await?;
    assert!(waited?, "the import never waited on the source row");
    let e = got.expect_err("the import sees the disconnect");
    assert!(e.contains("switched_off"), "{e}");
    assert_eq!(w.ctx.blobs.files()?, blobs_before, "the rejected pull wrote a blob");
    assert_eq!(w.documents(source).await?.len(), 0);
    assert_eq!(w.count("condition").await?, 0);
    Ok(())
}

#[tokio::test]
async fn a_second_pull_of_one_source_waits_for_the_first_import() -> R {
    let owned = world(Behaviour::default()).await?;
    let w = &owned;
    let source = w.connect(t0()).await?;
    // another run of this source imports: this one waits for its lock
    let mut other = w.db.pool.begin().await?;
    sqlx::query("select pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(format!("source.pull:{source}"))
        .execute(&mut *other)
        .await?;
    let pull = w.day(source, t0() + Duration::minutes(1));
    let first = async move {
        let waited = w.someone_waits().await?;
        other.commit().await?;
        anyhow::Ok(waited)
    };
    let (got, waited) = tokio::join!(pull, first);
    assert!(waited?, "the pull did not wait for the running import");
    assert_eq!(got?.map_err(anyhow::Error::msg)?.new, 5);
    assert_eq!(w.documents(source).await?.len(), 1);
    Ok(())
}

#[tokio::test]
async fn another_persons_portal_account_imports_nothing_into_the_profile() -> R {
    // Alex connects Lucía's profile but signs in at the portal with Sam's account
    let w = world(Behaviour::default()).await?;
    let lucia = w.fam.profile("lucia");
    let source = w.connect_for("lucia", t0()).await?;
    let e = w
        .day(source, t0() + Duration::minutes(1))
        .await?
        .expect_err("the pull stops");
    assert!(e.contains("another_patient"), "{e}");
    assert_eq!(
        w.fake.book().attempts,
        [format!("/fhir/Patient/{PATIENT}")],
        "nothing past the patient"
    );
    assert_eq!(w.documents(source).await?.len(), 0);
    assert_eq!(w.open_items_of(lucia).await?.len(), 0);
    let mut c = w.db.pool.acquire().await?;
    assert_eq!(list_records(&mut c, "condition", lucia, None, true).await?.len(), 0);
    Ok(())
}

#[tokio::test]
async fn a_patient_with_insufficient_demographics_stops_before_searching() -> R {
    for (profile, redact_patient) in [
        ("lucia", &["name", "birthDate"][..]),
        ("sam", &["name"][..]),
        ("sam", &["birthDate"][..]),
    ] {
        let w = world(Behaviour {
            redact_patient,
            ..Behaviour::default()
        })
        .await?;
        let source = w.connect_for(profile, t0()).await?;
        let got = w.day(source, t0() + Duration::minutes(1)).await?;
        assert_eq!(
            w.fake.book().attempts,
            [format!("/fhir/Patient/{PATIENT}")],
            "{profile}: clinical data was requested with {redact_patient:?} missing"
        );
        assert_eq!(w.documents(source).await?.len(), 0);
        assert_eq!(w.open_items_of(w.fam.profile(profile)).await?.len(), 0);
        let err = got.expect_err("the pull stops when the patient's identity cannot be verified");
        assert!(err.contains("patient_unverified"), "{err}");
    }
    Ok(())
}

#[tokio::test]
async fn a_next_link_back_to_a_page_already_read_is_a_problem_and_the_rest_still_arrives() -> R {
    let w = world(Behaviour {
        loop_next: Some("Condition"),
        ..Behaviour::default()
    })
    .await?;
    let source = w.connect(t0()).await?;
    let got = w
        .day(source, t0() + Duration::minutes(1))
        .await?
        .map_err(anyhow::Error::msg)?;
    let codes: Vec<(&str, &str)> = got
        .problems
        .iter()
        .map(|p| (p.code.as_str(), p.message.as_str()))
        .collect();
    assert_eq!(codes.len(), 1, "{codes:?}");
    assert_eq!(codes[0].0, "repeated_page");
    assert!(codes[0].1.starts_with("Condition: "), "{codes:?}");
    assert_eq!((got.new, w.count("condition").await?), (5, 1));
    assert_eq!(w.status_of_last_run(source).await?, "partial");
    Ok(())
}

#[tokio::test]
async fn a_run_where_no_search_answers_fails_and_stores_nothing() -> R {
    let w = world(Behaviour {
        fail: Some(("*", 503)),
        ..Behaviour::default()
    })
    .await?;
    let source = w.connect(t0()).await?;
    let e = w
        .day(source, t0() + Duration::minutes(1))
        .await?
        .expect_err("the run fails");
    assert!(e.contains("no_answer"), "{e}");
    assert_eq!(w.documents(source).await?.len(), 0);
    assert_eq!(w.status_of_last_run(source).await?, "error");
    Ok(())
}

#[tokio::test]
async fn a_server_that_refuses_every_token_gets_one_refresh_and_the_run_ends() -> R {
    let w = world(Behaviour {
        refuse_all: true,
        ..Behaviour::default()
    })
    .await?;
    let source = w.connect(t0()).await?;
    let got = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        w.day(source, t0() + Duration::minutes(1)),
    )
    .await
    .map_err(|_| anyhow::anyhow!("the run never ended"))??;
    let e = got.expect_err("the patient read fails");
    assert!(e.contains("patient_unread") && e.contains("401"), "{e}");
    let book = w.fake.book();
    assert_eq!((book.refreshes, book.unauthorized), (1, 2));
    assert_eq!(book.attempts, vec![format!("/fhir/Patient/{PATIENT}"); 2]);
    assert!(book.searches.is_empty());
    Ok(())
}

#[tokio::test]
async fn a_grant_without_a_patient_read_searches_nothing_and_stores_nothing() -> R {
    // Sam left Patient out at the consent screen: nothing could show the record is Sam's
    for scope in [
        "launch/patient offline_access patient/Observation.rs patient/Condition.rs",
        // a v2 search letter on Patient is not a read
        "launch/patient offline_access patient/Patient.s patient/Observation.rs patient/Condition.rs",
    ] {
        let w = world(Behaviour {
            scope: scope.into(),
            ..Behaviour::default()
        })
        .await?;
        let source = w.connect(t0()).await?;
        let e = w
            .day(source, t0() + Duration::minutes(1))
            .await?
            .expect_err("the pull stops");
        assert!(e.contains("patient_not_granted"), "{scope}: {e}");
        assert!(w.fake.book().attempts.is_empty(), "{scope}");
        assert_eq!(w.documents(source).await?.len(), 0, "{scope}");
        assert_eq!(w.open_items_of(w.sam()).await?.len(), 0, "{scope}");
    }
    Ok(())
}

#[tokio::test]
async fn a_patient_the_server_does_not_give_stops_the_run_and_stores_nothing() -> R {
    let w = world(Behaviour {
        patient_status: Some(500),
        ..Behaviour::default()
    })
    .await?;
    let source = w.connect(t0()).await?;
    let e = w
        .day(source, t0() + Duration::minutes(1))
        .await?
        .expect_err("the run fails");
    assert!(e.contains("patient_unread") && e.contains("500"), "{e}");
    assert_eq!(w.fake.book().attempts, [format!("/fhir/Patient/{PATIENT}")]);
    assert_eq!(w.documents(source).await?.len(), 0);
    Ok(())
}

#[tokio::test]
async fn a_grant_that_covers_no_search_reads_nothing_and_says_so() -> R {
    for scope in [
        "launch/patient offline_access patient/Patient.rs",
        // identity scopes only: an explicit answer, not a missing one
        "openid fhirUser launch/patient offline_access",
    ] {
        let w = world(Behaviour {
            scope: scope.into(),
            ..Behaviour::default()
        })
        .await?;
        let source = w.connect(t0()).await?;
        let got = w
            .day(source, t0() + Duration::minutes(1))
            .await?
            .map_err(anyhow::Error::msg)?;
        let codes: Vec<&str> = got.problems.iter().map(|p| p.code.as_str()).collect();
        assert_eq!(codes, ["not_granted"], "{scope}: {got:?}");
        assert_eq!(
            got.skipped.get("not_granted"),
            Some(&(9 + i64::from(!scope.contains("Patient"))))
        );
        assert!(w.fake.book().attempts.is_empty(), "{scope}");
        assert_eq!(w.documents(source).await?.len(), 0, "{scope}");
        assert_eq!(w.status_of_last_run(source).await?, "partial", "{scope}");
    }
    Ok(())
}

#[tokio::test]
async fn a_server_that_wants_a_status_is_asked_again_with_every_status() -> R {
    let w = world(Behaviour {
        require_status: Some("Condition"),
        ..Behaviour::default()
    })
    .await?;
    let source = w.connect(t0()).await?;
    let got = w
        .day(source, t0() + Duration::minutes(1))
        .await?
        .map_err(anyhow::Error::msg)?;
    assert!(got.problems.is_empty(), "{got:?}");
    assert_eq!(w.count("condition").await?, 1);
    assert!(
        w.searched_since(0)
            .await
            .contains("Condition?clinical-status=active,recurrence,relapse,inactive,remission,resolved"),
        "{:?}",
        w.searched_since(0).await
    );
    Ok(())
}

#[tokio::test]
async fn included_medicines_arrive_with_their_orders_without_a_read() -> R {
    let w = world(Behaviour::default()).await?;
    w.fake.book().extra.extend(two_named_medicines());
    let source = w.connect(t0()).await?;
    let got = w
        .day(source, t0() + Duration::minutes(1))
        .await?
        .map_err(anyhow::Error::msg)?;
    assert!(got.problems.is_empty(), "{got:?}");
    assert_eq!(w.medication_reads(), 0, "the search brought them");
    let mut c = w.db.pool.acquire().await?;
    let meds = list_records(&mut c, "medication", w.sam(), None, true).await?;
    for name in ["Metformin 850 mg", "Atorvastatin 20 mg"] {
        assert!(meds.iter().any(|m| m["name"] == name), "{name}: {meds:?}");
    }
    Ok(())
}

#[tokio::test]
async fn medication_requests_are_imported_without_medication_access() -> R {
    let w = world(Behaviour {
        scope: "launch/patient offline_access patient/Patient.r patient/MedicationRequest.s".into(),
        reject_include: true,
        ..Behaviour::default()
    })
    .await?;
    w.fake.book().extra.extend(two_named_medicines());
    let source = w.connect(t0()).await?;
    let got = w
        .day(source, t0() + Duration::minutes(1))
        .await?
        .map_err(anyhow::Error::msg)?;
    assert!(got.problems.is_empty(), "{got:?}");
    assert_eq!(
        w.count("medication").await?,
        2,
        "both orders with inline names were imported"
    );
    assert_eq!(
        w.searched_since(0).await,
        ["Patient/sam-rivera", "MedicationRequest?intent=order,plan"]
            .map(String::from)
            .into(),
        "only the granted requests were sent"
    );
    Ok(())
}

#[tokio::test]
async fn the_medication_reads_keep_to_the_run_budget_and_their_count() -> R {
    let w = world(Behaviour {
        include: false,
        medication_padding: 16 * 1024,
        stream_answers: true,
        ..Behaviour::default()
    })
    .await?;
    w.fake.book().extra.extend(two_named_medicines());
    let source = w.connect(t0()).await?;
    // the first run reads both, and measures what the patient and the searches weigh
    let first = w
        .day(source, t0() + Duration::minutes(1))
        .await?
        .map_err(anyhow::Error::msg)?;
    assert!(first.problems.is_empty(), "{first:?}");
    assert_eq!(w.medication_reads(), 2);
    let searches = w.fake.book().served;

    // a budget the searches fit in with room for one medicine: the second is not read
    w.fake.book().served = 0;
    let caps = smart::Caps {
        run_bytes: searches + 4096,
        ..smart::Caps::default()
    };
    let got = w
        .day_with(source, t0() + Duration::minutes(2), &caps)
        .await?
        .map_err(anyhow::Error::msg)?;
    assert_eq!(w.medication_reads(), 3, "one more read");
    let large: Vec<&str> = got
        .problems
        .iter()
        .filter(|p| p.code == "too_large")
        .map(|p| p.message.as_str())
        .collect();
    assert_eq!(large.len(), 1, "{:?}", got.problems);
    assert!(large[0].starts_with("Medication: "), "{large:?}");

    // a cap of one read: the other is said, with how many
    let caps = smart::Caps {
        medication_reads: 1,
        ..smart::Caps::default()
    };
    let got = w
        .day_with(source, t0() + Duration::minutes(3), &caps)
        .await?
        .map_err(anyhow::Error::msg)?;
    assert_eq!(w.medication_reads(), 4, "one more read");
    let left: Vec<&str> = got
        .problems
        .iter()
        .filter(|p| p.code == "too_many_medications")
        .map(|p| p.message.as_str())
        .collect();
    assert_eq!(left, ["Medication: 1 more medicines that orders name were not read"]);
    Ok(())
}

#[tokio::test]
async fn identical_failed_searches_are_one_problem_before_a_page_note() -> R {
    let w = world(Behaviour {
        fail: Some(("Observation", 503)),
        next_origin: Some("http://fhir.elsewhere.example".into()),
        ..Behaviour::default()
    })
    .await?;
    let source = w.connect(t0()).await?;
    let got = w
        .day(source, t0() + Duration::minutes(1))
        .await?
        .map_err(anyhow::Error::msg)?;
    let codes: Vec<(&str, &str)> = got
        .problems
        .iter()
        .map(|p| (p.code.as_str(), p.message.as_str()))
        .collect();
    assert_eq!(
        codes,
        [
            ("search_failed", "Observation: the server answered 503 (3 times)"),
            (
                "undeclared_host",
                "MedicationRequest: a next page on a host this connection did not declare was not read"
            ),
        ]
    );
    Ok(())
}

#[tokio::test]
async fn a_single_letter_in_a_patients_name_must_also_match() -> R {
    let w = world(Behaviour {
        given_initial: Some("B"),
        ..Behaviour::default()
    })
    .await?;
    sqlx::query("update profile set display_name = 'A Rivera' where id = $1")
        .bind(w.sam())
        .execute(&w.db.pool)
        .await?;
    let source = w.connect(t0()).await?;
    let got = w.day(source, t0() + Duration::minutes(1)).await?;
    assert_eq!(w.fake.book().attempts, [format!("/fhir/Patient/{PATIENT}")]);
    let err = got.expect_err("another given initial cannot verify the patient");
    assert!(
        err.contains("patient_unverified") || err.contains("another_patient"),
        "{err}"
    );
    assert!(w.documents(source).await?.is_empty());

    w.fake.set(|b| b.given_initial = Some("A"));
    let matched = w
        .day(source, t0() + Duration::minutes(2))
        .await?
        .map_err(anyhow::Error::msg)?;
    assert_eq!(matched.new, 5, "{matched:?}");
    Ok(())
}

#[tokio::test]
async fn a_patient_read_returning_another_id_stops_before_searching() -> R {
    let w = world(Behaviour {
        returned_patient_id: Some("another-id"),
        ..Behaviour::default()
    })
    .await?;
    let source = w.connect(t0()).await?;
    let got = w.day(source, t0() + Duration::minutes(1)).await?;
    assert_eq!(w.fake.book().attempts, [format!("/fhir/Patient/{PATIENT}")]);
    let err = got.expect_err("a Patient with the wrong id cannot verify the patient");
    assert!(err.contains("patient_unverified"), "{err}");
    assert!(w.documents(source).await?.is_empty());
    Ok(())
}

#[tokio::test]
async fn a_procedure_with_a_subject_display_but_no_reference_stays_out() -> R {
    let w = world(Behaviour::default()).await?;
    let id = "r3-proc-display";
    w.fake.book().extra.push(json!({
        "resourceType": "Procedure", "id": id, "status": "completed",
        "subject": {"display": "Lucia Rivera"}, "code": {"text": "Synthetic"},
    }));
    let source = w.connect(t0()).await?;
    let got = w
        .day(source, t0() + Duration::minutes(1))
        .await?
        .map_err(anyhow::Error::msg)?;
    let bundle = w.stored_bundle(source).await?;
    let stored = bundle["entry"]
        .as_array()
        .expect("the snapshot has entries")
        .iter()
        .any(|entry| entry["resource"]["id"] == id);
    assert_eq!((stored, got.new), (false, 5), "{got:?}");
    assert_eq!(w.status_of_last_run(source).await?, "partial");
    assert_eq!(got.problems.len(), 1, "{got:?}");
    assert_eq!(got.problems[0].code, "patient_reference");
    Ok(())
}

#[tokio::test]
async fn foreign_patient_resources_stay_out_of_the_stored_bundle() -> R {
    let w = world(Behaviour::default()).await?;
    let fixture: Value = serde_json::from_slice(&fhir_file("uscore-sam.json"))?;
    let template = fixture["entry"]
        .as_array()
        .expect("the fixture bundle has entries")
        .iter()
        .map(|entry| &entry["resource"])
        .find(|r| r["id"] == "sam-tsh")
        .expect("the lab Observation fixture");
    let make_observation = |id: &str, reference: String| {
        let mut observation = template.clone();
        observation["id"] = json!(id);
        observation["subject"]["reference"] = json!(reference);
        observation
    };
    let foreign = make_observation("other-tsh", "Patient/lucia-rivera".into());
    let matching = make_observation(
        "sam-absolute-tsh",
        format!("{}/Patient/{PATIENT}/_history/1", w.fake.issuer()),
    );
    w.fake.book().extra.extend([foreign, matching]);
    w.fake.book().extra.extend(two_named_medicines());
    let source = w.connect(t0()).await?;
    let got = w
        .day(source, t0() + Duration::minutes(1))
        .await?
        .map_err(anyhow::Error::msg)?;
    let bundle = w.stored_bundle(source).await?;
    let ids: BTreeSet<&str> = bundle["entry"]
        .as_array()
        .expect("the stored bundle has entries")
        .iter()
        .filter_map(|e| e["resource"]["id"].as_str())
        .collect();
    assert!(!ids.contains("other-tsh"), "{ids:?}");
    for id in ["sam-tsh", "sam-absolute-tsh", "metformin-850", "atorvastatin-20"] {
        assert!(ids.contains(id), "{id} missing from {ids:?}");
    }
    assert_eq!(w.status_of_last_run(source).await?, "partial");
    assert_eq!(got.problems.len(), 1, "{got:?}");
    assert_eq!(got.problems[0].code, "patient_reference");
    assert!(!got.problems[0].message.contains("lucia-rivera"), "{got:?}");
    assert_eq!(w.medication_reads(), 0);
    Ok(())
}

#[tokio::test]
async fn search_results_skip_a_foreign_allergy_patient_reference() -> R {
    let w = world(Behaviour {
        scope: "patient/Patient.r patient/AllergyIntolerance.s".into(),
        ..Behaviour::default()
    })
    .await?;
    let id = "r3-allergy-foreign";
    w.fake.book().extra.extend([
        json!({
            "resourceType": "AllergyIntolerance", "id": id,
            "patient": {"reference": "Patient/lucia-rivera"}, "code": {"text": "Synthetic"},
        }),
        json!({
            "resourceType": "AllergyIntolerance", "id": "sam-allergy",
            "patient": {"reference": format!("Patient/{PATIENT}")}, "code": {"text": "Synthetic"},
        }),
    ]);
    let source = w.connect(t0()).await?;
    let got = w
        .day(source, t0() + Duration::minutes(1))
        .await?
        .map_err(anyhow::Error::msg)?;
    let bundle = w.stored_bundle(source).await?;
    let entries = bundle["entry"].as_array().expect("the snapshot has entries");
    assert!(!entries.iter().any(|entry| entry["resource"]["id"] == id), "{got:?}");
    assert!(
        entries.iter().any(|entry| entry["resource"]["id"] == "sam-allergy"),
        "{got:?}"
    );
    assert_eq!(w.status_of_last_run(source).await?, "partial");
    assert_eq!(got.problems.len(), 1, "{got:?}");
    assert_eq!(got.problems[0].code, "patient_reference");
    Ok(())
}

#[tokio::test]
async fn search_results_skip_a_foreign_patient_entry() -> R {
    let w = world(Behaviour {
        scope: "patient/Patient.r patient/Encounter.s".into(),
        ..Behaviour::default()
    })
    .await?;
    w.fake.book().extra_search_entries.push((
        "Encounter",
        json!({
            "resource": {"resourceType": "Patient", "id": "lucia-rivera",
                "name": [{"family": "Rivera", "given": ["Lucia"]}], "birthDate": "2017-06-21"},
            "search": {"mode": "match"},
        }),
    ));
    let source = w.connect(t0()).await?;
    let got = w
        .day(source, t0() + Duration::minutes(1))
        .await?
        .map_err(anyhow::Error::msg)?;
    let bundle = w.stored_bundle(source).await?;
    let patients: Vec<&str> = bundle["entry"]
        .as_array()
        .expect("the snapshot has entries")
        .iter()
        .filter(|entry| entry["resource"]["resourceType"] == "Patient")
        .filter_map(|entry| entry["resource"]["id"].as_str())
        .collect();
    assert_eq!(patients, [PATIENT], "{got:?}");
    assert_eq!(w.status_of_last_run(source).await?, "partial");
    assert_eq!(got.problems.len(), 1, "{got:?}");
    assert_eq!(got.problems[0].code, "unexpected_resource");
    Ok(())
}

#[tokio::test]
async fn search_results_skip_medication_returned_without_an_include() -> R {
    let w = world(Behaviour {
        scope: "patient/Patient.r patient/MedicationRequest.s".into(),
        ..Behaviour::default()
    })
    .await?;
    let medicine = two_named_medicines()
        .into_iter()
        .find(|r| r["resourceType"] == "Medication")
        .expect("a Medication fixture");
    w.fake.book().extra_search_entries.push((
        "MedicationRequest",
        json!({"resource": medicine, "search": {"mode": "include"}}),
    ));
    let source = w.connect(t0()).await?;
    let got = w
        .day(source, t0() + Duration::minutes(1))
        .await?
        .map_err(anyhow::Error::msg)?;
    assert_eq!(
        w.searched_since(0).await,
        [
            format!("Patient/{PATIENT}"),
            "MedicationRequest?intent=order,plan".into()
        ]
        .into()
    );
    // Eight US Core types are outside the grant, plus the unsolicited Medication.
    assert_eq!(got.skipped.get("not_granted"), Some(&9), "{got:?}");
    let bundle = w.stored_bundle(source).await?;
    let entries = bundle["entry"].as_array().expect("the snapshot has entries");
    assert!(
        !entries
            .iter()
            .any(|entry| entry["resource"]["resourceType"] == "Medication"),
        "{got:?}"
    );
    assert_eq!(w.medication_reads(), 0);
    assert_eq!(w.status_of_last_run(source).await?, "partial");
    assert_eq!(got.problems[0].code, "unexpected_resource");
    Ok(())
}

#[tokio::test]
async fn a_server_that_rejects_status_lists_gets_one_search_per_status() -> R {
    let w = world(Behaviour {
        scope: "patient/Patient.r patient/Condition.s".into(),
        require_status: Some("Condition"),
        single_condition_status: true,
        ..Behaviour::default()
    })
    .await?;
    let source = w.connect(t0()).await?;
    let got = w
        .day(source, t0() + Duration::minutes(1))
        .await?
        .map_err(anyhow::Error::msg)?;
    assert!(got.problems.is_empty(), "{got:?}");
    assert_eq!(w.count("condition").await?, 1);
    assert_eq!(
        w.fake.book().searches,
        [
            "Patient/sam-rivera",
            "Condition",
            "Condition?clinical-status=active,recurrence,relapse,inactive,remission,resolved",
            "Condition?clinical-status=active",
            "Condition?clinical-status=recurrence",
            "Condition?clinical-status=relapse",
            "Condition?clinical-status=inactive",
            "Condition?clinical-status=remission",
            "Condition?clinical-status=resolved",
        ]
    );
    Ok(())
}

#[tokio::test]
async fn an_observation_search_keeps_the_category_system_from_the_grant() -> R {
    let category = "http://terminology.hl7.org/CodeSystem/observation-category|laboratory";
    let w = world(Behaviour {
        scope: format!("patient/Patient.r patient/Observation.s?category={category}"),
        ..Behaviour::default()
    })
    .await?;
    let source = w.connect(t0()).await?;
    let got = w
        .day(source, t0() + Duration::minutes(1))
        .await?
        .map_err(anyhow::Error::msg)?;
    assert_eq!(
        w.searched_since(0).await,
        [format!("Patient/{PATIENT}"), format!("Observation?category={category}"),].into()
    );
    assert_eq!(got.new, 1, "{got:?}");
    Ok(())
}

#[tokio::test]
async fn an_oversized_page_is_reported_separately_from_the_run_budget() -> R {
    let w = world(Behaviour {
        scope: "patient/Patient.r patient/AllergyIntolerance.s patient/Condition.s".into(),
        oversized_search: Some("AllergyIntolerance"),
        ..Behaviour::default()
    })
    .await?;
    let source = w.connect(t0()).await?;
    let got = w
        .day(source, t0() + Duration::minutes(1))
        .await?
        .map_err(anyhow::Error::msg)?;
    assert_eq!(got.problems.len(), 1, "{got:?}");
    assert_eq!(got.problems[0].code, "page_too_large");
    assert_eq!(w.status_of_last_run(source).await?, "partial");
    assert_eq!(w.count("condition").await?, 1, "the next search still ran");

    w.fake.set(|b| b.fail = Some(("Condition", 503)));
    let error = w
        .day(source, t0() + Duration::minutes(2))
        .await?
        .expect_err("no search answered with a usable page");
    assert!(error.contains("page_too_large: AllergyIntolerance: "), "{error}");
    Ok(())
}

#[tokio::test]
async fn the_last_status_retry_stays_within_the_run_budget() -> R {
    let w = world(Behaviour {
        scope: "patient/Patient.r patient/AllergyIntolerance.s patient/Condition.s".into(),
        fail: Some(("Condition", 503)),
        stream_answers: true,
        ..Behaviour::default()
    })
    .await?;
    w.fake.book().extra.push(json!({
        "resourceType": "AllergyIntolerance", "id": "sam-allergy",
        "patient": {"reference": format!("Patient/{PATIENT}")},
        "code": {"text": "Synthetic allergen"},
    }));
    let source = w.connect(t0()).await?;
    w.day(source, t0() + Duration::minutes(1))
        .await?
        .map_err(anyhow::Error::msg)?;
    let status_required = serde_json::to_vec(&outcome_body("the status parameter is required for this search"))?;
    let retry = serde_json::to_vec(&json!({
        "resourceType": "Bundle", "type": "searchset", "entry": [{"resource": {
            "resourceType": "Condition", "id": "sam-retry", "subject": {"reference": format!("Patient/{PATIENT}")},
            "code": {"text": "Synthetic condition"},
        }}],
    }))?;
    assert!(status_required.len() < retry.len());
    let run_bytes = {
        let mut book = w.fake.book();
        // The retry fits exactly after the Patient and AllergyIntolerance responses.
        // Charging the 400 response leaves too little room for it.
        let cap = book.served + retry.len();
        book.search_bodies
            .insert("Condition".into(), (StatusCode::BAD_REQUEST, status_required));
        book.search_bodies.insert(
            "Condition?clinical-status=active,recurrence,relapse,inactive,remission,resolved".into(),
            (StatusCode::OK, retry),
        );
        cap
    };
    let caps = smart::Caps {
        run_bytes,
        ..smart::Caps::default()
    };
    let got = w
        .day_with(source, t0() + Duration::minutes(2), &caps)
        .await?
        .map_err(anyhow::Error::msg)?;
    let searches = w.searched_since(0).await;
    assert!(searches.contains("Condition?clinical-status=active,recurrence,relapse,inactive,remission,resolved"));
    assert_eq!(got.problems.len(), 1, "{got:?}");
    assert_eq!(got.problems[0].code, "too_large");
    assert!(got.problems[0].message.starts_with("Condition: "), "{got:?}");
    assert_eq!(w.status_of_last_run(source).await?, "partial");
    assert_eq!(w.count("condition").await?, 0);
    assert_eq!(w.documents(source).await?.len(), 1, "the earlier snapshot is retained");
    assert!(got.duplicate > 0, "{got:?}");
    Ok(())
}

#[tokio::test]
async fn a_refused_access_tokens_body_counts_against_the_run_budget() -> R {
    let w = world(Behaviour {
        scope: "patient/Patient.r patient/Condition.s".into(),
        fail: Some(("Condition", 503)),
        stream_answers: true,
        ..Behaviour::default()
    })
    .await?;
    let source = w.connect(t0()).await?;
    w.day(source, t0() + Duration::minutes(1))
        .await?
        .expect_err("no search answered");
    let (patient_bytes, before) = {
        let mut book = w.fake.book();
        book.access = Some("nobody-holds-this".into());
        (book.served, book.attempts.len())
    };
    w.fake.set(|b| b.fail = None);
    let caps = smart::Caps {
        run_bytes: patient_bytes,
        ..smart::Caps::default()
    };
    let got = w.day_with(source, t0() + Duration::minutes(2), &caps).await?;
    let err = got.expect_err("the 401 body leaves too little room to verify the patient");
    assert!(err.starts_with("JobError: too_large: Patient: "), "{err}");
    assert!(w.documents(source).await?.is_empty());
    let book = w.fake.book();
    assert_eq!((book.unauthorized, book.refreshes), (1, 1));
    assert_eq!(
        book.attempts[before..],
        [format!("/fhir/Patient/{PATIENT}"), format!("/fhir/Patient/{PATIENT}")]
    );
    Ok(())
}

#[tokio::test]
async fn a_relative_patient_reference_uses_the_entry_fhir_base() -> R {
    let w = world(Behaviour {
        scope: "patient/Patient.r patient/Procedure.s".into(),
        ..Behaviour::default()
    })
    .await?;
    let entry = |id, base: Option<&str>, reference| {
        let mut entry = json!({"resource": {
            "resourceType": "Procedure", "id": id, "status": "completed",
            "subject": {"reference": reference}, "code": {"text": "Synthetic"},
        }});
        if let Some(base) = base {
            entry["fullUrl"] = json!(format!("{base}/Procedure/{id}"));
        }
        ("Procedure", entry)
    };
    let issuer = w.fake.issuer();
    let other_base = format!("{issuer}/other");
    w.fake.book().extra_search_entries.extend([
        entry("own-base", Some(&issuer), format!("Patient/{PATIENT}/_history/2")),
        entry("no-full-url", None, format!("Patient/{PATIENT}")),
        entry("other-base", Some(&other_base), format!("Patient/{PATIENT}")),
        entry(
            "other-host",
            Some("https://other.example/fhir"),
            format!("Patient/{PATIENT}"),
        ),
        entry("absolute-own", Some(&other_base), format!("{issuer}/Patient/{PATIENT}")),
    ]);
    let mut unresolvable = entry("unresolvable-base", None, format!("Patient/{PATIENT}"));
    unresolvable.1["fullUrl"] = json!("urn:uuid:fe67141b-1254-46d4-a6f4-76d49799d317");
    w.fake.book().extra_search_entries.push(unresolvable);
    let source = w.connect(t0()).await?;
    let got = w.day(source, t0()).await?.map_err(anyhow::Error::msg)?;
    let bundle = w.stored_bundle(source).await?;
    let entries = bundle["entry"].as_array().expect("the snapshot has entries");
    let ids: BTreeSet<_> = entries
        .iter()
        .filter_map(|entry| entry["resource"]["id"].as_str())
        .collect();
    for kept in ["own-base", "no-full-url", "absolute-own"] {
        assert!(ids.contains(kept), "{ids:?}");
    }
    for rejected in ["other-base", "other-host", "unresolvable-base"] {
        assert!(!ids.contains(rejected), "{ids:?}");
    }
    assert_eq!(w.status_of_last_run(source).await?, "partial");
    assert_eq!(got.problems[0].code, "patient_reference");
    Ok(())
}

#[tokio::test]
async fn a_condition_search_cannot_import_an_unsolicited_observation() -> R {
    let w = world(Behaviour {
        scope: "patient/Patient.r patient/Condition.s".into(),
        ..Behaviour::default()
    })
    .await?;
    w.fake.book().extra_search_entries.push((
        "Condition",
        json!({
            "resource": {
                "resourceType": "Observation", "id": "unsolicited-observation", "status": "final",
                "subject": {"reference": format!("Patient/{PATIENT}")},
                "code": {"text": "Synthetic observation"},
            },
            "search": {"mode": "match"},
        }),
    ));
    let source = w.connect(t0()).await?;
    let got = w.day(source, t0()).await?.map_err(anyhow::Error::msg)?;
    let bundle = w.stored_bundle(source).await?;
    let entries = bundle["entry"].as_array().expect("the snapshot has entries");
    assert!(!entries.iter().any(|e| e["resource"]["resourceType"] == "Observation"));
    assert!(entries.iter().any(|e| e["resource"]["resourceType"] == "Condition"));
    assert_eq!(w.status_of_last_run(source).await?, "partial");
    assert_eq!(got.problems[0].code, "unexpected_resource");
    Ok(())
}

#[tokio::test]
async fn future_pulls_use_the_scope_from_refresh_even_when_the_next_refresh_omits_it() -> R {
    let w = world(Behaviour::default()).await?;
    let source = w.connect(t0()).await?;
    let narrow = "patient/Patient.r patient/Condition.s";
    w.fake.set(|b| b.scope = narrow.into());

    // Let the initial access token expire. This pull refreshes it after planning its searches.
    w.day(source, t0() + Duration::hours(2))
        .await?
        .map_err(anyhow::Error::msg)?;
    let saved_scope = async || -> anyhow::Result<Value> {
        let mut c = w.db.pool.acquire().await?;
        let source = get_source(&mut c, source).await?.expect("the source");
        Ok(source.config["scope"].clone())
    };
    assert_eq!(saved_scope().await?, narrow);

    let before = w.fake.book().searches.len();
    w.fake.set(|b| b.omit_refresh_scope = true);
    w.day(source, t0() + Duration::hours(4))
        .await?
        .map_err(anyhow::Error::msg)?;
    assert_eq!(saved_scope().await?, narrow);
    assert_eq!(w.fake.book().refreshes, 2);
    assert_eq!(
        w.searched_since(before).await,
        [format!("Patient/{PATIENT}"), "Condition".into()].into()
    );
    Ok(())
}

#[tokio::test]
async fn an_old_poll_finishes_in_history_without_changing_the_new_grants_health() -> R {
    let w = world(Behaviour::default()).await?;
    let source = w.connect(t0()).await?;
    let (old_run, old_success) = {
        let mut c = w.db.pool.acquire().await?;
        (
            sources::begin_run(&mut c, source, "poll", None).await?,
            sources::begin_run(&mut c, source, "poll", None).await?,
        )
    };
    let reconnect = w.launch_for_source("sam", Some(source)).await?;
    assert_eq!(w.save_consent(reconnect, t0() + Duration::minutes(1)).await?, source);

    let mut c = w.db.pool.acquire().await?;
    sources::finish_run(&mut c, old_run, Err("old grant failed")).await?;
    let history = sources::list_runs(&mut c, source, 10).await?;
    let prior = history.iter().find(|r| r.id == old_run).expect("old run in history");
    assert_eq!(prior.error.as_deref(), Some("old grant failed"));
    let health = sources::source_health(&mut c, w.fam.household_id).await?;
    let health = health.iter().find(|h| h.id == source).expect("the source's health");
    assert_eq!(health.consecutive_failures, 0);
    assert_eq!(health.last_error, None);
    assert_eq!(health.last_run_status, None);
    assert_eq!(health.last_run_problem, None);

    let current_run = sources::begin_run(&mut c, source, "poll", None).await?;
    sources::finish_run(&mut c, current_run, Err("new grant failed")).await?;
    sources::finish_run(
        &mut c,
        old_success,
        Ok(&RunOutcome {
            cursor: Some(json!({"old": true})),
            ..RunOutcome::default()
        }),
    )
    .await?;
    let current = get_source(&mut c, source).await?.expect("the source");
    assert_eq!(current.last_success_at, None);
    assert_eq!(current.cursor, None);
    let health = sources::source_health(&mut c, w.fam.household_id).await?;
    let health = health.iter().find(|h| h.id == source).expect("the source's health");
    assert_eq!(health.consecutive_failures, 1);
    assert_eq!(health.last_error.as_deref(), Some("new grant failed"));
    assert_eq!(health.last_run_problem.as_deref(), Some("new grant failed"));
    Ok(())
}

#[tokio::test]
async fn bytes_from_a_broken_body_count_against_the_next_search() -> R {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    let w = world(Behaviour {
        scope: "patient/Patient.r patient/Condition.s patient/Observation.s?category=laboratory".into(),
        fail: Some(("*", 503)),
        stream_answers: true,
        ..Behaviour::default()
    })
    .await?;
    let source = w.connect(t0()).await?;
    w.day(source, t0()).await?.expect_err("no search answered");
    let patient_bytes = w.fake.book().served;

    let broken = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let authority = broken.local_addr()?.to_string();
    let first_page = serde_json::to_vec(&json!({
        "resourceType": "Bundle", "type": "searchset",
        "link": [{"relation": "next", "url": format!("http://{authority}/Condition?page=2")}],
    }))?;
    let partial_bytes = 16 * 1024;
    let caps = smart::Caps {
        run_bytes: patient_bytes + first_page.len() + partial_bytes + 32,
        ..smart::Caps::default()
    };
    w.fake
        .book()
        .search_bodies
        .insert("Condition".into(), (StatusCode::OK, first_page));
    w.fake.set(|b| b.fail = None);
    sqlx::query("update source set config = jsonb_set(config, '{hosts}', config->'hosts' || jsonb_build_array($2::text)) where id = $1")
        .bind(source)
        .bind(&authority)
        .execute(&w.db.pool)
        .await?;

    let serve = async {
        for bytes in [partial_bytes, 0] {
            let (socket, _) = broken.accept().await?;
            let mut io = BufReader::new(socket);
            let mut line = String::new();
            loop {
                line.clear();
                anyhow::ensure!(io.read_line(&mut line).await? > 0, "incomplete request headers");
                if line == "\r\n" {
                    break;
                }
            }
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                bytes + 1,
                " ".repeat(bytes)
            );
            io.get_mut().write_all(response.as_bytes()).await?;
            io.get_mut().shutdown().await?;
        }
        Ok::<_, anyhow::Error>(())
    };
    let poll = async {
        let got = w
            .day_with(source, t0() + Duration::minutes(1), &caps)
            .await?
            .map_err(anyhow::Error::msg)?;
        assert_eq!(got.new, 0, "{got:?}");
        assert_eq!(
            got.problems.iter().map(|p| p.code.as_str()).collect::<Vec<_>>(),
            ["unreachable", "too_large"]
        );
        assert!(got.problems[1].message.starts_with("Observation: "), "{got:?}");

        let got = w
            .day_with(source, t0() + Duration::minutes(2), &caps)
            .await?
            .map_err(anyhow::Error::msg)?;
        assert_eq!(got.new, 1, "{got:?}");
        assert_eq!(got.problems.len(), 1, "{got:?}");
        assert_eq!(got.problems[0].code, "unreachable");
        Ok::<_, anyhow::Error>(())
    };
    tokio::time::timeout(std::time::Duration::from_secs(15), async {
        tokio::try_join!(serve, poll)
    })
    .await??;
    Ok(())
}

#[tokio::test]
async fn a_reconnect_to_the_same_patient_stops_the_old_pull() -> R {
    let pause = Arc::new(ProcedurePause::default());
    let w = world(Behaviour {
        procedure_pause: Some(pause.clone()),
        ..Behaviour::default()
    })
    .await?;
    let source = w.connect(t0()).await?;
    let pull = w.day(source, t0() + Duration::minutes(1));
    let reconnect = async {
        pause.arrived.notified().await;
        oauth::disconnect(&w.db.pool, &w.sb, source, &egress()).await?;
        let launch = w.launch_for_source("sam", Some(source)).await?;
        assert_eq!(w.save_consent(launch, t0() + Duration::minutes(2)).await?, source);
        w.fake.set(|b| b.procedure_pause = None);
        pause.release.notify_one();
        anyhow::Ok(())
    };
    let (got, ()) = tokio::time::timeout(std::time::Duration::from_secs(30), async {
        tokio::try_join!(pull, reconnect)
    })
    .await??;
    let e = got.expect_err("the old pull stops");
    assert!(e.contains("grant_changed:"), "{e}");
    assert!(w.documents(source).await?.is_empty());
    assert_eq!(w.count("condition").await?, 0);
    assert!(w.open_items().await?.is_empty());

    let next = w
        .day(source, t0() + Duration::minutes(3))
        .await?
        .map_err(anyhow::Error::msg)?;
    assert_eq!(next.new, 5, "{next:?}");
    assert_eq!(w.count("condition").await?, 1);
    Ok(())
}

async fn assert_condition_searches(grant: &str, expected: &[&str]) -> R {
    let w = world(Behaviour {
        scope: format!("patient/Patient.r {grant}"),
        ..Behaviour::default()
    })
    .await?;
    let source = w.connect(t0()).await?;
    let got = w.day(source, t0()).await?.map_err(anyhow::Error::msg)?;
    let mut paths = vec![format!("Patient/{PATIENT}")];
    paths.extend(expected.iter().map(|t| t.to_string()));
    let mut actual = w.fake.book().searches.clone();
    paths.sort();
    actual.sort();
    assert_eq!(actual, paths);
    assert!(got.problems.is_empty(), "{got:?}");
    assert_eq!(got.skipped.get("not_granted"), Some(&8));
    Ok(())
}

#[tokio::test]
async fn a_health_concern_grant_searches_condition_with_the_granted_category() -> R {
    assert_condition_searches(
        "patient/Condition.rs?category=http://hl7.org/fhir/us/core/CodeSystem/condition-category%7Chealth-concern",
        &["Condition?category=http://hl7.org/fhir/us/core/CodeSystem/condition-category|health-concern"],
    )
    .await
}

#[tokio::test]
async fn two_condition_categories_get_one_search_each() -> R {
    assert_condition_searches(
        "patient/Condition.rs?category=http://terminology.hl7.org/CodeSystem/condition-category|problem-list-item \
         patient/Condition.rs?category=http://terminology.hl7.org/CodeSystem/condition-category|encounter-diagnosis \
         patient/Condition.s?category=http://terminology.hl7.org/CodeSystem/condition-category|problem-list-item \
         patient/Condition.r?category=health-concern user/Condition.rs?category=other",
        &[
            "Condition?category=http://terminology.hl7.org/CodeSystem/condition-category|problem-list-item",
            "Condition?category=http://terminology.hl7.org/CodeSystem/condition-category|encounter-diagnosis",
        ],
    )
    .await
}

#[tokio::test]
async fn an_unrestricted_condition_grant_keeps_the_unfiltered_search() -> R {
    assert_condition_searches(
        "patient/Condition.rs patient/Condition.rs?category=health-concern",
        &["Condition"],
    )
    .await
}

#[tokio::test]
async fn a_medication_read_with_the_wrong_id_keeps_other_reads_and_marks_the_run_partial() -> R {
    let w = world(Behaviour {
        include: false,
        ..Behaviour::default()
    })
    .await?;
    let mut resources = two_named_medicines();
    resources[0]["medicationReference"]["reference"] = json!(format!("{}/Medication/metformin-850", w.fake.issuer()));
    let mut wrong = resources[2].clone();
    wrong["id"] = json!("different-drug");
    wrong["code"]["coding"][0]["display"] = json!("Wrong drug");
    w.fake.book().medication_answers.insert("metformin-850".into(), wrong);
    w.fake.book().extra.extend(resources);
    let source = w.connect(t0()).await?;
    let got = w.day(source, t0()).await?.map_err(anyhow::Error::msg)?;
    let snapshot = w.stored_bundle(source).await?;
    let entries = snapshot["entry"].as_array().expect("snapshot entries");
    assert!(!entries.iter().any(|e| e["resource"]["id"] == "different-drug"));
    let mut c = w.db.pool.acquire().await?;
    let meds = list_records(&mut c, "medication", w.sam(), None, true).await?;
    assert!(meds.iter().any(|m| m["name"] == "Atorvastatin 20 mg"), "{meds:?}");
    assert!(!meds.iter().any(|m| m["name"] == "Wrong drug"), "{meds:?}");
    assert_eq!(w.medication_reads(), 2);
    assert_eq!(got.problems.len(), 1, "{got:?}");
    assert_eq!(got.problems[0].code, "medication_id_mismatch");
    assert_eq!(w.status_of_last_run(source).await?, "partial");
    Ok(())
}

#[tokio::test]
async fn verified_non_latin_patient_names_auto_accept_facts() -> R {
    let mut counts = Vec::new();
    for name in ["李", "Сам Ривера", "Σαμ Ριβέρα"] {
        let w = world(Behaviour {
            patient_name: Some(name),
            ..Behaviour::default()
        })
        .await?;
        sqlx::query("update profile set display_name = $1 where id = $2")
            .bind(name)
            .bind(w.sam())
            .execute(&w.db.pool)
            .await?;
        let source = w.connect(t0()).await?;
        let got = w.day(source, t0()).await?.map_err(anyhow::Error::msg)?;
        assert!(got.problems.is_empty(), "{name}: {got:?}");
        counts.push(w.count("condition").await?);

        // Keep the same date of birth and change the name alone.
        w.fake.set(|b| b.patient_name = Some("別人"));
        let e = w
            .day(source, t0() + Duration::minutes(1))
            .await?
            .expect_err("the name differs");
        assert!(e.contains("another_patient:"), "{e}");
        assert_eq!(w.documents(source).await?.len(), 1);
    }
    assert_eq!(counts, [1, 1, 1]);
    Ok(())
}

#[tokio::test]
async fn the_bytes_past_the_page_cap_can_exhaust_the_run_budget() -> R {
    let w = world(Behaviour {
        scope: "patient/Patient.r patient/AllergyIntolerance.s patient/Condition.s".into(),
        oversized_search: Some("AllergyIntolerance"),
        stream_answers: true,
        ..Behaviour::default()
    })
    .await?;
    let source = w.connect(t0()).await?;
    // Measure the Patient read without consuming another large response.
    w.fake.set(|b| b.fail = Some(("*", 503)));
    w.day(source, t0()).await?.expect_err("no search answered");
    let patient_bytes = w.fake.book().served;
    let before = w.fake.book().attempts.len();
    w.fake.set(|b| b.fail = None);
    let caps = smart::Caps {
        run_bytes: patient_bytes + smart::PAGE_BYTES + 1,
        ..smart::Caps::default()
    };
    let e = w
        .day_with(source, t0() + Duration::minutes(1), &caps)
        .await?
        .expect_err("no search answered");
    assert_eq!(
        w.fake.book().attempts[before..],
        [format!("/fhir/Patient/{PATIENT}"), "/fhir/AllergyIntolerance".into()]
    );
    assert!(e.starts_with("JobError: too_large: AllergyIntolerance:"), "{e}");

    // The same declared size can be refused before reading a body. The Condition still fits.
    w.fake.set(|b| b.stream_answers = false);
    let got = w
        .day_with(source, t0() + Duration::minutes(2), &caps)
        .await?
        .map_err(anyhow::Error::msg)?;
    assert_eq!(got.problems.len(), 1, "{got:?}");
    assert_eq!(got.problems[0].code, "page_too_large");
    assert_eq!(w.count("condition").await?, 1);
    Ok(())
}
