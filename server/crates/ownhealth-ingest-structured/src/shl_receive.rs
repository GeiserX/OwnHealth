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
//! 3. [`fetch`] POSTs the manifest request through `net::post_json` (a `U` link: one GET of the file,
//!    with `recipient` in the query) under the connector egress guard: https, public addresses
//!    only, no redirects, [`MAX_BYTES`] per answer. Each file, embedded or at its location (fetched
//!    once), is stored as a blob of the profile BEFORE it is decrypted (bytes first), and comes back
//!    as a [`Held`]: its sha256 and its content type.
//!    A file stays stored when a later step refuses the import (it is the profile's, under its key,
//!    and goes with the profile): sent back as `held`, it opens again without a second request.
//! 4. [`open`] decrypts the held files with the link's key (`jose::decrypt`) and sorts them:
//!    `application/fhir+json` is read as FHIR, `application/smart-health-card` goes to slice 22
//!    (`shc::read`), and `application/smart-api-access` (or a type we do not know) is counted as not
//!    imported, with its reason.
//! 5. [`check`] asks about the issuer host of every card before any key is fetched (slice 22's
//!    rule); the route then sends the held files back with the person's yes, so the link is not
//!    asked twice. Then it verifies the cards with `shc::check`.
//! 6. [`store`] keeps everything for one profile: each FHIR file is one document on channel `shl`
//!    whose rows go through `fhir::import_resources`; each card file goes to `shc::store`. Each
//!    document's `flags.shl` names the link's host, label and the stored received file.
//!
//! The passcode goes to the link's server in the request body and nowhere else: never into a URL,
//! a log line, an error, a row or a blob. The key never leaves this request either.

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
use crate::shc::{self, Ask, Card, Checked, Route};

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
/// "files array": the three content types a manifest file may have.
pub const FHIR: &str = "application/fhir+json";
pub const SMART_HEALTH_CARD: &str = "application/smart-health-card";
pub const API_ACCESS: &str = "application/smart-api-access";
/// Every reason code, as the UI lists them (ui/src/lib/logic/shc.ts SHL_REASONS).
pub const REASONS: [&str; 15] = [
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
    #[error("a file received before is no longer stored")]
    Held,
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

/// True when the text holds a link (`shlink:/`, alone or after a viewer URL).
pub fn is_link(text: &str) -> bool {
    text.contains("shlink:/")
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
        Some(v) => Some(
            v.as_f64()
                .and_then(|s| DateTime::from_timestamp(s as i64, 0))
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
        NetError::BadUrl | NetError::NotHttps | NetError::Http { .. } | NetError::Write(_) => {
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

/// Store one received file as a blob of `profile` (bytes first).
fn keep(blobs: &BlobStore, profile: Uuid, bytes: &[u8], content_type: &str) -> std::result::Result<Held, Error> {
    let sha = blobs
        .put_bytes(BlobOwner::Profile(profile), bytes)
        .map_err(|e| Error::Other(e.into()))?;
    Ok(Held {
        sha,
        content_type: base_type(content_type),
    })
}

/// Fetch the link's files for `profile` and store each as a blob before anything decrypts it. Call
/// this only for a link whose host the person approved. `passcode` is sent in the manifest request
/// body and nowhere else. Every request goes through `route`; the whole fetch stops after
/// [`FETCH_LIMIT`].
pub async fn fetch(
    link: &Link,
    passcode: Option<&str>,
    blobs: &BlobStore,
    profile: Uuid,
    route: &Route,
) -> std::result::Result<std::result::Result<Vec<Held>, Refusal>, Error> {
    let passcode = passcode.filter(|p| !p.is_empty());
    if link.needs_passcode() && passcode.is_none() {
        return Ok(Err(Refusal::PasscodeNeeded));
    }
    match tokio::time::timeout(FETCH_LIMIT, fetch_all(link, passcode, blobs, profile, route)).await {
        Ok(got) => got,
        Err(_) => Ok(Err(Refusal::Fetch("timeout".into()))),
    }
}

async fn fetch_all(
    link: &Link,
    passcode: Option<&str>,
    blobs: &BlobStore,
    profile: Uuid,
    route: &Route,
) -> std::result::Result<std::result::Result<Vec<Held>, Refusal>, Error> {
    if link.direct() {
        // "U": SHALL NOT make a request for the manifest; a GET of url with the recipient
        let Ok(mut url) = url::Url::parse(&link.url) else {
            return Ok(Err(Refusal::Link("the url is not a URL")));
        };
        url.query_pairs_mut().append_pair("recipient", RECIPIENT);
        let (to, egress) = routed(route, url.as_str());
        let got = match net::get(&to, &egress, &[("accept", "application/jose")]).await {
            Ok(got) => got,
            Err(e) => return Ok(Err(net_refusal(e))),
        };
        if let Some(r) = status_refusal(got.status) {
            return Ok(Err(r));
        }
        return Ok(Ok(vec![keep(blobs, profile, &got.body, "")?]));
    }
    // "SMART Health Link Manifest Request"
    let mut body = json!({"recipient": RECIPIENT});
    if let Some(p) = passcode {
        body["passcode"] = json!(p);
    }
    let (to, egress) = routed(route, &link.url);
    let got = match net::post_json(&to, &egress, &[("accept", "application/json")], &body).await {
        Ok(got) => got,
        Err(e) => return Ok(Err(net_refusal(e))),
    };
    if got.status == 401 {
        // "remainingAttempts": the number of attempts left before the link is disabled
        let left = serde_json::from_slice::<Value>(&got.body)
            .ok()
            .and_then(|v| v.get("remainingAttempts").and_then(Value::as_i64));
        return Ok(Err(Refusal::Passcode(left)));
    }
    if let Some(r) = status_refusal(got.status) {
        return Ok(Err(r));
    }
    let Ok(manifest) = serde_json::from_slice::<Value>(&got.body) else {
        return Ok(Err(Refusal::Manifest("it is not JSON")));
    };
    if manifest.get("status").and_then(Value::as_str) == Some("no-longer-valid") {
        return Ok(Err(Refusal::Inactive));
    }
    let files = match manifest.get("files") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(files)) => files.clone(),
        Some(_) => return Ok(Err(Refusal::Manifest("files is not a list"))),
    };
    if files.is_empty() {
        return Ok(Err(Refusal::NoFiles));
    }
    if files.len() > MAX_FILES {
        return Ok(Err(Refusal::TooMany));
    }
    let mut held = Vec::new();
    for f in &files {
        let Some(content_type) = f.get("contentType").and_then(Value::as_str) else {
            return Ok(Err(Refusal::Manifest("a file has no contentType")));
        };
        let bytes = if let Some(embedded) = f.get("embedded").and_then(Value::as_str) {
            embedded.as_bytes().to_vec()
        } else if let Some(location) = f.get("location").and_then(Value::as_str) {
            // ".files.location links": short-lived and intended for single use, so each is fetched
            // once; it may be on another public host than the manifest
            let (to, egress) = routed(route, location);
            let got = match net::get(&to, &egress, &[("accept", "application/jose")]).await {
                Ok(got) => got,
                Err(e) => return Ok(Err(net_refusal(e))),
            };
            if let Some(r) = status_refusal(got.status) {
                return Ok(Err(r));
            }
            got.body
        } else {
            return Ok(Err(Refusal::Manifest("a file has neither embedded nor location")));
        };
        if bytes.len() > MAX_BYTES {
            return Ok(Err(Refusal::Fetch("too_large".into())));
        }
        held.push(keep(blobs, profile, &bytes, content_type)?);
    }
    Ok(Ok(held))
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
    Skipped { content_type: String, reason: &'static str },
}

/// What a plaintext says it is when nothing else does.
fn sniff(plaintext: &[u8]) -> &'static str {
    match serde_json::from_slice::<Value>(plaintext) {
        Ok(v) if v.get("verifiableCredential").is_some() => SMART_HEALTH_CARD,
        Ok(v) if v.get("resourceType").is_some() => FHIR,
        _ => "",
    }
}

/// Decrypt the held files of `profile` with the link's key and sort them. A held file that is not
/// stored, or does not open with this key, refuses the whole import: a file that opens with the
/// link's key came from the link, whoever sent its sha back.
pub fn open(link: &Link, held: &[Held], blobs: &BlobStore, profile: Uuid) -> std::result::Result<Vec<Opened>, Stop> {
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
        let cty = opened
            .header
            .get("cty")
            .and_then(Value::as_str)
            .map(base_type)
            .unwrap_or_default();
        let content_type = [h.content_type.clone(), cty]
            .into_iter()
            .find(|t| !t.is_empty())
            .unwrap_or_else(|| sniff(&opened.plaintext).to_string());
        let held = Held {
            sha: h.sha.clone(),
            content_type: content_type.clone(),
        };
        match content_type.as_str() {
            FHIR => {
                fhir::read_entries(&opened.plaintext, false).map_err(|_| Refusal::NotFhir)?;
                out.push(Opened::Fhir {
                    held,
                    plaintext: opened.plaintext,
                });
            }
            SMART_HEALTH_CARD => {
                let input = shc::read(&opened.plaintext).map_err(|refusal| Stop::Card { refusal, iss: None })?;
                out.push(Opened::Cards {
                    held,
                    plaintext: opened.plaintext,
                    input,
                });
            }
            API_ACCESS => out.push(Opened::Skipped {
                content_type,
                reason: "api_access",
            }),
            _ => out.push(Opened::Skipped {
                content_type,
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

/// Ask about every issuer host of the link's cards first, then verify each card file with
/// `shc::check` (slice 22's rules: one question per host, nothing fetched before the yes).
pub async fn check(opened: &[Opened], approved: &[String], route: &Route, now: DateTime<Utc>) -> CardsChecked {
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
    let mut all = Vec::new();
    for o in opened {
        let Opened::Cards { input, .. } = o else { continue };
        match shc::check(input, approved, route, now).await {
            Checked::Verified(cards) => all.push(cards),
            Checked::Refused { refusal, iss } => return CardsChecked::Stopped(Stop::Card { refusal, iss }),
            Checked::NeedsApproval(asks) => return CardsChecked::NeedsApproval(asks),
        }
    }
    CardsChecked::Verified(all)
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
            Opened::Skipped { content_type, reason } => {
                out.not_imported.push((content_type.clone(), reason));
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
        let shl = json!({"host": link.host, "label": link.label, "content_type": held.content_type,
            "received": held.sha});
        sqlx::query(
            "update document set flags = flags || jsonb_build_object('shl', cast($1 as jsonb)), \
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
    let sha = blobs
        .put_bytes(BlobOwner::Profile(profile.id), plaintext)
        .map_err(|e| Error::Other(e.into()))?;
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
