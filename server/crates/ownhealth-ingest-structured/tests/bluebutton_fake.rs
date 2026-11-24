//! CMS v3 over loopback with synthetic CMS and HL7 data. No vendor call reaches the internet.

mod common;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use axum::extract::{Form, Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use base64::Engine as _;
use chrono::{Duration, Utc};
use ownhealth_core::connector_oauth::{self as oauth, Launch, OAuthError};
use ownhealth_core::jobs::JobContext;
use ownhealth_core::net::Egress;
use ownhealth_core::secrets::{self, SecretBox, SecretString};
use ownhealth_core::sources::{self, get_source};
use ownhealth_ingest_structured::bluebutton;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
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
}

#[derive(Clone)]
struct Fake {
    issuer: String,
    basic: String,
    calls: Arc<Mutex<Calls>>,
    broken_refresh: bool,
    wrong_reference: bool,
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
    fake.calls.lock().expect("fake CMS request log").challenge = q.get("code_challenge").cloned();
    Json(json!({"code": "fake-code"})).into_response()
}

async fn token(State(fake): State<Fake>, headers: HeaderMap, Form(q): Form<HashMap<String, String>>) -> Response {
    if headers.get("authorization").and_then(|h| h.to_str().ok()) != Some(&fake.basic)
        || q.contains_key("client_secret")
    {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let mut calls = fake.calls.lock().expect("fake CMS request log");
    let n = match q.get("grant_type").map(String::as_str) {
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
            0
        }
        Some("refresh_token") => {
            let old = calls.refreshes;
            calls.refreshes += 1;
            if !calls.token_issued || q.get("refresh_token") != Some(&format!("fake-rotation-{old}")) {
                return (StatusCode::BAD_REQUEST, Json(json!({"error": "invalid_grant"}))).into_response();
            }
            if fake.broken_refresh {
                // The vendor retired the old token but the caller received no replacement.
                return (StatusCode::BAD_GATEWAY, "broken refresh response").into_response();
            }
            calls.refreshes
        }
        _ => return StatusCode::BAD_REQUEST.into_response(),
    };
    Json(json!({
        "access_token": format!("fake-access-{n}"), "refresh_token": format!("fake-rotation-{n}"),
        "token_type": "Bearer", "expires_in": 3600, "patient": PATIENT, "scope": SCOPE,
    }))
    .into_response()
}

fn fixture(name: &str) -> Value {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../fixtures/bluebutton");
    let bytes = std::fs::read(dir.join(name)).expect("vendored Blue Button fixture");
    serde_json::from_slice(&bytes).expect("FHIR JSON fixture")
}

async fn search(
    State(fake): State<Fake>,
    Path(kind): Path<String>,
    Query(q): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Response {
    let mut calls = fake.calls.lock().expect("fake CMS request log");
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
    let Some(offset) = q.get("_offset").and_then(|s| s.parse::<usize>().ok()) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    if !binding || q.get("_count").map(String::as_str) != Some("50") {
        return StatusCode::BAD_REQUEST.into_response();
    }
    calls.pages.push((kind.clone(), offset));
    let resources = match (kind.as_str(), offset) {
        ("Patient", 0) => {
            let mut p = fixture("cms-patient.json");
            p["name"] = json!([{"given":["Sam"],"family":"Rivera"}]);
            p["birthDate"] = json!("1982-02-02");
            vec![p]
        }
        ("Coverage", 0) => vec![fixture("cms-coverage.json")],
        ("ExplanationOfBenefit", 0) => {
            let templates = [
                fixture("cms-eob.json"),
                fixture("BB-EOBInpatient1-nonfinancial.json"),
                fixture("BB-EOBPharmacy1-nonfinancial.json"),
            ];
            (0..50)
                .map(|i| {
                    let mut e = templates[i % templates.len()].clone();
                    e["id"] = json!(format!("claim-{i}"));
                    e["patient"]["reference"] = json!(format!("Patient/{PATIENT}"));
                    e
                })
                .collect()
        }
        ("ExplanationOfBenefit", 50) => {
            let mut e = fixture("cms-eob.json");
            if fake.wrong_reference {
                e["patient"]["reference"] = json!(format!("https://elsewhere.example/v3/fhir/Patient/{PATIENT}"));
            }
            vec![e]
        }
        _ => vec![],
    };
    let n = resources.len();
    let entries: Vec<Value> = resources
        .into_iter()
        .map(|r| json!({"resource": r, "search": {"mode": "match"}}))
        .collect();
    // CMS may report this page's count as total, and its published v3 examples omit fullUrl.
    // The client must never follow the origin supplied in a pagination link.
    let links = if kind == "ExplanationOfBenefit" && offset == 0 {
        json!([{"relation":"next","url":format!("https://elsewhere.example/v3/fhir/{kind}/?_count=50&_offset=50")}])
    } else {
        json!([])
    };
    Json(json!({"resourceType":"Bundle","type":"searchset","total":n,"entry":entries,"link":links})).into_response()
}

struct Setup {
    env: common::Env,
    fake: Fake,
    ctx: JobContext,
    key: SecretBox,
    source_id: Uuid,
    server: tokio::task::JoinHandle<()>,
}

impl Drop for Setup {
    fn drop(&mut self) {
        self.server.abort();
    }
}

async fn setup(broken_refresh: bool, wrong_reference: bool) -> anyhow::Result<Setup> {
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
    let root = format!("http://{}", listener.local_addr()?);
    let secret = ownhealth_core::ids::token_hex(24);
    let fake = Fake {
        issuer: format!("{root}/v3/fhir"),
        basic: format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD.encode(format!("{CLIENT_ID}:{secret}"))
        ),
        calls: Arc::new(Mutex::new(Calls::default())),
        broken_refresh,
        wrong_reference,
    };
    let router = Router::new()
        .route("/v3/o/authorize/", get(authorize))
        .route("/v3/o/token/", post(token))
        .route("/v3/fhir/{kind}/", get(search))
        .with_state(fake.clone());
    let server = tokio::spawn(async move { axum::serve(listener, router).await.expect("serve fake CMS") });
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
    let spec = oauth::kind_spec("bluebutton").expect("Blue Button is available for launch");
    let endpoints = oauth::resolve(&spec, &fake.issuer, &egress()).await?;
    let launch = oauth::begin(
        &mut tx,
        &Launch {
            kind: "bluebutton",
            client: &client,
            profile_id: profile,
            user_id: None,
            session_id: None,
            label: "Medicare",
            source_id: None,
            binder: BINDER,
            endpoints: &endpoints,
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
    let tokens = oauth::exchange(
        &pending,
        &client,
        code["code"].as_str().expect("the fake issued a code"),
        &egress(),
    )
    .await?;
    let source_id = oauth::save_connection(&mut tx, &key, &pending, &client, &tokens, Utc::now()).await?;
    tx.commit().await?;
    Ok(Setup {
        env,
        fake,
        ctx,
        key,
        source_id,
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

#[tokio::test]
async fn v3_refreshes_once_and_never_retries_a_spent_token() -> common::R {
    let setup = setup(true, false).await?;
    let now = Utc::now() + Duration::hours(2);
    for _ in 0..2 {
        assert!(matches!(
            oauth::access_token(&setup.ctx.pool, &setup.key, setup.source_id, &egress(), now).await,
            Err(OAuthError::ReconsentNeeded)
        ));
    }
    assert_eq!(setup.fake.calls.lock().expect("fake CMS request log").refreshes, 1);
    let needs_consent: bool = sqlx::query_scalar("select reconsent_needed_at is not null from source where id = $1")
        .bind(setup.source_id)
        .fetch_one(&setup.env.db.pool)
        .await?;
    assert!(needs_consent);
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
    let setup = setup(false, false).await?;
    assert!(sources::poller_for("bluebutton").is_some());
    let first = pull(&setup, 200).await?;
    assert!(first.problems.is_empty(), "{:?}", first.problems);
    assert_eq!((first.seen, first.new, first.duplicate), (52, 52, 0));
    assert_eq!(first.skipped, [("context".into(), 1)].into());
    assert_eq!(first.cursor, Some(json!({"claim_count": 51})));
    assert_eq!(
        setup.fake.calls.lock().expect("fake CMS request log").pages,
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
async fn v3_page_cap_and_foreign_patient_reference_are_visible() -> common::R {
    let setup = setup(false, true).await?;
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
    let setup = setup(false, false).await?;
    let now = Utc::now() + Duration::hours(2);
    let egress = egress();
    let refresh = || oauth::access_token(&setup.ctx.pool, &setup.key, setup.source_id, &egress, now);
    let (a, b) = tokio::join!(refresh(), refresh());
    assert!(a?.expose() == b?.expose());
    assert_eq!(setup.fake.calls.lock().expect("fake CMS request log").refreshes, 1);
    assert!(refresh().await.is_ok());
    assert_eq!(setup.fake.calls.lock().expect("fake CMS request log").refreshes, 1);
    Ok(())
}
