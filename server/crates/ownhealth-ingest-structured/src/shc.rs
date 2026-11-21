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
//! 4. [`check`] runs them for one import: one question per issuer host, nothing fetched until the
//!    person approved every host, each issuer's keys and revocation list fetched once, and the whole
//!    check stopped after [`VERIFY_LIMIT`]. It holds no database connection.
//! 5. [`store`] keeps the verified cards for one person: the input bytes become one document on
//!    channel `shc`, named for its signer and date, and each card's FHIR bundle goes to
//!    `fhir::import_resources`. A card already stored, in any form, adds no second document.
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
/// The QR search runs at this long side, then at half and a quarter of it ([`search_sizes`]).
const MAX_SEARCH_SIDE: u32 = 4000;
/// A photo larger than this on a side is refused from its header, before it is decoded.
pub const MAX_PHOTO_SIDE: u32 = 8192;
/// The most memory the photo decoder may take: an 8192 x 8192 photo in RGB, with room.
const MAX_PHOTO_ALLOC: u64 = 192 * 1024 * 1024;
/// Every reason code, as the UI lists them (ui/src/lib/logic/shc.ts SHC_REASONS).
pub const REASONS: [&str; 19] = [
    "no_card",
    "photo_too_large",
    "numeric",
    "chunk_missing",
    "chunks_unread",
    "too_many",
    "not_jws",
    "header",
    "payload",
    "issuer",
    "private_issuer",
    "keys",
    "unknown_kid",
    "key",
    "bad_signature",
    "expired",
    "not_yet",
    "revoked",
    "revocation_list",
];
/// Every detail code of a keys or revocation-list refusal (ui/src/lib/logic/shc.ts SHC_DETAILS):
/// what went wrong, never the server's own text. `http` stands for `http_<status>`.
pub const DETAILS: [&str; 10] = [
    "dns",
    "timeout",
    "connect",
    "too_large",
    "not_json",
    "not_jwks",
    "not_crl",
    "other_kid",
    "old_list",
    "http",
];
/// The whole verification of one import stops after this.
pub const VERIFY_LIMIT: std::time::Duration = std::time::Duration::from_secs(30);

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
    #[error("the photo is larger than {MAX_PHOTO_SIDE} pixels on a side")]
    PhotoTooLarge,
    #[error("{found} of the {total} QR codes of a split card could be read")]
    ChunksUnread { found: usize, total: usize },
    /// The detail is one of [`DETAILS`]. A card is refused when its issuer's list cannot be read,
    /// because then nothing shows that the issuer did not revoke it.
    #[error("the issuer's revocation list could not be read ({0})")]
    RevocationList(String),
    #[error("the card is damaged and cannot be read")]
    NotJws,
    #[error("the card's header does not meet the SMART Health Cards rules: {0}")]
    Header(&'static str),
    #[error("the card's content does not meet the SMART Health Cards rules: {0}")]
    Payload(&'static str),
    #[error("the card's issuer is not an https URL without a trailing slash")]
    Issuer,
    #[error("{0} has a private, loopback or link-local address, so its keys were not fetched")]
    PrivateIssuer(String),
    /// The detail is one of [`DETAILS`].
    #[error("the issuer's keys could not be fetched ({0})")]
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
            Refusal::PhotoTooLarge => "photo_too_large",
            Refusal::ChunksUnread { .. } => "chunks_unread",
            Refusal::RevocationList(_) => "revocation_list",
        }
    }

    /// The one value the UI puts in its sentence: a date, a host, `2/3` for a missing chunk, or why
    /// the keys could not be fetched.
    pub fn detail(&self) -> Option<String> {
        match self {
            Refusal::Expired(d)
            | Refusal::NotYet(d)
            | Refusal::PrivateIssuer(d)
            | Refusal::Keys(d)
            | Refusal::RevocationList(d) => Some(d.clone()),
            Refusal::ChunkMissing { missing, total } => Some(format!("{missing}/{total}")),
            Refusal::ChunksUnread { found, total } => Some(format!("{found}/{total}")),
            _ => None,
        }
    }
}

/// Every text this module writes for a person, for the wording lint.
pub fn phrases() -> Value {
    let all = samples();
    json!({ "en": all.iter().map(ToString::to_string).collect::<Vec<_>>() })
}

/// One refusal of each kind, in [`REASONS`] order.
fn samples() -> Vec<Refusal> {
    vec![
        Refusal::NoCard("photo"),
        Refusal::PhotoTooLarge,
        Refusal::Numeric,
        Refusal::ChunkMissing { missing: 2, total: 3 },
        Refusal::ChunksUnread { found: 2, total: 3 },
        Refusal::TooMany,
        Refusal::NotJws,
        Refusal::Header("zip is not DEF"),
        Refusal::Payload("nbf"),
        Refusal::Issuer,
        Refusal::PrivateIssuer("127.0.0.1".into()),
        Refusal::Keys("http_404".into()),
        Refusal::UnknownKid,
        Refusal::Key("kid is not the key's thumbprint"),
        Refusal::BadSignature,
        Refusal::Expired("2026-01-01".into()),
        Refusal::NotYet("2026-01-01".into()),
        Refusal::Revoked,
        Refusal::RevocationList("not_crl".into()),
    ]
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
        return Ok(Input {
            form: Form::Photo(mime),
            cards: photo_cards(bytes)?,
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
    // each shc:/ starts a code, also when codes are pasted back to back with no space between them
    let words: Vec<String> = text
        .split("shc:/")
        .skip(1)
        .map(|rest| format!("shc:/{}", rest.split_whitespace().next().unwrap_or_default()))
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

/// The cards in a photo of one or more QR codes. The photo is searched at up to 4000 px on its long
/// side, then at half and a quarter of that, until the codes found make whole cards: a printed card
/// photographed close is read at full size, a phone photo of a sheet often only once it is smaller.
fn photo_cards(bytes: &[u8]) -> std::result::Result<Vec<String>, Refusal> {
    let img = decode_photo(bytes)?;
    let mut sizes = search_sizes(img.width(), img.height()).into_iter();
    let Some((w, h)) = sizes.next() else {
        return Err(Refusal::NoCard("photo"));
    };
    let base = if (w, h) == img.dimensions() {
        img
    } else {
        image::imageops::thumbnail(&img, w, h)
    };
    let mut found = std::collections::BTreeSet::new();
    found.extend(qr_texts(&base));
    let mut got = from_shc_texts(&found.iter().cloned().collect::<Vec<_>>(), "photo");
    for (w, h) in sizes {
        if got.is_ok() {
            break;
        }
        found.extend(qr_texts(&image::imageops::thumbnail(&base, w, h)));
        got = from_shc_texts(&found.iter().cloned().collect::<Vec<_>>(), "photo");
    }
    got
}

/// Decode a photo to grayscale. A photo over [`MAX_PHOTO_SIDE`] on a side, or one that would take
/// more than [`MAX_PHOTO_ALLOC`] to decode, is refused from its header.
fn decode_photo(bytes: &[u8]) -> std::result::Result<image::GrayImage, Refusal> {
    let no_card = |_| Refusal::NoCard("photo");
    let mut reader = image::ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(no_card)?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_PHOTO_SIDE);
    limits.max_image_height = Some(MAX_PHOTO_SIDE);
    limits.max_alloc = Some(MAX_PHOTO_ALLOC);
    reader.limits(limits);
    match reader.decode() {
        Ok(img) => Ok(img.to_luma8()),
        Err(image::ImageError::Limits(_)) => Err(Refusal::PhotoTooLarge),
        Err(_) => Err(Refusal::NoCard("photo")),
    }
}

/// Every QR code in one grayscale image whose text starts with `shc:/`. rxing's multi-code reader,
/// trying harder, QR only.
fn qr_texts(img: &image::GrayImage) -> Vec<String> {
    let mut hints = rxing::DecodeHints::default()
        .with(rxing::DecodeHintValue::TryHarder(true))
        .with(rxing::DecodeHintValue::PossibleFormats(
            std::collections::HashSet::from([rxing::BarcodeFormat::QR_CODE]),
        ));
    rxing::helpers::detect_multiple_in_luma_with_hints(img.as_raw().clone(), img.width(), img.height(), &mut hints)
        .map(|found| {
            found
                .into_iter()
                .map(|r| r.getText().to_string())
                .filter(|t| t.starts_with("shc:/"))
                .collect()
        })
        .unwrap_or_default()
}

/// The sizes the QR search runs at for a photo of `w` x `h`: fitted to 4000 px on its long side,
/// then 2000 and 1000 px for each of those the photo is larger than.
pub fn search_sizes(w: u32, h: u32) -> Vec<(u32, u32)> {
    let fit = |w: u32, h: u32, side: u32| {
        let long = w.max(h);
        if long <= side {
            return (w, h);
        }
        let scale = |n: u32| {
            u32::try_from(u64::from(n) * u64::from(side) / u64::from(long))
                .unwrap_or(1)
                .max(1)
        };
        (scale(w), scale(h))
    };
    let (w, h) = fit(w, h, MAX_SEARCH_SIDE);
    let mut sizes = vec![(w, h)];
    for side in [MAX_SEARCH_SIDE / 2, MAX_SEARCH_SIDE / 4] {
        if w.max(h) > side {
            sizes.push(fit(w, h, side));
        }
    }
    sizes
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
            // a photo that shows only some codes is most often a photo taken too far away
            if what == "photo" {
                return Err(Refusal::ChunksUnread {
                    found: chunks.len(),
                    total: n,
                });
            }
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
    // every resource must name its type, or the import cannot read it: refused here, before anything
    // is stored
    let typed = |e: &Value| {
        e.get("resource")
            .is_none_or(|r| r.get("resourceType").is_some_and(Value::is_string))
    };
    if !bundle
        .get("entry")
        .and_then(Value::as_array)
        .is_none_or(|e| e.iter().all(typed))
    {
        return Err(Refusal::Payload("a bundle entry has no resource type"));
    }
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
/// An `iss` whose host is a literal private, loopback or link-local address is refused here, so the
/// person is never asked about it. A name is never looked up here: that waits for the person's yes,
/// and the egress guard checks every address it resolves to.
fn issuer_of(iss: &str) -> std::result::Result<Issuer, Refusal> {
    let url = url::Url::parse(iss).map_err(|_| Refusal::Issuer)?;
    let ip = match url.host() {
        Some(url::Host::Ipv4(ip)) => Some(std::net::IpAddr::V4(ip)),
        Some(url::Host::Ipv6(ip)) => Some(std::net::IpAddr::V6(ip)),
        _ => None,
    };
    if let Some(ip) = ip
        && !net::is_public(ip)
    {
        return Err(Refusal::PrivateIssuer(ip.to_string()));
    }
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
    /// The host the person approved.
    pub host: String,
    /// SHA-256 of the compact JWS, hex.
    pub jws_sha: String,
    /// The patient the card names, as the card says it: untrusted text.
    pub name: Option<String>,
    pub birth_date: Option<String>,
    pub kid: String,
    pub issued: DateTime<Utc>,
    pub expires: Option<DateTime<Utc>>,
    pub types: Vec<String>,
    pub bundle: Value,
}

/// Each issuer's JWKS and each (issuer, kid) revocation list, fetched once per import.
#[derive(Default)]
pub struct Keys {
    jwks: HashMap<String, Value>,
    crl: HashMap<(String, String), Value>,
}

/// Why a fetch gave no JSON: a private address, or one of [`DETAILS`].
enum Fetch {
    Private(String),
    Failed(String),
}

async fn fetch_json(url: &str, route: &Route) -> std::result::Result<Value, Fetch> {
    let (from, egress) = route(url);
    let got = net::get(&from, &egress, &[("accept", "application/json")])
        .await
        .map_err(fetch_failure)?;
    if got.status != 200 {
        return Err(Fetch::Failed(format!("http_{}", got.status)));
    }
    serde_json::from_slice(&got.body).map_err(|_| Fetch::Failed("not_json".into()))
}

/// The detail code of a network error: what went wrong, never its text.
fn fetch_failure(e: NetError) -> Fetch {
    match e {
        NetError::PrivateAddress(host) => Fetch::Private(host),
        NetError::Resolve(_) => Fetch::Failed("dns".into()),
        NetError::Timeout(_) => Fetch::Failed("timeout".into()),
        NetError::TooLarge(_) => Fetch::Failed("too_large".into()),
        NetError::BadUrl | NetError::NotHttps | NetError::Http { .. } | NetError::Write(_) => {
            Fetch::Failed("connect".into())
        }
    }
}

fn keys_refusal(f: Fetch) -> Refusal {
    match f {
        Fetch::Private(host) => Refusal::PrivateIssuer(host),
        Fetch::Failed(code) => Refusal::Keys(code),
    }
}

fn list_refusal(f: Fetch) -> Refusal {
    match f {
        Fetch::Private(host) => Refusal::PrivateIssuer(host),
        Fetch::Failed(code) => Refusal::RevocationList(code),
    }
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
        let jwks = fetch_json(&format!("{}/.well-known/jwks.json", unverified.iss), route)
            .await
            .map_err(keys_refusal)?;
        keys.jwks.insert(unverified.iss.clone(), jwks);
    }
    let jwks = &keys.jwks[&unverified.iss];
    // "Signing keys in the .keys[] array can be identified by kid"
    let jwk = jwks
        .get("keys")
        .and_then(Value::as_array)
        .ok_or(Refusal::Keys("not_jwks".into()))?
        .iter()
        .find(|k| k.get("kid").and_then(Value::as_str) == Some(kid.as_str()))
        .ok_or(Refusal::UnknownKid)?
        .clone();
    check_key(&jwk, &kid)?;
    let key = jose::es256_key(&jwk).map_err(|_| Refusal::Key("it is not a P-256 point"))?;
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
    // "Revocation": a key with crlVersion has a revocation list at <iss>/.well-known/crl/<kid>.json,
    // `{"kid", "method", "ctr", "rids": [...]}`, and verifiers SHALL download it whether or not the
    // card has a rid; a card with a rid in it is rejected. A list that cannot be read, is not a list,
    // is another key's or is older than the key's crlVersion refuses the card: it proves nothing.
    if jwk.get("crlVersion").is_some() {
        let at = (unverified.iss.clone(), kid.clone());
        if !keys.crl.contains_key(&at) {
            let crl = fetch_json(&format!("{}/.well-known/crl/{kid}.json", unverified.iss), route)
                .await
                .map_err(list_refusal)?;
            if !crl.get("rids").is_some_and(Value::is_array) {
                return Err(Refusal::RevocationList("not_crl".into()));
            }
            if crl.get("kid").and_then(Value::as_str) != Some(kid.as_str()) {
                return Err(Refusal::RevocationList("other_kid".into()));
            }
            // `crlVersion` is the list's update counter: a list with a lower `ctr` is an old copy
            if let Some(version) = jwk.get("crlVersion").and_then(counter)
                && crl.get("ctr").and_then(counter).is_none_or(|ctr| ctr < version)
            {
                return Err(Refusal::RevocationList("old_list".into()));
            }
            keys.crl.insert(at.clone(), crl);
        }
        if let Some(rid) = &claims.rid
            && revoked(&keys.crl[&at], rid, claims.nbf)
        {
            return Err(Refusal::Revoked);
        }
    }
    let (name, birth_date) = subject(&claims.bundle);
    Ok(Card {
        host: unverified.host.clone(),
        jws_sha: hex::encode(Sha256::digest(jws.as_bytes())),
        name,
        birth_date,
        iss: claims.iss,
        kid,
        issued: claims.nbf,
        expires: claims.exp,
        types: claims.types,
        bundle: claims.bundle,
    })
}

/// A counter written as a number or as a string of digits (the spec's template quotes it).
fn counter(v: &Value) -> Option<u64> {
    v.as_u64().or_else(|| v.as_str().and_then(|s| s.parse().ok()))
}

/// The patient a card names, as the card says it (untrusted): `name[0].text`, else its given names
/// and family, and `birthDate`.
fn subject(bundle: &Value) -> (Option<String>, Option<String>) {
    let patient = bundle.get("entry").and_then(Value::as_array).and_then(|e| {
        e.iter()
            .filter_map(|e| e.get("resource"))
            .find(|r| r.get("resourceType").and_then(Value::as_str) == Some("Patient"))
    });
    let Some(p) = patient else {
        return (None, None);
    };
    let name = p.pointer("/name/0").and_then(|n| {
        n.get("text").and_then(Value::as_str).map(str::to_string).or_else(|| {
            let mut parts: Vec<&str> = n
                .get("given")
                .and_then(Value::as_array)
                .map(|g| g.iter().filter_map(Value::as_str).collect())
                .unwrap_or_default();
            parts.extend(n.get("family").and_then(Value::as_str));
            (!parts.is_empty()).then(|| parts.join(" "))
        })
    });
    let born = p.get("birthDate").and_then(Value::as_str).map(str::to_string);
    (name.map(|n| clip(&n, 120)), born.map(|b| clip(&b, 10)))
}

fn clip(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
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
    NeedsApproval(Vec<Ask>),
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
    /// The weakest identity result of the cards: matched, hinted, missing or mismatch.
    pub identity: String,
}

/// One question for the person: may OwnHealth fetch keys from this host, for these issuers?
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ask {
    pub host: String,
    pub issuers: Vec<String>,
}

/// What came of checking one import, before anything is stored.
#[derive(Debug, Clone, PartialEq)]
pub enum Checked {
    NeedsApproval(Vec<Ask>),
    Refused { refusal: Refusal, iss: Option<String> },
    Verified(Vec<Card>),
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

/// Check one import: who signed each card, whether the person approved its host, and every check of
/// [`verify`]. No database.
pub async fn check(input: &Input, approved: &[String], route: &Route, now: DateTime<Utc>) -> Checked {
    check_within(input, approved, route, now, VERIFY_LIMIT).await
}

/// [`check`], stopped after `limit`: an issuer that stalls answers, one after another, would
/// otherwise hold the import for 15 s per fetch.
pub async fn check_within(
    input: &Input,
    approved: &[String],
    route: &Route,
    now: DateTime<Utc>,
    limit: std::time::Duration,
) -> Checked {
    let mut issuers = Vec::new();
    for jws in &input.cards {
        match issuer(jws) {
            Ok(i) => issuers.push(i),
            Err(refusal) => return Checked::Refused { refusal, iss: None },
        }
    }
    // one question per host: the person approves where the keys come from, and two issuers on one
    // host share it
    let mut asks: Vec<Ask> = Vec::new();
    for i in issuers.iter().filter(|i| !approved.contains(&i.host)) {
        match asks.iter_mut().find(|a| a.host == i.host) {
            Some(a) if !a.issuers.contains(&i.iss) => a.issuers.push(i.iss.clone()),
            Some(_) => {}
            None => asks.push(Ask {
                host: i.host.clone(),
                issuers: vec![i.iss.clone()],
            }),
        }
    }
    if !asks.is_empty() {
        return Checked::NeedsApproval(asks);
    }
    let mut at: Option<String> = None;
    let verified = tokio::time::timeout(limit, async {
        let mut keys = Keys::default();
        let mut cards = Vec::new();
        for (jws, i) in input.cards.iter().zip(&issuers) {
            at = Some(i.iss.clone());
            match verify(jws, &mut keys, route, now).await {
                Ok(card) => cards.push(card),
                Err(refusal) => {
                    return Checked::Refused {
                        refusal,
                        iss: Some(i.iss.clone()),
                    };
                }
            }
        }
        Checked::Verified(cards)
    })
    .await;
    verified.unwrap_or(Checked::Refused {
        refusal: Refusal::Keys("timeout".into()),
        iss: at,
    })
}

/// Receive one import for `profile` at `now`: [`check`], then [`store`].
pub async fn receive(
    conn: &mut PgConnection,
    blobs: &BlobStore,
    profile: &Profile,
    received: &Received<'_>,
    route: &Route,
    now: DateTime<Utc>,
) -> Result<Outcome> {
    Ok(match check(received.input, received.approved, route, now).await {
        Checked::NeedsApproval(asks) => Outcome::NeedsApproval(asks),
        Checked::Refused { refusal, iss } => Outcome::Refused { refusal, iss },
        Checked::Verified(cards) => Outcome::Imported(store(conn, blobs, profile, received, cards).await?),
    })
}

/// What a document shows of each card it holds: at most this many rows per card.
const MAX_ENTRIES: usize = 50;

/// The rows of a card as its document shows them: type, date and a short text for each resource
/// except the Patient.
fn entries(bundle: &Value) -> Vec<Value> {
    let text_of = |c: &Value| {
        c.get("text")
            .or_else(|| c.pointer("/coding/0/display"))
            .or_else(|| c.pointer("/coding/0/code"))
            .and_then(Value::as_str)
            .map(str::to_string)
    };
    bundle
        .get("entry")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|e| e.get("resource"))
        .filter(|r| r.get("resourceType").and_then(Value::as_str) != Some("Patient"))
        .take(MAX_ENTRIES)
        .map(|r| {
            let date = ["occurrenceDateTime", "effectiveDateTime", "issued"]
                .iter()
                .find_map(|k| r.get(*k).and_then(Value::as_str))
                .map(|d| clip(d, 10));
            let what = r.get("vaccineCode").or_else(|| r.get("code")).and_then(text_of);
            let value = r
                .pointer("/valueQuantity/value")
                .map(|v| {
                    let unit = r
                        .pointer("/valueQuantity/unit")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    format!("{v} {unit}").trim().to_string()
                })
                .or_else(|| r.get("valueString").and_then(Value::as_str).map(str::to_string))
                .or_else(|| r.get("valueCodeableConcept").and_then(text_of));
            let text = match (what, value) {
                (Some(w), Some(v)) => Some(format!("{w}: {v}")),
                (w, v) => w.or(v),
            };
            json!({
                "type": r.get("resourceType").and_then(Value::as_str),
                "date": date,
                "text": text.map(|t| clip(&t, 120)),
            })
        })
        .collect()
}

/// Store verified cards for `profile`: one document on channel `shc`, and each card's bundle. Run
/// it in the transaction that checked the person may import for `profile`.
///
/// The document is named `SMART Health Card · <host> · <issued>` (a file or photo keeps the name it
/// came with), its provider is the signing host and its date the card's `nbf`. When an active
/// document of this person already holds every card, that document is used again and nothing new
/// is stored.
pub async fn store(
    conn: &mut PgConnection,
    blobs: &BlobStore,
    profile: &Profile,
    received: &Received<'_>,
    cards: Vec<Card>,
) -> Result<Imported> {
    let Received {
        bytes, filename, input, ..
    } = *received;
    let held: Vec<Value> = cards.iter().map(|c| json!({"jws": c.jws_sha})).collect();
    // one store per profile at a time, so two forms of one card sent together share a document
    sqlx::query("select pg_advisory_xact_lock(hashtext($1))")
        .bind(format!("shc.store:{}", profile.id))
        .execute(&mut *conn)
        .await?;
    let earlier: Option<Uuid> = sqlx::query_scalar(
        "select id from document where coalesce(profile_id, household_id) = coalesce($1, $2) \
         and status = 'active' and flags -> 'shc' @> cast($3 as jsonb) order by created_at limit 1",
    )
    .bind(Some(profile.id))
    .bind(profile.household_id)
    .bind(Value::from(held))
    .fetch_optional(&mut *conn)
    .await?;

    let mut hosts: Vec<&str> = Vec::new();
    for c in &cards {
        if !hosts.contains(&c.host.as_str()) {
            hosts.push(&c.host);
        }
    }
    let hosts = hosts.join(", ");
    let issued = cards.iter().map(|c| c.issued).min();
    let (document_id, created) = match earlier {
        Some(id) => (id, false),
        None => {
            let mime = match input.form {
                Form::Text => "text/plain",
                Form::File => MIME,
                Form::Photo(m) => m,
            };
            let named = format!("SMART Health Card · {hosts} · {}", issued.map(day).unwrap_or_default());
            let name = match (input.form, filename.filter(|f| !f.trim().is_empty())) {
                (Form::Text, _) | (_, None) => named,
                (_, Some(given)) => given.to_string(),
            };
            let sha = blobs.put_bytes(bytes).map_err(|e| Error::Other(e.into()))?;
            let doc = create_document(
                &mut *conn,
                &NewDocument {
                    filename: Some(&name),
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
                .map(|c| {
                    json!({"iss": c.iss, "host": c.host, "kid": c.kid, "issued": c.issued,
                        "expires": c.expires, "jws": c.jws_sha, "entries": entries(&c.bundle)})
                })
                .collect();
            sqlx::query(
                "update document set flags = flags || jsonb_build_object('shc', cast($1 as jsonb)), \
                 provider_name = $2, reported_at = $3 where id = $4",
            )
            .bind(Value::from(signed))
            .bind(&hosts)
            .bind(issued)
            .bind(doc.id)
            .execute(&mut *conn)
            .await?;
            (doc.id, doc.created)
        }
    };

    let mut summary = ImportSummary::new(document_id.to_string());
    let mut identity: Option<String> = None;
    for (jws, card) in input.cards.iter().zip(&cards) {
        let bundle = serde_json::to_vec(&card.bundle)?;
        let (_, entries) = fhir::read_entries(&bundle, false)?;
        let key = format!("shc-{}", &hex::encode(Sha256::digest(jws.as_bytes()))[..16]);
        let label = format!("SMART Health Card · {}", card.host);
        let (one, who) = fhir::import_resources(&mut *conn, &entries, profile, document_id, &key, None, &label).await?;
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
    // a card stored before keeps the import summary it got then
    if earlier.is_none() {
        fhir::mark_imported(&mut *conn, document_id, &summary, identity.as_deref()).await?;
    }
    Ok(Imported {
        document_id,
        created,
        cards,
        summary,
        identity: identity.unwrap_or_else(|| "missing".into()),
    })
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

    /// Every network error has its detail code; a lookup that fails is `dns`.
    #[test]
    fn each_network_error_has_a_detail_code() {
        let code = |e| match fetch_failure(e) {
            Fetch::Private(h) => format!("private {h}"),
            Fetch::Failed(c) => c,
        };
        assert_eq!(code(NetError::Resolve("issuer.example".into())), "dns");
        assert_eq!(code(NetError::Timeout("issuer.example".into())), "timeout");
        assert_eq!(code(NetError::TooLarge(1)), "too_large");
        assert_eq!(code(NetError::PrivateAddress("10.0.0.1".into())), "private 10.0.0.1");
        for e in [
            NetError::BadUrl,
            NetError::NotHttps,
            NetError::Write("w".into()),
            NetError::Http {
                host: "h".into(),
                message: "m".into(),
            },
        ] {
            assert_eq!(code(e), "connect");
        }
        for d in ["dns", "timeout", "too_large", "connect", "old_list"] {
            assert!(DETAILS.contains(&d), "{d}");
        }
    }

    /// The QR search runs at most at 4000 px, then at 2000 and 1000 px when the photo is larger.
    #[test]
    fn a_photo_is_searched_at_4000_then_2000_then_1000_pixels() {
        assert_eq!(search_sizes(4968, 2160), vec![(4000, 1739), (2000, 869), (1000, 434)]);
        assert_eq!(search_sizes(6000, 8000), vec![(3000, 4000), (1500, 2000), (750, 1000)]);
        assert_eq!(search_sizes(1080, 1107), vec![(1080, 1107), (975, 1000)]);
        assert_eq!(search_sizes(800, 600), vec![(800, 600)]);
    }

    #[test]
    fn a_literal_private_address_is_not_an_issuer_to_ask_about() {
        for (iss, ip) in [
            ("https://10.0.0.1", "10.0.0.1"),
            ("https://192.168.1.20:8443/shc", "192.168.1.20"),
            ("https://[fe80::1]", "fe80::1"),
        ] {
            assert_eq!(issuer_of(iss), Err(Refusal::PrivateIssuer(ip.into())), "{iss}");
        }
        assert_eq!(
            issuer_of("https://8.8.8.8:8443/shc").map(|i| i.host),
            Ok("8.8.8.8:8443".into())
        );
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

    /// The UI words every refusal and every detail code the server sends (ui/src/lib/logic/shc.ts).
    #[test]
    fn the_ui_lists_every_refusal_and_detail_code() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../../ui/src/lib/logic/shc.ts");
        let ts = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("{path}: {e}"));
        assert_eq!(ts_list(&ts, "SHC_REASONS"), REASONS.to_vec());
        assert_eq!(ts_list(&ts, "SHC_DETAILS"), DETAILS.to_vec());
        let codes: Vec<&str> = samples().iter().map(Refusal::code).collect();
        assert_eq!(codes, REASONS.to_vec(), "one sample per refusal, in REASONS order");
    }
}
