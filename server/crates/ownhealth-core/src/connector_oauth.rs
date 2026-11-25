//! The OAuth part every connector shares (connectors.md 6.2): SMART on FHIR, Blue Button and Dexcom
//! launch, call back, reconnect, disconnect and refresh through this module only, and keep their
//! tokens nowhere else.
//!
//! - **Launch.** [`begin`] builds the authorize URL: PKCE S256, a 43-character `state`, and the hash of
//!   a binder cookie the route sets, kept with the verifier in `auth_challenge` (purpose `connector`).
//! - **Callback.** [`take_state`] uses the state up (once, within [`STATE_TTL_SECONDS`], in the
//!   browser that started it); [`exchange`] trades the code as a public client or with the client
//!   secret (`client_secret_basic` or `client_secret_post`); [`save_connection`] stores both tokens sealed
//!   under the profile's data key, with the refresh token held by `source.secret_id` and the endpoints in
//!   `source.config`. A reconnect is the same flow for an existing source, and it clears
//!   `reconsent_needed_at`.
//! - **Refresh.** [`access_token`] locks the source row `FOR UPDATE`, posts the refresh, stores the
//!   rotated token, commits, and only then hands the access token out. A second caller waits on the
//!   lock and reuses the first one's result. A reply with no refresh token keeps the old one.
//!   `invalid_grant` sets `reconsent_needed_at`; so does a rotated token that could not be stored.
//!   Blue Button persists that marker before sending its single-use token and clears it atomically
//!   with the replacement, so a lost reply or interrupted refresh can never spend it twice. A request
//!   that provably never left (no connection, a name that does not resolve) clears it again and
//!   waits for a retry.
//! - **Disconnect.** [`disconnect`] calls the revocation endpoint when the server offers one, then
//!   deletes both tokens and the cursor. [`revoke_profile`] does the vendor half before a
//!   profile delete. [`revoke_page`] says when the vendor's own page is shown as well.
//! - **Declared hosts** (connectors.md 6.3 rule 7). [`SourceHttp`] is the client a connector's poller
//!   uses: it refuses any host the source did not declare at connect time before `net` resolves it.
//!   `net` then does the address half (public, https, no redirects, capped).
//!
//! No token, verifier or code appears in a log line, an error message, a `Debug` print or `source.config`.

use base64::Engine as _;
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::postgres::PgAdvisoryLock;
use sqlx::{Connection, PgConnection, PgPool};
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

/// SMART App Launch, standalone patient access (connectors.md section 7): a patient picked at the
/// vendor, a refresh token, and read and search on each US Core type the SMART poller reads
/// (ownhealth-ingest-structured `smart.rs`), never a wildcard or a write scope. A server that does
/// not list `permission-v2` (a SMART 1.0 server) is asked for the same types as `.read` ([`v1_scopes`]).
pub const SMART_SCOPES: &str = "openid fhirUser launch/patient offline_access patient/Patient.rs \
    patient/AllergyIntolerance.rs patient/Condition.rs patient/DiagnosticReport.rs patient/DocumentReference.rs \
    patient/Encounter.rs patient/Immunization.rs patient/Medication.rs patient/MedicationRequest.rs \
    patient/Observation.rs patient/Procedure.rs";

/// The connector rule for SMART v1 servers: each `patient/<type>.rs` becomes `patient/<type>.read`.
pub fn v1_scopes(scopes: &str) -> String {
    scopes
        .split_whitespace()
        .map(|s| match s.strip_suffix(".rs") {
            Some(t) if s.starts_with("patient/") => format!("{t}.read"),
            _ => s.to_string(),
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// CMS Blue Button v3 requests only the three resources the poller reads.
pub const BLUEBUTTON_SCOPES: &str =
    "launch/patient patient/Patient.rs patient/Coverage.rs patient/ExplanationOfBenefit.rs";
const BLUEBUTTON_ISSUER: &str = "https://api.bluebutton.cms.gov/v3/fhir";
const BLUEBUTTON_SANDBOX: &str = "https://sandbox.bluebutton.cms.gov/v3/fhir";

/// The switch that lets a debug build with dev auth reach a fake vendor on loopback.
pub const DEV_LOOPBACK_SWITCH: &str = "OWNHEALTH_DEV_CONNECTOR_LOOPBACK";

/// How connector requests go out: https to public addresses only. In a debug build that runs with
/// dev auth, `dev_loopback_switch == Some("1")` widens it to plain http on loopback, so a person can
/// try a flow against a local fake; a release build ignores the switch, as it does without dev auth.
pub fn configured_egress(dev_auth: bool, dev_loopback_switch: Option<&str>) -> Egress {
    // never in a release build, as the SHC dev map (shc.rs)
    if cfg!(debug_assertions) && dev_auth && dev_loopback_switch == Some("1") {
        return Egress {
            allow_private: true,
            https_only: false,
            ..Egress::connector()
        };
    }
    Egress::connector()
}

/// [`configured_egress`] with the switch read from the environment ([`DEV_LOOPBACK_SWITCH`]): what
/// the connector routes and the pollers use.
pub fn connector_egress(dev_auth: bool) -> Egress {
    configured_egress(dev_auth, std::env::var(DEV_LOOPBACK_SWITCH).ok().as_deref())
}

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
    /// The pull's grant was replaced while it was running.
    #[error("the connection's grant changed")]
    GrantChanged,
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
    /// CMS's v3 OAuth endpoints are siblings of the fixed FHIR base.
    BlueButton,
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

/// The kinds the launch route serves.
pub fn kind_spec(kind: &str) -> Option<KindSpec> {
    match kind {
        "smart" => Some(KindSpec {
            discovery: Discovery::Smart,
            scopes: SMART_SCOPES,
            default_issuer: None,
            revoke_page: None,
        }),
        "bluebutton" => Some(KindSpec {
            discovery: Discovery::BlueButton,
            scopes: BLUEBUTTON_SCOPES,
            default_issuer: Some(BLUEBUTTON_ISSUER),
            revoke_page: Some("https://www.medicare.gov/account/"),
        }),
        _ => None,
    }
}

/// The vendor's own page to show after a disconnect or a profile delete. Shown when the revocation
/// failed or is not offered; for Blue Button always, because CMS's `revoke_token` ends the tokens
/// while the app stays listed in the person's Medicare account until they remove it there.
pub fn revoke_page(kind: &str, outcome: Revocation) -> Option<&'static str> {
    let page = kind_spec(kind)?.revoke_page;
    (kind == "bluebutton" || matches!(outcome, Revocation::Failed | Revocation::NotOffered))
        .then_some(page)
        .flatten()
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

/// The production and sandbox bases have fixed HTTPS origins. Tests can use a v3 FHIR base
/// on loopback only when connector egress was widened by the debug switch.
pub fn bluebutton_base(base: &str, egress: &Egress) -> bool {
    if matches!(base, BLUEBUTTON_ISSUER | BLUEBUTTON_SANDBOX) {
        return true;
    }
    !egress.https_only
        && egress.allow_private
        && safe_endpoint(base, egress)
        && url::Url::parse(base).is_ok_and(|u| {
            u.scheme() == "http" && u.path() == "/v3/fhir" && u.query().is_none() && u.fragment().is_none()
        })
}

fn bluebutton_endpoints(base: &str, egress: &Egress) -> Result<Endpoints, OAuthError> {
    let root = base
        .strip_suffix("/fhir")
        .filter(|_| bluebutton_base(base, egress))
        .ok_or_else(|| {
            refused(
                "bad_issuer",
                "Choose the CMS Blue Button v3 production or sandbox server.",
            )
        })?;
    Ok(Endpoints {
        issuer: base.into(),
        authorize: format!("{root}/o/authorize/"),
        token: format!("{root}/o/token/"),
        revocation: Some(format!("{root}/o/revoke_token/")),
        scopes: BLUEBUTTON_SCOPES.into(),
        aud: None,
        auth: ClientAuth::Basic,
    })
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
        Discovery::BlueButton => bluebutton_endpoints(&issuer, egress)?,
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
    // SMART v2 scopes (`.rs`) only on a server that lists `permission-v2`; a SMART 1.0 server lists
    // neither marker (they came with 2.0), so anything else is asked for v1 `.read`
    let scopes = match listed("capabilities", "permission-v2") {
        Some(true) => scopes.to_string(),
        _ => v1_scopes(scopes),
    };
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
        scopes,
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

fn bluebutton_requires_secret() -> OAuthError {
    refused(
        "client_secret_required",
        "Blue Button needs a registered client with a secret.",
    )
}

fn check_bluebutton_client(client: &ConnectorClient, endpoints: &Endpoints, egress: &Egress) -> Result<(), OAuthError> {
    if client.vendor != "bluebutton" || client.secret.is_none() {
        return Err(bluebutton_requires_secret());
    }
    let registered = client
        .issuer
        .as_deref()
        .unwrap_or(BLUEBUTTON_ISSUER)
        .trim()
        .trim_end_matches('/');
    if registered != endpoints.issuer || bluebutton_endpoints(registered, egress)? != *endpoints {
        return Err(refused(
            "bad_issuer",
            "This client is registered for another Blue Button server.",
        ));
    }
    Ok(())
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
        self.get_bounded(url, headers, self.egress.max_bytes)
            .await
            .map_err(|(err, _)| err)
    }

    /// GET with a per-request body cap within this connection's egress limit. An error includes
    /// how many body bytes arrived before it.
    pub async fn get_bounded(
        &self,
        url: &str,
        headers: &[(&str, &str)],
        max_bytes: usize,
    ) -> Result<Fetched, (OAuthError, usize)> {
        self.check(url).map_err(|err| (err, 0))?;
        let egress = Egress {
            max_bytes: max_bytes.min(self.egress.max_bytes),
            ..self.egress.clone()
        };
        net::get_counted(url, &egress, headers)
            .await
            .map_err(|(err, bytes)| (OAuthError::Net(err), bytes))
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
    if l.kind == "bluebutton" && !l.client.confidential {
        return Err(bluebutton_requires_secret());
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
    if p.kind == "bluebutton" {
        check_bluebutton_client(client, &p.endpoints, egress)?;
    }
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
    let tokens = parse_tokens(&req.send(&http, &p.endpoints.token).await?)?;
    if p.kind == "bluebutton" && tokens.patient.is_none() {
        // OwnHealth keeps nothing of this sign-in, so nothing it issued may stay alive at CMS
        if let Some(url) = &p.endpoints.revocation {
            let (token, hint) = match &tokens.refresh {
                Some(r) => (r, "refresh_token"),
                None => (&tokens.access, "access_token"),
            };
            let revoke = TokenRequest::new(
                client,
                p.endpoints.auth,
                vec![("token", token.expose().into()), ("token_type_hint", hint.into())],
            );
            match revoke.send(&http, url).await {
                Ok(f) if f.status == 200 => {}
                Ok(f) => tracing::warn!(
                    status = f.status,
                    "CMS refused to revoke a sign-in that named no patient"
                ),
                Err(e) => tracing::warn!(error = %e, "a sign-in that named no patient could not be revoked"),
            }
        }
        return Err(refused(
            "no_patient",
            "The Medicare sign-in did not identify a patient.",
        ));
    }
    Ok(tokens)
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
/// source with a new grant identity and the old grant's errors cleared. The source is switched
/// back on. Returns its id. The sealed access token becomes visible with the grant on commit.
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
    let access_at = access_until(t, now);
    let grant_at = after(now, t.refresh_expires_in);
    let grant_id = Uuid::new_v4();
    let source_id = p.source_id.unwrap_or_else(Uuid::new_v4);
    let seal_access = secrets::seal_source_access(sb, Some(p.profile_id), source_id, Some(grant_id), &t.access);
    if p.source_id.is_none() {
        let access = seal_access.await?;
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
        sqlx::query(
            "insert into source (household_id, profile_id, kind, label, config, secret_id, poll_seconds, created_by, \
             connector_client_id, access_expires_at, grant_expires_at, oauth_grant_id, id, oauth_access_token) \
             values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14)",
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
        .bind(grant_id)
        .bind(source_id)
        .bind(&access)
        .execute(&mut *conn)
        .await?;
        return Ok(source_id);
    }
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
    let access = seal_access.await?;
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
         access_expires_at = $4, grant_expires_at = $5, oauth_grant_id = $6, oauth_access_token = $7, \
         last_error_at = null, last_error = null, consecutive_failures = 0, \
         cursor = case when kind = 'bluebutton' then null else cursor end where id = $1",
    )
    .bind(source_id)
    .bind(&config)
    .bind(secret_id)
    .bind(access_at)
    .bind(grant_at)
    .bind(grant_id)
    .bind(&access)
    .execute(&mut *conn)
    .await?;
    if let (Some(old), None) = (old, secret_id) {
        sqlx::query("delete from secret where id = $1")
            .bind(old)
            .execute(&mut *conn)
            .await?;
    }
    Ok(source_id)
}

// ----------------------------------------------------------------- refresh ----

/// The source and its grant, including sources that predate grant identities.
type Grant = (Uuid, Option<Uuid>);

fn access_until(tokens: &TokenSet, now: DateTime<Utc>) -> DateTime<Utc> {
    now + Duration::seconds(tokens.expires_in.unwrap_or(UNKNOWN_LIFETIME_SECONDS))
}

async fn needs_reconsent(conn: &mut PgConnection, grant: Grant, now: DateTime<Utc>) -> Result<bool, sqlx::Error> {
    let result = sqlx::query(
        "update source set reconsent_needed_at = $2, oauth_access_token = null where id = $1 and enabled \
         and oauth_grant_id is not distinct from $3",
    )
    .bind(grant.0)
    .bind(now)
    .bind(grant.1)
    .execute(conn)
    .await?;
    Ok(result.rows_affected() > 0)
}

/// The request provably never left this server: refused before it was sent (an undeclared host,
/// an address the egress refuses, a name that does not resolve) or no connection opened. A timeout,
/// a dropped reply or any answer may come after the vendor read the request.
fn never_sent(e: &OAuthError) -> bool {
    matches!(
        e,
        OAuthError::UndeclaredHost(_)
            | OAuthError::Net(
                NetError::BadUrl
                    | NetError::NotHttps
                    | NetError::PrivateAddress(_)
                    | NetError::Resolve(_)
                    | NetError::Connect(_)
            )
    )
}

#[derive(sqlx::FromRow)]
struct Locked {
    household_id: Uuid,
    profile_id: Option<Uuid>,
    secret_id: Option<Uuid>,
    connector_client_id: Option<Uuid>,
    config: Value,
    reconsent_needed_at: Option<DateTime<Utc>>,
    enabled: bool,
    oauth_grant_id: Option<Uuid>,
    oauth_access_token: Option<Vec<u8>>,
    access_expires_at: Option<DateTime<Utc>>,
}

/// A usable access token for the connection: the committed token, or a refresh. `now` is the clock the
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
    access_token_inner(pool, sb, source_id, egress, now, None, None).await
}

/// Get a token for the grant a pull started with. If the FHIR endpoint rejected `rejected`, refresh
/// that token once (or reuse a token another pull has already refreshed for the same grant).
pub async fn access_token_for_grant(
    pool: &PgPool,
    sb: &SecretBox,
    source: &Source,
    egress: &Egress,
    now: DateTime<Utc>,
    rejected: Option<&SecretString>,
) -> Result<SecretString, OAuthError> {
    access_token_inner(pool, sb, source.id, egress, now, Some(source.oauth_grant_id), rejected).await
}

async fn access_token_inner(
    pool: &PgPool,
    sb: &SecretBox,
    source_id: Uuid,
    egress: &Egress,
    now: DateTime<Utc>,
    expected_grant: Option<Option<Uuid>>,
    rejected: Option<&SecretString>,
) -> Result<SecretString, OAuthError> {
    let fresh_after = now + Duration::seconds(MARGIN_SECONDS);
    // Normal pulls can use the committed token without taking the refresh lock. A 401 goes
    // straight to the locked path, which can compare against a token another worker refreshed.
    if let (Some(expected), None) = (expected_grant, rejected) {
        let stored: Option<(Option<Uuid>, Vec<u8>)> = sqlx::query_as(
            "select profile_id, oauth_access_token from source where id = $1 and enabled \
             and reconsent_needed_at is null and oauth_grant_id is not distinct from $2 \
             and access_expires_at > $3 and oauth_access_token is not null",
        )
        .bind(source_id)
        .bind(expected)
        .bind(fresh_after)
        .fetch_optional(pool)
        .await?;
        if let Some((profile, sealed)) = stored {
            return Ok(secrets::open_source_access(sb, profile, source_id, expected, &sealed).await?);
        }
    }
    let kinds: Vec<&str> = OAUTH_KINDS.to_vec();
    let mut conn = pool.acquire().await?;
    let single_use = sqlx::query_scalar::<_, bool>("select kind = 'bluebutton' from source where id = $1")
        .bind(source_id)
        .fetch_optional(&mut *conn)
        .await?
        .unwrap_or(false);
    // The session lock spans both transactions of a Blue Button refresh. Other callers wait
    // and reuse the committed result. A dropped future releases the lock.
    let mut guard;
    let conn: &mut PgConnection = if single_use {
        guard = PgAdvisoryLock::new(format!("connector.refresh:{source_id}"))
            .acquire(conn)
            .await?;
        &mut guard
    } else {
        &mut conn
    };
    let mut tx = conn.begin().await?;
    let row: Option<Locked> = sqlx::query_as(
        "select household_id, profile_id, secret_id, connector_client_id, config, reconsent_needed_at, enabled, \
         oauth_grant_id, oauth_access_token, access_expires_at from source where id = $1 and kind = any($2) for update",
    )
    .bind(source_id)
    .bind(&kinds)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(Locked {
        household_id: household,
        profile_id: profile,
        secret_id,
        connector_client_id: client_id,
        config,
        reconsent_needed_at,
        enabled,
        oauth_grant_id,
        oauth_access_token,
        access_expires_at,
    }) = row
    else {
        return Err(OAuthError::Core(Error::NotFound("Unknown connection.".into())));
    };
    if expected_grant.is_some_and(|expected| expected != oauth_grant_id) {
        return Err(OAuthError::GrantChanged);
    }
    if reconsent_needed_at.is_some() {
        return Err(OAuthError::ReconsentNeeded);
    }
    if !enabled {
        return Err(refused("disconnected", "This connection is switched off."));
    }
    let grant = (source_id, oauth_grant_id);
    if let Some(sealed) = oauth_access_token.as_deref()
        && access_expires_at.is_some_and(|until| until > fresh_after)
    {
        let token = secrets::open_source_access(sb, profile, source_id, oauth_grant_id, sealed).await?;
        if rejected.is_none_or(|old| !bool::from(token.expose().as_bytes().ct_eq(old.expose().as_bytes()))) {
            return Ok(token);
        }
    }
    // A profile's rotated token is sealed under the profile's key. Without the keys this process
    // could read an older token sealed under the secrets key, spend it at the vendor, and then fail
    // to store the new one: refuse before the request, so the stored token stays good.
    if profile.is_some() && !sb.has_keys() {
        return Err(refused(
            "profile_keys_missing",
            "This process has no profile keys, so it cannot store this connection's next token.",
        ));
    }
    let Some(secret_id) = secret_id else {
        // the vendor gave no refresh token and the access token is gone
        needs_reconsent(&mut tx, grant, now).await?;
        tx.commit().await?;
        return Err(OAuthError::ReconsentNeeded);
    };
    let endpoints = endpoints_of(&config)?;
    let client = match client_id {
        Some(c) => load_client(&mut tx, Some(sb), household, c).await?,
        None => None,
    }
    .ok_or_else(|| refused("client_gone", "The vendor client of this connection is gone."))?;
    // The rotated token is sealed under the profile's key: open that key now. A keyring that cannot
    // open it (another OWNHEALTH_SECRETS_KEY, the key row unreadable) could still read a token sealed
    // under the secrets key, and the vendor would rotate a token this process cannot store.
    if let Some(p) = profile
        && let Err(e) = secrets::open_profile_key(sb, p).await
    {
        tracing::warn!(error = %e, profile = %p, "the profile's key does not open here, so the refresh waits");
        return Err(refused(
            "profile_key_unopened",
            "This process cannot open the profile's key, so it cannot store this connection's next token.",
        ));
    }
    if single_use {
        check_bluebutton_client(&client, &endpoints, egress)?;
    }
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
    if single_use {
        // Commit the demand for consent before sending a token that CMS may spend even if we
        // never receive the reply. Only a committed replacement clears it. Re-lock the source
        // and check the grant because reconnect and disconnect use the row lock.
        needs_reconsent(&mut tx, grant, now).await?;
        tx.commit().await?;
        tx = conn.begin().await?;
        let current: Option<bool> = sqlx::query_scalar(
            "select enabled and oauth_grant_id is not distinct from $2 and secret_id is not distinct from $3 \
             from source where id = $1 for update",
        )
        .bind(source_id)
        .bind(oauth_grant_id)
        .bind(secret_id)
        .fetch_optional(&mut *tx)
        .await?;
        if current != Some(true) {
            return Err(OAuthError::GrantChanged);
        }
    }
    let answer = req.send(&http, &endpoints.token).await.and_then(|a| parse_tokens(&a));
    let t = match answer {
        Ok(t)
            if !single_use
                || (t.refresh.as_ref().is_some_and(|token| {
                    !bool::from(token.expose().as_bytes().ct_eq(refresh.expose().as_bytes()))
                }) && t.patient.as_deref().is_none_or(|patient| config["patient"] == patient)) =>
        {
            t
        }
        Err(e) if single_use && never_sent(&e) => {
            // The token never left, so CMS cannot have spent it: take the marker back under the
            // row lock this transaction holds, and let the poller try again later.
            sqlx::query(
                "update source set reconsent_needed_at = null where id = $1 and oauth_grant_id is not distinct from $2",
            )
            .bind(source_id)
            .bind(oauth_grant_id)
            .execute(&mut *tx)
            .await?;
            tx.commit().await?;
            tracing::warn!(%source_id, error = %e, "the Blue Button refresh never reached CMS, so it waits for a retry");
            return Err(e);
        }
        _ if single_use => {
            tracing::warn!(%source_id, "the Blue Button refresh may have spent its token: the connection needs consent again");
            return Err(OAuthError::ReconsentNeeded);
        }
        Ok(t) => t,
        Err(OAuthError::Vendor { code, .. }) if code == "invalid_grant" => {
            needs_reconsent(&mut tx, grant, now).await?;
            tx.commit().await?;
            return Err(OAuthError::ReconsentNeeded);
        }
        Err(e) => return Err(e),
    };
    let rotated = t.refresh.is_some();
    let stored: Result<(), OAuthError> = async {
        // A profile's token goes under the profile's key, also when the token it replaces was sealed
        // under the secrets key; a vendor that does not rotate keeps the same token, which is sealed
        // again under the profile's key once. The key lookup runs on a connection of its own and waits
        // only on the profile's key row, which a profile delete takes only after it removed the source
        // rows, so the source row held here cannot close a cycle with a delete.
        match (profile, &t.refresh) {
            (Some(p), Some(r)) => secrets::replace_profile_secret(&mut tx, sb, secret_id, p, r).await?,
            (Some(p), None) => {
                if !secrets::sealed_for_profile(&mut tx, secret_id, p).await? {
                    secrets::replace_profile_secret(&mut tx, sb, secret_id, p, &refresh).await?;
                }
            }
            (None, Some(r)) => secrets::replace_secret(&mut tx, sb, secret_id, r).await?,
            (None, None) => {}
        }
        let access = secrets::seal_source_access(sb, profile, source_id, oauth_grant_id, &t.access).await?;
        sqlx::query(
            "update source set access_expires_at = $2, grant_expires_at = coalesce($3, grant_expires_at), \
             config = case when $4::text is null then config else jsonb_set(config, '{scope}', to_jsonb($4::text)) end, \
             oauth_access_token = $5, reconsent_needed_at = null where id = $1",
        )
        .bind(source_id)
        .bind(access_until(&t, now))
        .bind(after(now, t.refresh_expires_in))
        .bind(&t.scope)
        .bind(&access)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }
    .await;
    match stored {
        Ok(()) => Ok(t.access),
        Err(e) => {
            if single_use {
                // The preflight already committed the reconsent marker. A failed COMMIT can
                // itself have an uncertain outcome, so the next read consults the stored row.
                tracing::warn!(%source_id, "the rotated Blue Button token could not be confirmed");
                return Err(OAuthError::ReconsentNeeded);
            }
            if !rotated {
                return Err(e);
            }
            // the vendor already retired the token we hold: say so instead of failing silently later
            tracing::warn!(%source_id, "a rotated refresh token could not be stored: the connection needs consent again");
            let mut conn = pool.acquire().await?;
            if needs_reconsent(&mut conn, grant, now).await? {
                Err(OAuthError::ReconsentNeeded)
            } else {
                Err(OAuthError::GrantChanged)
            }
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
        if client.vendor == "bluebutton" {
            check_bluebutton_client(&client, &endpoints, egress)?;
        }
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
        "select household_id, secret_id, connector_client_id, config \
         from source where id = $1 and kind = any($2) for update",
    )
    .bind(source_id)
    .bind(&kinds)
    .fetch_optional(conn)
    .await?)
}

/// Revoke at the vendor (when it offers it), then delete both tokens and the cursor and switch
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
        "update source set secret_id = null, oauth_access_token = null, cursor = null, enabled = false, access_expires_at = null, \
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
        let revoke_page = revoke_page(&kind, outcome).map(String::from);
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

    #[tokio::test]
    async fn blue_button_uses_only_cms_v3_and_basic_client_auth() {
        let spec = kind_spec("bluebutton").expect("Blue Button is available for launch");
        let egress = Egress::connector();
        let cms = spec.default_issuer.expect("production CMS base");
        let endpoints = resolve(&spec, cms, &egress).await.expect("CMS v3 endpoints");
        assert_eq!(endpoints.issuer, "https://api.bluebutton.cms.gov/v3/fhir");
        assert_eq!(endpoints.authorize, "https://api.bluebutton.cms.gov/v3/o/authorize/");
        assert_eq!(endpoints.token, "https://api.bluebutton.cms.gov/v3/o/token/");
        assert_eq!(
            endpoints.revocation.as_deref(),
            Some("https://api.bluebutton.cms.gov/v3/o/revoke_token/")
        );
        assert_eq!(endpoints.auth, ClientAuth::Basic);
        for issuer in [
            "https://api.bluebutton.cms.gov/v2/fhir",
            "https://api.bluebutton.cms.gov.evil.example/v3/fhir",
            "https://api.bluebutton.cms.gov/v3/fhir?other=1",
        ] {
            assert!(resolve(&spec, issuer, &egress).await.is_err());
        }
    }

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
    fn a_v1_server_is_asked_for_read_on_the_same_types() {
        let v1 = v1_scopes(SMART_SCOPES);
        assert!(v1.contains("patient/Observation.read"), "{v1}");
        assert!(v1.contains("launch/patient offline_access"), "{v1}");
        assert!(!v1.contains(".rs"), "{v1}");
        assert_eq!(v1.split_whitespace().count(), SMART_SCOPES.split_whitespace().count());
        assert!(
            SMART_SCOPES
                .split_whitespace()
                .all(|s| !s.starts_with("patient/") || (s.ends_with(".rs") && !s.contains('*'))),
            "read and search on named types only"
        );
    }

    #[test]
    fn the_dev_loopback_switch_widens_only_a_debug_build_with_dev_auth() {
        assert_eq!(configured_egress(false, Some("1")), Egress::connector());
        assert_eq!(configured_egress(true, Some("0")), Egress::connector());
        assert_eq!(configured_egress(true, None), Egress::connector());
        let dev = configured_egress(true, Some("1"));
        assert_eq!(dev.allow_private, cfg!(debug_assertions), "{dev:?}");
        assert_eq!(dev.https_only, !cfg!(debug_assertions), "{dev:?}");
    }

    #[test]
    fn a_smart_client_is_any_ehr_vendor_and_the_others_are_their_own() {
        assert!(client_fits("smart", "epic"));
        assert!(!client_fits("smart", "bluebutton"));
        assert!(client_fits("bluebutton", "bluebutton"));
        assert!(!client_fits("dexcom", "epic"));
    }
}
