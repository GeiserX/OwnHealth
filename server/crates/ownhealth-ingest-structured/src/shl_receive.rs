//! SMART Health Links, received (slice 23): a link a clinic or another app sends, pasted as
//! `shlink:/…` text or as a viewer URL that ends in `#shlink:/…`.
//!
//! Spec: HL7 FHIR IG "SMART Health Cards and Links", Health Links Specification,
//! <https://build.fhir.org/ig/HL7/smart-health-cards-and-links/links-specification.html>. A comment
//! that quotes a section uses that page's heading. The sending side is ownhealth-ops `shl.rs`; what
//! it writes is read here as it is.
//!
//! 1. [`parse`] reads the link. It uses no network. A link whose `url` is a literal private,
//!    loopback or link-local address, or whose `exp` has passed, is refused here, so the person is
//!    never asked about it.
//! 2. The person approves the link's host (the route asks), and types the passcode once when the
//!    link has the `P` flag. Nothing is fetched before that.
//! 3. [`download`] POSTs the manifest request through `net::post_json` (a `U` link: one GET of the
//!    file, with `recipient` in the query) under the connector egress guard: https, public addresses
//!    only, no redirects, [`MAX_BYTES`] per answer. Each file, embedded or at its location (fetched
//!    once; a location that answers 404 or 410 asks to read the link again), is held in memory.
//!    The route checks again that the person may import, then [`keep_all`] stores each file as a
//!    blob of the profile BEFORE it is decrypted (bytes first); each comes back as a [`Held`]: its
//!    sha256 and its content type. ([`fetch`] is both steps at once.)
//!    A file stays stored when a later step refuses the import (it is the profile's, under its key,
//!    and goes with the profile): sent back as `held`, it opens again without a second request,
//!    through [`reopen`], which keeps to the type the file has.
//! 4. [`open`] decrypts the held files with the link's key (`jose::decrypt`) and sorts them:
//!    `application/fhir+json` is read as FHIR, `application/smart-health-card` goes to slice 22
//!    (`shc::read`), and `application/smart-api-access` (or a type we do not know) is counted as not
//!    imported, with its reason.
//! 5. [`check`] asks about the issuer host of every card before any key is fetched (slice 22's
//!    rule); the route then sends the held files back with the person's yes, so the link is not
//!    asked twice. Then it verifies the cards with `shc::verify`, with one issuer key cache and one
//!    time limit for the whole link.
//! 6. [`store`] keeps everything for one profile: each FHIR file is one document on channel `shl`
//!    whose rows go through `fhir::import_resources`; each card file goes to `shc::store`. Each
//!    document's `flags.shl` names the host, label and stored received file of the first link it
//!    came through; the same file received through another link later keeps that provenance.
//!
//! The passcode goes to the link's server in the request body of a `P` link and nowhere else: never
//! into a URL, a log line, an error, a row or a blob, and never to a link without the `P` flag. The
//! key never leaves this request either.

use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use chrono::{DateTime, Utc};
use ownhealth_core::access::Profile;
use ownhealth_core::blobs::{BlobOwner, BlobStore};
use ownhealth_core::documents::{NewDocument, create_document};
use ownhealth_core::ingest::ImportSummary;
use ownhealth_core::jose;
use ownhealth_core::net::{self, Egress, NetError};
use ownhealth_core::{Error, Result};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::PgConnection;
use uuid::Uuid;

use crate::fhir;
use crate::shc::{self, Ask, Card, Route};

/// `document_source.channel` of a file received through a link.
pub const CHANNEL: &str = "shl";
/// "SMART Health Link Manifest Request": `recipient` is required. We name the app, never the person.
pub const RECIPIENT: &str = "OwnHealth";
/// The cap on each answer: the manifest, and each file before and after it is decrypted.
pub const MAX_BYTES: usize = 8 * 1024 * 1024;
/// At most this many files in one link.
pub const MAX_FILES: usize = 10;
/// Every fetch of one import stops after this.
pub const FETCH_LIMIT: Duration = Duration::from_secs(60);
/// The check of every card of one link stops after this, whatever the number of its card files:
/// twice slice 22's limit for one card file, as the issuers' keys are fetched once for the link.
pub const VERIFY_LIMIT: Duration = Duration::from_secs(2 * shc::VERIFY_LIMIT.as_secs());
/// "files array": the three content types a manifest file may have.
pub const FHIR: &str = "application/fhir+json";
pub const SMART_HEALTH_CARD: &str = "application/smart-health-card";
pub const API_ACCESS: &str = "application/smart-api-access";
/// Every reason code, as the UI lists them (ui/src/lib/logic/shc.ts SHL_REASONS).
pub const REASONS: [&str; 16] = [
    "link",
    "private_host",
    "expired",
    "passcode_needed",
    "passcode",
    "inactive",
    "redirect",
    "rate_limited",
    "fetch",
    "manifest",
    "no_files",
    "too_many",
    "decrypt",
    "not_fhir",
    "held",
    "location_expired",
];
/// Why a file of a link was not imported (ui/src/lib/logic/shc.ts SHL_NOT_IMPORTED).
pub const NOT_IMPORTED: [&str; 2] = ["api_access", "unknown_type"];

/// Why a link was not imported. Each message names what was wrong, never a key, a passcode or a
/// server's own text.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Refusal {
    #[error("this is not a SMART Health Link that can be read: {0}")]
    Link(&'static str),
    #[error("{0} has a private, loopback or link-local address, so the link was not opened")]
    PrivateHost(String),
    #[error("the link expired on {0}")]
    Expired(String),
    #[error("the link needs its passcode")]
    PasscodeNeeded,
    /// The link's server says how many attempts are left, when it says.
    #[error("the passcode is not right")]
    Passcode(Option<i64>),
    #[error("the link is no longer active")]
    Inactive,
    #[error("the link's server answered with a redirect, which is not followed")]
    Redirect,
    #[error("the link's server asks to wait before the next request")]
    RateLimited,
    /// What went wrong: `dns`, `timeout`, `connect`, `too_large` or `http_<status>`.
    #[error("the link's server could not be read ({0})")]
    Fetch(String),
    #[error("the link's manifest does not meet the SMART Health Links rules: {0}")]
    Manifest(&'static str),
    #[error("the link holds no files")]
    NoFiles,
    #[error("the link holds more than {MAX_FILES} files")]
    TooMany,
    #[error("a file of the link does not open with the link's key")]
    Decrypt,
    #[error("a FHIR file of the link cannot be read")]
    NotFhir,
    /// Also a file sent back under another type than the file has.
    #[error("a file received before is not stored as it was sent back")]
    Held,
    /// A file's location answered 404 or 410 while the manifest answered: the location expired or
    /// was used, and reading the link again gives a fresh one.
    #[error("a file of the link is no longer at the address the link's server gave; read the link again")]
    LocationExpired,
}

impl Refusal {
    /// A stable code for the UI, which words each one in es and en.
    pub fn code(&self) -> &'static str {
        match self {
            Refusal::Link(_) => "link",
            Refusal::PrivateHost(_) => "private_host",
            Refusal::Expired(_) => "expired",
            Refusal::PasscodeNeeded => "passcode_needed",
            Refusal::Passcode(_) => "passcode",
            Refusal::Inactive => "inactive",
            Refusal::Redirect => "redirect",
            Refusal::RateLimited => "rate_limited",
            Refusal::Fetch(_) => "fetch",
            Refusal::Manifest(_) => "manifest",
            Refusal::NoFiles => "no_files",
            Refusal::TooMany => "too_many",
            Refusal::Decrypt => "decrypt",
            Refusal::NotFhir => "not_fhir",
            Refusal::Held => "held",
            Refusal::LocationExpired => "location_expired",
        }
    }

    /// The one value the UI puts in its sentence: a host, a date, the attempts left or a fetch code.
    pub fn detail(&self) -> Option<String> {
        match self {
            Refusal::PrivateHost(d) | Refusal::Expired(d) | Refusal::Fetch(d) => Some(d.clone()),
            Refusal::Passcode(left) => left.map(|n| n.to_string()),
            _ => None,
        }
    }
}

/// Every text this module writes for a person, for the wording lint.
pub fn phrases() -> Value {
    json!({ "en": samples().iter().map(ToString::to_string).collect::<Vec<_>>() })
}

/// One refusal of each kind, in [`REASONS`] order.
fn samples() -> Vec<Refusal> {
    vec![
        Refusal::Link("the key is not 32 bytes"),
        Refusal::PrivateHost("127.0.0.1".into()),
        Refusal::Expired("2026-01-01".into()),
        Refusal::PasscodeNeeded,
        Refusal::Passcode(Some(4)),
        Refusal::Inactive,
        Refusal::Redirect,
        Refusal::RateLimited,
        Refusal::Fetch("http_500".into()),
        Refusal::Manifest("a file has no contentType"),
        Refusal::NoFiles,
        Refusal::TooMany,
        Refusal::Decrypt,
        Refusal::NotFhir,
        Refusal::Held,
        Refusal::LocationExpired,
    ]
}

/// What stopped an import: the link itself, or a card it holds (slice 22's refusal and the issuer
/// it names).
#[derive(Debug, Clone, PartialEq)]
pub enum Stop {
    Link(Refusal),
    Card { refusal: shc::Refusal, iss: Option<String> },
}

impl From<Refusal> for Stop {
    fn from(r: Refusal) -> Self {
        Stop::Link(r)
    }
}

// ------------------------------------------------------------------ the link ----

/// A link as its payload says it ("Construct a SMART Health Link Payload"). Its Debug never shows
/// the key.
#[derive(Clone, PartialEq, Eq)]
pub struct Link {
    /// The manifest URL, or with the `U` flag the file's own URL.
    pub url: String,
    /// The host the person approves: the url's host, with its port when it is not 443.
    pub host: String,
    key: [u8; jose::KEY_LEN],
    pub exp: Option<DateTime<Utc>>,
    /// The flag letters, as sent.
    pub flag: String,
    /// At most 80 characters; text the sender wrote, untrusted.
    pub label: Option<String>,
}

impl std::fmt::Debug for Link {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Link")
            .field("url", &self.url)
            .field("flag", &self.flag)
            .field("label", &self.label)
            .finish_non_exhaustive()
    }
}

impl Link {
    /// `P`: the manifest needs a passcode.
    pub fn needs_passcode(&self) -> bool {
        self.flag.contains('P')
    }

    /// `U`: the url is the one encrypted file, fetched with a GET.
    pub fn direct(&self) -> bool {
        self.flag.contains('U')
    }
}

/// True when the text holds a link (`shlink:/`, alone or after a viewer URL). Text that starts with
/// `{` is a card envelope (JSON), whatever its strings hold, and stays with the card receiver.
pub fn is_link(text: &str) -> bool {
    !text.trim_start().starts_with('{') && text.contains("shlink:/")
}

/// Read a link from pasted text at `now`: `shlink:/<payload>`, optionally "prefixed with a viewer
/// URL that ends with `#`". No network.
pub fn parse(text: &str, now: DateTime<Utc>) -> std::result::Result<Link, Refusal> {
    let start = text.find("shlink:/").ok_or(Refusal::Link("there is no shlink:/"))?;
    let b64 = text[start + "shlink:/".len()..]
        .split(|c: char| c.is_whitespace())
        .next()
        .unwrap_or_default()
        .trim_end_matches('=');
    let raw = URL_SAFE_NO_PAD
        .decode(b64)
        .map_err(|_| Refusal::Link("the payload is not base64url"))?;
    let p: Value = serde_json::from_slice(&raw).map_err(|_| Refusal::Link("the payload is not JSON"))?;
    if !p.is_object() {
        return Err(Refusal::Link("the payload is not JSON"));
    }
    // "v": MAY be omitted when the default value (1) applies; a later version may mean other rules
    if p.get("v").is_some_and(|v| v.as_i64() != Some(1)) {
        return Err(Refusal::Link("its version is not 1"));
    }
    let url = p
        .get("url")
        .and_then(Value::as_str)
        .ok_or(Refusal::Link("there is no url"))?;
    let host = host_of(url)?;
    let key = p
        .get("key")
        .and_then(Value::as_str)
        .filter(|k| k.len() == 43)
        .and_then(|k| URL_SAFE_NO_PAD.decode(k).ok())
        .and_then(|k| <[u8; jose::KEY_LEN]>::try_from(k).ok())
        .ok_or(Refusal::Link("the key is not 32 bytes"))?;
    let exp = match p.get("exp") {
        None | Some(Value::Null) => None,
        // "exp": seconds since the epoch, which may have a fraction; kept to the millisecond
        Some(v) => Some(
            v.as_f64()
                .and_then(|s| DateTime::from_timestamp_millis((s * 1000.0) as i64))
                .ok_or(Refusal::Link("exp is not a date"))?,
        ),
    };
    let flag = match p.get("flag") {
        None => String::new(),
        Some(v) => v
            .as_str()
            .filter(|f| f.chars().all(|c| c.is_ascii_uppercase()))
            .ok_or(Refusal::Link("flag is not a string of letters"))?
            .to_string(),
    };
    // "U": SHALL NOT be used in combination with P
    if flag.contains('U') && flag.contains('P') {
        return Err(Refusal::Link("it has both the U and the P flag"));
    }
    if let Some(exp) = exp
        && exp < now
    {
        return Err(Refusal::Expired(exp.format("%Y-%m-%d").to_string()));
    }
    let label = p
        .get("label")
        .and_then(Value::as_str)
        .map(|l| l.chars().filter(|c| !c.is_control()).take(80).collect::<String>())
        .filter(|l| !l.trim().is_empty());
    Ok(Link {
        url: url.to_string(),
        host,
        key,
        exp,
        flag,
        label,
    })
}

/// The url's host as the person sees it, with the port when it is not 443. The url must be https,
/// with no user name; a literal private, loopback or link-local address is refused here. A name is
/// never looked up here: that waits for the person's yes, and the egress guard checks every
/// address it resolves to.
fn host_of(url: &str) -> std::result::Result<String, Refusal> {
    let u = url::Url::parse(url).map_err(|_| Refusal::Link("the url is not a URL"))?;
    let ip = match u.host() {
        Some(url::Host::Ipv4(ip)) => Some(std::net::IpAddr::V4(ip)),
        Some(url::Host::Ipv6(ip)) => Some(std::net::IpAddr::V6(ip)),
        _ => None,
    };
    if let Some(ip) = ip
        && !net::is_public(ip)
    {
        return Err(Refusal::PrivateHost(ip.to_string()));
    }
    if u.scheme() != "https" || !u.username().is_empty() || u.password().is_some() {
        return Err(Refusal::Link("the url is not an https URL"));
    }
    let host = u
        .host_str()
        .filter(|h| !h.is_empty())
        .ok_or(Refusal::Link("the url has no host"))?;
    Ok(match u.port() {
        Some(port) => format!("{host}:{port}"),
        None => host.to_string(),
    })
}

// ------------------------------------------------------------------ fetching ----

/// A received file, stored as a blob of the profile before it was decrypted: its sha256 (hex) and
/// the content type the manifest gave it (empty for a `U` link: the JWE `cty` says it).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Held {
    pub sha: String,
    pub content_type: String,
}

/// The base of a content type, lower case, without parameters (`;fhirVersion=4.0.1`).
fn base_type(t: &str) -> String {
    t.split(';').next().unwrap_or_default().trim().to_ascii_lowercase()
}

/// The detail code of a network error: what went wrong, never its text.
fn net_refusal(e: NetError) -> Refusal {
    match e {
        NetError::PrivateAddress(host) => Refusal::PrivateHost(host),
        NetError::Resolve(_) => Refusal::Fetch("dns".into()),
        NetError::Timeout(_) => Refusal::Fetch("timeout".into()),
        NetError::TooLarge(_) => Refusal::Fetch("too_large".into()),
        NetError::BadUrl | NetError::NotHttps | NetError::Http { .. } | NetError::Connect(_) | NetError::Write(_) => {
            Refusal::Fetch("connect".into())
        }
    }
}

/// How the link's server answered, by its status alone. Redirects are never followed.
fn status_refusal(status: u16) -> Option<Refusal> {
    match status {
        200 => None,
        300..=399 => Some(Refusal::Redirect),
        404 => Some(Refusal::Inactive),
        429 => Some(Refusal::RateLimited),
        s => Some(Refusal::Fetch(format!("http_{s}"))),
    }
}

/// `url` through `route`, with the per-answer cap of a link.
fn routed(route: &Route, url: &str) -> (String, Egress) {
    let (to, egress) = route(url);
    (
        to,
        Egress {
            max_bytes: MAX_BYTES,
            ..egress
        },
    )
}

/// Store `bytes` as a blob of `profile`. Sealing and syncing a file of up to [`MAX_BYTES`] runs on
/// the blocking pool, off the async workers.
async fn put_blocking(blobs: &BlobStore, profile: Uuid, bytes: Vec<u8>) -> std::result::Result<String, Error> {
    let blobs = blobs.clone();
    tokio::task::spawn_blocking(move || blobs.put_bytes(BlobOwner::Profile(profile), &bytes))
        .await
        .map_err(|e| Error::Other(e.into()))?
        .map_err(|e| Error::Other(e.into()))
}

/// A file of the link as it arrived, not stored yet: its bytes and the content type the manifest
/// gave it (empty for a `U` link). Its Debug shows only the size.
#[derive(Clone, PartialEq, Eq)]
pub struct Fetched {
    bytes: Vec<u8>,
    content_type: String,
}

impl std::fmt::Debug for Fetched {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Fetched")
            .field("bytes", &self.bytes.len())
            .field("content_type", &self.content_type)
            .finish()
    }
}

/// Store each fetched file as a blob of `profile` (bytes first), before anything decrypts it. The
/// route calls this right after it checked again that the person may import for `profile`.
pub async fn keep_all(blobs: &BlobStore, profile: Uuid, files: Vec<Fetched>) -> std::result::Result<Vec<Held>, Error> {
    let mut held = Vec::with_capacity(files.len());
    for f in files {
        let sha = put_blocking(blobs, profile, f.bytes).await?;
        held.push(Held {
            sha,
            content_type: base_type(&f.content_type),
        });
    }
    Ok(held)
}

/// Fetch the link's files for `profile` and store each as a blob before anything decrypts it
/// ([`download`], then [`keep_all`]). Call this only for a link whose host the person approved.
pub async fn fetch(
    link: &Link,
    passcode: Option<&str>,
    blobs: &BlobStore,
    profile: Uuid,
    route: &Route,
) -> std::result::Result<std::result::Result<Vec<Held>, Refusal>, Error> {
    fetch_within(link, passcode, blobs, profile, route, FETCH_LIMIT).await
}

/// [`fetch`] with the limit on the whole fetch given (a test uses a short one).
pub async fn fetch_within(
    link: &Link,
    passcode: Option<&str>,
    blobs: &BlobStore,
    profile: Uuid,
    route: &Route,
    limit: Duration,
) -> std::result::Result<std::result::Result<Vec<Held>, Refusal>, Error> {
    match download_within(link, passcode, route, limit).await {
        Ok(files) => Ok(Ok(keep_all(blobs, profile, files).await?)),
        Err(r) => Ok(Err(r)),
    }
}

/// Fetch the link's files into memory, at most [`MAX_FILES`] of [`MAX_BYTES`] each; nothing is
/// stored. Call this only for a link whose host the person approved. `passcode` is sent in the
/// manifest request body of a `P` link and nowhere else; any other link never sends it. Every
/// request goes through `route`; the whole fetch stops after [`FETCH_LIMIT`].
pub async fn download(
    link: &Link,
    passcode: Option<&str>,
    route: &Route,
) -> std::result::Result<Vec<Fetched>, Refusal> {
    download_within(link, passcode, route, FETCH_LIMIT).await
}

async fn download_within(
    link: &Link,
    passcode: Option<&str>,
    route: &Route,
    limit: Duration,
) -> std::result::Result<Vec<Fetched>, Refusal> {
    let passcode = passcode.filter(|p| !p.is_empty());
    if link.needs_passcode() && passcode.is_none() {
        return Err(Refusal::PasscodeNeeded);
    }
    // "passcode": only a link with the P flag has one; the passcode a client sends with any other
    // link goes nowhere
    let passcode = passcode.filter(|_| link.needs_passcode());
    match tokio::time::timeout(limit, download_all(link, passcode, route)).await {
        Ok(got) => got,
        Err(_) => Err(Refusal::Fetch("timeout".into())),
    }
}

async fn download_all(
    link: &Link,
    passcode: Option<&str>,
    route: &Route,
) -> std::result::Result<Vec<Fetched>, Refusal> {
    if link.direct() {
        // "U": SHALL NOT make a request for the manifest; a GET of url with the recipient
        let Ok(mut url) = url::Url::parse(&link.url) else {
            return Err(Refusal::Link("the url is not a URL"));
        };
        url.query_pairs_mut().append_pair("recipient", RECIPIENT);
        let (to, egress) = routed(route, url.as_str());
        let got = net::get(&to, &egress, &[("accept", "application/jose")])
            .await
            .map_err(net_refusal)?;
        if let Some(r) = status_refusal(got.status) {
            return Err(r);
        }
        return Ok(vec![Fetched {
            bytes: got.body,
            content_type: String::new(),
        }]);
    }
    // "SMART Health Link Manifest Request"
    let mut body = json!({"recipient": RECIPIENT});
    if let Some(p) = passcode {
        body["passcode"] = json!(p);
    }
    let (to, egress) = routed(route, &link.url);
    let got = net::post_json(&to, &egress, &[("accept", "application/json")], &body)
        .await
        .map_err(net_refusal)?;
    if got.status == 401 {
        // "remainingAttempts": the number of attempts left before the link is disabled
        let left = serde_json::from_slice::<Value>(&got.body)
            .ok()
            .and_then(|v| v.get("remainingAttempts").and_then(Value::as_i64));
        return Err(Refusal::Passcode(left));
    }
    if let Some(r) = status_refusal(got.status) {
        return Err(r);
    }
    let Ok(manifest) = serde_json::from_slice::<Value>(&got.body) else {
        return Err(Refusal::Manifest("it is not JSON"));
    };
    if manifest.get("status").and_then(Value::as_str) == Some("no-longer-valid") {
        return Err(Refusal::Inactive);
    }
    let files = match manifest.get("files") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(files)) => files.clone(),
        Some(_) => return Err(Refusal::Manifest("files is not a list")),
    };
    if files.is_empty() {
        return Err(Refusal::NoFiles);
    }
    if files.len() > MAX_FILES {
        return Err(Refusal::TooMany);
    }
    let mut out = Vec::new();
    for f in &files {
        let Some(content_type) = f.get("contentType").and_then(Value::as_str) else {
            return Err(Refusal::Manifest("a file has no contentType"));
        };
        let bytes = if let Some(embedded) = f.get("embedded").and_then(Value::as_str) {
            embedded.as_bytes().to_vec()
        } else if let Some(location) = f.get("location").and_then(Value::as_str) {
            // ".files.location links": short-lived and intended for single use, so each is fetched
            // once; it may be on another public host than the manifest
            let (to, egress) = routed(route, location);
            let got = net::get(&to, &egress, &[("accept", "application/jose")])
                .await
                .map_err(net_refusal)?;
            // a location that is gone expired or was used, while the manifest answered: the link
            // may well be active, and reading it again gives fresh locations
            if matches!(got.status, 404 | 410) {
                return Err(Refusal::LocationExpired);
            }
            if let Some(r) = status_refusal(got.status) {
                return Err(r);
            }
            got.body
        } else {
            return Err(Refusal::Manifest("a file has neither embedded nor location"));
        };
        if bytes.len() > MAX_BYTES {
            return Err(Refusal::Fetch("too_large".into()));
        }
        out.push(Fetched {
            bytes,
            content_type: content_type.to_string(),
        });
    }
    Ok(out)
}

// ------------------------------------------------------------------- opening ----

/// A decrypted file of the link.
#[derive(Debug, Clone, PartialEq)]
pub enum Opened {
    /// A FHIR resource or bundle.
    Fhir { held: Held, plaintext: Vec<u8> },
    /// A `.smart-health-card` file, read by slice 22.
    Cards {
        held: Held,
        plaintext: Vec<u8>,
        input: shc::Input,
    },
    /// Not imported: `reason` is one of [`NOT_IMPORTED`].
    Skipped { held: Held, reason: &'static str },
}

impl Opened {
    /// The stored file, with the type the server read it as: the manifest's contentType, else the
    /// JWE `cty`, else what the content says. An answer that asks about issuers sends these back, so
    /// a file the manifest left untyped comes back with the type it was read as.
    pub fn held(&self) -> &Held {
        match self {
            Opened::Fhir { held, .. } | Opened::Cards { held, .. } | Opened::Skipped { held, .. } => held,
        }
    }
}

/// What a plaintext says it is when nothing else does. Content we do not recognise is still given a
/// type, so a retry that sends the file back skips it instead of refusing the link as held.
fn sniff(plaintext: &[u8]) -> &'static str {
    match serde_json::from_slice::<Value>(plaintext) {
        Ok(v) if v.get("verifiableCredential").is_some() => SMART_HEALTH_CARD,
        Ok(v) if v.get("resourceType").is_some() => FHIR,
        _ => "application/octet-stream",
    }
}

/// Decrypt the held files of `profile` with the link's key and sort them. A held file that is not
/// stored, or does not open with this key, refuses the whole import: a file that opens with the
/// link's key came from the link, whoever sent its sha back.
pub fn open(link: &Link, held: &[Held], blobs: &BlobStore, profile: Uuid) -> std::result::Result<Vec<Opened>, Stop> {
    open_files(link, held, blobs, profile, false)
}

/// [`open`] for files an earlier answer stored and the client sent back (`held`). Their types come
/// from the form, so the server keeps to the type each file has: a `U` link's file is typed again
/// from its JWE `cty` or its content, as the first answer did; a manifest file sent back as FHIR or
/// as a card file must read as one with the reader the first answer used (a card file in any form
/// slice 22 reads: a byte order mark, shc:/ text, a photo), and one sent back with no type is
/// refused. A type we do not import only keeps the file out, as the manifest may say.
pub fn reopen(link: &Link, held: &[Held], blobs: &BlobStore, profile: Uuid) -> std::result::Result<Vec<Opened>, Stop> {
    open_files(link, held, blobs, profile, true)
}

fn open_files(
    link: &Link,
    held: &[Held],
    blobs: &BlobStore,
    profile: Uuid,
    resumed: bool,
) -> std::result::Result<Vec<Opened>, Stop> {
    if held.len() > MAX_FILES {
        return Err(Refusal::TooMany.into());
    }
    let owner = BlobOwner::Profile(profile);
    let mut out = Vec::new();
    for h in held {
        // the size first: a stored file over the cap is never read whole
        match blobs.size(owner, &h.sha) {
            Ok(n) if n <= MAX_BYTES as u64 => {}
            Ok(_) => return Err(Refusal::Fetch("too_large".into()).into()),
            Err(_) => return Err(Refusal::Held.into()),
        }
        let bytes = blobs.get_bytes(owner, &h.sha).map_err(|_| Refusal::Held)?;
        let text = std::str::from_utf8(&bytes).map_err(|_| Refusal::Decrypt)?.trim();
        let opened = jose::decrypt(&link.key, text, MAX_BYTES).map_err(|e| match e {
            jose::JoseError::TooLarge(_) => Refusal::Fetch("too_large".into()),
            _ => Refusal::Decrypt,
        })?;
        // the manifest's contentType; for a U link the JWE cty; else what the content says
        let said = if resumed && link.direct() {
            String::new()
        } else {
            base_type(&h.content_type)
        };
        // a manifest file sent back must read as the type it came back with; the first answer read
        // it with the same reader, so a file that does not was sent back under another type
        let sent_back = resumed && !link.direct();
        if sent_back && said.is_empty() {
            return Err(Refusal::Held.into());
        }
        let cty = opened
            .header
            .get("cty")
            .and_then(Value::as_str)
            .map(base_type)
            .unwrap_or_default();
        let content_type = [said, cty]
            .into_iter()
            .find(|t| !t.is_empty())
            .unwrap_or_else(|| sniff(&opened.plaintext).to_string());
        let held = Held {
            sha: h.sha.clone(),
            content_type: content_type.clone(),
        };
        match content_type.as_str() {
            FHIR => {
                fhir::read_entries(&opened.plaintext, false)
                    .map_err(|_| if sent_back { Refusal::Held } else { Refusal::NotFhir })?;
                out.push(Opened::Fhir {
                    held,
                    plaintext: opened.plaintext,
                });
            }
            SMART_HEALTH_CARD => {
                let input = shc::read(&opened.plaintext).map_err(|refusal| {
                    if sent_back {
                        Stop::Link(Refusal::Held)
                    } else {
                        Stop::Card { refusal, iss: None }
                    }
                })?;
                out.push(Opened::Cards {
                    held,
                    plaintext: opened.plaintext,
                    input,
                });
            }
            API_ACCESS => out.push(Opened::Skipped {
                held,
                reason: "api_access",
            }),
            _ => out.push(Opened::Skipped {
                held,
                reason: "unknown_type",
            }),
        }
    }
    Ok(out)
}

// ------------------------------------------------------------------ checking ----

/// What came of checking the cards of a link, before anything is stored.
#[derive(Debug, Clone, PartialEq)]
pub enum CardsChecked {
    /// The person has not approved these issuer hosts yet. Nothing was fetched from them.
    NeedsApproval(Vec<Ask>),
    Stopped(Stop),
    /// The verified cards of each card file, in the order of the opened files.
    Verified(Vec<Vec<Card>>),
}

/// Ask about every issuer host of the link's cards first, then verify every card of every card file
/// with `shc::verify` (slice 22's rules: one question per host, nothing fetched before the yes). The
/// card files of one link share one issuer key cache, so an issuer's keys and revocation lists are
/// fetched once per link, and the whole check stops after [`VERIFY_LIMIT`].
pub async fn check(opened: &[Opened], approved: &[String], route: &Route, now: DateTime<Utc>) -> CardsChecked {
    check_within(opened, approved, route, now, VERIFY_LIMIT).await
}

/// [`check`], stopped after `limit` for the whole link.
pub async fn check_within(
    opened: &[Opened],
    approved: &[String],
    route: &Route,
    now: DateTime<Utc>,
    limit: Duration,
) -> CardsChecked {
    let mut asks: Vec<Ask> = Vec::new();
    for o in opened {
        let Opened::Cards { input, .. } = o else { continue };
        for jws in &input.cards {
            let i = match shc::issuer(jws) {
                Ok(i) => i,
                Err(refusal) => return CardsChecked::Stopped(Stop::Card { refusal, iss: None }),
            };
            if approved.contains(&i.host) {
                continue;
            }
            match asks.iter_mut().find(|a| a.host == i.host) {
                Some(a) if !a.issuers.contains(&i.iss) => a.issuers.push(i.iss.clone()),
                Some(_) => {}
                None => asks.push(Ask {
                    host: i.host.clone(),
                    issuers: vec![i.iss.clone()],
                }),
            }
        }
    }
    if !asks.is_empty() {
        return CardsChecked::NeedsApproval(asks);
    }
    let mut at: Option<String> = None;
    let verified = tokio::time::timeout(limit, async {
        let mut keys = shc::Keys::default();
        let mut all = Vec::new();
        for o in opened {
            let Opened::Cards { input, .. } = o else { continue };
            // each file's cards stay together, in the order of the opened files
            let mut cards = Vec::with_capacity(input.cards.len());
            for jws in &input.cards {
                let iss = shc::issuer(jws).ok().map(|i| i.iss);
                at.clone_from(&iss);
                match shc::verify(jws, &mut keys, route, now).await {
                    Ok(card) => cards.push(card),
                    Err(refusal) => return CardsChecked::Stopped(Stop::Card { refusal, iss }),
                }
            }
            all.push(cards);
        }
        CardsChecked::Verified(all)
    })
    .await;
    verified.unwrap_or(CardsChecked::Stopped(Stop::Card {
        refusal: shc::Refusal::Keys("timeout".into()),
        iss: at,
    }))
}

// ------------------------------------------------------------------- storing ----

/// One document the import made or found again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Doc {
    pub document_id: Uuid,
    /// False when the same file was received before (its import is not run twice).
    pub created: bool,
    pub content_type: String,
}

/// What one import of a link did.
#[derive(Debug, Clone, PartialEq)]
pub struct Imported {
    pub documents: Vec<Doc>,
    pub cards: Vec<Card>,
    pub summary: ImportSummary,
    /// The weakest identity result of the files: matched, hinted, missing or mismatch.
    pub identity: String,
    /// (content type, reason) of each file that was not imported.
    pub not_imported: Vec<(String, &'static str)>,
}

/// The identity results of `fhir::import_resources`, the weakest first.
const IDENTITY_RANK: [&str; 4] = ["mismatch", "missing", "hinted", "matched"];

fn weaker(before: Option<String>, now: &str) -> Option<String> {
    let rank = |w: &str| IDENTITY_RANK.iter().position(|r| *r == w).unwrap_or(0);
    match before {
        Some(b) if rank(&b) <= rank(now) => Some(b),
        _ => Some(now.to_string()),
    }
}

fn add(sum: &mut ImportSummary, one: &ImportSummary) {
    sum.accepted += one.accepted;
    sum.queued += one.queued;
    sum.skipped += one.skipped;
    for (reason, n) in &one.reasons {
        *sum.reasons.entry(reason.clone()).or_default() += n;
    }
}

/// Store the opened files of a link for `profile`, with the verified cards of each card file in
/// order ([`check`]). Run it in the transaction that checked the person may import for `profile`.
/// `approved` is the person's yes, for slice 22's store.
pub async fn store(
    conn: &mut PgConnection,
    blobs: &BlobStore,
    profile: &Profile,
    link: &Link,
    opened: &[Opened],
    cards: Vec<Vec<Card>>,
    approved: &[String],
) -> Result<Imported> {
    let name = link
        .label
        .clone()
        .unwrap_or_else(|| format!("SMART Health Link · {}", link.host));
    let import_label = format!("SMART Health Link · {}", link.host);
    let mut cards = cards.into_iter();
    let mut out = Imported {
        documents: Vec::new(),
        cards: Vec::new(),
        summary: ImportSummary::new(String::new()),
        identity: String::new(),
        not_imported: Vec::new(),
    };
    let mut identity: Option<String> = None;
    for o in opened {
        let (held, document_id, created, one, who) = match o {
            Opened::Skipped { held, reason } => {
                out.not_imported.push((held.content_type.clone(), reason));
                continue;
            }
            Opened::Fhir { held, plaintext } => {
                let (id, created, one, who) = store_fhir(conn, blobs, profile, &name, &import_label, plaintext).await?;
                (held, id, created, one, who)
            }
            Opened::Cards { held, plaintext, input } => {
                let verified = cards.next().unwrap_or_default();
                let received = shc::Received {
                    bytes: plaintext,
                    filename: Some(&name),
                    input,
                    approved,
                };
                let got = shc::store(&mut *conn, blobs, profile, &received, verified).await?;
                out.cards.extend(got.cards.iter().cloned());
                (held, got.document_id, got.created, got.summary, got.identity)
            }
        };
        // the first link a file came through stays its provenance, as provider_name does
        let shl = json!({"host": link.host, "label": link.label, "content_type": held.content_type,
            "received": held.sha});
        sqlx::query(
            "update document set flags = case when flags ? 'shl' then flags \
             else flags || jsonb_build_object('shl', cast($1 as jsonb)) end, \
             provider_name = coalesce(provider_name, $2) where id = $3",
        )
        .bind(shl)
        .bind(&link.host)
        .bind(document_id)
        .execute(&mut *conn)
        .await?;
        add(&mut out.summary, &one);
        identity = weaker(identity, &who);
        out.documents.push(Doc {
            document_id,
            created,
            content_type: held.content_type.clone(),
        });
    }
    out.summary.document_id = out
        .documents
        .first()
        .map(|d| d.document_id.to_string())
        .unwrap_or_default();
    out.identity = identity.unwrap_or_else(|| "missing".into());
    Ok(out)
}

/// One FHIR file: a document on channel `shl` holding the decrypted file, and its rows through
/// `fhir::import_resources`. The same file received again is the same document, and its import is
/// not run twice. Returns (document, created, summary, identity).
async fn store_fhir(
    conn: &mut PgConnection,
    blobs: &BlobStore,
    profile: &Profile,
    name: &str,
    label: &str,
    plaintext: &[u8],
) -> Result<(Uuid, bool, ImportSummary, String)> {
    let sha = put_blocking(blobs, profile.id, plaintext.to_vec()).await?;
    let doc = create_document(
        &mut *conn,
        &NewDocument {
            filename: Some(name),
            channel: CHANNEL,
            ..NewDocument::upload(
                profile.household_id,
                Some(profile.id),
                &sha,
                FHIR,
                i64::try_from(plaintext.len()).unwrap_or(i64::MAX),
            )
        },
    )
    .await?;
    let (done, identity): (Option<Value>, Option<String>) =
        sqlx::query_as("select flags -> 'import', identity_match from document where id = $1")
            .bind(doc.id)
            .fetch_one(&mut *conn)
            .await?;
    if let Some(done) = done.filter(Value::is_object) {
        return Ok((
            doc.id,
            doc.created,
            fhir::summary_from(&done),
            identity.unwrap_or_else(|| "missing".into()),
        ));
    }
    let (bundle_id, entries) = fhir::read_entries(plaintext, false)?;
    let key = bundle_id.unwrap_or_else(|| format!("shl-{}", &hex::encode(Sha256::digest(plaintext))[..16]));
    let (summary, who) = fhir::import_resources(&mut *conn, &entries, profile, doc.id, &key, None, label).await?;
    fhir::mark_imported(&mut *conn, doc.id, &summary, Some(&who)).await?;
    Ok((doc.id, doc.created, summary, who))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> DateTime<Utc> {
        DateTime::from_timestamp(1_795_262_400, 0).unwrap_or_default()
    }

    fn payload(p: &Value) -> String {
        format!("shlink:/{}", URL_SAFE_NO_PAD.encode(p.to_string()))
    }

    const KEY: &str = "rxTgYlOaKJPFtcEd0qcceN8wEU4p94SqAwIWQe6uX7Q";

    /// What our own sender writes (ownhealth-ops shl::shlink), read back with and without a viewer.
    #[test]
    fn a_link_reads_alone_and_after_a_viewer_url() -> std::result::Result<(), Refusal> {
        let text = payload(&json!({"url": "https://share.rivera.test/api/shl/abc", "key": KEY,
            "exp": now().timestamp() + 86_400, "flag": "P", "label": "Para la consulta"}));
        let link = parse(&text, now())?;
        assert_eq!(link.url, "https://share.rivera.test/api/shl/abc");
        assert_eq!(link.host, "share.rivera.test");
        assert!(link.needs_passcode() && !link.direct());
        assert_eq!(link.label.as_deref(), Some("Para la consulta"));
        let viewed = parse(&format!("https://viewer.example.org/#{text}\n"), now())?;
        assert_eq!(viewed, link);
        assert!(!format!("{link:?}").contains(KEY), "the key is in the Debug output");
        Ok(())
    }

    #[test]
    fn a_link_that_breaks_the_payload_rules_is_refused() {
        let at = |p: Value| parse(&payload(&p), now()).map(|_| ()).map_err(|r| r.code());
        let url = "https://share.rivera.test/api/shl/abc";
        assert_eq!(at(json!({"url": url, "key": "short"})), Err("link"));
        assert_eq!(at(json!({"key": KEY})), Err("link"));
        assert_eq!(
            at(json!({"url": "http://share.rivera.test/x", "key": KEY})),
            Err("link")
        );
        assert_eq!(at(json!({"url": url, "key": KEY, "flag": "PU"})), Err("link"));
        assert_eq!(at(json!({"url": url, "key": KEY, "v": 2})), Err("link"));
        assert_eq!(
            at(json!({"url": url, "key": KEY, "exp": now().timestamp() - 1})),
            Err("expired")
        );
        assert_eq!(
            at(json!({"url": "https://192.168.1.20/shl", "key": KEY})),
            Err("private_host")
        );
        assert_eq!(at(json!({"url": url, "key": KEY, "flag": "LU", "v": 1})), Ok(()));
        assert_eq!(
            parse("shlink:/%%%", now()).map_err(|r| r.code()).map(|_| ()),
            Err("link")
        );
    }

    /// "exp": a number of seconds, which may have a fraction; a link that has not expired yet by
    /// that fraction is read.
    #[test]
    fn a_fractional_exp_is_compared_with_its_fraction() {
        let url = "https://share.rivera.test/api/shl/abc";
        let exp = now().timestamp() as f64 + 0.75;
        let at = now() + chrono::Duration::milliseconds(250);
        let text = payload(&json!({"url": url, "key": KEY, "exp": exp}));
        let link = parse(&text, at).map_err(|r| r.code());
        assert_eq!(
            link.as_ref().map(|l| l.exp.map(|e| e.timestamp_millis())),
            Ok(Some(now().timestamp_millis() + 750))
        );
        let late = now() + chrono::Duration::milliseconds(800);
        assert_eq!(parse(&text, late).map(|_| ()).map_err(|r| r.code()), Err("expired"));
    }

    /// A pasted card envelope (JSON) stays a card even when one of its strings holds `shlink:/`.
    #[test]
    fn a_json_card_envelope_is_not_a_link() {
        let link = payload(&json!({"url": "https://share.rivera.test/m", "key": KEY}));
        assert!(is_link(&link));
        assert!(is_link(&format!("https://viewer.example.org/#{link}")));
        assert!(is_link(&format!("  {link}\n")));
        let envelope = json!({"verifiableCredential": ["a.b.c"], "note": link}).to_string();
        assert!(!is_link(&envelope), "{envelope}");
        assert!(!is_link(&format!("\n {envelope}")));
    }

    #[test]
    fn a_file_arrives_on_a_channel_the_database_allows() {
        assert!(ownhealth_core::documents::CHANNELS.contains(&CHANNEL));
    }

    #[test]
    fn each_status_has_its_reason_and_redirects_are_refused() {
        assert_eq!(status_refusal(200), None);
        for s in [301, 302, 303, 307, 308] {
            assert_eq!(status_refusal(s), Some(Refusal::Redirect), "{s}");
        }
        assert_eq!(status_refusal(404), Some(Refusal::Inactive));
        assert_eq!(status_refusal(429), Some(Refusal::RateLimited));
        assert_eq!(status_refusal(500), Some(Refusal::Fetch("http_500".into())));
    }

    /// The quoted values of `export const <name> = [ ... ]` in a TypeScript file.
    fn ts_list(ts: &str, name: &str) -> Vec<String> {
        let start = ts
            .find(&format!("export const {name} = ["))
            .unwrap_or_else(|| panic!("no {name}"));
        let body = &ts[start..];
        let body = &body[..body.find(']').unwrap_or(body.len())];
        body.split('\'').skip(1).step_by(2).map(str::to_string).collect()
    }

    /// The UI words every refusal and every not-imported reason the server sends.
    #[test]
    fn the_ui_lists_every_link_refusal_and_not_imported_reason() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../../ui/src/lib/logic/shc.ts");
        let ts = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("{path}: {e}"));
        assert_eq!(ts_list(&ts, "SHL_REASONS"), REASONS.to_vec());
        assert_eq!(ts_list(&ts, "SHL_NOT_IMPORTED"), NOT_IMPORTED.to_vec());
        let codes: Vec<&str> = samples().iter().map(Refusal::code).collect();
        assert_eq!(codes, REASONS.to_vec(), "one sample per refusal, in REASONS order");
    }
}
