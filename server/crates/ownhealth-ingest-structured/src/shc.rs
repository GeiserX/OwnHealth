//! SMART Health Cards, received (slice 22): a vaccination or lab card that a person pastes as
//! `shc:/` text, uploads as a `.smart-health-card` file, or photographs as a QR code.
//!
//! Spec: SMART Health Cards Framework 1.4.0, <https://spec.smarthealth.cards/>. Each rule below
//! names the heading of that page it comes from.
//!
//! 1. [`read`] turns the input into compact JWSs. It uses no network.
//! 2. [`issuer`] reads who a card says signed it (the payload `iss`, not verified yet) and the host
//!    that its keys would come from.
//! 3. [`verify`] fetches `<iss>/.well-known/jwks.json` through `net::get` under the connector
//!    egress guard (https, public addresses only, no redirects, 1 MiB, 15 s), verifies the ES256
//!    signature with `jose.rs`, and checks the kid, nbf, exp, revocation and credential type.
//! 4. [`receive`] runs them for one person. Nothing is fetched until the person approved every
//!    issuer host. Nothing is stored until every card verified. Then the input bytes become one
//!    document on channel `shc`, and each card's FHIR bundle goes to `fhir::import_resources`.
//!
//! A verified card proves which issuer key signed it. It never proves that the issuer deserves
//! trust, so the UI says "signed by <iss>" and nothing more.

use std::collections::{BTreeMap, HashMap};
use std::io::Cursor;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use chrono::{DateTime, Utc};
use ownhealth_core::access::Profile;
use ownhealth_core::blobs::BlobStore;
use ownhealth_core::documents::{NewDocument, create_document};
use ownhealth_core::ingest::ImportSummary;
use ownhealth_core::jose::{self, JoseError};
use ownhealth_core::net::{self, Egress, NetError};
use ownhealth_core::{Error, Result};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::PgConnection;
use uuid::Uuid;

use crate::fhir;

/// `document_source.channel` of a received card.
pub const CHANNEL: &str = "shc";
/// "via File Download": a `.smart-health-card` file is served as `application/smart-health-card`.
pub const MIME: &str = "application/smart-health-card";
/// The inflated payload of one card. A QR card holds at most 1195 JWS characters ("Health Cards as
/// QR Codes"); the spec sets no size for a file card, so we cap it at 1 MiB.
pub const MAX_PAYLOAD: usize = 1024 * 1024;
/// At most this many cards in one input.
pub const MAX_CARDS: usize = 20;
/// "Health Cards are encoded as Compact Serialization JSON Web Signatures (JWS)": this type SHALL
/// be present in `vc.type`.
const HEALTH_CARD: &str = "https://smarthealth.cards#health-card";
/// RFC 7519 4.1.5 lets a verifier allow "some small leeway" on `nbf` for clock skew.
const NBF_LEEWAY_SECONDS: i64 = 300;
/// A photo larger than this on its long side is scaled down before the QR search.
const MAX_PHOTO_SIDE: u32 = 2000;

/// Why a card was not imported. Each message names what was wrong, never a key or a payload.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Refusal {
    /// "text", "file" or "photo".
    #[error("no SMART Health Card was found in this {0}")]
    NoCard(&'static str),
    #[error("the shc:/ code is not a valid number string")]
    Numeric,
    #[error("part {missing} of the {total} QR codes of a split card is missing")]
    ChunkMissing { missing: usize, total: usize },
    #[error("more than {MAX_CARDS} cards came in one import")]
    TooMany,
    #[error("the card is not a compact JWS")]
    NotJws,
    #[error("the card's header does not meet the SMART Health Cards rules: {0}")]
    Header(&'static str),
    #[error("the card's content does not meet the SMART Health Cards rules: {0}")]
    Payload(&'static str),
    #[error("the card's issuer is not an https URL without a trailing slash")]
    Issuer,
    #[error("{0} has a private, loopback or link-local address, so its keys were not fetched")]
    PrivateIssuer(String),
    #[error("the issuer's keys could not be fetched: {0}")]
    Keys(String),
    #[error("the issuer publishes no key with this card's kid")]
    UnknownKid,
    #[error("the issuer's key does not meet the SMART Health Cards rules: {0}")]
    Key(&'static str),
    #[error("the signature does not verify with the issuer's published key")]
    BadSignature,
    #[error("the card expired on {0}")]
    Expired(String),
    #[error("the card is valid only from {0}")]
    NotYet(String),
    #[error("the issuer lists this card as revoked")]
    Revoked,
}

impl Refusal {
    /// A stable code for the UI, which words each one in es and en.
    pub fn code(&self) -> &'static str {
        match self {
            Refusal::NoCard(_) => "no_card",
            Refusal::Numeric => "numeric",
            Refusal::ChunkMissing { .. } => "chunk_missing",
            Refusal::TooMany => "too_many",
            Refusal::NotJws => "not_jws",
            Refusal::Header(_) => "header",
            Refusal::Payload(_) => "payload",
            Refusal::Issuer => "issuer",
            Refusal::PrivateIssuer(_) => "private_issuer",
            Refusal::Keys(_) => "keys",
            Refusal::UnknownKid => "unknown_kid",
            Refusal::Key(_) => "key",
            Refusal::BadSignature => "bad_signature",
            Refusal::Expired(_) => "expired",
            Refusal::NotYet(_) => "not_yet",
            Refusal::Revoked => "revoked",
        }
    }

    /// The one value the UI puts in its sentence: a date, a host, `2/3` for a missing chunk, or why
    /// the keys could not be fetched.
    pub fn detail(&self) -> Option<String> {
        match self {
            Refusal::Expired(d) | Refusal::NotYet(d) | Refusal::PrivateIssuer(d) | Refusal::Keys(d) => Some(d.clone()),
            Refusal::ChunkMissing { missing, total } => Some(format!("{missing}/{total}")),
            _ => None,
        }
    }
}

/// Every text this module writes for a person, for the wording lint.
pub fn phrases() -> Value {
    let all = [
        Refusal::NoCard("photo"),
        Refusal::Numeric,
        Refusal::ChunkMissing { missing: 2, total: 3 },
        Refusal::TooMany,
        Refusal::NotJws,
        Refusal::Header("zip is not DEF"),
        Refusal::Payload("nbf"),
        Refusal::Issuer,
        Refusal::PrivateIssuer("127.0.0.1".into()),
        Refusal::Keys("HTTP 404".into()),
        Refusal::UnknownKid,
        Refusal::Key("kid is not the key's thumbprint"),
        Refusal::BadSignature,
        Refusal::Expired("2026-01-01".into()),
        Refusal::NotYet("2026-01-01".into()),
        Refusal::Revoked,
    ];
    json!({ "en": all.iter().map(ToString::to_string).collect::<Vec<_>>() })
}

// ------------------------------------------------------------------ reading ----

/// What the person gave: shc:/ text, a `.smart-health-card` file, or a photo (its mime type).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Form {
    Text,
    File,
    Photo(&'static str),
}

/// The input, read into compact JWSs. Nothing in it is verified yet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Input {
    pub form: Form,
    pub cards: Vec<String>,
}

/// Read pasted text, a file or a photo into its cards. CPU work (a photo's QR search), no network.
pub fn read(bytes: &[u8]) -> std::result::Result<Input, Refusal> {
    if let Some(mime) = photo_mime(bytes) {
        let texts = qr_texts(bytes)?;
        return Ok(Input {
            form: Form::Photo(mime),
            cards: from_shc_texts(&texts, "photo")?,
        });
    }
    let text = std::str::from_utf8(bytes).map_err(|_| Refusal::NoCard("file"))?;
    let text = text.trim_start_matches('\u{feff}').trim();
    if text.starts_with('{') {
        return Ok(Input {
            form: Form::File,
            cards: from_file(text)?,
        });
    }
    let words: Vec<String> = text
        .split_whitespace()
        .filter(|w| w.starts_with("shc:/"))
        .map(str::to_string)
        .collect();
    Ok(Input {
        form: Form::Text,
        cards: from_shc_texts(&words, "text")?,
    })
}

fn photo_mime(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        Some("image/jpeg")
    } else {
        None
    }
}

/// "via File Download": a JSON object whose `verifiableCredential` array holds the JWS strings.
fn from_file(text: &str) -> std::result::Result<Vec<String>, Refusal> {
    let doc: Value = serde_json::from_str(text).map_err(|_| Refusal::NoCard("file"))?;
    let cards: Vec<String> = doc
        .get("verifiableCredential")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect())
        .unwrap_or_default();
    if cards.is_empty() {
        return Err(Refusal::NoCard("file"));
    }
    if cards.len() > MAX_CARDS {
        return Err(Refusal::TooMany);
    }
    Ok(cards)
}

/// Every QR code in the photo whose text starts with `shc:/`.
fn qr_texts(bytes: &[u8]) -> std::result::Result<Vec<String>, Refusal> {
    let no_card = || Refusal::NoCard("photo");
    let mut reader = image::ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|_| no_card())?;
    let mut limits = image::Limits::default();
    limits.max_alloc = Some(256 * 1024 * 1024);
    reader.limits(limits);
    let mut img = reader.decode().map_err(|_| no_card())?.to_luma8();
    if img.width().max(img.height()) > MAX_PHOTO_SIDE {
        let scale = f64::from(MAX_PHOTO_SIDE) / f64::from(img.width().max(img.height()));
        let (w, h) = (
            (f64::from(img.width()) * scale).round() as u32,
            (f64::from(img.height()) * scale).round() as u32,
        );
        img = image::imageops::thumbnail(&img, w.max(1), h.max(1));
    }
    let mut prepared = rqrr::PreparedImage::prepare(img);
    let texts: Vec<String> = prepared
        .detect_grids()
        .iter()
        .filter_map(|g| g.decode().ok().map(|(_, text)| text))
        .filter(|t| t.starts_with("shc:/"))
        .collect();
    if texts.is_empty() {
        return Err(no_card());
    }
    Ok(texts)
}

/// "Encoding QRs": each `shc:/` text is one card, or one chunk `shc:/<C>/<N>/` of a card split
/// over N QR codes ("Chunking Larger SHCs (deprecated)"; still read, and in any order, as the spec
/// asks of consumers).
fn from_shc_texts(texts: &[String], what: &'static str) -> std::result::Result<Vec<String>, Refusal> {
    let mut cards = Vec::new();
    let mut chunks: BTreeMap<usize, String> = BTreeMap::new();
    let mut total = None;
    for text in texts {
        let body = text.strip_prefix("shc:/").ok_or(Refusal::Numeric)?;
        let parts: Vec<&str> = body.split('/').collect();
        match parts.as_slice() {
            [digits] => cards.push(numeric(digits)?),
            [c, n, digits] => {
                let c: usize = c.parse().map_err(|_| Refusal::Numeric)?;
                let n: usize = n.parse().map_err(|_| Refusal::Numeric)?;
                if c == 0 || c > n || n > MAX_CARDS || total.is_some_and(|t| t != n) {
                    return Err(Refusal::Numeric);
                }
                total = Some(n);
                let decoded = numeric(digits)?;
                if chunks
                    .insert(c, decoded.clone())
                    .is_some_and(|before| before != decoded)
                {
                    return Err(Refusal::Numeric);
                }
            }
            _ => return Err(Refusal::Numeric),
        }
    }
    if let Some(n) = total {
        if let Some(missing) = (1..=n).find(|c| !chunks.contains_key(c)) {
            return Err(Refusal::ChunkMissing { missing, total: n });
        }
        cards.push(chunks.into_values().collect());
    }
    if cards.is_empty() {
        return Err(Refusal::NoCard(what));
    }
    if cards.len() > MAX_CARDS {
        return Err(Refusal::TooMany);
    }
    Ok(cards)
}

/// "Encoding QRs": each JWS character is two digits, `ord(c) - 45`. The JWS alphabet (`-` to `z`)
/// gives 00 to 77.
pub fn numeric(digits: &str) -> std::result::Result<String, Refusal> {
    let bytes = digits.as_bytes();
    if bytes.is_empty() || !bytes.len().is_multiple_of(2) || !bytes.iter().all(u8::is_ascii_digit) {
        return Err(Refusal::Numeric);
    }
    bytes
        .chunks(2)
        .map(|pair| {
            let n = (pair[0] - b'0') * 10 + (pair[1] - b'0');
            if n > b'z' - b'-' {
                Err(Refusal::Numeric)
            } else {
                Ok(char::from(n + b'-'))
            }
        })
        .collect()
}

// --------------------------------------------------------------- the issuer ----

/// Who a card says signed it, before any check: the payload `iss` and its host (with the port when
/// it is not 443). The person approves this host before anything is fetched from it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Issuer {
    pub iss: String,
    pub host: String,
}

/// The claims of a card that a person sees.
#[derive(Debug, Clone, PartialEq)]
struct Claims {
    iss: String,
    nbf: DateTime<Utc>,
    exp: Option<DateTime<Utc>>,
    types: Vec<String>,
    rid: Option<String>,
    bundle: Value,
}

/// The header checks and the unverified issuer of one card. Nothing it returns is trusted; it only
/// picks the issuer to ask the person about and the key to verify with.
pub fn issuer(jws: &str) -> std::result::Result<Issuer, Refusal> {
    let (header, payload) = jose::jws_unverified(jws, MAX_PAYLOAD).map_err(jose_refusal)?;
    kid_of(&header)?;
    Ok(claims(&payload)?.issuer)
}

/// "Health Cards are Compact": the header has `alg: ES256`, `zip: DEF` and a `kid`.
fn kid_of(header: &jose::Header) -> std::result::Result<String, Refusal> {
    if header.get("alg").and_then(Value::as_str) != Some("ES256") {
        return Err(Refusal::Header("alg is not ES256"));
    }
    if header.get("zip").and_then(Value::as_str) != Some("DEF") {
        return Err(Refusal::Header("zip is not DEF"));
    }
    header
        .get("kid")
        .and_then(Value::as_str)
        .filter(|k| !k.is_empty())
        .map(str::to_string)
        .ok_or(Refusal::Header("there is no kid"))
}

struct Read {
    issuer: Issuer,
    claims: Claims,
}

/// The payload ("Health Cards are encoded as Compact Serialization JSON Web Signatures (JWS)"):
/// `iss`, `nbf` (seconds), an optional `exp` ("Expiration of Health Cards"), `vc.type` with the
/// health-card type, and `vc.credentialSubject.fhirBundle`. `vc.rid` is the revocation id
/// ("Revocation").
fn claims(payload: &[u8]) -> std::result::Result<Read, Refusal> {
    let p: Value = serde_json::from_slice(payload).map_err(|_| Refusal::Payload("it is not JSON"))?;
    let iss = p
        .get("iss")
        .and_then(Value::as_str)
        .ok_or(Refusal::Payload("there is no iss"))?;
    let issuer = issuer_of(iss)?;
    let seconds = |v: Option<&Value>| {
        v.and_then(Value::as_f64)
            .and_then(|s| DateTime::from_timestamp(s as i64, 0))
    };
    let nbf = seconds(p.get("nbf")).ok_or(Refusal::Payload("there is no nbf date"))?;
    let exp = match p.get("exp") {
        None => None,
        some => Some(seconds(some).ok_or(Refusal::Payload("exp is not a date"))?),
    };
    let vc = p.get("vc").ok_or(Refusal::Payload("there is no vc"))?;
    let types: Vec<String> = vc
        .get("type")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect())
        .unwrap_or_default();
    if !types.iter().any(|t| t == HEALTH_CARD) {
        return Err(Refusal::Payload("vc.type has no health-card type"));
    }
    let bundle = vc
        .pointer("/credentialSubject/fhirBundle")
        .filter(|b| b.get("resourceType").and_then(Value::as_str) == Some("Bundle"))
        .cloned()
        .ok_or(Refusal::Payload("there is no FHIR Bundle"))?;
    let rid = vc.get("rid").and_then(Value::as_str).map(str::to_string);
    Ok(Read {
        claims: Claims {
            iss: issuer.iss.clone(),
            nbf,
            exp,
            types,
            rid,
            bundle,
        },
        issuer,
    })
}

/// "Determining keys associated with an issuer": `iss` SHALL use https and SHALL NOT end in `/`.
fn issuer_of(iss: &str) -> std::result::Result<Issuer, Refusal> {
    let url = url::Url::parse(iss).map_err(|_| Refusal::Issuer)?;
    let host = url.host_str().filter(|h| !h.is_empty()).ok_or(Refusal::Issuer)?;
    if url.scheme() != "https"
        || iss.ends_with('/')
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(Refusal::Issuer);
    }
    let host = match url.port() {
        Some(port) => format!("{host}:{port}"),
        None => host.to_string(),
    };
    Ok(Issuer {
        iss: iss.to_string(),
        host,
    })
}

fn jose_refusal(e: JoseError) -> Refusal {
    match e {
        JoseError::Unsupported("zip") => Refusal::Header("zip is not DEF"),
        JoseError::Unsupported(_) => Refusal::Header("it names a field we do not read"),
        JoseError::TooLarge(_) => Refusal::Payload("it is larger than 1 MiB"),
        JoseError::BadSignature => Refusal::BadSignature,
        JoseError::Malformed(_) | JoseError::Decrypt | JoseError::Key(_) => Refusal::NotJws,
    }
}

// ------------------------------------------------------------- verification ----

/// Where an issuer URL (its JWKS or a revocation list) is fetched from, and under which egress.
pub type Route = dyn Fn(&str) -> (String, Egress) + Send + Sync;

/// The issuer's own URL under the connector egress guard: https only, public addresses only, no
/// redirects, 1 MiB, 15 s. A JWKS on a private, loopback or link-local address is refused there.
pub fn public(url: &str) -> (String, Egress) {
    (url.to_string(), Egress::connector())
}

/// A card that verified: who signed it and what it holds.
#[derive(Debug, Clone, PartialEq)]
pub struct Card {
    pub iss: String,
    pub kid: String,
    pub issued: DateTime<Utc>,
    pub expires: Option<DateTime<Utc>>,
    pub types: Vec<String>,
    pub bundle: Value,
}

/// Each issuer's JWKS, fetched once per import.
#[derive(Default)]
pub struct Keys {
    jwks: HashMap<String, Value>,
}

async fn fetch_json(url: &str, route: &Route) -> std::result::Result<Value, Refusal> {
    let (from, egress) = route(url);
    let got = net::get(&from, &egress, &[("accept", "application/json")])
        .await
        .map_err(|e| match e {
            NetError::PrivateAddress(host) => Refusal::PrivateIssuer(host),
            other => Refusal::Keys(other.to_string()),
        })?;
    if got.status != 200 {
        return Err(Refusal::Keys(format!("HTTP {}", got.status)));
    }
    serde_json::from_slice(&got.body).map_err(|_| Refusal::Keys("the answer is not JSON".into()))
}

/// The RFC 7638 thumbprint of an EC public key: SHA-256 of `{"crv","kty","x","y"}` in that order,
/// base64url.
pub fn thumbprint(jwk: &Value) -> Option<String> {
    let field = |k: &str| jwk.get(k).and_then(Value::as_str);
    let canonical = format!(
        r#"{{"crv":{},"kty":{},"x":{},"y":{}}}"#,
        Value::from(field("crv")?),
        Value::from(field("kty")?),
        Value::from(field("x")?),
        Value::from(field("y")?)
    );
    Some(URL_SAFE_NO_PAD.encode(Sha256::digest(canonical.as_bytes())))
}

/// "Determining keys associated with an issuer": the key SHALL have `kty: EC`, `use: sig`,
/// `alg: ES256`, `crv: P-256`, a `kid` equal to its RFC 7638 thumbprint, and no private `d`.
fn check_key(jwk: &Value, kid: &str) -> std::result::Result<(), Refusal> {
    let field = |k: &str| jwk.get(k).and_then(Value::as_str);
    if field("kty") != Some("EC") || field("crv") != Some("P-256") {
        return Err(Refusal::Key("it is not an EC P-256 key"));
    }
    if field("use") != Some("sig") || field("alg") != Some("ES256") {
        return Err(Refusal::Key("it is not marked use sig and alg ES256"));
    }
    if jwk.get("d").is_some() {
        return Err(Refusal::Key("it carries a private part"));
    }
    if thumbprint(jwk).as_deref() != Some(kid) {
        return Err(Refusal::Key("kid is not the key's thumbprint"));
    }
    Ok(())
}

fn day(t: DateTime<Utc>) -> String {
    t.format("%Y-%m-%d").to_string()
}

/// Verify one card at `now`. The JWKS comes from `<iss>/.well-known/jwks.json` ("Determining keys
/// associated with an issuer") through `route`; call this only for an issuer host the person
/// approved.
pub async fn verify(
    jws: &str,
    keys: &mut Keys,
    route: &Route,
    now: DateTime<Utc>,
) -> std::result::Result<Card, Refusal> {
    let header = jose::jws_header(jws).map_err(jose_refusal)?;
    let kid = kid_of(&header)?;
    let unverified = issuer(jws)?;
    if !keys.jwks.contains_key(&unverified.iss) {
        let jwks = fetch_json(&format!("{}/.well-known/jwks.json", unverified.iss), route).await?;
        keys.jwks.insert(unverified.iss.clone(), jwks);
    }
    let jwks = &keys.jwks[&unverified.iss];
    // "Signing keys in the .keys[] array can be identified by kid"
    let jwk = jwks
        .get("keys")
        .and_then(Value::as_array)
        .ok_or(Refusal::Keys("the answer is not a JWKS".into()))?
        .iter()
        .find(|k| k.get("kid").and_then(Value::as_str) == Some(kid.as_str()))
        .ok_or(Refusal::UnknownKid)?;
    check_key(jwk, &kid)?;
    let key = jose::es256_key(jwk).map_err(|_| Refusal::Key("it is not a P-256 point"))?;
    let verified = jose::verify_es256(jws, &key, MAX_PAYLOAD).map_err(jose_refusal)?;
    let Read { claims, .. } = claims(&verified.payload)?;
    // jose.rs: the verified iss must be the issuer whose JWKS gave the key
    if claims.iss != unverified.iss {
        return Err(Refusal::Payload("iss changed"));
    }
    // RFC 7519 4.1.5: not accepted before nbf
    if claims.nbf > now + chrono::Duration::seconds(NBF_LEEWAY_SECONDS) {
        return Err(Refusal::NotYet(day(claims.nbf)));
    }
    // "Expiration of Health Cards": reject an exp that is before the verification time
    if let Some(exp) = claims.exp
        && exp < now
    {
        return Err(Refusal::Expired(day(exp)));
    }
    // "Revocation": a key with crlVersion has a revocation list; a card with a rid in it is rejected
    if jwk.get("crlVersion").is_some()
        && let Some(rid) = &claims.rid
    {
        let crl = fetch_json(&format!("{}/.well-known/crl/{kid}.json", unverified.iss), route).await?;
        if revoked(&crl, rid, claims.nbf) {
            return Err(Refusal::Revoked);
        }
    }
    Ok(Card {
        iss: claims.iss,
        kid,
        issued: claims.nbf,
        expires: claims.exp,
        types: claims.types,
        bundle: claims.bundle,
    })
}

/// "Revocation": a `rids` entry is the rid, or `<rid>.<timestamp>`, which revokes only cards whose
/// nbf is before the timestamp.
fn revoked(crl: &Value, rid: &str, nbf: DateTime<Utc>) -> bool {
    crl.get("rids").and_then(Value::as_array).is_some_and(|rids| {
        rids.iter()
            .filter_map(Value::as_str)
            .any(|entry| match entry.split_once('.') {
                None => entry == rid,
                Some((id, ts)) => id == rid && ts.parse::<i64>().is_ok_and(|ts| nbf.timestamp() < ts),
            })
    })
}

// ------------------------------------------------------------------ receive ----

/// What came of one import.
#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    /// Nothing was fetched or stored: the person has not approved these issuer hosts yet.
    NeedsApproval(Vec<Issuer>),
    /// Nothing was stored. `iss` is the issuer the refused card names, when it got that far.
    Refused {
        refusal: Refusal,
        iss: Option<String>,
    },
    Imported(Imported),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Imported {
    pub document_id: Uuid,
    /// False when the same bytes were received before (the import then skips what it already has).
    pub created: bool,
    pub cards: Vec<Card>,
    pub summary: ImportSummary,
}

/// The identity results of `fhir::import_resources`, the weakest first.
const IDENTITY_RANK: [&str; 4] = ["mismatch", "missing", "hinted", "matched"];

/// One import as the person sent it.
#[derive(Debug, Clone, Copy)]
pub struct Received<'a> {
    /// The bytes as given: the pasted text, the file or the photo. They become the document.
    pub bytes: &'a [u8],
    pub filename: Option<&'a str>,
    /// `bytes`, read with [`read`].
    pub input: &'a Input,
    /// The issuer hosts the person said yes to. Any other host stops the import before a fetch.
    pub approved: &'a [String],
}

/// Receive one import for `profile` at `now`.
pub async fn receive(
    conn: &mut PgConnection,
    blobs: &BlobStore,
    profile: &Profile,
    received: &Received<'_>,
    route: &Route,
    now: DateTime<Utc>,
) -> Result<Outcome> {
    let Received {
        bytes,
        filename,
        input,
        approved,
    } = *received;
    let mut issuers = Vec::new();
    for jws in &input.cards {
        match issuer(jws) {
            Ok(i) => issuers.push(i),
            Err(refusal) => return Ok(Outcome::Refused { refusal, iss: None }),
        }
    }
    let mut waiting: Vec<Issuer> = Vec::new();
    for i in &issuers {
        if !approved.contains(&i.host) && !waiting.contains(i) {
            waiting.push(i.clone());
        }
    }
    if !waiting.is_empty() {
        return Ok(Outcome::NeedsApproval(waiting));
    }
    let mut keys = Keys::default();
    let mut cards = Vec::new();
    for (jws, i) in input.cards.iter().zip(&issuers) {
        match verify(jws, &mut keys, route, now).await {
            Ok(card) => cards.push(card),
            Err(refusal) => {
                return Ok(Outcome::Refused {
                    refusal,
                    iss: Some(i.iss.clone()),
                });
            }
        }
    }

    let (mime, default_name) = match input.form {
        Form::Text => ("text/plain", "smart-health-card.txt"),
        Form::File => (MIME, "health-card.smart-health-card"),
        Form::Photo("image/png") => ("image/png", "health-card.png"),
        Form::Photo(other) => (other, "health-card.jpg"),
    };
    let sha = blobs.put_bytes(bytes).map_err(|e| Error::Other(e.into()))?;
    let doc = create_document(
        &mut *conn,
        &NewDocument {
            filename: Some(filename.filter(|f| !f.is_empty()).unwrap_or(default_name)),
            channel: CHANNEL,
            ..NewDocument::upload(
                profile.household_id,
                Some(profile.id),
                &sha,
                mime,
                i64::try_from(bytes.len()).unwrap_or(i64::MAX),
            )
        },
    )
    .await?;
    let signed: Vec<Value> = cards
        .iter()
        .map(|c| json!({"iss": c.iss, "kid": c.kid, "issued": c.issued, "expires": c.expires}))
        .collect();
    sqlx::query("update document set flags = flags || jsonb_build_object('shc', cast($1 as jsonb)) where id = $2")
        .bind(Value::from(signed))
        .bind(doc.id)
        .execute(&mut *conn)
        .await?;

    let mut summary = ImportSummary::new(doc.id.to_string());
    let mut identity: Option<String> = None;
    for (jws, card) in input.cards.iter().zip(&cards) {
        let bundle = serde_json::to_vec(&card.bundle)?;
        let (_, entries) = fhir::read_entries(&bundle, false)?;
        let key = format!("shc-{}", &hex::encode(Sha256::digest(jws.as_bytes()))[..16]);
        let (one, who) =
            fhir::import_resources(&mut *conn, &entries, profile, doc.id, &key, None, "SMART Health Card").await?;
        summary.accepted += one.accepted;
        summary.queued += one.queued;
        summary.skipped += one.skipped;
        for (reason, n) in one.reasons {
            *summary.reasons.entry(reason).or_default() += n;
        }
        let rank = |w: &str| IDENTITY_RANK.iter().position(|r| *r == w).unwrap_or(0);
        if identity.as_deref().is_none_or(|before| rank(&who) < rank(before)) {
            identity = Some(who);
        }
    }
    fhir::mark_imported(&mut *conn, doc.id, &summary, identity.as_deref()).await?;
    Ok(Outcome::Imported(Imported {
        document_id: doc.id,
        created: doc.created,
        cards,
        summary,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The spec's own example ("Encoding QRs"): `shc:/56762909524320603460292437404460…` starts the
    /// header `{"zip":"DEF"` of the example card.
    #[test]
    fn the_spec_example_digits_decode_to_its_jws_header() {
        assert_eq!(
            numeric("56762909524320603460292437404460"),
            Ok("eyJ6aXAiOiJERUYi".to_string())
        );
        assert_eq!(numeric("5"), Err(Refusal::Numeric));
        assert_eq!(numeric("78"), Err(Refusal::Numeric), "78 is past 'z'");
        assert_eq!(numeric("5a"), Err(Refusal::Numeric));
    }

    /// The RFC 7638 3.2 rule for an EC key, on the spec's published example issuer key: its kid is
    /// its thumbprint.
    #[test]
    fn the_thumbprint_of_the_spec_example_key_is_its_kid() {
        let jwk = json!({"kty": "EC", "kid": "3Kfdg-XwP-7gXyywtUfUADwBumDOPKMQx-iELL11W9s", "use": "sig",
            "alg": "ES256", "crv": "P-256", "x": "11XvRWy1I2S0EyJlyf_bWfw_TQ5CJJNLw78bHXNxcgw",
            "y": "eZXwxvO1hvCY0KucrPfKo7yAyMT6Ajc3N7OkAB6VYy8"});
        assert_eq!(
            thumbprint(&jwk).as_deref(),
            Some("3Kfdg-XwP-7gXyywtUfUADwBumDOPKMQx-iELL11W9s")
        );
        assert_eq!(check_key(&jwk, "3Kfdg-XwP-7gXyywtUfUADwBumDOPKMQx-iELL11W9s"), Ok(()));
    }

    #[test]
    fn a_card_arrives_on_a_channel_the_database_allows() {
        assert!(ownhealth_core::documents::CHANNELS.contains(&CHANNEL));
    }

    #[test]
    fn an_issuer_is_https_without_a_trailing_slash() {
        assert_eq!(
            issuer_of("https://issuer.labnorte.example"),
            Ok(Issuer {
                iss: "https://issuer.labnorte.example".into(),
                host: "issuer.labnorte.example".into()
            })
        );
        assert_eq!(
            issuer_of("https://127.0.0.1:8443/shc").map(|i| i.host),
            Ok("127.0.0.1:8443".into())
        );
        for bad in [
            "http://issuer.labnorte.example",
            "https://issuer.labnorte.example/",
            "https://user@issuer.labnorte.example",
            "https://issuer.labnorte.example?x=1",
            "issuer.labnorte.example",
        ] {
            assert_eq!(issuer_of(bad), Err(Refusal::Issuer), "{bad}");
        }
    }

    #[test]
    fn a_revocation_entry_with_a_time_revokes_only_older_cards() {
        let crl = json!({"kid": "k", "method": "rid", "ctr": 2, "rids": ["AQPCj4wwk6Mt", "lHKzqFUMjhs.1636977600"]});
        let at = |s: i64| DateTime::from_timestamp(s, 0).unwrap_or_default();
        assert!(revoked(&crl, "AQPCj4wwk6Mt", at(1_700_000_000)));
        assert!(revoked(&crl, "lHKzqFUMjhs", at(1_636_977_599)));
        assert!(!revoked(&crl, "lHKzqFUMjhs", at(1_636_977_600)));
        assert!(!revoked(&crl, "other", at(1)));
    }
}
