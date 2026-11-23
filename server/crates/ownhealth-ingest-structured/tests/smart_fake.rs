//! SMART standalone patient launch and the US Core poller (connectors.md 6.2, slice 15) against a
//! fake SMART server on loopback. The fake serves `fixtures/fhir/uscore-sam.json` through discovery,
//! a token endpoint (PKCE S256, refresh tokens that rotate and die once used) and the US Core
//! searches, one resource per page. Only the newest access token reads the FHIR API, so every pull
//! proves it used the token of its own refresh. Synthetic data only (the Rivera family, the
//! fictional LabNorte hospital); nothing here reaches the internet.

mod common;

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::{Arc, Mutex, MutexGuard};

use axum::extract::{Form, Path, RawQuery, State};
use axum::http::{HeaderMap, StatusCode};
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
    /// The `scope` of every token answer.
    scope: String,
    /// The `capabilities` discovery lists.
    capabilities: Vec<&'static str>,
    /// A search of this type (`*`: every type) answers this status.
    fail: Option<(&'static str, u16)>,
    /// The next-page links point at this origin instead of the fake's own.
    next_origin: Option<String>,
    /// The first page of this type's search answers 3 s late.
    stall: Option<&'static str>,
    /// This type's search links its first page to itself as `next`.
    loop_next: Option<&'static str>,
    /// Every FHIR request is refused for its token, even with the newest one.
    refuse_all: bool,
    /// The Patient read answers this status.
    patient_status: Option<u16>,
    /// Demographics the vendor withholds from the Patient read.
    redact_patient: &'static [&'static str],
    /// `_include=MedicationRequest:medication` brings the named Medications (US Core); off, the
    /// server ignores it.
    include: bool,
    /// This type's search answers 400 "status is required" without its status parameter.
    require_status: Option<&'static str>,
    /// Bytes of filler in each Medication read answer.
    medication_padding: usize,
}

impl Default for Behaviour {
    fn default() -> Self {
        Behaviour {
            scope: ALL.into(),
            capabilities: vec![
                "launch-standalone",
                "client-public",
                "permission-offline",
                "permission-v2",
            ],
            fail: None,
            next_origin: None,
            stall: None,
            loop_next: None,
            refuse_all: false,
            patient_status: None,
            redact_patient: &[],
            include: true,
            require_status: None,
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
    /// FHIR requests refused for their bearer token
    unauthorized: usize,
    /// every token handed out, to look for them in the database
    handed_out: Vec<String>,
    /// resources the server gained after the launch
    extra: Vec<Value>,
    /// body bytes of every Patient read and search answer (Medication reads not counted)
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
            Json(mint(&s)).into_response()
        }
        _ => oauth_error(StatusCode::BAD_REQUEST, "unsupported_grant_type"),
    }
}

/// The bearer check of every FHIR request: only the newest access token reads.
fn authorized(s: &Shared, headers: &HeaderMap) -> bool {
    let given = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));
    let refuse_all = lock(&s.behaviour).refuse_all;
    let mut book = lock(&s.book);
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
    (status, [("content-type", "application/fhir+json")], bytes).into_response()
}

fn outcome(status: StatusCode, text: &str) -> Response {
    let body = json!({"resourceType": "OperationOutcome",
                      "issue": [{"severity": "error", "code": "processing", "diagnostics": text}]});
    (status, [("content-type", "application/fhir+json")], Json(body)).into_response()
}

async fn read_patient(State(s): State<Shared>, Path(id): Path<String>, headers: HeaderMap) -> Response {
    if !authorized(&s, &headers) {
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
                    fields.remove(*field);
                }
            }
            fhir_answer(&s, StatusCode::OK, &p, true)
        }
        None => outcome(StatusCode::NOT_FOUND, "no such patient"),
    }
}

/// A Medication the server holds apart, named by a MedicationRequest's `medicationReference`.
async fn read_medication(State(s): State<Shared>, Path(id): Path<String>, headers: HeaderMap) -> Response {
    if !authorized(&s, &headers) {
        return outcome(StatusCode::UNAUTHORIZED, "expired token");
    }
    lock(&s.book).searches.push(format!("Medication/{id}"));
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

fn has_code(r: &Value, field: &str, code: &str) -> bool {
    r[field].as_array().is_some_and(|cats| {
        cats.iter().any(|c| {
            c["coding"]
                .as_array()
                .is_some_and(|cs| cs.iter().any(|x| x["code"] == code))
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
    headers: HeaderMap,
) -> Response {
    if !authorized(&s, &headers) {
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
        lock(&s.book).searches.push(entry);
    }
    if let Some((t, status)) = b.fail
        && (t == rtype || t == "*")
    {
        return outcome(StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY), "down");
    }
    if page == 1 && b.stall == Some(rtype.as_str()) {
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
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
    let extra = lock(&s.book).extra.clone();
    let hits: Vec<Value> = s
        .record
        .iter()
        .chain(extra.iter())
        .filter(|r| r["resourceType"] == rtype.as_str())
        .filter(|r| match rtype.as_str() {
            "Observation" => has_code(r, "category", category.as_deref().unwrap_or_default()),
            "MedicationRequest" => intents.iter().any(|i| r["intent"] == i.as_str()),
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
        "meta": {"lastUpdated": Utc::now().to_rfc3339()}, "total": hits.len(), "link": links, "entry": entries});
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
                source_id: None,
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
    let before = w.fake.book().searches.len();
    // Sam removed the app at the portal: every refresh token is dead
    w.fake.book().live.clear();
    let now = t0() + Duration::days(1);
    let got = w.day(source, now).await?;
    let e = got.expect_err("the pull stops");
    assert!(e.contains("reconsent_needed"), "{e}");
    assert_eq!(w.reconsent(source).await?, Some(now));
    assert_eq!(w.fake.book().searches.len(), before, "no search without a token");
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
    let w = world(Behaviour {
        stall: Some("Procedure"),
        ..Behaviour::default()
    })
    .await?;
    let source = w.connect(t0()).await?;
    let pull = w.day(source, t0() + Duration::minutes(1));
    let cut = async {
        tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
        oauth::disconnect(&w.db.pool, &w.sb, source, &egress()).await
    };
    let (got, cut) = tokio::join!(pull, cut);
    cut?;
    let e = got?.expect_err("the pull stops");
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
    let (got, waited) = tokio::join!(pull, cut);
    assert!(waited?, "the import never waited on the source row");
    let e = got.expect_err("the import sees the disconnect");
    assert!(e.contains("switched_off"), "{e}");
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
        w.searched_since(0).await,
        ["Patient/sam-rivera".to_string()].into(),
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
            w.searched_since(0).await,
            ["Patient/sam-rivera".to_string()].into(),
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
        assert!(w.searched_since(0).await.is_empty(), "{scope}");
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
    assert_eq!(w.searched_since(0).await, ["Patient/sam-rivera".to_string()].into());
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
        assert!(w.searched_since(0).await.is_empty(), "{scope}");
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
async fn no_medication_is_read_when_the_grant_leaves_it_out() -> R {
    let narrow = [
        "launch/patient",
        "offline_access",
        "patient/Patient.rs",
        "patient/MedicationRequest.rs",
        "patient/Condition.rs",
        "patient/Observation.rs",
    ]
    .join(" ");
    let w = world(Behaviour {
        scope: narrow,
        include: false,
        ..Behaviour::default()
    })
    .await?;
    w.fake.book().extra.extend(two_named_medicines());
    let source = w.connect(t0()).await?;
    w.day(source, t0() + Duration::minutes(1))
        .await?
        .map_err(anyhow::Error::msg)?;
    assert_eq!(w.medication_reads(), 0);
    Ok(())
}

#[tokio::test]
async fn the_medication_reads_keep_to_the_run_budget_and_their_count() -> R {
    let w = world(Behaviour {
        include: false,
        medication_padding: 16 * 1024,
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
