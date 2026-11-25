//! A fake OAuth vendor for the connector tests (connectors.md 6.3 rule 10): an axum server on
//! loopback with SMART discovery, a token endpoint (authorization code with PKCE S256, refresh tokens
//! that rotate and die once used) and RFC 7009 revocation. It never calls out, and every token is
//! synthetic. The core tests and the API tests share it.
//!
//! [`Fake::recorded`] plays one vendor of `data/connectors/connectors.yaml` instead: that vendor's
//! recorded discovery document and its token answers, from `fixtures/connectors/<vendor>/`.
#![allow(dead_code)]

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use axum::extract::{Form, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use base64::Engine as _;
use serde_json::{Value, json};

pub const CLIENT_ID: &str = "fake-client-id";
pub const CLIENT_SECRET: &str = "fake-client-secret-not-real";
pub const PATIENT: &str = "fake-patient-alex-rivera";
pub const REDIRECT: &str = "https://ownhealth.rivera.example/api/connectors/callback";

/// What the fake does; tests change it between calls.
#[derive(Debug, Clone)]
pub struct Behaviour {
    /// A refresh answers with a new refresh token and retires the old one.
    pub rotate: bool,
    /// Held inside every token request, to widen a race.
    pub delay_ms: u64,
    /// A refresh answers this status with `server_error`.
    pub fail_refresh: Option<u16>,
    /// The revocation endpoint answers 503.
    pub fail_revoke: bool,
    /// Discovery lists a revocation endpoint.
    pub revocation: bool,
    /// Discovery lists S256 among the PKCE methods.
    pub s256: bool,
    pub expires_in: i64,
    pub refresh_expires_in: Option<i64>,
    /// The `token_type` of every token answer.
    pub token_type: String,
    /// Held inside every revocation request.
    pub revoke_delay_ms: u64,
    /// Discovery names these instead of the fake's own endpoints (a server that lies).
    pub authorize_url: Option<String>,
    pub token_url: Option<String>,
    pub revoke_url: Option<String>,
    /// A vendor's recorded discovery document, served with the fake's own endpoints in it.
    pub discovery: Option<Value>,
    /// The sign-in's and a refresh's token answer, `$ACCESS`, `$REFRESH` and `$PATIENT` filled in.
    /// A refresh answer without `refresh_token` keeps the old token unless `rotate` retires it.
    pub code_reply: Option<Value>,
    pub refresh_reply: Option<Value>,
}

impl Default for Behaviour {
    fn default() -> Self {
        Behaviour {
            rotate: true,
            delay_ms: 0,
            fail_refresh: None,
            fail_revoke: false,
            revocation: true,
            s256: true,
            expires_in: 3600,
            refresh_expires_in: None,
            token_type: "Bearer".into(),
            revoke_delay_ms: 0,
            authorize_url: None,
            token_url: None,
            revoke_url: None,
            discovery: None,
            code_reply: None,
            refresh_reply: None,
        }
    }
}

/// What the fake saw and holds.
#[derive(Debug, Default)]
pub struct Book {
    /// code -> the PKCE challenge it was issued for
    pub codes: HashMap<String, String>,
    /// refresh tokens the fake still accepts
    pub live: HashSet<String>,
    pub minted: usize,
    pub refresh_calls: usize,
    pub in_flight: usize,
    pub max_in_flight: usize,
    pub revoked: Vec<String>,
    /// per token request: public | basic | post
    pub auth_seen: Vec<String>,
    /// every access and refresh token handed out, to search for them elsewhere
    pub handed_out: Vec<String>,
    /// every request the fake answered: discovery, token and revocation
    pub requests: usize,
}

#[derive(Clone)]
struct Shared {
    base: String,
    book: Arc<Mutex<Book>>,
    behaviour: Arc<Mutex<Behaviour>>,
}

pub struct Fake {
    pub base: String,
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

/// The value of `key` in a URL's query (the fake's own values need no decoding).
pub fn param(url: &str, key: &str) -> Option<String> {
    let query = url.split_once('?')?.1;
    query.split('&').find_map(|kv| {
        let (k, v) = kv.split_once('=')?;
        (k == key).then(|| v.to_string())
    })
}

/// PKCE S256 of the verifier (sha256 through the core's hex helper, so no other crate is needed).
fn s256(verifier: &str) -> String {
    let hex = ownhealth_core::connector_oauth::binder_hash(verifier);
    let raw: Vec<u8> = (0..hex.len())
        .step_by(2)
        .filter_map(|i| u8::from_str_radix(&hex[i..i + 2], 16).ok())
        .collect();
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(raw)
}

impl Fake {
    pub async fn start() -> Fake {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind the fake vendor");
        let port = listener.local_addr().expect("fake address").port();
        let base = format!("http://127.0.0.1:{port}");
        let shared = Shared {
            base: base.clone(),
            book: Arc::default(),
            behaviour: Arc::default(),
        };
        let app = Router::new()
            .route("/fhir/.well-known/smart-configuration", get(discovery))
            .route("/token", post(token))
            .route("/revoke", post(revoke))
            .with_state(shared.clone());
        let task = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        Fake {
            base,
            book: shared.book,
            behaviour: shared.behaviour,
            task,
        }
    }

    /// One vendor of `connectors.yaml`, from its recorded fixtures: its discovery document, and
    /// refresh tokens that rotate when its refresh answer carries a new one.
    pub async fn recorded(vendor: &str) -> Fake {
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../../../fixtures/connectors");
        let read = |file: &str| -> Value {
            let path = format!("{dir}/{vendor}/{file}");
            let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e}"));
            serde_json::from_str(&text).unwrap_or_else(|e| panic!("{path}: {e}"))
        };
        let (discovery, tokens) = (read("smart-configuration.json"), read("tokens.json"));
        let fake = Fake::start().await;
        fake.set(|b| {
            b.rotate = tokens["refresh"].get("refresh_token").is_some();
            b.discovery = Some(discovery);
            b.code_reply = Some(tokens["code"].clone());
            b.refresh_reply = Some(tokens["refresh"].clone());
        });
        fake
    }

    /// The FHIR base, which is the SMART issuer.
    pub fn issuer(&self) -> String {
        format!("{}/fhir", self.base)
    }

    pub fn set(&self, f: impl FnOnce(&mut Behaviour)) {
        f(&mut lock(&self.behaviour));
    }

    pub fn book(&self) -> MutexGuard<'_, Book> {
        lock(&self.book)
    }

    /// The person logs in and consents at the vendor: a code bound to the URL's PKCE challenge.
    pub fn consent(&self, authorize_url: &str) -> String {
        let challenge = param(authorize_url, "code_challenge").unwrap_or_default();
        let code = format!("fake-code-{}", uuid::Uuid::new_v4().simple());
        lock(&self.book).codes.insert(code.clone(), challenge);
        code
    }
}

async fn discovery(State(s): State<Shared>) -> Json<Value> {
    let b = lock(&s.behaviour).clone();
    lock(&s.book).requests += 1;
    if let Some(mut doc) = b.discovery.clone() {
        // the vendor's own document, with each endpoint it names moved here
        for (field, path) in [
            ("authorization_endpoint", "authorize"),
            ("token_endpoint", "token"),
            ("revocation_endpoint", "revoke"),
        ] {
            if doc.get(field).is_some() {
                doc[field] = json!(format!("{}/{path}", s.base));
            }
        }
        return Json(doc);
    }
    let mut meta = json!({
        "authorization_endpoint": b.authorize_url.clone().unwrap_or_else(|| format!("{}/authorize", s.base)),
        "token_endpoint": b.token_url.clone().unwrap_or_else(|| format!("{}/token", s.base)),
        "code_challenge_methods_supported": if b.s256 { json!(["S256"]) } else { json!(["plain"]) },
        "token_endpoint_auth_methods_supported": ["client_secret_basic", "client_secret_post"],
        "capabilities": ["launch-standalone", "client-confidential-symmetric", "permission-offline"],
    });
    if b.revocation {
        meta["revocation_endpoint"] = json!(b.revoke_url.clone().unwrap_or_else(|| format!("{}/revoke", s.base)));
    }
    Json(meta)
}

fn oauth_error(status: StatusCode, code: &str, description: &str) -> Response {
    (status, Json(json!({"error": code, "error_description": description}))).into_response()
}

/// How the client authenticated, or None when its credential is wrong.
fn client_auth(headers: &HeaderMap, form: &HashMap<String, String>) -> Option<&'static str> {
    if let Some(h) = headers.get("authorization").and_then(|v| v.to_str().ok()) {
        let raw = base64::engine::general_purpose::STANDARD
            .decode(h.strip_prefix("Basic ")?)
            .ok()?;
        let ok = raw == format!("{CLIENT_ID}:{CLIENT_SECRET}").as_bytes() && !form.contains_key("client_secret");
        return ok.then_some("basic");
    }
    match (form.get("client_id"), form.get("client_secret")) {
        (Some(id), Some(secret)) if id == CLIENT_ID && secret == CLIENT_SECRET => Some("post"),
        (Some(id), None) if id == CLIENT_ID => Some("public"),
        _ => None,
    }
}

/// A token answer: the fake's own, or `template` with its `$` words filled in. A template decides by
/// itself whether a refresh token comes back.
fn mint(s: &Shared, b: &Behaviour, with_refresh: bool, template: Option<&Value>) -> Value {
    let with_refresh = template.map_or(with_refresh, |t| t.get("refresh_token").is_some());
    let mut book = lock(&s.book);
    book.minted += 1;
    let n = book.minted;
    let tag = uuid::Uuid::new_v4().simple().to_string();
    let access = format!("fake-access-{n}-{tag}");
    book.handed_out.push(access.clone());
    let mut out = json!({
        "access_token": access, "token_type": b.token_type, "expires_in": b.expires_in,
        "scope": "openid fhirUser launch/patient offline_access patient/*.rs", "patient": PATIENT,
    });
    if with_refresh {
        let refresh = format!("fake-refresh-{n}-{tag}");
        book.live.insert(refresh.clone());
        book.handed_out.push(refresh.clone());
        out["refresh_token"] = json!(refresh);
        if let Some(r) = b.refresh_expires_in {
            out["refresh_token_expires_in"] = json!(r);
        }
    }
    let Some(template) = template else {
        return out;
    };
    let mut filled = template.clone();
    if let Some(m) = filled.as_object_mut() {
        for v in m.values_mut() {
            let word = match v.as_str() {
                Some("$ACCESS") => out["access_token"].clone(),
                Some("$REFRESH") => out["refresh_token"].clone(),
                Some("$PATIENT") => json!(PATIENT),
                _ => continue,
            };
            *v = word;
        }
        if let Some(r) = out.get("refresh_token_expires_in") {
            m.insert("refresh_token_expires_in".into(), r.clone());
        }
    }
    filled
}

async fn token(State(s): State<Shared>, headers: HeaderMap, Form(form): Form<HashMap<String, String>>) -> Response {
    let b = lock(&s.behaviour).clone();
    {
        let mut book = lock(&s.book);
        book.requests += 1;
        book.in_flight += 1;
        book.max_in_flight = book.max_in_flight.max(book.in_flight);
    }
    if b.delay_ms > 0 {
        tokio::time::sleep(Duration::from_millis(b.delay_ms)).await;
    }
    let out = answer(&s, &b, &headers, &form);
    lock(&s.book).in_flight -= 1;
    out
}

fn answer(s: &Shared, b: &Behaviour, headers: &HeaderMap, form: &HashMap<String, String>) -> Response {
    let Some(auth) = client_auth(headers, form) else {
        return oauth_error(StatusCode::UNAUTHORIZED, "invalid_client", "unknown client");
    };
    lock(&s.book).auth_seen.push(auth.into());
    let field = |k: &str| form.get(k).cloned().unwrap_or_default();
    match form.get("grant_type").map(String::as_str) {
        Some("authorization_code") => {
            let challenge = lock(&s.book).codes.remove(&field("code"));
            let Some(challenge) = challenge else {
                return oauth_error(StatusCode::BAD_REQUEST, "invalid_grant", "unknown code");
            };
            if field("redirect_uri") != REDIRECT || s256(&field("code_verifier")) != challenge {
                return oauth_error(StatusCode::BAD_REQUEST, "invalid_grant", "PKCE verifier mismatch");
            }
            Json(mint(s, b, true, b.code_reply.as_ref())).into_response()
        }
        Some("refresh_token") => {
            lock(&s.book).refresh_calls += 1;
            if let Some(status) = b.fail_refresh {
                let status = StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
                return oauth_error(status, "server_error", "the vendor is down");
            }
            let old = field("refresh_token");
            let live = lock(&s.book).live.contains(&old);
            if !live {
                // a description that quotes the token: it must never be repeated by the app
                return oauth_error(
                    StatusCode::BAD_REQUEST,
                    "invalid_grant",
                    &format!("refresh token {old} was already used or revoked"),
                );
            }
            if b.rotate {
                lock(&s.book).live.remove(&old);
            }
            Json(mint(s, b, b.rotate, b.refresh_reply.as_ref())).into_response()
        }
        _ => oauth_error(StatusCode::BAD_REQUEST, "unsupported_grant_type", "no"),
    }
}

async fn revoke(State(s): State<Shared>, headers: HeaderMap, Form(form): Form<HashMap<String, String>>) -> Response {
    lock(&s.book).requests += 1;
    let delay = lock(&s.behaviour).revoke_delay_ms;
    if delay > 0 {
        tokio::time::sleep(Duration::from_millis(delay)).await;
    }
    if client_auth(&headers, &form).is_none() {
        return oauth_error(StatusCode::UNAUTHORIZED, "invalid_client", "unknown client");
    }
    if lock(&s.behaviour).fail_revoke {
        return oauth_error(StatusCode::SERVICE_UNAVAILABLE, "temporarily_unavailable", "down");
    }
    let token = form.get("token").cloned().unwrap_or_default();
    let mut book = lock(&s.book);
    book.live.remove(&token);
    book.revoked.push(token);
    StatusCode::OK.into_response()
}
