//! CMS v3 over loopback with synthetic CMS and HL7 data. No vendor call reaches the internet.

mod common;

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use axum::extract::{Form, Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use base64::Engine as _;
use chrono::{Duration, Utc};
use ownhealth_core::connector_oauth::{self as oauth, ConnectorClient, Endpoints, Launch, OAuthError};
use ownhealth_core::jobs::{JobContext, JobError};
use ownhealth_core::net::Egress;
use ownhealth_core::secrets::{self, SecretBox, SecretString};
use ownhealth_core::sources::{self, get_source};
use ownhealth_ingest_structured::bluebutton;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use uuid::Uuid;

const PATIENT: &str = "-20140000010000";
const BINDER: &str = "blue-button-browser-binder";
const CLIENT_ID: &str = "rivera-medicare";
const REDIRECT: &str = "https://ownhealth.example/api/connectors/callback";
const SCOPE: &str = "launch/patient patient/Patient.rs patient/Coverage.rs patient/ExplanationOfBenefit.rs";

#[derive(Default)]
struct Calls {
    challenge: Option<String>,
    token_issued: bool,
    refreshes: usize,
    pages: Vec<(String, usize)>,
    revoked: Vec<String>,
}

/// What the claim search serves.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum Eob {
    /// 50 claims with a next link, then one more.
    #[default]
    Full,
    /// 30 claims with a next link, then one more at offset 30.
    Short,
    /// Full, with fullUrl on every claim and another server's fullUrl on claim-3.
    ForeignUrl,
    /// The second page is the first page again, with a next link.
    Repeated,
    /// 51 claims on a page of 50.
    Oversized,
    /// The first page answers this status.
    Fails(u16),
}

/// How the fake CMS behaves. Tests change it between calls.
#[derive(Clone, Default)]
struct Mode {
    /// The refresh retires the old token, then the reply is lost.
    broken_refresh: bool,
    /// The second claim page names the patient on another server.
    wrong_reference: bool,
    /// The refresh answers with the refresh token it was sent.
    same_refresh: bool,
    /// The refresh answers for another patient.
    other_patient: bool,
    /// The code exchange names no patient.
    no_patient: bool,
    /// The scope the token answers grant, when narrower than the request.
    scope: Option<String>,
    eob: Eob,
    /// SQL run once, when the first claim page is asked for: a change during the run.
    mid_run: Option<(PgPool, String)>,
}

#[derive(Clone)]
struct Fake {
    issuer: String,
    basic: String,
    calls: Arc<Mutex<Calls>>,
    mode: Arc<Mutex<Mode>>,
}

impl Fake {
    fn mode(&self) -> std::sync::MutexGuard<'_, Mode> {
        self.mode.lock().expect("fake CMS mode")
    }

    fn calls(&self) -> std::sync::MutexGuard<'_, Calls> {
        self.calls.lock().expect("fake CMS request log")
    }
}

async fn authorize(State(fake): State<Fake>, Query(q): Query<HashMap<String, String>>) -> Response {
    if q.get("state").is_none_or(|s| s.len() < 16)
        || q.get("code_challenge_method").map(String::as_str) != Some("S256")
        || q.get("client_id").map(String::as_str) != Some(CLIENT_ID)
        || q.get("redirect_uri").map(String::as_str) != Some(REDIRECT)
        || q.get("scope").map(String::as_str) != Some(SCOPE)
    {
        return StatusCode::BAD_REQUEST.into_response();
    }
    fake.calls().challenge = q.get("code_challenge").cloned();
    Json(json!({"code": "fake-code"})).into_response()
}

async fn token(State(fake): State<Fake>, headers: HeaderMap, Form(q): Form<HashMap<String, String>>) -> Response {
    if headers.get("authorization").and_then(|h| h.to_str().ok()) != Some(&fake.basic)
        || q.contains_key("client_secret")
    {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let mode = fake.mode().clone();
    let scope = mode.scope.as_deref().unwrap_or(SCOPE);
    let mut calls = fake.calls();
    let (n, refresh, patient) = match q.get("grant_type").map(String::as_str) {
        Some("authorization_code") => {
            let challenge = q
                .get("code_verifier")
                .map(|v| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(Sha256::digest(v.as_bytes())));
            if q.get("code").map(String::as_str) != Some("fake-code")
                || calls.challenge.take().is_none_or(|c| challenge.as_deref() != Some(&c))
            {
                return StatusCode::BAD_REQUEST.into_response();
            }
            calls.token_issued = true;
            if mode.no_patient {
                return Json(json!({
                    "access_token": "fake-orphan-access", "refresh_token": "fake-orphan",
                    "token_type": "Bearer", "expires_in": 3600, "scope": scope,
                }))
                .into_response();
            }
            (0, "fake-rotation-0".to_string(), PATIENT)
        }
        Some("refresh_token") => {
            let old = calls.refreshes;
            calls.refreshes += 1;
            if !calls.token_issued || q.get("refresh_token") != Some(&format!("fake-rotation-{old}")) {
                return (StatusCode::BAD_REQUEST, Json(json!({"error": "invalid_grant"}))).into_response();
            }
            if mode.broken_refresh {
                // The vendor retired the old token but the caller received no replacement.
                return (StatusCode::BAD_GATEWAY, "broken refresh response").into_response();
            }
            let n = calls.refreshes;
            let refresh = if mode.same_refresh { old } else { n };
            let patient = if mode.other_patient { "-20140000099999" } else { PATIENT };
            (n, format!("fake-rotation-{refresh}"), patient)
        }
        _ => return StatusCode::BAD_REQUEST.into_response(),
    };
    Json(json!({
        "access_token": format!("fake-access-{n}"), "refresh_token": refresh,
        "token_type": "Bearer", "expires_in": 3600, "patient": patient, "scope": scope,
    }))
    .into_response()
}

async fn revoke(State(fake): State<Fake>, headers: HeaderMap, Form(q): Form<HashMap<String, String>>) -> Response {
    if headers.get("authorization").and_then(|h| h.to_str().ok()) != Some(&fake.basic) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    fake.calls().revoked.push(q.get("token").cloned().unwrap_or_default());
    StatusCode::OK.into_response()
}

fn fixture(name: &str) -> Value {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../fixtures/bluebutton");
    let bytes = std::fs::read(dir.join(name)).expect("vendored Blue Button fixture");
    serde_json::from_slice(&bytes).expect("FHIR JSON fixture")
}

/// Claims `claim-<i>` for each i, made from the CMS and CARIN examples.
fn claims(ids: std::ops::Range<usize>) -> Vec<Value> {
    let templates = [
        fixture("cms-eob.json"),
        fixture("BB-EOBInpatient1-nonfinancial.json"),
        fixture("BB-EOBPharmacy1-nonfinancial.json"),
    ];
    ids.map(|i| {
        let mut e = templates[i % templates.len()].clone();
        e["id"] = json!(format!("claim-{i}"));
        e["patient"]["reference"] = json!(format!("Patient/{PATIENT}"));
        e
    })
    .collect()
}

async fn search(
    State(fake): State<Fake>,
    Path(kind): Path<String>,
    Query(q): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Response {
    let offset = q.get("_offset").and_then(|s| s.parse::<usize>().ok());
    if kind == "ExplanationOfBenefit" && offset == Some(0) {
        let change = fake.mode().mid_run.take();
        if let Some((pool, sql)) = change {
            sqlx::query(sqlx::AssertSqlSafe(sql))
                .execute(&pool)
                .await
                .expect("a change during the run");
        }
    }
    let mode = fake.mode().clone();
    let mut calls = fake.calls();
    let bearer = format!("Bearer fake-access-{}", calls.refreshes);
    if !calls.token_issued || headers.get("authorization").and_then(|h| h.to_str().ok()) != Some(&bearer) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let binding = match kind.as_str() {
        "Patient" => q.get("_id").map(String::as_str) == Some(PATIENT),
        "Coverage" => q.get("beneficiary").is_some_and(|p| p == &format!("Patient/{PATIENT}")),
        "ExplanationOfBenefit" => q.get("patient").map(String::as_str) == Some(PATIENT),
        _ => false,
    };
    let Some(offset) = offset else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    if !binding || q.get("_count").map(String::as_str) != Some("50") {
        return StatusCode::BAD_REQUEST.into_response();
    }
    calls.pages.push((kind.clone(), offset));
    let last = || {
        let mut e = fixture("cms-eob.json");
        if mode.wrong_reference {
            e["patient"]["reference"] = json!(format!("https://elsewhere.example/v3/fhir/Patient/{PATIENT}"));
        }
        vec![e]
    };
    let (resources, next) = match (kind.as_str(), mode.eob, offset) {
        ("Patient", _, 0) => {
            let mut p = fixture("cms-patient.json");
            p["name"] = json!([{"given":["Sam"],"family":"Rivera"}]);
            p["birthDate"] = json!("1982-02-02");
            (vec![p], false)
        }
        ("Coverage", _, 0) => (vec![fixture("cms-coverage.json")], false),
        ("ExplanationOfBenefit", Eob::Fails(status), 0) => {
            return StatusCode::from_u16(status).expect("an HTTP status").into_response();
        }
        ("ExplanationOfBenefit", Eob::Short, 0) => (claims(0..30), true),
        ("ExplanationOfBenefit", Eob::Short, 30) => (last(), false),
        ("ExplanationOfBenefit", Eob::Oversized, 0) => (claims(0..51), false),
        ("ExplanationOfBenefit", Eob::Repeated, 0 | 50) => (claims(0..50), true),
        ("ExplanationOfBenefit", Eob::Repeated, 100) => (last(), false),
        ("ExplanationOfBenefit", Eob::Full | Eob::ForeignUrl, 0) => (claims(0..50), true),
        ("ExplanationOfBenefit", Eob::Full | Eob::ForeignUrl, 50) => (last(), false),
        _ => (vec![], false),
    };
    let n = resources.len();
    let entries: Vec<Value> = resources
        .into_iter()
        .map(|r| {
            let mut entry = json!({"resource": r, "search": {"mode": "match"}});
            if mode.eob == Eob::ForeignUrl && kind == "ExplanationOfBenefit" {
                let id = r["id"].as_str().unwrap_or_default();
                entry["fullUrl"] = if id == "claim-3" {
                    json!(format!("https://elsewhere.example/v3/fhir/{kind}/{id}"))
                } else {
                    json!(format!("{}/{kind}/{id}", fake.issuer))
                };
            }
            entry
        })
        .collect();
    // CMS may report this page's count as total, and its published v3 examples omit fullUrl.
    // The client must never follow the origin supplied in a pagination link.
    let links = if next {
        json!([{"relation":"next","url":format!("https://elsewhere.example/v3/fhir/{kind}/?_count=50&_offset={}", offset + n)}])
    } else {
        json!([])
    };
    Json(json!({"resourceType":"Bundle","type":"searchset","total":n,"entry":entries,"link":links})).into_response()
}

fn router(fake: Fake) -> Router {
    Router::new()
        .route("/v3/o/authorize/", get(authorize))
        .route("/v3/o/token/", post(token))
        .route("/v3/o/revoke_token/", post(revoke))
        .route("/v3/fhir/{kind}/", get(search))
        .with_state(fake)
}

fn serve(listener: tokio::net::TcpListener, fake: Fake) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move { axum::serve(listener, router(fake)).await.expect("serve fake CMS") })
}

struct Setup {
    env: common::Env,
    fake: Fake,
    ctx: JobContext,
    key: SecretBox,
    source_id: Uuid,
    profile: Uuid,
    client: ConnectorClient,
    endpoints: Endpoints,
    addr: SocketAddr,
    server: tokio::task::JoinHandle<()>,
}

impl Drop for Setup {
    fn drop(&mut self) {
        self.server.abort();
    }
}

/// A sign-in at the fake: the launch, the browser's visit to the authorize page, and the state that
/// comes back. Returns the launch and the code to exchange.
async fn sign_in(
    env: &common::Env,
    client: &ConnectorClient,
    endpoints: &Endpoints,
    profile: Uuid,
    source_id: Option<Uuid>,
) -> anyhow::Result<(oauth::Pending, String)> {
    let mut tx = env.tx().await?;
    let launch = oauth::begin(
        &mut tx,
        &Launch {
            kind: "bluebutton",
            client,
            profile_id: profile,
            user_id: None,
            session_id: None,
            label: "Medicare",
            source_id,
            binder: BINDER,
            endpoints,
        },
    )
    .await?;
    tx.commit().await?;
    let http = oauth::SourceHttp::new(endpoints.hosts(), egress());
    let short = launch.url.replace(&launch.state, "short");
    assert_eq!(http.get(&short, &[]).await?.status, 400);
    let grant = http.get(&launch.url, &[]).await?;
    assert_eq!(grant.status, 200);
    let code: Value = serde_json::from_slice(&grant.body)?;
    let mut tx = env.tx().await?;
    let pending = oauth::take_state(&mut tx, &launch.state, BINDER).await?;
    tx.commit().await?;
    Ok((pending, code["code"].as_str().expect("the fake issued a code").into()))
}

async fn setup(mode: Mode) -> anyhow::Result<Setup> {
    let env = common::Env::new().await?;
    let mut tx = env.tx().await?;
    let family = common::family(&mut tx).await?;
    let profile = family.profile("sam");
    sqlx::query("update profile set birth_date = '1982-02-02' where id = $1")
        .bind(profile)
        .execute(&mut *tx)
        .await?;
    let settings = env.db.settings(env.dir.path())?;
    let key = SecretBox::from_settings(&settings)?.with_keys(env.blobs.keyring());
    let ctx = JobContext::new(env.db.pool.clone(), Arc::new(settings), env.blobs.clone());
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await?;
    let addr = listener.local_addr()?;
    let secret = ownhealth_core::ids::token_hex(24);
    let fake = Fake {
        issuer: format!("http://{addr}/v3/fhir"),
        basic: format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD.encode(format!("{CLIENT_ID}:{secret}"))
        ),
        calls: Arc::new(Mutex::new(Calls::default())),
        mode: Arc::new(Mutex::new(mode)),
    };
    let server = serve(listener, fake.clone());
    let secret_id = secrets::store_secret(
        &mut tx,
        &key,
        family.household_id,
        "connector_client_secret",
        None,
        &SecretString::new(secret),
    )
    .await?;
    let client: Uuid = sqlx::query_scalar(
        "insert into connector_client (household_id, vendor, issuer, client_id, client_secret_id, redirect_uri)
         values ($1, 'bluebutton', $2, $3, $4, $5) returning id",
    )
    .bind(family.household_id)
    .bind(&fake.issuer)
    .bind(CLIENT_ID)
    .bind(secret_id)
    .bind(REDIRECT)
    .fetch_one(&mut *tx)
    .await?;
    let client = oauth::load_client(&mut tx, Some(&key), family.household_id, client)
        .await?
        .expect("registered Medicare client");
    tx.commit().await?;
    let spec = oauth::kind_spec("bluebutton").expect("Blue Button is available for launch");
    let endpoints = oauth::resolve(&spec, &fake.issuer, &egress()).await?;
    let (pending, code) = sign_in(&env, &client, &endpoints, profile, None).await?;
    let tokens = oauth::exchange(&pending, &client, &code, &egress()).await?;
    let mut tx = env.tx().await?;
    let source_id = oauth::save_connection(&mut tx, &key, &pending, &client, &tokens, Utc::now()).await?;
    tx.commit().await?;
    Ok(Setup {
        env,
        fake,
        ctx,
        key,
        source_id,
        profile,
        client,
        endpoints,
        addr,
        server,
    })
}

fn egress() -> Egress {
    Egress {
        allow_private: true,
        https_only: false,
        ..Egress::connector()
    }
}

async fn needs_consent(setup: &Setup) -> anyhow::Result<bool> {
    Ok(
        sqlx::query_scalar("select reconsent_needed_at is not null from source where id = $1")
            .bind(setup.source_id)
            .fetch_one(&setup.env.db.pool)
            .await?,
    )
}

#[tokio::test]
async fn v3_refreshes_once_and_never_retries_a_spent_token() -> common::R {
    let setup = setup(Mode {
        broken_refresh: true,
        ..Mode::default()
    })
    .await?;
    let now = Utc::now() + Duration::hours(2);
    for _ in 0..2 {
        assert!(matches!(
            oauth::access_token(&setup.ctx.pool, &setup.key, setup.source_id, &egress(), now).await,
            Err(OAuthError::ReconsentNeeded)
        ));
    }
    assert_eq!(setup.fake.calls().refreshes, 1);
    assert!(needs_consent(&setup).await?);
    Ok(())
}

#[tokio::test]
async fn a_refresh_that_never_reached_cms_keeps_the_connection() -> common::R {
    let mut setup = setup(Mode::default()).await?;
    let now = Utc::now() + Duration::hours(2);
    // CMS is down: nothing listens on its port, so the single-use token never leaves
    let stopped = std::mem::replace(&mut setup.server, tokio::spawn(async {}));
    stopped.abort();
    let _ = stopped.await;
    let refused = oauth::access_token(&setup.ctx.pool, &setup.key, setup.source_id, &egress(), now).await;
    assert!(matches!(refused, Err(OAuthError::Net(_))), "{refused:?}");
    assert!(!needs_consent(&setup).await?);
    // the poller retries later instead of asking for a new Medicare sign-in
    let source = get_source(&mut *setup.ctx.pool.acquire().await?, setup.source_id)
        .await?
        .expect("connected source");
    let stop = bluebutton::pull(&setup.ctx, &source, &egress(), now, 200)
        .await
        .err()
        .expect("CMS is down");
    assert!(
        !stop.permanent && stop.message.starts_with("token_unreachable"),
        "{}",
        stop.message
    );
    assert!(!needs_consent(&setup).await?);
    // CMS is back: the same refresh token is spent once and the connection goes on
    let listener = tokio::net::TcpListener::bind(setup.addr).await?;
    setup.server = serve(listener, setup.fake.clone());
    assert!(
        oauth::access_token(&setup.ctx.pool, &setup.key, setup.source_id, &egress(), now)
            .await
            .is_ok()
    );
    assert_eq!(setup.fake.calls().refreshes, 1);
    assert!(!needs_consent(&setup).await?);
    Ok(())
}

async fn pull(setup: &Setup, pages: usize) -> anyhow::Result<sources::RunOutcome> {
    let mut conn = setup.ctx.pool.acquire().await?;
    let source = get_source(&mut conn, setup.source_id).await?.expect("connected source");
    let run = sources::begin_run(&mut conn, source.id, "poll", None).await?;
    drop(conn);
    let outcome = bluebutton::pull(&setup.ctx, &source, &egress(), Utc::now(), pages).await?;
    sources::finish_run(&mut *setup.ctx.pool.acquire().await?, run, Ok(&outcome)).await?;
    Ok(outcome)
}

async fn groups(setup: &Setup) -> anyhow::Result<Vec<(String, i64)>> {
    Ok(sqlx::query_as(
        "select payload->>'resource_type', (payload->>'count')::bigint          from review_item where kind = 'fhir_unmapped_summary' and payload#>>'{source,id}' = $1 order by 1",
    )
    .bind(setup.source_id.to_string())
    .fetch_all(&setup.ctx.pool)
    .await?)
}

#[tokio::test]
async fn v3_is_registered_to_pull_medicare_claims() -> common::R {
    let setup = setup(Mode::default()).await?;
    assert!(sources::poller_for("bluebutton").is_some());
    let first = pull(&setup, 200).await?;
    assert!(first.problems.is_empty(), "{:?}", first.problems);
    assert_eq!((first.seen, first.new, first.duplicate), (52, 52, 0));
    assert_eq!(first.skipped, [("context".into(), 1)].into());
    assert_eq!(first.cursor, Some(json!({"claim_count": 51})));
    assert_eq!(
        setup.fake.calls().pages,
        [
            ("Patient".into(), 0),
            ("Coverage".into(), 0),
            ("ExplanationOfBenefit".into(), 0),
            ("ExplanationOfBenefit".into(), 50)
        ],
    );
    let second = pull(&setup, 200).await?;
    assert_eq!((second.seen, second.new, second.duplicate), (52, 0, 52));
    assert_eq!(second.cursor, first.cursor);
    assert_eq!(
        groups(&setup).await?,
        [("Coverage".into(), 1), ("ExplanationOfBenefit".into(), 51)],
    );
    let rows: Vec<(Uuid, String, i64)> = sqlx::query_as(
        "select distinct d.profile_id, encode(d.blob_sha256,'hex'), (d.flags->>'unprojected')::bigint          from document d join document_source s on d.id = s.document_id where s.channel = 'bluebutton'",
    )
    .fetch_all(&setup.ctx.pool)
    .await?;
    assert_eq!(rows.len(), 1);
    let (profile, sha, held) = &rows[0];
    assert_eq!(*held, 52);
    let owner = ownhealth_core::blobs::BlobOwner::Profile(*profile);
    assert!(setup.ctx.blobs.is_sealed(owner, sha)?);
    let stored: Value = serde_json::from_slice(&setup.ctx.blobs.get_bytes(owner, sha)?)?;
    assert_eq!(stored["entry"].as_array().expect("stored bundle entries").len(), 53);
    Ok(())
}

#[tokio::test]
async fn a_pull_whose_claim_search_failed_shows_no_older_claim_count() -> common::R {
    let setup = setup(Mode::default()).await?;
    assert_eq!(pull(&setup, 200).await?.cursor, Some(json!({"claim_count": 51})));
    setup.fake.mode().eob = Eob::Fails(500);
    let failed = pull(&setup, 200).await?;
    assert_eq!(failed.problems[0].code, "search_failed");
    assert_eq!(failed.cursor, Some(json!({"claim_count": null})));
    let shown: Option<i32> = sqlx::query_scalar("select (cursor->>'claim_count')::int4 from source where id = $1")
        .bind(setup.source_id)
        .fetch_one(&setup.ctx.pool)
        .await?;
    assert_eq!(shown, None);
    Ok(())
}

#[tokio::test]
async fn v3_page_cap_and_foreign_patient_reference_are_visible() -> common::R {
    let setup = setup(Mode {
        wrong_reference: true,
        ..Mode::default()
    })
    .await?;
    let capped = pull(&setup, 1).await?;
    assert_eq!(capped.cursor, Some(json!({"claim_count": 50})));
    assert_eq!((capped.seen, capped.new), (51, 51));
    assert_eq!(capped.problems[0].code, "too_many_pages");
    let next = pull(&setup, 200).await?;
    assert_eq!(next.cursor, capped.cursor);
    assert_eq!((next.seen, next.new, next.duplicate), (51, 0, 51));
    assert_eq!(next.skipped["patient_reference"], 1);
    assert_eq!(next.problems[0].code, "patient_reference");
    let runs = sources::list_runs(&mut *setup.ctx.pool.acquire().await?, setup.source_id, 2).await?;
    assert!(runs.iter().all(|r| r.status == "partial"));
    Ok(())
}

#[tokio::test]
async fn concurrent_refreshes_reuse_the_sealed_replacement() -> common::R {
    let setup = setup(Mode::default()).await?;
    let now = Utc::now() + Duration::hours(2);
    let egress = egress();
    let refresh = || oauth::access_token(&setup.ctx.pool, &setup.key, setup.source_id, &egress, now);
    let (a, b) = tokio::join!(refresh(), refresh());
    assert!(a?.expose() == b?.expose());
    assert_eq!(setup.fake.calls().refreshes, 1);
    assert!(refresh().await.is_ok());
    assert_eq!(setup.fake.calls().refreshes, 1);
    Ok(())
}

#[tokio::test]
async fn a_sign_in_that_names_no_patient_is_refused_and_its_tokens_revoked() -> common::R {
    let setup = setup(Mode::default()).await?;
    setup.fake.mode().no_patient = true;
    let (pending, code) = sign_in(
        &setup.env,
        &setup.client,
        &setup.endpoints,
        setup.profile,
        Some(setup.source_id),
    )
    .await?;
    let refused = oauth::exchange(&pending, &setup.client, &code, &egress()).await;
    assert!(
        matches!(refused, Err(OAuthError::Refused { code: "no_patient", .. })),
        "{refused:?}"
    );
    // nothing at CMS stays alive for a grant OwnHealth refused; the connection keeps its own token
    assert_eq!(setup.fake.calls().revoked, ["fake-orphan"]);
    Ok(())
}

/// A change to the connection or the profile while the run reads (switched off, a new sign-in, a
/// new birth date) leaves nothing imported.
#[tokio::test]
async fn a_change_during_the_run_imports_nothing() -> common::R {
    let setup = setup(Mode::default()).await?;
    let (source, profile) = (setup.source_id, setup.profile);
    let cases = [
        (
            format!("update profile set birth_date = '1990-03-03' where id = '{profile}'"),
            format!("update profile set birth_date = '1982-02-02' where id = '{profile}'"),
            "profile_changed",
        ),
        (
            format!("update source set enabled = false where id = '{source}'"),
            format!("update source set enabled = true where id = '{source}'"),
            "grant_changed",
        ),
        (
            format!("update source set oauth_grant_id = gen_random_uuid() where id = '{source}'"),
            "select 1".into(),
            "grant_changed",
        ),
    ];
    for (during, undo, code) in cases {
        setup.fake.mode().mid_run = Some((setup.ctx.pool.clone(), during.clone()));
        let current = get_source(&mut *setup.ctx.pool.acquire().await?, source)
            .await?
            .expect("connected source");
        let stop: JobError = match bluebutton::pull(&setup.ctx, &current, &egress(), Utc::now(), 200).await {
            Ok(outcome) => anyhow::bail!("{during}: imported {outcome:?}"),
            Err(e) => e,
        };
        assert!(stop.message.starts_with(code), "{during}: {}", stop.message);
        assert!(setup.fake.mode().mid_run.is_none(), "{during}: the claim search ran");
        sqlx::query(sqlx::AssertSqlSafe(undo)).execute(&setup.ctx.pool).await?;
    }
    let imported: i64 = sqlx::query_scalar("select count(*) from document_source where channel = 'bluebutton'")
        .fetch_one(&setup.ctx.pool)
        .await?;
    assert_eq!(imported, 0);
    Ok(())
}

/// The second refresh waits for the first one's whole single-use exchange, so it never reads the
/// consent marker the first one commits before it sends the token.
#[tokio::test]
async fn a_waiting_refresh_never_reads_the_marker_of_one_in_flight() -> common::R {
    let setup = setup(Mode::default()).await?;
    let now = Utc::now() + Duration::hours(2);
    let egress = egress();
    let pool = &setup.ctx.pool;
    // hold the row, so both refreshes queue before either one starts
    let mut hold = pool.begin().await?;
    sqlx::query("select 1 from source where id = $1 for update")
        .bind(setup.source_id)
        .execute(&mut *hold)
        .await?;
    let refresh = || oauth::access_token(pool, &setup.key, setup.source_id, &egress, now);
    let release = async {
        let until = std::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            let waiting: i64 = sqlx::query_scalar(
                "select count(*) from pg_stat_activity where datname = current_database() and wait_event_type = 'Lock'",
            )
            .fetch_one(pool)
            .await?;
            if waiting >= 2 {
                break;
            }
            anyhow::ensure!(std::time::Instant::now() < until, "the two refreshes never queued");
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        hold.rollback().await?;
        anyhow::Ok(())
    };
    let (a, b, released) = tokio::join!(refresh(), refresh(), release);
    released?;
    let (a, b) = (a?, b?);
    assert!(a.expose() == b.expose());
    assert_eq!(setup.fake.calls().refreshes, 1);
    assert!(!needs_consent(&setup).await?);
    Ok(())
}

/// A refresh answer that keeps the spent token or names another patient is never stored.
#[tokio::test]
async fn a_refresh_answer_that_does_not_rotate_or_names_another_patient_asks_for_consent() -> common::R {
    for mode in [
        Mode {
            same_refresh: true,
            ..Mode::default()
        },
        Mode {
            other_patient: true,
            ..Mode::default()
        },
    ] {
        let setup = setup(mode).await?;
        let now = Utc::now() + Duration::hours(2);
        let answer = oauth::access_token(&setup.ctx.pool, &setup.key, setup.source_id, &egress(), now).await;
        assert!(matches!(answer, Err(OAuthError::ReconsentNeeded)), "{answer:?}");
        assert_eq!(setup.fake.calls().refreshes, 1);
        assert!(needs_consent(&setup).await?);
        let token: Option<Vec<u8>> = sqlx::query_scalar("select oauth_access_token from source where id = $1")
            .bind(setup.source_id)
            .fetch_one(&setup.ctx.pool)
            .await?;
        assert!(token.is_none());
    }
    Ok(())
}

/// A client registered for another Blue Button server never sends the refresh token.
#[tokio::test]
async fn a_refresh_with_a_client_of_another_server_sends_nothing() -> common::R {
    let setup = setup(Mode::default()).await?;
    sqlx::query("update connector_client set issuer = 'https://sandbox.bluebutton.cms.gov/v3/fhir' where id = $1")
        .bind(setup.client.id)
        .execute(&setup.ctx.pool)
        .await?;
    let now = Utc::now() + Duration::hours(2);
    let answer = oauth::access_token(&setup.ctx.pool, &setup.key, setup.source_id, &egress(), now).await;
    assert!(
        matches!(answer, Err(OAuthError::Refused { code: "bad_issuer", .. })),
        "{answer:?}"
    );
    assert_eq!(setup.fake.calls().refreshes, 0);
    assert!(!needs_consent(&setup).await?);
    Ok(())
}

/// The claim search's page edges: a short page with a next link, a foreign fullUrl, a repeated page,
/// and a page over the count.
#[tokio::test]
async fn v3_claim_pages_follow_the_count_and_refuse_odd_pages() -> common::R {
    let setup = setup(Mode::default()).await?;
    let claim_pages = |setup: &Setup| -> Vec<usize> {
        let mut calls = setup.fake.calls();
        let pages = calls
            .pages
            .iter()
            .filter(|(kind, _)| kind == "ExplanationOfBenefit")
            .map(|(_, offset)| *offset)
            .collect();
        calls.pages.clear();
        pages
    };
    let code = |o: &sources::RunOutcome| o.problems.first().map(|p| p.code.clone());

    setup.fake.mode().eob = Eob::Short;
    let short = pull(&setup, 200).await?;
    assert_eq!(claim_pages(&setup), [0, 30]);
    assert!(short.problems.is_empty(), "{:?}", short.problems);
    assert_eq!(short.cursor, Some(json!({"claim_count": 31})));

    setup.fake.mode().eob = Eob::ForeignUrl;
    let foreign = pull(&setup, 200).await?;
    assert_eq!(claim_pages(&setup), [0, 50]);
    assert_eq!(code(&foreign).as_deref(), Some("resource_url"));
    assert_eq!(foreign.skipped["resource_url"], 1);
    assert_eq!(foreign.cursor, Some(json!({"claim_count": 50})));

    setup.fake.mode().eob = Eob::Repeated;
    let repeated = pull(&setup, 200).await?;
    assert_eq!(claim_pages(&setup), [0, 50]);
    assert_eq!(code(&repeated).as_deref(), Some("repeated_page"));
    assert_eq!(repeated.cursor, Some(json!({"claim_count": 50})));

    setup.fake.mode().eob = Eob::Oversized;
    let oversized = pull(&setup, 200).await?;
    assert_eq!(claim_pages(&setup), [0]);
    assert_eq!(code(&oversized).as_deref(), Some("not_fhir"));
    assert_ne!(oversized.cursor, Some(json!({"claim_count": 51})));
    Ok(())
}

/// A person who withheld the claims at Medicare: the claim search is never sent.
#[tokio::test]
async fn a_grant_without_claims_never_searches_them() -> common::R {
    let setup = setup(Mode {
        scope: Some("launch/patient patient/Patient.rs patient/Coverage.rs".into()),
        ..Mode::default()
    })
    .await?;
    let outcome = pull(&setup, 200).await?;
    assert_eq!(
        setup.fake.calls().pages,
        [("Patient".into(), 0), ("Coverage".into(), 0)]
    );
    assert_eq!(outcome.problems[0].code, "bluebutton_not_granted");
    assert_eq!(outcome.skipped["not_granted"], 1);
    Ok(())
}
