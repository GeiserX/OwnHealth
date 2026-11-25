//! The browser-facing part of Open Wearables OAuth. The sidecar's authorize, user and
//! management APIs stay on its private network.

use std::time::Duration;

use axum::extract::State;
use axum::http::{HeaderValue, Method, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get};
use axum::{Json, Router};
use ownhealth_core::config::Settings;
use ownhealth_core::{Runtime, providers};
use ownhealth_devices::openwearables::PROVIDERS;
use reqwest::{Client, Url, redirect::Policy};
use serde::Serialize;

use crate::extract::SessionCaller;

const PREFIX: &str = "/ow";
const OAUTH: &str = "/api/v1/oauth/";
const SUCCESS: &str = "/api/v1/oauth/success";
const ERROR: &str = "/api/v1/oauth/error";
const MAX_RESULT_BYTES: usize = 64 * 1024;

#[derive(Clone)]
struct Upstream {
    origin: Url,
    client: Client,
}

impl Upstream {
    fn new(settings: &Settings) -> Option<Self> {
        let origin = settings.ow_url.clone()?;
        let client = Client::builder()
            .no_proxy()
            .redirect(Policy::none())
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(20))
            .build()
            .inspect_err(|_| tracing::error!("could not initialize the Open Wearables proxy"))
            .ok()?;
        Some(Self { origin, client })
    }

    fn url(&self, path: &str, query: Option<&str>) -> Url {
        let mut url = self.origin.clone();
        url.set_path(path);
        url.set_query(query);
        url
    }
}

fn allowed(provider: &str) -> bool {
    PROVIDERS.contains(&provider) && providers::ingest_allowed(provider)
}

/// Keep the whole /ow namespace out of the SPA fallback, including requests for unknown paths.
pub fn proxy_router(settings: &Settings) -> Router<Runtime> {
    Router::new()
        .route("/ow", any(proxy))
        .route("/ow/", any(proxy))
        .route("/ow/{*path}", any(proxy))
        .with_state(Upstream::new(settings))
}

async fn proxy(State(upstream): State<Option<Upstream>>, method: Method, uri: Uri) -> Response {
    let mut response = forward(upstream, method, uri).await;
    let headers = response.headers_mut();
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(header::REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
    headers.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static("sandbox; default-src 'none'; frame-ancestors 'none'"),
    );
    response
}

async fn forward(upstream: Option<Upstream>, method: Method, uri: Uri) -> Response {
    let path = &uri.path()[PREFIX.len()..];
    let provider = path
        .strip_prefix(OAUTH)
        .and_then(|tail| tail.strip_suffix("/callback"))
        .filter(|p| allowed(p));
    if provider.is_none() && !matches!(path, SUCCESS | ERROR) {
        return StatusCode::NOT_FOUND.into_response();
    }
    if method != Method::GET {
        return (StatusCode::METHOD_NOT_ALLOWED, [(header::ALLOW, "GET")]).into_response();
    }
    let Some(upstream) = upstream else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let url = upstream.url(path, uri.query());
    if provider.is_none() && !result_allowed(&url, None) {
        return StatusCode::NOT_FOUND.into_response();
    }

    // Only the callback query comes from the browser. OW sees a fresh GET with no caller headers
    // or body. Do not log reqwest errors, which can contain the authorization code and state.
    let Ok(mut res) = upstream.client.get(url).send().await else {
        return StatusCode::BAD_GATEWAY.into_response();
    };
    if res.status().is_redirection() {
        // OW 0.11 answers 303 with an unprefixed result path. Never follow or relay another target.
        if res.status() == StatusCode::SEE_OTHER
            && let Some(provider) = provider
            && let Some(location) = res.headers().get(header::LOCATION)
            && let Some(location) = result_location(&upstream, provider, location)
        {
            return (StatusCode::SEE_OTHER, [(header::LOCATION, location)]).into_response();
        }
        return StatusCode::BAD_GATEWAY.into_response();
    }

    // The pinned sidecar's result pages are JSON. Treat any other response type as plain text.
    let content_type = if res
        .headers()
        .get(header::CONTENT_TYPE)
        .is_some_and(|v| v == "application/json")
    {
        "application/json"
    } else {
        "text/plain; charset=utf-8"
    };
    let mut body = Vec::new();
    loop {
        match res.chunk().await {
            Ok(Some(chunk)) if body.len() + chunk.len() <= MAX_RESULT_BYTES => body.extend_from_slice(&chunk),
            Ok(None) => break,
            _ => return StatusCode::BAD_GATEWAY.into_response(),
        }
    }
    (res.status(), [(header::CONTENT_TYPE, content_type)], body).into_response()
}

fn result_allowed(url: &Url, expected: Option<&str>) -> bool {
    let mut values = url.query_pairs().filter(|(k, _)| k == "provider");
    match values.next() {
        Some((_, p)) => allowed(&p) && expected.is_none_or(|want| want == p) && values.next().is_none(),
        None => url.path() == ERROR, // OW's error page receives only a message
    }
}

fn result_location(upstream: &Upstream, provider: &str, location: &HeaderValue) -> Option<HeaderValue> {
    let raw = location.to_str().ok()?;
    let uri: Uri = raw.parse().ok()?;
    if !raw.starts_with('/') || !matches!(uri.path(), SUCCESS | ERROR) {
        return None;
    }
    let url = upstream.url(uri.path(), uri.query());
    if !result_allowed(&url, Some(provider)) {
        return None;
    }
    HeaderValue::from_str(&format!("{PREFIX}{raw}")).ok()
}

#[derive(Serialize)]
struct Setup {
    api_base_url: String,
    providers: Vec<SetupProvider>,
}

#[derive(Serialize)]
struct SetupProvider {
    id: &'static str,
    mcp: providers::Mcp,
    redirect_uri: String,
}

/// The signed-in guide reads the same provider policy the MCP readers and the poller use.
pub fn setup_router() -> Router<Runtime> {
    Router::new().route("/wearables/setup", get(setup))
}

async fn setup(State(rt): State<Runtime>, _caller: SessionCaller) -> Json<Setup> {
    let origin = rt.settings.ow_public_url.as_deref().unwrap_or(&rt.settings.base_url);
    let api_base_url = format!("{}{PREFIX}", origin.trim_end_matches('/'));
    let providers = PROVIDERS
        .into_iter()
        .filter_map(|id| {
            let policy = providers::policy().providers.get(id)?;
            (policy.ingest == providers::Ingest::Allow).then(|| SetupProvider {
                id,
                mcp: policy.mcp,
                redirect_uri: format!("{api_base_url}{OAUTH}{id}/callback"),
            })
        })
        .collect();
    Json(Setup {
        api_base_url,
        providers,
    })
}
