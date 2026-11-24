//! SMART Health Links received (slice 23), against a fake link server these tests run on loopback.
//! Spec: HL7 FHIR IG "SMART Health Cards and Links", Health Links Specification. Nothing here
//! reaches the internet.

mod common;

use std::collections::HashSet;
use std::io::Write as _;
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::extract::{Request, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use chrono::{DateTime, Utc};
use common::{Env, R, family};
use ownhealth_core::access::{Profile, get_profile};
use ownhealth_core::blobs::BlobOwner;
use ownhealth_core::jose;
use ownhealth_core::net::Egress;
use ownhealth_ingest_structured::shc;
use ownhealth_ingest_structured::shl_receive::{
    self as shl, CardsChecked, Held, Link, MAX_BYTES, Opened, Refusal, Stop,
};
use p256::ecdsa::signature::Signer as _;
use p256::ecdsa::{Signature, SigningKey};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

const SHL: &str = "https://shl.labnorte.example";
const SHL_HOST: &str = "shl.labnorte.example";
const ISS: &str = "https://issuer.labnorte.example";
const ISS_HOST: &str = "issuer.labnorte.example";
/// An obviously fake test passcode.
const PIN: &str = "test-pin-1234";
const KEY: [u8; 32] = [7; 32];

fn b64(bytes: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(bytes)
}

/// 2026-11-21 12:00 UTC.
fn now() -> DateTime<Utc> {
    DateTime::from_timestamp(1_795_262_400, 0).unwrap_or_default()
}

/// A link as ownhealth-ops `shl::shlink` writes it: url, key, exp, flag, label.
fn shlink(url: &str, flag: Option<&str>) -> String {
    let mut p = json!({"url": url, "key": b64(&KEY), "exp": now().timestamp() + 86_400, "label": "LabNorte"});
    if let Some(f) = flag {
        p["flag"] = json!(f);
    }
    format!("shlink:/{}", b64(p.to_string().as_bytes()))
}

fn link(url: &str, flag: Option<&str>) -> anyhow::Result<Link> {
    Ok(shl::parse(&shlink(url, flag), now())?)
}

fn bundle() -> Value {
    json!({"resourceType": "Bundle", "type": "collection", "entry": [
        {"fullUrl": "resource:0", "resource": {"resourceType": "Patient",
            "name": [{"family": "Rivera", "given": ["Alex"]}], "birthDate": "1979-05-14"}},
        {"fullUrl": "resource:1", "resource": {"resourceType": "Immunization", "status": "completed",
            "vaccineCode": {"coding": [{"system": "http://hl7.org/fhir/sid/cvx", "code": "207"}]},
            "patient": {"reference": "resource:0"}, "occurrenceDateTime": "2026-03-02",
            "lotNumber": "LN-0042", "performer": [{"actor": {"display": "LabNorte"}}]}}
    ]})
}

fn jwe(plaintext: &[u8], cty: Option<&str>) -> String {
    jose::encrypt(&KEY, plaintext, cty, true).expect("jwe")
}

fn fhir_jwe() -> String {
    jwe(bundle().to_string().as_bytes(), Some("application/fhir+json"))
}

// --------------------------------------------------------------- the fake ----

/// One request the fake saw: method, path and query, content type, body.
type Logged = (String, String, String, String);

/// A fake link server: `POST /m` answers the manifest, `GET /u` a U link's file, each file path is
/// served ONCE (a single-use location), `/.well-known/jwks.json` an issuer's keys. It logs every
/// request as (method, path and query, content type, body).
#[derive(Clone, Default)]
struct Fake {
    manifest: Option<(u16, Value)>,
    files: Vec<(String, String)>,
    direct: Option<String>,
    jwks: Option<Value>,
    log: Arc<Mutex<Vec<Logged>>>,
    served: Arc<Mutex<HashSet<String>>>,
}

impl Fake {
    fn requests(&self) -> Vec<Logged> {
        self.log.lock().map(|l| l.clone()).unwrap_or_default()
    }

    fn count(&self, method: &str, path: &str) -> usize {
        self.requests()
            .iter()
            .filter(|(m, p, _, _)| m == method && p.split('?').next() == Some(path))
            .count()
    }
}

async fn handle(State(f): State<Fake>, req: Request) -> Response {
    let method = req.method().to_string();
    let path = req.uri().path().to_string();
    let pq = req.uri().path_and_query().map(ToString::to_string).unwrap_or_default();
    let ct = req
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    let body = axum::body::to_bytes(req.into_body(), 1 << 20).await.unwrap_or_default();
    if let Ok(mut log) = f.log.lock() {
        log.push((method.clone(), pq, ct, String::from_utf8_lossy(&body).into_owned()));
    }
    let jose = |b: String| ([(header::CONTENT_TYPE, "application/jose")], b).into_response();
    match (method.as_str(), path.as_str()) {
        ("POST", "/m") => match &f.manifest {
            Some((status, v)) => (
                StatusCode::from_u16(*status).unwrap_or(StatusCode::OK),
                [(header::LOCATION, "/elsewhere")],
                axum::Json(v.clone()),
            )
                .into_response(),
            None => StatusCode::NOT_FOUND.into_response(),
        },
        ("GET", "/u") => f.direct.clone().map_or(StatusCode::NOT_FOUND.into_response(), jose),
        ("GET", "/.well-known/jwks.json") => f
            .jwks
            .clone()
            .map_or(StatusCode::NOT_FOUND.into_response(), |j| axum::Json(j).into_response()),
        ("GET", "/moved") => (StatusCode::FOUND, [(header::LOCATION, "/f/1")]).into_response(),
        ("GET", p) => {
            let first = f.served.lock().map(|mut s| s.insert(p.to_string())).unwrap_or(false);
            match f.files.iter().find(|(fp, _)| fp == p) {
                Some((_, b)) if first => jose(b.clone()),
                _ => StatusCode::NOT_FOUND.into_response(),
            }
        }
        _ => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn spawn(f: &Fake) -> anyhow::Result<u16> {
    let app = Router::new().fallback(handle).with_state(f.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();
    tokio::spawn(async move { axum::serve(listener, app).await });
    Ok(port)
}

/// The test link host and the test issuer, both served from the loopback site; every other URL goes
/// out as in production.
fn local(port: u16) -> impl Fn(&str) -> (String, Egress) + Send + Sync {
    move |url: &str| {
        for origin in [SHL, ISS] {
            if let Some(rest) = url.strip_prefix(origin)
                && rest.starts_with('/')
            {
                let egress = Egress {
                    allow_private: true,
                    https_only: false,
                    ..Egress::connector()
                };
                return (format!("http://127.0.0.1:{port}{rest}"), egress);
            }
        }
        shc::public(url)
    }
}

fn manifest_with(files: Value) -> Option<(u16, Value)> {
    Some((200, json!({"status": "finalized", "files": files})))
}

async fn fetch(
    f: &Fake,
    link: &Link,
    passcode: Option<&str>,
    env: &Env,
    profile: uuid::Uuid,
) -> anyhow::Result<Result<Vec<Held>, Refusal>> {
    let port = spawn(f).await?;
    Ok(shl::fetch(link, passcode, &env.blobs, profile, &local(port)).await?)
}

async fn alex(tx: &mut sqlx::PgConnection) -> anyhow::Result<Profile> {
    let fam = family(&mut *tx).await?;
    Ok(get_profile(&mut *tx, fam.profile("alex")).await?.expect("alex"))
}

// ---------------------------------------------------------------- the tests ----

#[tokio::test]
async fn a_single_use_location_is_fetched_once_and_its_fhir_file_imports_on_channel_shl() -> R {
    let env = Env::new().await?;
    let mut tx = env.tx().await?;
    let profile = alex(&mut tx).await?;
    let file = fhir_jwe();
    let f = Fake {
        manifest: manifest_with(json!([{"contentType": "application/fhir+json;fhirVersion=4.0.1",
            "location": format!("{SHL}/f/1")}])),
        files: vec![("/f/1".into(), file.clone())],
        ..Fake::default()
    };
    let link = link(&format!("{SHL}/m"), Some("P"))?;
    let held = fetch(&f, &link, Some(PIN), &env, profile.id).await??;
    // "SMART Health Link Manifest Request": a JSON POST with the recipient and the passcode
    let reqs = f.requests();
    assert_eq!(reqs.len(), 2, "{reqs:?}");
    let (m, path, ct, body) = &reqs[0];
    assert_eq!(
        (m.as_str(), path.as_str(), ct.as_str()),
        ("POST", "/m", "application/json")
    );
    assert_eq!(
        serde_json::from_str::<Value>(body)?,
        json!({"recipient": "OwnHealth", "passcode": PIN})
    );
    assert_eq!(f.count("GET", "/f/1"), 1, "the location was fetched more than once");
    // bytes first: the file as received is stored for the profile
    assert_eq!(held.len(), 1);
    assert_eq!(held[0].content_type, "application/fhir+json");
    assert_eq!(
        env.blobs.get_bytes(BlobOwner::Profile(profile.id), &held[0].sha)?,
        file.as_bytes()
    );

    let opened = shl::open(&link, &held, &env.blobs, profile.id).map_err(|s| anyhow::anyhow!("{s:?}"))?;
    let CardsChecked::Verified(cards) = shl::check(&opened, &[SHL_HOST.into()], &shc::public, now()).await else {
        panic!("no cards to check");
    };
    let got = shl::store(&mut tx, &env.blobs, &profile, &link, &opened, cards, &[SHL_HOST.into()]).await?;
    assert_eq!(got.documents.len(), 1);
    assert!(got.documents[0].created);
    assert_eq!(got.identity, "matched");
    assert_eq!((got.summary.accepted, got.summary.queued), (1, 0), "{:?}", got.summary);
    assert!(got.not_imported.is_empty());
    let (channel, mime, name, host, received): (String, String, Option<String>, Option<String>, Option<String>) =
        sqlx::query_as(
            "select s.channel, d.mime, d.filename, d.flags -> 'shl' ->> 'host', d.flags -> 'shl' ->> 'received' \
             from document d join document_source s on s.document_id = d.id where d.id = $1",
        )
        .bind(got.documents[0].document_id)
        .fetch_one(&mut *tx)
        .await?;
    assert_eq!((channel.as_str(), mime.as_str()), ("shl", "application/fhir+json"));
    assert_eq!(name.as_deref(), Some("LabNorte"));
    assert_eq!(host.as_deref(), Some(SHL_HOST));
    assert_eq!(received.as_deref(), Some(held[0].sha.as_str()));
    let code: Option<String> = sqlx::query_scalar("select code from immunization where profile_id = $1")
        .bind(profile.id)
        .fetch_one(&mut *tx)
        .await?;
    assert_eq!(code.as_deref(), Some("207"));
    // the passcode is kept nowhere: no row of the profile's documents holds it
    let leaked: i64 = sqlx::query_scalar(
        "select count(*) from document where profile_id = $1 and (flags::text like $2 or coalesce(filename, '') like $2)",
    )
    .bind(profile.id)
    .bind(format!("%{PIN}%"))
    .fetch_one(&mut *tx)
    .await?;
    assert_eq!(leaked, 0);

    // the same file again: the same document, its import not run twice
    let again = shl::store(&mut tx, &env.blobs, &profile, &link, &opened, Vec::new(), &[]).await?;
    assert_eq!(
        (again.documents[0].document_id, again.documents[0].created),
        (got.documents[0].document_id, false)
    );
    assert_eq!(again.summary.accepted, 1, "the first import's summary");
    let n: i64 = sqlx::query_scalar("select count(*) from immunization where profile_id = $1")
        .bind(profile.id)
        .fetch_one(&mut *tx)
        .await?;
    assert_eq!(n, 1);
    Ok(())
}

#[tokio::test]
async fn a_wrong_passcode_says_how_many_attempts_are_left_and_fetches_no_file() -> R {
    let env = Env::new().await?;
    let f = Fake {
        manifest: Some((401, json!({"remainingAttempts": 3}))),
        files: vec![("/f/1".into(), fhir_jwe())],
        ..Fake::default()
    };
    let link = link(&format!("{SHL}/m"), Some("P"))?;
    let got = fetch(&f, &link, Some("test-pin-0000"), &env, uuid::Uuid::new_v4()).await?;
    assert_eq!(got, Err(Refusal::Passcode(Some(3))));
    assert_eq!(Refusal::Passcode(Some(3)).detail().as_deref(), Some("3"));
    assert_eq!(f.requests().len(), 1);
    Ok(())
}

#[tokio::test]
async fn a_passcode_link_without_its_passcode_asks_nothing_of_the_server() -> R {
    let env = Env::new().await?;
    let f = Fake {
        manifest: manifest_with(json!([])),
        ..Fake::default()
    };
    let link = link(&format!("{SHL}/m"), Some("LP"))?;
    for pc in [None, Some("")] {
        let got = fetch(&f, &link, pc, &env, uuid::Uuid::new_v4()).await?;
        assert_eq!(got, Err(Refusal::PasscodeNeeded));
    }
    assert!(f.requests().is_empty());
    Ok(())
}

#[tokio::test]
async fn an_inactive_link_is_refused() -> R {
    let env = Env::new().await?;
    let link = link(&format!("{SHL}/m"), None)?;
    // "if the SMART Health Link is no longer active, the Resource Server SHALL respond with a 404"
    let f = Fake::default();
    assert_eq!(
        fetch(&f, &link, None, &env, uuid::Uuid::new_v4()).await?,
        Err(Refusal::Inactive)
    );
    let f = Fake {
        manifest: Some((200, json!({"status": "no-longer-valid", "files": []}))),
        ..Fake::default()
    };
    assert_eq!(
        fetch(&f, &link, None, &env, uuid::Uuid::new_v4()).await?,
        Err(Refusal::Inactive)
    );
    let f = Fake {
        manifest: manifest_with(json!([])),
        ..Fake::default()
    };
    assert_eq!(
        fetch(&f, &link, None, &env, uuid::Uuid::new_v4()).await?,
        Err(Refusal::NoFiles)
    );
    let f = Fake {
        manifest: Some((429, json!({}))),
        ..Fake::default()
    };
    assert_eq!(
        fetch(&f, &link, None, &env, uuid::Uuid::new_v4()).await?,
        Err(Refusal::RateLimited)
    );
    let many: Vec<Value> = (0..11)
        .map(|_| json!({"contentType": "application/fhir+json", "embedded": "x"}))
        .collect();
    let f = Fake {
        manifest: manifest_with(json!(many)),
        ..Fake::default()
    };
    assert_eq!(
        fetch(&f, &link, None, &env, uuid::Uuid::new_v4()).await?,
        Err(Refusal::TooMany)
    );
    Ok(())
}

#[tokio::test]
async fn a_redirect_is_refused_and_never_followed() -> R {
    let env = Env::new().await?;
    let link = link(&format!("{SHL}/m"), None)?;
    let f = Fake {
        manifest: Some((302, json!({}))),
        ..Fake::default()
    };
    assert_eq!(
        fetch(&f, &link, None, &env, uuid::Uuid::new_v4()).await?,
        Err(Refusal::Redirect)
    );
    assert_eq!(f.requests().len(), 1, "the redirect was followed");
    // a file location that redirects
    let f = Fake {
        manifest: manifest_with(json!([{"contentType": "application/fhir+json", "location": format!("{SHL}/moved")}])),
        files: vec![("/f/1".into(), fhir_jwe())],
        ..Fake::default()
    };
    assert_eq!(
        fetch(&f, &link, None, &env, uuid::Uuid::new_v4()).await?,
        Err(Refusal::Redirect)
    );
    assert_eq!(f.count("GET", "/f/1"), 0, "the redirect was followed");
    Ok(())
}

#[tokio::test]
async fn a_private_host_is_refused_without_a_connection() -> R {
    let env = Env::new().await?;
    let f = Fake {
        manifest: manifest_with(json!([{"contentType": "application/fhir+json", "embedded": fhir_jwe()}])),
        ..Fake::default()
    };
    let port = spawn(&f).await?;
    // a name that resolves to loopback, approved by the person: the egress guard refuses it
    let link = shl::parse(&shlink(&format!("https://localhost:{port}/m"), None), now())?;
    assert_eq!(link.host, format!("localhost:{port}"));
    let got = shl::fetch(&link, None, &env.blobs, uuid::Uuid::new_v4(), &shc::public).await?;
    assert_eq!(got, Err(Refusal::PrivateHost("localhost".into())));
    // a file location on a private address, from a public manifest
    let f2 = Fake {
        manifest: manifest_with(json!([{"contentType": "application/fhir+json",
            "location": format!("https://localhost:{port}/f/1")}])),
        ..Fake::default()
    };
    let link2 = link_at(&f2, &format!("{SHL}/m")).await?;
    let got = shl::fetch(&link2.0, None, &env.blobs, uuid::Uuid::new_v4(), &local(link2.1)).await?;
    assert_eq!(got, Err(Refusal::PrivateHost("localhost".into())));
    assert!(f.requests().is_empty(), "the private address was contacted");
    // a literal private address is refused before anyone is asked
    assert_eq!(
        shl::parse(&shlink(&format!("https://127.0.0.1:{port}/m"), None), now()),
        Err(Refusal::PrivateHost("127.0.0.1".into()))
    );
    Ok(())
}

async fn link_at(f: &Fake, url: &str) -> anyhow::Result<(Link, u16)> {
    Ok((link(url, None)?, spawn(f).await?))
}

#[tokio::test]
async fn a_u_link_is_one_get_with_the_recipient_and_no_manifest_request() -> R {
    let env = Env::new().await?;
    let mut tx = env.tx().await?;
    let profile = alex(&mut tx).await?;
    let f = Fake {
        manifest: manifest_with(json!([])),
        direct: Some(fhir_jwe()),
        ..Fake::default()
    };
    let link = link(&format!("{SHL}/u"), Some("U"))?;
    let held = fetch(&f, &link, None, &env, profile.id).await??;
    let reqs = f.requests();
    assert_eq!(reqs.len(), 1, "{reqs:?}");
    assert_eq!(
        (reqs[0].0.as_str(), reqs[0].1.as_str()),
        ("GET", "/u?recipient=OwnHealth")
    );
    // the file's type comes from its JWE cty
    let opened = shl::open(&link, &held, &env.blobs, profile.id).map_err(|s| anyhow::anyhow!("{s:?}"))?;
    assert!(
        matches!(&opened[..], [Opened::Fhir { held, .. }] if held.content_type == "application/fhir+json"),
        "{opened:?}"
    );
    Ok(())
}

#[tokio::test]
async fn the_received_bytes_are_stored_before_they_are_decrypted() -> R {
    let env = Env::new().await?;
    let mut tx = env.tx().await?;
    let profile = alex(&mut tx).await?.id;
    // a file sealed under another key: it does not open, and is stored all the same
    let other = jose::encrypt(
        &[9; 32],
        bundle().to_string().as_bytes(),
        Some("application/fhir+json"),
        true,
    )
    .expect("jwe");
    let f = Fake {
        manifest: manifest_with(json!([{"contentType": "application/fhir+json", "location": format!("{SHL}/f/1")}])),
        files: vec![("/f/1".into(), other.clone())],
        ..Fake::default()
    };
    let link = link(&format!("{SHL}/m"), None)?;
    let held = fetch(&f, &link, None, &env, profile).await??;
    assert_eq!(
        env.blobs.get_bytes(BlobOwner::Profile(profile), &held[0].sha)?,
        other.as_bytes()
    );
    assert_eq!(
        shl::open(&link, &held, &env.blobs, profile),
        Err(Stop::Link(Refusal::Decrypt))
    );
    // a sha that is not stored for this profile
    let gone = Held {
        sha: hex::encode(Sha256::digest(b"never stored")),
        content_type: "application/fhir+json".into(),
    };
    assert_eq!(
        shl::open(&link, &[gone], &env.blobs, profile),
        Err(Stop::Link(Refusal::Held))
    );
    Ok(())
}

#[tokio::test]
async fn a_file_over_the_cap_is_refused() -> R {
    let env = Env::new().await?;
    let f = Fake {
        manifest: manifest_with(json!([{"contentType": "application/fhir+json", "location": format!("{SHL}/f/1")}])),
        files: vec![("/f/1".into(), "a".repeat(MAX_BYTES + 1))],
        ..Fake::default()
    };
    let link = link(&format!("{SHL}/m"), None)?;
    assert_eq!(
        fetch(&f, &link, None, &env, uuid::Uuid::new_v4()).await?,
        Err(Refusal::Fetch("too_large".into()))
    );
    Ok(())
}

#[tokio::test]
async fn smart_api_access_and_an_unknown_type_are_counted_as_not_imported() -> R {
    let env = Env::new().await?;
    let mut tx = env.tx().await?;
    let profile = alex(&mut tx).await?;
    let access = jwe(
        br#"{"aud": "https://fhir.labnorte.example", "access_token": "test"}"#,
        None,
    );
    let f = Fake {
        manifest: manifest_with(json!([
            {"contentType": "application/smart-api-access", "embedded": access},
            {"contentType": "text/plain", "embedded": jwe(b"hola", None)},
            {"contentType": "application/fhir+json", "embedded": fhir_jwe()}
        ])),
        ..Fake::default()
    };
    let link = link(&format!("{SHL}/m"), None)?;
    let held = fetch(&f, &link, None, &env, profile.id).await??;
    assert_eq!(held.len(), 3);
    assert_eq!(f.requests().len(), 1, "embedded files need no request");
    let opened = shl::open(&link, &held, &env.blobs, profile.id).map_err(|s| anyhow::anyhow!("{s:?}"))?;
    let got = shl::store(&mut tx, &env.blobs, &profile, &link, &opened, Vec::new(), &[]).await?;
    assert_eq!(got.documents.len(), 1);
    assert_eq!(
        got.not_imported,
        vec![
            ("application/smart-api-access".to_string(), "api_access"),
            ("text/plain".to_string(), "unknown_type")
        ]
    );
    Ok(())
}

// ------------------------------------------------------ a card in the link ----

fn signing_key() -> (SigningKey, Value) {
    let key = SigningKey::from_slice(&[7; 32]).expect("scalar");
    let point = key.verifying_key().to_sec1_point(false);
    let (x, y) = (b64(&point.as_bytes()[1..33]), b64(&point.as_bytes()[33..65]));
    let kid = b64(&Sha256::digest(
        format!(r#"{{"crv":"P-256","kty":"EC","x":"{x}","y":"{y}"}}"#).as_bytes(),
    ));
    let jwk = json!({"kty": "EC", "kid": kid, "use": "sig", "alg": "ES256", "crv": "P-256", "x": x, "y": y});
    (key, jwk)
}

/// A SMART Health Card from `ISS`: DEFLATE, then ES256 over the compact parts.
fn card(key: &SigningKey, kid: &str) -> String {
    let payload = json!({"iss": ISS, "nbf": now().timestamp() - 86_400, "vc": {
        "type": ["https://smarthealth.cards#health-card"],
        "credentialSubject": {"fhirVersion": "4.0.1", "fhirBundle": bundle()}}});
    let mut enc = flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::best());
    enc.write_all(payload.to_string().as_bytes()).expect("deflate");
    let header = json!({"alg": "ES256", "zip": "DEF", "kid": kid});
    let input = format!(
        "{}.{}",
        b64(header.to_string().as_bytes()),
        b64(&enc.finish().expect("deflate"))
    );
    let sig: Signature = key.sign(input.as_bytes());
    format!("{input}.{}", b64(&sig.to_bytes()))
}

#[tokio::test]
async fn a_card_in_a_link_waits_for_its_issuer_then_imports_from_the_held_file_without_a_second_request() -> R {
    let env = Env::new().await?;
    let mut tx = env.tx().await?;
    let profile = alex(&mut tx).await?;
    let (key, jwk) = signing_key();
    let file = json!({"verifiableCredential": [card(&key, jwk["kid"].as_str().unwrap_or_default())]});
    let f = Fake {
        manifest: manifest_with(json!([{"contentType": "application/smart-health-card",
            "location": format!("{SHL}/f/1")}])),
        files: vec![(
            "/f/1".into(),
            jwe(file.to_string().as_bytes(), Some("application/smart-health-card")),
        )],
        jwks: Some(json!({"keys": [jwk]})),
        ..Fake::default()
    };
    let port = spawn(&f).await?;
    let link = link(&format!("{SHL}/m"), None)?;
    let held = shl::fetch(&link, None, &env.blobs, profile.id, &local(port)).await??;
    let opened = shl::open(&link, &held, &env.blobs, profile.id).map_err(|s| anyhow::anyhow!("{s:?}"))?;
    // slice 22's rule: the issuer host is asked about before its keys are fetched
    let only_link = [SHL_HOST.to_string()];
    let asked = shl::check(&opened, &only_link, &local(port), now()).await;
    assert_eq!(
        asked,
        CardsChecked::NeedsApproval(vec![shc::Ask {
            host: ISS_HOST.into(),
            issuers: vec![ISS.into()]
        }])
    );
    assert_eq!(f.count("GET", "/.well-known/jwks.json"), 0);
    let before = f.requests().len();

    // the person says yes: the held file opens again, and the link is not asked a second time
    let approved = [SHL_HOST.to_string(), ISS_HOST.to_string()];
    let opened = shl::open(&link, &held, &env.blobs, profile.id).map_err(|s| anyhow::anyhow!("{s:?}"))?;
    let CardsChecked::Verified(cards) = shl::check(&opened, &approved, &local(port), now()).await else {
        panic!("not verified");
    };
    let got = shl::store(&mut tx, &env.blobs, &profile, &link, &opened, cards, &approved).await?;
    let reqs = f.requests();
    assert_eq!(reqs.len(), before + 1, "{reqs:?}");
    assert_eq!(f.count("POST", "/m") + f.count("GET", "/f/1"), 2);
    assert_eq!(got.cards.len(), 1);
    assert_eq!(got.cards[0].iss, ISS);
    assert_eq!(got.identity, "matched");
    let (channel, host): (String, Option<String>) = sqlx::query_as(
        "select s.channel, d.flags -> 'shl' ->> 'host' from document d \
         join document_source s on s.document_id = d.id where d.id = $1",
    )
    .bind(got.documents[0].document_id)
    .fetch_one(&mut *tx)
    .await?;
    assert_eq!((channel.as_str(), host.as_deref()), ("shc", Some(SHL_HOST)));
    Ok(())
}

#[tokio::test]
async fn a_damaged_card_in_a_link_stops_the_import_with_slice_22s_reason() -> R {
    let env = Env::new().await?;
    let mut tx = env.tx().await?;
    let profile = alex(&mut tx).await?.id;
    let f = Fake {
        manifest: manifest_with(json!([{"contentType": "application/smart-health-card",
            "embedded": jwe(br#"{"verifiableCredential": []}"#, None)}])),
        ..Fake::default()
    };
    let link = link(&format!("{SHL}/m"), None)?;
    let held = fetch(&f, &link, None, &env, profile).await??;
    assert_eq!(
        shl::open(&link, &held, &env.blobs, profile),
        Err(Stop::Card {
            refusal: shc::Refusal::NoCard("file"),
            iss: None
        })
    );
    Ok(())
}

// --------------------------------------------- several files, limits, types ----

/// A bundle with one Patient and one immunization on `on`.
fn bundle_for(family: &str, given: &str, birth: &str, on: &str) -> Vec<u8> {
    json!({"resourceType": "Bundle", "type": "collection", "entry": [
        {"fullUrl": "resource:0", "resource": {"resourceType": "Patient",
            "name": [{"family": family, "given": [given]}], "birthDate": birth}},
        {"fullUrl": "resource:1", "resource": {"resourceType": "Immunization", "status": "completed",
            "vaccineCode": {"coding": [{"system": "http://hl7.org/fhir/sid/cvx", "code": "207"}]},
            "patient": {"reference": "resource:0"}, "occurrenceDateTime": on}}
    ]})
    .to_string()
    .into_bytes()
}

/// An opened FHIR file, as `open` hands it to `store`; `received` stands for its stored sha.
fn opened_fhir(plaintext: Vec<u8>, received: char) -> Opened {
    Opened::Fhir {
        held: Held {
            sha: received.to_string().repeat(64),
            content_type: shl::FHIR.into(),
        },
        plaintext,
    }
}

#[tokio::test]
async fn a_link_with_a_file_for_someone_else_reports_the_weakest_identity_of_its_files() -> R {
    let env = Env::new().await?;
    let mut tx = env.tx().await?;
    let profile = alex(&mut tx).await?;
    let link = link(&format!("{SHL}/m"), None)?;
    // alex's file first, then one that names Carmen Ortega: the link as a whole is a mismatch
    let opened = [
        opened_fhir(bundle_for("Rivera", "Alex", "1979-05-14", "2026-03-02"), 'a'),
        opened_fhir(bundle_for("Ortega", "Carmen", "1955-01-30", "2025-10-10"), 'b'),
    ];
    let got = shl::store(&mut tx, &env.blobs, &profile, &link, &opened, Vec::new(), &[]).await?;
    assert_eq!(got.documents.len(), 2);
    assert_eq!(got.identity, "mismatch", "{got:?}");
    // carmen's row waits in review; alex's is accepted
    assert_eq!((got.summary.accepted, got.summary.queued), (1, 1), "{:?}", got.summary);
    // and the other order gives the same answer
    let again = [opened[1].clone(), opened[0].clone()];
    let got = shl::store(&mut tx, &env.blobs, &profile, &link, &again, Vec::new(), &[]).await?;
    assert_eq!(got.identity, "mismatch");
    Ok(())
}

#[tokio::test]
async fn the_same_file_through_a_second_link_keeps_the_first_links_provenance() -> R {
    let env = Env::new().await?;
    let mut tx = env.tx().await?;
    let profile = alex(&mut tx).await?;
    let first = link(&format!("{SHL}/m"), None)?;
    let second = link("https://other.labnorte.example/m", None)?;
    let file = bundle_for("Rivera", "Alex", "1979-05-14", "2026-03-02");
    let a = shl::store(
        &mut tx,
        &env.blobs,
        &profile,
        &first,
        &[opened_fhir(file.clone(), 'a')],
        Vec::new(),
        &[],
    )
    .await?;
    let b = shl::store(
        &mut tx,
        &env.blobs,
        &profile,
        &second,
        &[opened_fhir(file, 'b')],
        Vec::new(),
        &[],
    )
    .await?;
    assert_eq!(
        (b.documents[0].document_id, b.documents[0].created),
        (a.documents[0].document_id, false)
    );
    let (host, received, provider): (Option<String>, Option<String>, Option<String>) = sqlx::query_as(
        "select flags -> 'shl' ->> 'host', flags -> 'shl' ->> 'received', provider_name from document where id = $1",
    )
    .bind(a.documents[0].document_id)
    .fetch_one(&mut *tx)
    .await?;
    let first_sha = "a".repeat(64);
    assert_eq!(
        (host.as_deref(), received.as_deref(), provider.as_deref()),
        (Some(SHL_HOST), Some(first_sha.as_str()), Some(SHL_HOST))
    );
    Ok(())
}

#[tokio::test]
async fn a_link_server_that_never_answers_stops_at_the_fetch_limit() -> R {
    let env = Env::new().await?;
    // a server that accepts every connection and never answers
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let port = l.local_addr()?.port();
    tokio::spawn(async move {
        let mut open = Vec::new();
        while let Ok((s, _)) = l.accept().await {
            open.push(s);
        }
    });
    let link = link(&format!("{SHL}/m"), None)?;
    let limit = std::time::Duration::from_millis(300);
    let got = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        shl::fetch_within(&link, None, &env.blobs, uuid::Uuid::new_v4(), &local(port), limit),
    )
    .await
    .map_err(|_| anyhow::anyhow!("the fetch ran past its limit"))??;
    assert_eq!(got, Err(Refusal::Fetch("timeout".into())));
    Ok(())
}

/// A received file stored for the profile, as `fetch` leaves it.
fn stored(env: &Env, profile: uuid::Uuid, file: &str, content_type: &str) -> anyhow::Result<Held> {
    Ok(Held {
        sha: env.blobs.put_bytes(BlobOwner::Profile(profile), file.as_bytes())?,
        content_type: content_type.into(),
    })
}

#[tokio::test]
async fn a_fhir_file_that_is_not_fhir_is_refused_when_it_opens() -> R {
    let env = Env::new().await?;
    let mut tx = env.tx().await?;
    let profile = alex(&mut tx).await?.id;
    let link = link(&format!("{SHL}/m"), None)?;
    let held = stored(
        &env,
        profile,
        &jwe(br#"{"hello": "world"}"#, Some(shl::FHIR)),
        shl::FHIR,
    )?;
    assert_eq!(
        shl::open(&link, &[held], &env.blobs, profile),
        Err(Stop::Link(Refusal::NotFhir))
    );
    Ok(())
}

#[tokio::test]
async fn the_manifest_content_type_wins_over_the_files_own_cty() -> R {
    let env = Env::new().await?;
    let mut tx = env.tx().await?;
    let profile = alex(&mut tx).await?.id;
    let link = link(&format!("{SHL}/m"), None)?;
    // the manifest says api access; the JWE says FHIR: the manifest's type decides
    let held = stored(&env, profile, &fhir_jwe(), shl::API_ACCESS)?;
    let opened = shl::open(&link, &[held], &env.blobs, profile).map_err(|s| anyhow::anyhow!("{s:?}"))?;
    assert_eq!(
        opened,
        vec![Opened::Skipped {
            content_type: shl::API_ACCESS.into(),
            reason: "api_access"
        }]
    );
    Ok(())
}
