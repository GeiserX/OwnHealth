//! The OAuth part every connector shares (connectors.md 6.2): SMART on FHIR, Blue Button and Dexcom
//! launch, call back, reconnect, disconnect and refresh through this module only, and keep their
//! tokens nowhere else.
//!
//! - **Launch.** [`begin`] builds the authorize URL: PKCE S256, a 43-character `state`, and the hash of
//!   a binder cookie the route sets, kept with the verifier in `auth_challenge` (purpose `connector`).
//! - **Callback.** [`take_state`] uses the state up (once, within [`STATE_TTL_SECONDS`], in the
//!   browser that started it); [`exchange`] trades the code as a public client or with the client
//!   secret (`client_secret_basic` or `client_secret_post`); [`save_connection`] stores the refresh
//!   token in `secret` (`source.secret_id`), sealed under the profile's data key, and the endpoints in
//!   `source.config`. A reconnect is the same flow for an existing source, and it clears
//!   `reconsent_needed_at`.
//! - **Refresh.** [`access_token`] locks the source row `FOR UPDATE`, posts the refresh, stores the
//!   rotated token, commits, and only then hands the access token out. A second caller waits on the
//!   lock and reuses the first one's result. A reply with no refresh token keeps the old one.
//!   `invalid_grant` sets `reconsent_needed_at`; so does a rotated token that could not be stored.
//! - **Disconnect.** [`disconnect`] calls the revocation endpoint when the server offers one, then
//!   deletes the refresh token and the cursor. [`revoke_profile`] does the vendor half before a
//!   profile delete.
//! - **Declared hosts** (connectors.md 6.3 rule 7). [`SourceHttp`] is the client a connector's poller
//!   uses: it refuses any host the source did not declare at connect time before `net` resolves it.
//!   `net` then does the address half (public, https, no redirects, capped).
//!
//! Access tokens live only in this process's memory, until they expire. No token, verifier or code
//! appears in a log line, an error message, a `Debug` print or `source.config`.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};

use base64::Engine as _;
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::{PgConnection, PgPool};
use subtle::ConstantTimeEq;
use uuid::Uuid;

use crate::error::Error;
use crate::net::{self, Egress, Fetched, NetError};
use crate::secrets::{self, SecretBox, SecretString};
use crate::sources::Source;

/// The source kinds that connect through OAuth here.
pub const OAUTH_KINDS: [&str; 3] = ["smart", "bluebutton", "dexcom"];
/// The `auth_challenge.purpose` of a launch.
pub const PURPOSE: &str = "connector";
/// Ten minutes for the round trip through the vendor's login and consent.
pub const STATE_TTL_SECONDS: i64 = 600;
/// A new connection is polled daily, which also refreshes its token daily.
pub const DEFAULT_POLL_SECONDS: i32 = 86_400;
/// An access token this close to its end is refreshed instead of used.
const MARGIN_SECONDS: i64 = 60;
/// How long an access token with no `expires_in` is kept.
const UNKNOWN_LIFETIME_SECONDS: i64 = 300;

/// SMART App Launch, standalone patient access (connectors.md section 7).
pub const SMART_SCOPES: &str = "openid fhirUser launch/patient offline_access patient/*.rs";

// ------------------------------------------------------------------ errors ----

/// Why a connector call stopped. No variant carries a token, a code, a verifier or a vendor's text.
#[derive(Debug, thiserror::Error)]
pub enum OAuthError {
    /// The launch cannot finish: an unknown, used or expired state, another browser, a source that
    /// changed. `code` is stable and goes into the redirect back to the app.
    #[error("{message}")]
    Refused { code: &'static str, message: &'static str },
    /// The vendor no longer accepts this connection (`invalid_grant`, or a rotated token we could not
    /// keep): `reconsent_needed_at` is set and the person connects again.
    #[error("the vendor no longer accepts this connection: connect it again")]
    ReconsentNeeded,
    /// The vendor answered with an error or an unexpected status: only the status and the OAuth error
    /// code, never its description or body.
    #[error("the vendor refused the request ({status}, {code})")]
    Vendor { status: u16, code: String },
    /// A request to a host the source did not declare; nothing was sent.
    #[error("{0} is not a host this connection declared")]
    UndeclaredHost(String),
    #[error(transparent)]
    Net(#[from] NetError),
    #[error(transparent)]
    Core(#[from] Error),
}

impl From<sqlx::Error> for OAuthError {
    fn from(e: sqlx::Error) -> Self {
        OAuthError::Core(Error::Db(e))
    }
}

impl From<OAuthError> for Error {
    fn from(e: OAuthError) -> Self {
        match e {
            OAuthError::Core(e) => e,
            other => Error::Other(anyhow::anyhow!(other.to_string())),
        }
    }
}

fn refused(code: &'static str, message: &'static str) -> OAuthError {
    OAuthError::Refused { code, message }
}

/// A vendor's error code as we repeat it: OAuth codes are short ASCII words; anything else is
/// `unexpected`, so a vendor's text never reaches a log or a screen.
fn vendor_code(code: Option<&str>) -> String {
    match code {
        Some(c)
            if !c.is_empty()
                && c.len() <= 40
                && c.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'.' || b == b'-') =>
        {
            c.to_string()
        }
        _ => "unexpected".into(),
    }
}

// -------------------------------------------------------------- endpoints ----

/// How a confidential client proves itself at the token endpoint. A client with no secret is public
/// and sends only its `client_id`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClientAuth {
    /// `client_secret_basic`: the id and secret in an `Authorization: Basic` header.
    Basic,
    /// `client_secret_post`: the id and secret in the form body.
    Post,
}

/// Where one connection talks. Saved in `source.config.endpoints` at connect time: a refresh, a
/// revocation and a poll use these and no others.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Endpoints {
    /// The server the data comes from (SMART: the FHIR base URL).
    pub issuer: String,
    pub authorize: String,
    pub token: String,
    #[serde(default)]
    pub revocation: Option<String>,
    pub scopes: String,
    /// SMART's `aud`: the FHIR base the token is for.
    #[serde(default)]
    pub aud: Option<String>,
    pub auth: ClientAuth,
}

impl Endpoints {
    /// The hosts the connection may call: the data server, the token and the revocation endpoint.
    /// The authorize endpoint is only visited by the person's browser.
    pub fn hosts(&self) -> Vec<String> {
        let mut out: Vec<String> = [Some(&self.issuer), Some(&self.token), self.revocation.as_ref()]
            .into_iter()
            .flatten()
            .filter_map(|u| authority(u))
            .collect();
        out.sort();
        out.dedup();
        out
    }
}

/// `host:port` of an http(s) URL, the host in lower case; None when it is not one.
pub fn authority(url: &str) -> Option<String> {
    let u = url::Url::parse(url).ok()?;
    if !matches!(u.scheme(), "http" | "https") {
        return None;
    }
    let host = u.host_str()?.to_ascii_lowercase();
    Some(format!("{host}:{}", u.port_or_known_default()?))
}

/// Whether `url` may be an address of a connection: the sign-in page the browser is sent to, or a
/// token, revocation or data endpoint. https with a host and no user name or password; plain http only
/// to a loopback address, and only where `egress` allows plain http (tests and the debug-only dev
/// switch). So `javascript:`, `data:` and every other scheme are refused.
pub fn safe_endpoint(url: &str, egress: &Egress) -> bool {
    let Ok(u) = url::Url::parse(url) else {
        return false;
    };
    if !u.username().is_empty() || u.password().is_some() {
        return false;
    }
    let loopback = match u.host() {
        Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
        Some(url::Host::Domain(d)) => d.eq_ignore_ascii_case("localhost"),
        None => return false,
    };
    match u.scheme() {
        "https" => true,
        "http" => loopback && !egress.https_only,
        _ => false,
    }
}

/// How a kind finds its endpoints.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Discovery {
    /// SMART App Launch: `<issuer>/.well-known/smart-configuration`.
    Smart,
    /// Fixed paths under the issuer, for a vendor with one API (Blue Button, Dexcom).
    Fixed {
        authorize: &'static str,
        token: &'static str,
        revocation: Option<&'static str>,
        auth: ClientAuth,
    },
}

/// What a connector kind brings to the shared routes. A kind's slice adds its arm to [`kind_spec`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KindSpec {
    pub discovery: Discovery,
    pub scopes: &'static str,
    /// The issuer when the household's client names none (one API for every person).
    pub default_issuer: Option<&'static str>,
    /// The vendor's own page where a person removes the app's access, shown when a revocation fails.
    pub revoke_page: Option<&'static str>,
}

/// The kinds the launch route serves. SMART is here; Blue Button and Dexcom add their arm with their
/// slice (18, 38).
pub fn kind_spec(kind: &str) -> Option<KindSpec> {
    match kind {
        "smart" => Some(KindSpec {
            discovery: Discovery::Smart,
            scopes: SMART_SCOPES,
            default_issuer: None,
            revoke_page: None,
        }),
        _ => None,
    }
}

/// Whether a household client registered for `vendor` serves sources of `kind`: Blue Button and
/// Dexcom clients are their own vendor; a SMART client is any EHR vendor's (Epic, Oracle, Meditech).
pub fn client_fits(kind: &str, vendor: &str) -> bool {
    match kind {
        "smart" => !matches!(vendor, "bluebutton" | "dexcom"),
        other => vendor == other,
    }
}

fn join(issuer: &str, path: &str) -> String {
    format!("{}/{}", issuer.trim_end_matches('/'), path.trim_start_matches('/'))
}

/// The endpoints of `issuer` for a kind: read from the server for SMART, built from fixed paths
/// otherwise.
pub async fn resolve(spec: &KindSpec, issuer: &str, egress: &Egress) -> Result<Endpoints, OAuthError> {
    let issuer = issuer.trim().trim_end_matches('/').to_string();
    if authority(&issuer).is_none() || !safe_endpoint(&issuer, egress) {
        return Err(refused("bad_issuer", "The server address is not a secure web address."));
    }
    let endpoints = match &spec.discovery {
        Discovery::Smart => discover_smart(&issuer, spec.scopes, egress).await?,
        Discovery::Fixed {
            authorize,
            token,
            revocation,
            auth,
        } => Endpoints {
            authorize: join(&issuer, authorize),
            token: join(&issuer, token),
            revocation: revocation.map(|r| join(&issuer, r)),
            scopes: spec.scopes.into(),
            aud: None,
            auth: *auth,
            issuer,
        },
    };
    // the browser is sent to the sign-in page, and the client secret goes to the token endpoint: a
    // server that names anything but a secure web address gets neither
    let named = [
        Some(&endpoints.authorize),
        Some(&endpoints.token),
        endpoints.revocation.as_ref(),
    ];
    if !named.into_iter().flatten().all(|u| safe_endpoint(u, egress)) {
        return Err(refused(
            "bad_endpoint",
            "The vendor's server names an address that is not a secure web address.",
        ));
    }
    Ok(endpoints)
}

async fn discover_smart(issuer: &str, scopes: &str, egress: &Egress) -> Result<Endpoints, OAuthError> {
    let url = join(issuer, ".well-known/smart-configuration");
    let f = net::get(&url, egress, &[("accept", "application/json")]).await?;
    let bad = |code: &str| OAuthError::Vendor {
        status: f.status,
        code: code.into(),
    };
    if f.status != 200 {
        return Err(bad("discovery_failed"));
    }
    let meta: Value = serde_json::from_slice(&f.body).map_err(|_| bad("discovery_failed"))?;
    let field = |k: &str| meta[k].as_str().filter(|s| !s.is_empty()).map(String::from);
    let (Some(authorize), Some(token)) = (field("authorization_endpoint"), field("token_endpoint")) else {
        return Err(bad("discovery_failed"));
    };
    let listed = |k: &str, v: &str| meta[k].as_array().map(|a| a.iter().any(|x| x == v));
    if listed("code_challenge_methods_supported", "S256") == Some(false) {
        return Err(bad("no_pkce_s256"));
    }
    let auth = match (
        listed("token_endpoint_auth_methods_supported", "client_secret_basic"),
        listed("token_endpoint_auth_methods_supported", "client_secret_post"),
    ) {
        (Some(false), Some(true)) => ClientAuth::Post,
        _ => ClientAuth::Basic,
    };
    Ok(Endpoints {
        issuer: issuer.into(),
        authorize,
        token,
        revocation: field("revocation_endpoint"),
        scopes: scopes.into(),
        aud: Some(issuer.into()),
        auth,
    })
}

// ----------------------------------------------------------------- clients ----

/// A household's registration with one vendor (`connector_client`). `secret` is read only when the
/// caller asked for it.
#[derive(Debug, Clone)]
pub struct ConnectorClient {
    pub id: Uuid,
    pub household_id: Uuid,
    pub vendor: String,
    pub issuer: Option<String>,
    pub client_id: String,
    /// The client has a secret (`client_secret_id`), opened or not: it is bound to its `issuer`.
    pub confidential: bool,
    pub secret: Option<SecretString>,
    pub redirect_uri: String,
}

/// vendor, issuer, client_id, client_secret_id, redirect_uri
type ClientRow = (String, Option<String>, String, Option<Uuid>, String);

/// The household's client `id`, with its secret opened when `sb` is given. None for another
/// household's client.
pub async fn load_client(
    conn: &mut PgConnection,
    sb: Option<&SecretBox>,
    household_id: Uuid,
    id: Uuid,
) -> Result<Option<ConnectorClient>, OAuthError> {
    let row: Option<ClientRow> = sqlx::query_as(
        "select vendor, issuer, client_id, client_secret_id, redirect_uri from connector_client \
         where id = $1 and household_id = $2",
    )
    .bind(id)
    .bind(household_id)
    .fetch_optional(&mut *conn)
    .await?;
    let Some((vendor, issuer, client_id, secret_id, redirect_uri)) = row else {
        return Ok(None);
    };
    let secret = match (sb, secret_id) {
        (Some(sb), Some(sid)) => Some(secrets::read_secret(conn, sb, household_id, sid).await?),
        _ => None,
    };
    Ok(Some(ConnectorClient {
        id,
        household_id,
        vendor,
        issuer,
        client_id,
        confidential: secret_id.is_some(),
        secret,
        redirect_uri,
    }))
}

/// RFC 6749 2.3.1: the id and secret are form-encoded before they are joined and base64-encoded.
fn basic(client_id: &str, secret: &str) -> String {
    let enc = |s: &str| url::form_urlencoded::byte_serialize(s.as_bytes()).collect::<String>();
    let raw = format!("{}:{}", enc(client_id), enc(secret));
    format!("Basic {}", base64::engine::general_purpose::STANDARD.encode(raw))
}

/// A token-endpoint request's form and headers, with the client's credential added the way the
/// endpoints say.
struct TokenRequest {
    fields: Vec<(&'static str, String)>,
    authorization: Option<String>,
}

impl TokenRequest {
    fn new(client: &ConnectorClient, auth: ClientAuth, fields: Vec<(&'static str, String)>) -> TokenRequest {
        let mut r = TokenRequest {
            fields,
            authorization: None,
        };
        match (&client.secret, auth) {
            (None, _) => r.fields.push(("client_id", client.client_id.clone())),
            (Some(s), ClientAuth::Basic) => r.authorization = Some(basic(&client.client_id, s.expose())),
            (Some(s), ClientAuth::Post) => {
                r.fields.push(("client_id", client.client_id.clone()));
                r.fields.push(("client_secret", s.expose().to_string()));
            }
        }
        r
    }

    async fn send(&self, http: &SourceHttp, url: &str) -> Result<Fetched, OAuthError> {
        let mut headers = vec![("accept", "application/json")];
        if let Some(a) = &self.authorization {
            headers.push(("authorization", a.as_str()));
        }
        let fields: Vec<(&str, &str)> = self.fields.iter().map(|(k, v)| (*k, v.as_str())).collect();
        http.post_form(url, &headers, &fields).await
    }
}

// ------------------------------------------------------------ declared hosts ----

/// The HTTP client of one connection: every request goes to a host the source declared when it
/// connected, or nowhere (connectors.md 6.3 rule 7). A poller builds it with
/// [`SourceHttp::for_source`] and its own `Egress`; `net` then checks the address.
#[derive(Debug, Clone)]
pub struct SourceHttp {
    hosts: Vec<String>,
    egress: Egress,
}

impl SourceHttp {
    pub fn new(hosts: Vec<String>, egress: Egress) -> SourceHttp {
        SourceHttp { hosts, egress }
    }

    /// The hosts in `source.config.hosts`; a source that declared none can call nothing.
    pub fn for_source(source: &Source, egress: Egress) -> SourceHttp {
        SourceHttp::new(declared_hosts(&source.config), egress)
    }

    fn check(&self, url: &str) -> Result<(), OAuthError> {
        let Some(a) = authority(url) else {
            return Err(OAuthError::Net(NetError::BadUrl));
        };
        if self.hosts.contains(&a) {
            Ok(())
        } else {
            Err(OAuthError::UndeclaredHost(a))
        }
    }

    pub async fn get(&self, url: &str, headers: &[(&str, &str)]) -> Result<Fetched, OAuthError> {
        self.check(url)?;
        Ok(net::get(url, &self.egress, headers).await?)
    }

    pub async fn post_form(
        &self,
        url: &str,
        headers: &[(&str, &str)],
        fields: &[(&str, &str)],
    ) -> Result<Fetched, OAuthError> {
        self.check(url)?;
        Ok(net::post_form(url, &self.egress, headers, fields).await?)
    }

    pub async fn post_json(&self, url: &str, headers: &[(&str, &str)], body: &Value) -> Result<Fetched, OAuthError> {
        self.check(url)?;
        Ok(net::post_json(url, &self.egress, headers, body).await?)
    }
}

fn declared_hosts(config: &Value) -> Vec<String> {
    config["hosts"]
        .as_array()
        .map(|a| a.iter().filter_map(|h| h.as_str().map(String::from)).collect())
        .unwrap_or_default()
}

fn endpoints_of(config: &Value) -> Result<Endpoints, OAuthError> {
    serde_json::from_value(config["endpoints"].clone()).map_err(|_| {
        refused(
            "not_connected",
            "This connection has no vendor endpoints: connect it again.",
        )
    })
}

// ------------------------------------------------------------------ launch ----

/// What the binder cookie leaves in the challenge.
pub fn binder_hash(binder: &str) -> String {
    hex::encode(Sha256::digest(binder.as_bytes()))
}

/// `n` random bytes, base64url without padding.
fn token_urlsafe(n: usize) -> String {
    let mut b = vec![0u8; n];
    rand::fill(&mut b[..]);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b)
}

fn s256(verifier: &str) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

/// One launch: who starts it, for which profile, and with which client and endpoints. No `Debug`: it
/// holds the binder.
pub struct Launch<'a> {
    pub kind: &'a str,
    pub client: &'a ConnectorClient,
    pub endpoints: &'a Endpoints,
    pub profile_id: Uuid,
    pub label: &'a str,
    /// A reconnect: the source the new grant replaces.
    pub source_id: Option<Uuid>,
    pub user_id: Option<Uuid>,
    pub session_id: Option<Uuid>,
    /// The value of the browser's binder cookie; only its hash is kept.
    pub binder: &'a str,
}

/// A stored launch: the vendor URL to send the browser to, and the `state` it carries (the route names
/// the launch's binder cookie after it).
#[derive(Debug, Clone)]
pub struct Begun {
    pub url: String,
    pub state: String,
}

/// Store the launch and return the vendor URL to send the browser to.
pub async fn begin(conn: &mut PgConnection, l: &Launch<'_>) -> Result<Begun, OAuthError> {
    if l.binder.len() < 16 {
        return Err(refused("bad_binder", "The browser binder is missing."));
    }
    // resolve checked it against the egress; whatever was stored, never a script or a data URL
    let mut url = url::Url::parse(&l.endpoints.authorize)
        .ok()
        .filter(|u| {
            matches!(u.scheme(), "https" | "http")
                && u.host().is_some()
                && u.username().is_empty()
                && u.password().is_none()
        })
        .ok_or_else(|| {
            refused(
                "bad_endpoint",
                "The vendor's sign-in address is not a secure web address.",
            )
        })?;
    let (state, verifier) = (token_urlsafe(32), token_urlsafe(48));
    let data = json!({
        "kind": l.kind, "client": l.client.id, "profile_id": l.profile_id, "label": l.label,
        "source_id": l.source_id, "endpoints": l.endpoints, "verifier": verifier,
        "binder": binder_hash(l.binder),
    });
    sqlx::query("delete from auth_challenge where expires_at < now() - interval '1 day'")
        .execute(&mut *conn)
        .await?;
    sqlx::query(
        "insert into auth_challenge (purpose, challenge, user_id, session_id, data, expires_at) \
         values ($1, $2, $3, $4, $5, now() + make_interval(secs => $6))",
    )
    .bind(PURPOSE)
    .bind(state.as_bytes())
    .bind(l.user_id)
    .bind(l.session_id)
    .bind(&data)
    .bind(STATE_TTL_SECONDS as f64)
    .execute(&mut *conn)
    .await?;
    let challenge = s256(&verifier);
    {
        let mut q = url.query_pairs_mut();
        q.append_pair("response_type", "code")
            .append_pair("client_id", &l.client.client_id)
            .append_pair("redirect_uri", &l.client.redirect_uri)
            .append_pair("scope", &l.endpoints.scopes)
            .append_pair("state", &state)
            .append_pair("code_challenge", &challenge)
            .append_pair("code_challenge_method", "S256");
        if let Some(aud) = &l.endpoints.aud {
            q.append_pair("aud", aud);
        }
    }
    Ok(Begun { url: url.into(), state })
}

/// A launch whose state came back: everything the callback needs. The verifier never prints.
#[derive(Debug, Clone)]
pub struct Pending {
    pub kind: String,
    pub client: Uuid,
    pub profile_id: Uuid,
    pub label: String,
    pub source_id: Option<Uuid>,
    pub endpoints: Endpoints,
    pub user_id: Option<Uuid>,
    pub session_id: Option<Uuid>,
    verifier: SecretString,
}

/// Use the state up and return its launch. Refused (`state_invalid`) when it is unknown, used,
/// expired or another purpose's, and (`wrong_browser`) when `binder` is not the cookie set at launch.
/// The state is used up in every case: commit even on a refusal.
pub async fn take_state(conn: &mut PgConnection, state: &str, binder: &str) -> Result<Pending, OAuthError> {
    let invalid = || {
        refused(
            "state_invalid",
            "This connection request expired or was already used. Start again.",
        )
    };
    let row: Option<(Option<Uuid>, Option<Uuid>, Value)> = sqlx::query_as(
        "update auth_challenge set used_at = now() where challenge = $1 and purpose = $2 and used_at is null \
         and expires_at > now() returning user_id, session_id, data",
    )
    .bind(state.as_bytes())
    .bind(PURPOSE)
    .fetch_optional(&mut *conn)
    .await?;
    let Some((user_id, session_id, data)) = row else {
        return Err(invalid());
    };
    let kept = data["binder"].as_str().unwrap_or_default();
    if binder.is_empty() || !bool::from(kept.as_bytes().ct_eq(binder_hash(binder).as_bytes())) {
        return Err(refused(
            "wrong_browser",
            "This connection was started in another browser. Start again.",
        ));
    }
    let text = |k: &str| data[k].as_str().map(String::from);
    let id = |k: &str| text(k).and_then(|s| s.parse::<Uuid>().ok());
    let (Some(kind), Some(client), Some(profile_id), Some(verifier)) =
        (text("kind"), id("client"), id("profile_id"), text("verifier"))
    else {
        return Err(invalid());
    };
    let endpoints: Endpoints = serde_json::from_value(data["endpoints"].clone()).map_err(|_| invalid())?;
    Ok(Pending {
        kind,
        client,
        profile_id,
        label: text("label").unwrap_or_default(),
        source_id: id("source_id"),
        endpoints,
        user_id,
        session_id,
        verifier: SecretString::new(verifier),
    })
}

/// What a token endpoint gave. `Debug` prints no token.
#[derive(Debug, Clone)]
pub struct TokenSet {
    pub access: SecretString,
    pub refresh: Option<SecretString>,
    pub expires_in: Option<i64>,
    /// When the vendor says how long the refresh token (the grant) lasts.
    pub refresh_expires_in: Option<i64>,
    pub scope: Option<String>,
    /// SMART: the FHIR id of the patient the grant is for.
    pub patient: Option<String>,
}

fn seconds(v: &Value) -> Option<i64> {
    match v {
        Value::Number(n) => n.as_i64().or_else(|| n.as_f64().map(|f| f as i64)),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
    .filter(|s| *s > 0)
}

/// Read a token endpoint's answer. Only the OAuth `error` code of a refusal is kept.
fn parse_tokens(f: &Fetched) -> Result<TokenSet, OAuthError> {
    let body: Value = serde_json::from_slice(&f.body).unwrap_or(Value::Null);
    if f.status != 200 {
        return Err(OAuthError::Vendor {
            status: f.status,
            code: vendor_code(body["error"].as_str()),
        });
    }
    let text = |k: &str| body[k].as_str().filter(|s| !s.is_empty()).map(String::from);
    let bad = |code: &str| OAuthError::Vendor {
        status: f.status,
        code: code.into(),
    };
    let access = text("access_token").ok_or_else(|| bad("no_access_token"))?;
    if text("token_type").is_some_and(|t| !t.eq_ignore_ascii_case("bearer")) {
        return Err(bad("not_bearer"));
    }
    Ok(TokenSet {
        access: SecretString::new(access),
        refresh: text("refresh_token").map(SecretString::new),
        expires_in: seconds(&body["expires_in"]),
        refresh_expires_in: seconds(&body["refresh_token_expires_in"]),
        scope: text("scope"),
        patient: text("patient"),
    })
}

/// Trade the code for tokens, with the launch's PKCE verifier.
pub async fn exchange(
    p: &Pending,
    client: &ConnectorClient,
    code: &str,
    egress: &Egress,
) -> Result<TokenSet, OAuthError> {
    let req = TokenRequest::new(
        client,
        p.endpoints.auth,
        vec![
            ("grant_type", "authorization_code".into()),
            ("code", code.into()),
            ("redirect_uri", client.redirect_uri.clone()),
            ("code_verifier", p.verifier.expose().into()),
        ],
    );
    let http = SourceHttp::new(p.endpoints.hosts(), egress.clone());
    parse_tokens(&req.send(&http, &p.endpoints.token).await?)
}

fn config_for(p: &Pending, t: &TokenSet) -> Value {
    json!({
        "endpoints": p.endpoints, "hosts": p.endpoints.hosts(), "scope": t.scope, "patient": t.patient,
    })
}

fn after(now: DateTime<Utc>, secs: Option<i64>) -> Option<DateTime<Utc>> {
    secs.map(|s| now + Duration::seconds(s))
}

/// Store a finished launch, in the caller's transaction: a new source, or (a reconnect) the same
/// source with the new grant, `reconsent_needed_at` cleared and switched back on. Returns the source
/// id. Call [`remember`] after the commit to keep the access token for the first poll.
pub async fn save_connection(
    conn: &mut PgConnection,
    sb: &SecretBox,
    p: &Pending,
    client: &ConnectorClient,
    t: &TokenSet,
    now: DateTime<Utc>,
) -> Result<Uuid, OAuthError> {
    let household: Option<Uuid> = sqlx::query_scalar("select household_id from profile where id = $1")
        .bind(p.profile_id)
        .fetch_optional(&mut *conn)
        .await?;
    if household != Some(client.household_id) || p.client != client.id || !client_fits(&p.kind, &client.vendor) {
        return Err(refused(
            "wrong_household",
            "This connection belongs to another household. Start again.",
        ));
    }
    let config = config_for(p, t);
    let (access_at, grant_at) = (after(now, t.expires_in), after(now, t.refresh_expires_in));
    let Some(source_id) = p.source_id else {
        let secret_id = match &t.refresh {
            Some(r) => Some(
                secrets::store_profile_secret(
                    conn,
                    sb,
                    client.household_id,
                    p.profile_id,
                    "connector_refresh_token",
                    r,
                )
                .await?,
            ),
            None => None,
        };
        let id: Uuid = sqlx::query_scalar(
            "insert into source (household_id, profile_id, kind, label, config, secret_id, poll_seconds, created_by, \
             connector_client_id, access_expires_at, grant_expires_at) values ($1, $2, $3, $4, $5, $6, $7, $8, $9, \
             $10, $11) returning id",
        )
        .bind(client.household_id)
        .bind(p.profile_id)
        .bind(&p.kind)
        .bind(&p.label)
        .bind(&config)
        .bind(secret_id)
        .bind(DEFAULT_POLL_SECONDS)
        .bind(p.user_id)
        .bind(client.id)
        .bind(access_at)
        .bind(grant_at)
        .fetch_one(&mut *conn)
        .await?;
        forget(id);
        return Ok(id);
    };
    let old: Option<(Option<Uuid>, Option<String>)> = sqlx::query_as(
        "select secret_id, config->>'patient' from source where id = $1 and household_id = $2 and profile_id = $3 \
         and kind = $4 and connector_client_id = $5 for update",
    )
    .bind(source_id)
    .bind(client.household_id)
    .bind(p.profile_id)
    .bind(&p.kind)
    .bind(client.id)
    .fetch_optional(&mut *conn)
    .await?;
    let Some((old, patient)) = old else {
        return Err(refused(
            "source_changed",
            "This connection was removed or changed. Start again.",
        ));
    };
    // another account at the vendor would bring another person's records into this profile, from
    // this one's cursor: the old grant, config and cursor stay
    if patient.is_some() && t.patient != patient {
        return Err(refused(
            "another_patient",
            "This sign-in at the vendor is for another patient. Sign in with the account of this person.",
        ));
    }
    // the new grant replaces the old one; without a refresh token there is nothing left to keep
    let secret_id = match (&t.refresh, old) {
        (Some(r), Some(id)) => {
            secrets::replace_profile_secret(conn, sb, id, p.profile_id, r).await?;
            Some(id)
        }
        (Some(r), None) => Some(
            secrets::store_profile_secret(
                conn,
                sb,
                client.household_id,
                p.profile_id,
                "connector_refresh_token",
                r,
            )
            .await?,
        ),
        (None, _) => None,
    };
    sqlx::query(
        "update source set config = $2, secret_id = $3, enabled = true, reconsent_needed_at = null, \
         access_expires_at = $4, grant_expires_at = $5 where id = $1",
    )
    .bind(source_id)
    .bind(&config)
    .bind(secret_id)
    .bind(access_at)
    .bind(grant_at)
    .execute(&mut *conn)
    .await?;
    if let (Some(old), None) = (old, secret_id) {
        sqlx::query("delete from secret where id = $1")
            .bind(old)
            .execute(&mut *conn)
            .await?;
    }
    forget(source_id);
    Ok(source_id)
}

// ----------------------------------------------------------------- refresh ----

/// The access tokens in memory, with the instant each stops being used.
type AccessCache = Mutex<HashMap<Uuid, (SecretString, DateTime<Utc>)>>;

static ACCESS: LazyLock<AccessCache> = LazyLock::new(|| Mutex::new(HashMap::new()));

fn cached(source_id: Uuid, now: DateTime<Utc>) -> Option<SecretString> {
    let m = ACCESS.lock().ok()?;
    m.get(&source_id)
        .filter(|(_, until)| *until > now + Duration::seconds(MARGIN_SECONDS))
        .map(|(t, _)| t.clone())
}

/// Keep a connection's access token in memory until it expires (never on disk).
pub fn remember(source_id: Uuid, t: &TokenSet, now: DateTime<Utc>) {
    let until = now + Duration::seconds(t.expires_in.unwrap_or(UNKNOWN_LIFETIME_SECONDS));
    if let Ok(mut m) = ACCESS.lock() {
        m.insert(source_id, (t.access.clone(), until));
    }
}

/// Drop a connection's access token from memory.
pub fn forget(source_id: Uuid) {
    if let Ok(mut m) = ACCESS.lock() {
        m.remove(&source_id);
    }
}

async fn needs_reconsent(conn: &mut PgConnection, source_id: Uuid, now: DateTime<Utc>) -> Result<(), sqlx::Error> {
    sqlx::query("update source set reconsent_needed_at = $2 where id = $1")
        .bind(source_id)
        .bind(now)
        .execute(conn)
        .await
        .map(|_| ())
}

/// household, profile, secret, client, config, reconsent, enabled
type Locked = (
    Uuid,
    Option<Uuid>,
    Option<Uuid>,
    Option<Uuid>,
    Value,
    Option<DateTime<Utc>>,
    bool,
);

/// A usable access token for the connection: the one in memory, or a refresh. `now` is the clock the
/// expiry is counted from (tests move it).
///
/// The refresh holds the source row `FOR UPDATE` from before the request until the rotated token is
/// committed, so two refreshes of one source never use one refresh token twice: the second waits and
/// then reuses the first one's access token.
pub async fn access_token(
    pool: &PgPool,
    sb: &SecretBox,
    source_id: Uuid,
    egress: &Egress,
    now: DateTime<Utc>,
) -> Result<SecretString, OAuthError> {
    if let Some(t) = cached(source_id, now) {
        return Ok(t);
    }
    let kinds: Vec<&str> = OAUTH_KINDS.to_vec();
    // the profile's key before the row lock: a profile delete holds its key row, then waits on this
    // source row, and a first key lookup under the lock would wait on the delete
    let owner: Option<Uuid> = sqlx::query_scalar("select profile_id from source where id = $1")
        .bind(source_id)
        .fetch_optional(pool)
        .await?
        .flatten();
    if let Some(p) = owner {
        secrets::prepare_profile_key(sb, p).await;
    }
    let mut tx = pool.begin().await?;
    let row: Option<Locked> = sqlx::query_as(
        "select household_id, profile_id, secret_id, connector_client_id, config, reconsent_needed_at, enabled \
         from source where id = $1 and kind = any($2) for update",
    )
    .bind(source_id)
    .bind(&kinds)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((household, profile, secret_id, client_id, config, reconsent, enabled)) = row else {
        return Err(OAuthError::Core(Error::NotFound("Unknown connection.".into())));
    };
    if reconsent.is_some() {
        return Err(OAuthError::ReconsentNeeded);
    }
    if !enabled {
        return Err(refused("disconnected", "This connection is switched off."));
    }
    // another refresh finished while this one waited for the lock
    if let Some(t) = cached(source_id, now) {
        return Ok(t);
    }
    let Some(secret_id) = secret_id else {
        // the vendor gave no refresh token and the access token is gone
        needs_reconsent(&mut tx, source_id, now).await?;
        tx.commit().await?;
        return Err(OAuthError::ReconsentNeeded);
    };
    let endpoints = endpoints_of(&config)?;
    let client = match client_id {
        Some(c) => load_client(&mut tx, Some(sb), household, c).await?,
        None => None,
    }
    .ok_or_else(|| refused("client_gone", "The vendor client of this connection is gone."))?;
    let refresh = secrets::read_secret(&mut tx, sb, household, secret_id).await?;
    let req = TokenRequest::new(
        &client,
        endpoints.auth,
        vec![
            ("grant_type", "refresh_token".into()),
            ("refresh_token", refresh.expose().into()),
        ],
    );
    let http = SourceHttp::new(declared_hosts(&config), egress.clone());
    let answer = req.send(&http, &endpoints.token).await?;
    let t = match parse_tokens(&answer) {
        Ok(t) => t,
        Err(OAuthError::Vendor { code, .. }) if code == "invalid_grant" => {
            needs_reconsent(&mut tx, source_id, now).await?;
            tx.commit().await?;
            forget(source_id);
            return Err(OAuthError::ReconsentNeeded);
        }
        Err(e) => return Err(e),
    };
    let rotated = t.refresh.is_some();
    // the second caller may read the token as soon as the lock goes, so it is in memory first
    remember(source_id, &t, now);
    let stored: Result<(), OAuthError> = async {
        if let Some(r) = &t.refresh {
            // under the profile's key, also when the token it replaces was sealed under the secrets key
            match profile {
                Some(p) => secrets::replace_profile_secret(&mut tx, sb, secret_id, p, r).await?,
                None => secrets::replace_secret(&mut tx, sb, secret_id, r).await?,
            }
        }
        sqlx::query(
            "update source set access_expires_at = $2, grant_expires_at = coalesce($3, grant_expires_at) \
             where id = $1",
        )
        .bind(source_id)
        .bind(after(now, t.expires_in))
        .bind(after(now, t.refresh_expires_in))
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }
    .await;
    match stored {
        Ok(()) => Ok(t.access),
        Err(e) => {
            forget(source_id);
            if !rotated {
                return Err(e);
            }
            // the vendor already retired the token we hold: say so instead of failing silently later
            tracing::warn!(%source_id, "a rotated refresh token could not be stored: the connection needs consent again");
            let mut conn = pool.acquire().await?;
            needs_reconsent(&mut conn, source_id, now).await?;
            Err(OAuthError::ReconsentNeeded)
        }
    }
}

// ------------------------------------------------------------ revoke, disconnect ----

/// What happened at the vendor when a connection was removed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Revocation {
    Revoked,
    /// The server offers no revocation endpoint: the person removes the access at the vendor.
    NotOffered,
    /// The revocation endpoint refused or did not answer.
    Failed,
    /// No refresh token was held, so there was nothing to revoke.
    NoToken,
}

/// RFC 7009 for the held refresh token, best effort and never an error. `conn` holds the row lock.
async fn revoke_held(
    conn: &mut PgConnection,
    sb: Option<&SecretBox>,
    (source_id, household, secret_id, client_id, config): (Uuid, Uuid, Option<Uuid>, Option<Uuid>, &Value),
    egress: &Egress,
) -> Revocation {
    let Some(secret_id) = secret_id else {
        return Revocation::NoToken;
    };
    let Ok(endpoints) = endpoints_of(config) else {
        return Revocation::NotOffered;
    };
    let Some(url) = endpoints.revocation.clone() else {
        return Revocation::NotOffered;
    };
    let tried: Result<Fetched, OAuthError> = async {
        let sb = sb.ok_or_else(|| refused("no_key", "The secrets key is not set."))?;
        let client = match client_id {
            Some(c) => load_client(&mut *conn, Some(sb), household, c).await?,
            None => None,
        }
        .ok_or_else(|| refused("client_gone", "The vendor client of this connection is gone."))?;
        let token = secrets::read_secret(&mut *conn, sb, household, secret_id).await?;
        let req = TokenRequest::new(
            &client,
            endpoints.auth,
            vec![
                ("token", token.expose().into()),
                ("token_type_hint", "refresh_token".into()),
            ],
        );
        req.send(&SourceHttp::new(declared_hosts(config), egress.clone()), &url)
            .await
    }
    .await;
    match tried {
        Ok(f) if f.status == 200 => Revocation::Revoked,
        Ok(f) => {
            tracing::warn!(%source_id, status = f.status, "the vendor refused the revocation");
            Revocation::Failed
        }
        Err(e) => {
            tracing::warn!(%source_id, error = %e, "the revocation did not go through");
            Revocation::Failed
        }
    }
}

type Held = (Uuid, Option<Uuid>, Option<Uuid>, Value);

async fn lock_held(conn: &mut PgConnection, source_id: Uuid) -> Result<Option<Held>, OAuthError> {
    let kinds: Vec<&str> = OAUTH_KINDS.to_vec();
    Ok(sqlx::query_as(
        "select household_id, secret_id, connector_client_id, config from source where id = $1 and kind = any($2) \
         for update",
    )
    .bind(source_id)
    .bind(&kinds)
    .fetch_optional(conn)
    .await?)
}

/// Revoke at the vendor (when it offers it), then delete the refresh token and the cursor and switch
/// the source off. The source row, its runs and what it imported stay; a reconnect starts it again.
pub async fn disconnect(
    pool: &PgPool,
    sb: &SecretBox,
    source_id: Uuid,
    egress: &Egress,
) -> Result<Revocation, OAuthError> {
    let mut tx = pool.begin().await?;
    let Some((household, secret_id, client_id, config)) = lock_held(&mut tx, source_id).await? else {
        return Err(OAuthError::Core(Error::NotFound("Unknown connection.".into())));
    };
    let outcome = revoke_held(
        &mut tx,
        Some(sb),
        (source_id, household, secret_id, client_id, &config),
        egress,
    )
    .await;
    sqlx::query(
        "update source set secret_id = null, cursor = null, enabled = false, access_expires_at = null, \
         grant_expires_at = null, reconsent_needed_at = null where id = $1",
    )
    .bind(source_id)
    .execute(&mut *tx)
    .await?;
    if let Some(id) = secret_id {
        sqlx::query("delete from secret where id = $1")
            .bind(id)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    forget(source_id);
    Ok(outcome)
}

/// One connection's revocation before a profile delete.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Revoked {
    pub source_id: Uuid,
    pub kind: String,
    pub label: String,
    pub outcome: Revocation,
    /// Where the person removes the access by hand when the revocation failed.
    pub revoke_page: Option<String>,
}

/// Revoke every connection of the profile at its vendor, best effort, before the profile delete (which
/// deletes the tokens whatever happens here). Each connection gets at most `limit` in all; one that
/// takes longer counts as failed. `sb` is None when no secrets key is set: every held token then
/// counts as failed.
pub async fn revoke_profile(
    pool: &PgPool,
    sb: Option<&SecretBox>,
    profile_id: Uuid,
    egress: &Egress,
    limit: std::time::Duration,
) -> Result<Vec<Revoked>, OAuthError> {
    let kinds: Vec<&str> = OAUTH_KINDS.to_vec();
    let ids: Vec<(Uuid, String, String)> = sqlx::query_as(
        "select id, kind, label from source where profile_id = $1 and kind = any($2) and secret_id is not null \
         order by created_at, id",
    )
    .bind(profile_id)
    .bind(&kinds)
    .fetch_all(pool)
    .await?;
    let mut out = Vec::with_capacity(ids.len());
    for (source_id, kind, label) in ids {
        // the cap covers the lot: the wait for a refresh that holds the row, the name lookup, the request
        let one = async {
            let mut tx = pool.begin().await?;
            let outcome = match lock_held(&mut tx, source_id).await? {
                Some((household, secret_id, client_id, config)) => {
                    revoke_held(
                        &mut tx,
                        sb,
                        (source_id, household, secret_id, client_id, &config),
                        egress,
                    )
                    .await
                }
                None => Revocation::NoToken,
            };
            tx.commit().await?;
            Ok::<_, OAuthError>(outcome)
        };
        let outcome = match tokio::time::timeout(limit, one).await {
            Ok(done) => done?,
            Err(_) => {
                tracing::warn!(%source_id, "the revocation before a profile delete took too long");
                Revocation::Failed
            }
        };
        forget(source_id);
        let revoke_page = match outcome {
            Revocation::Failed | Revocation::NotOffered => {
                kind_spec(&kind).and_then(|s| s.revoke_page).map(String::from)
            }
            _ => None,
        };
        out.push(Revoked {
            source_id,
            kind,
            label,
            outcome,
            revoke_page,
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_vendor_error_code_is_kept_only_when_it_is_a_plain_word() {
        assert_eq!(vendor_code(Some("invalid_grant")), "invalid_grant");
        assert_eq!(vendor_code(Some("token rt-123 is gone")), "unexpected");
        assert_eq!(vendor_code(Some(&"x".repeat(41))), "unexpected");
        assert_eq!(vendor_code(None), "unexpected");
    }

    #[test]
    fn the_declared_hosts_carry_the_port_and_skip_the_browser_only_endpoint() {
        let e = Endpoints {
            issuer: "https://FHIR.LabNorte.example/r4".into(),
            authorize: "https://login.labnorte.example/authorize".into(),
            token: "https://fhir.labnorte.example:8443/token".into(),
            revocation: None,
            scopes: SMART_SCOPES.into(),
            aud: None,
            auth: ClientAuth::Basic,
        };
        assert_eq!(
            e.hosts(),
            vec!["fhir.labnorte.example:443", "fhir.labnorte.example:8443"]
        );
        assert_eq!(authority("ftp://fhir.labnorte.example/"), None);
    }

    #[test]
    fn an_endpoint_is_https_or_plain_http_to_loopback_only_where_the_egress_allows_it() {
        let wide = Egress {
            allow_private: true,
            https_only: false,
            ..Egress::connector()
        };
        let strict = Egress::connector();
        for e in [&wide, &strict] {
            assert!(safe_endpoint("https://login.labnorte.example/authorize", e));
            assert!(!safe_endpoint(
                "javascript:alert(1)//https://login.labnorte.example/",
                e
            ));
            assert!(!safe_endpoint("data:text/html,<script>alert(1)</script>", e));
            assert!(!safe_endpoint("https://alex:pw@login.labnorte.example/authorize", e));
            assert!(!safe_endpoint("https://alex@login.labnorte.example/authorize", e));
            assert!(!safe_endpoint("http://login.labnorte.example/authorize", e));
            assert!(!safe_endpoint("ftp://login.labnorte.example/authorize", e));
            assert!(!safe_endpoint("not a url", e));
        }
        assert!(safe_endpoint("http://127.0.0.1:8080/authorize", &wide));
        assert!(safe_endpoint("http://[::1]:8080/authorize", &wide));
        assert!(safe_endpoint("http://localhost:8080/authorize", &wide));
        assert!(!safe_endpoint("http://10.0.0.1/authorize", &wide));
        assert!(!safe_endpoint("http://127.0.0.1:8080/authorize", &strict));
    }

    #[test]
    fn basic_auth_form_encodes_the_id_and_secret() {
        // RFC 6749 2.3.1: "a:b" as client id is sent as a%3Ab
        let h = basic("a:b", "s p");
        let raw = base64::engine::general_purpose::STANDARD
            .decode(h.trim_start_matches("Basic "))
            .expect("base64");
        assert_eq!(raw, b"a%3Ab:s+p");
    }

    #[test]
    fn a_smart_client_is_any_ehr_vendor_and_the_others_are_their_own() {
        assert!(client_fits("smart", "epic"));
        assert!(!client_fits("smart", "bluebutton"));
        assert!(client_fits("bluebutton", "bluebutton"));
        assert!(!client_fits("dexcom", "epic"));
    }
}
