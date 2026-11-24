//! SMART Health Cards receive (slice 22) over REST; the work is in `ownhealth_ingest_structured::shc`.
//!
//! ```text
//! POST /api/shc   multipart: profile, then text (pasted shc:/ text) or file (a .smart-health-card
//!                 file or a photo of the QR code), and one approve field per issuer host the
//!                 person said yes to
//! ```
//!
//! - 200 `{"status": "needs_approval", "hosts": [{"host", "issuers": [iss]}]}`: nothing was fetched
//!   or stored. One entry per host, which may serve several issuers. The person approves the hosts
//!   and the same form is sent again with `approve`.
//! - 200 `{"status": "imported", "document_id", "created", "identity", "cards": [{"iss", "host",
//!   "issued", "expires", "types", "name", "birth_date"}], "summary"}`. `identity` is matched,
//!   hinted, missing or mismatch; `name` and `birth_date` are what the card says, unverified.
//! - 422 `{"detail": {"code": "shc_refused", "reason", "message", "iss", "detail"}}`: nothing was
//!   stored. `reason` is [`shc::Refusal::code`], `detail` its date, host, count or detail code.
//!
//! No database connection is held while an issuer is contacted: the permission is checked, the
//! connection goes back to the pool, the cards are checked over the network, and then a transaction
//! checks the permission again and stores them.
//!
//! SMART Health Links received (slice 23; the work is in `ownhealth_ingest_structured::shl_receive`)
//! use the same route: `text` holds `shlink:/…`, alone or after a viewer URL, with these fields too:
//! `passcode` (once, for a link with the P flag; it goes only into the manifest request body) and
//! `held` (`<sha256> <content type>`, the files an earlier answer stored, sent back with the yes for
//! their card issuers so the link is not asked twice).
//!
//! - 200 `{"status": "needs_approval", "hosts": [{"host", "issuers": []}], "link": {"host", "label",
//!   "expires", "passcode", "direct"}}`: the link's host, before anything is fetched.
//! - 200 the same with the issuer hosts of the cards the link holds, and `"held": [{"sha",
//!   "content_type"}]`: the files were fetched and stored, no issuer was contacted.
//! - 200 `{"status": "imported", "link", "document_id", "created", "identity", "cards", "summary",
//!   "documents": [{"document_id", "created", "content_type"}], "not_imported": [{"content_type",
//!   "reason"}]}`. `document_id` is the first document, or null when no file was imported.
//! - 422 `{"detail": {"code": "shl_refused", "reason", "message", "detail"}}`, `reason` one of
//!   `shl_receive::REASONS`; a card the link holds is refused as `shc_refused` above.

use axum::extract::{DefaultBodyLimit, FromRequest, Multipart, Request, State};
use axum::http::StatusCode;
use axum::routing::post;
use axum::{Json, Router};
use ownhealth_core::audit::{AuditEvent, write_audit};
use ownhealth_core::net::Egress;
use ownhealth_core::{Error, Runtime};
use ownhealth_ingest_structured::shc::{self, Ask, Checked, Imported, Received, Refusal};
use ownhealth_ingest_structured::shl_receive::{self, CardsChecked, Held, Link, Stop};
use serde_json::{Value, json};

use crate::Caller;
use crate::error::ApiError;
use crate::routes::auth::{Fail, R};

/// A photo of a QR code, a card file or pasted text: 20 MiB is a large phone photo.
const MAX_BODY: usize = 20 * 1024 * 1024;
/// A text field (the profile, a host, pasted text) holds at most 1 MiB.
const MAX_FIELD: usize = 1024 * 1024;
/// At most this many `approve` fields: one issuer host per card, and the host of a link.
const MAX_APPROVED: usize = shc::MAX_CARDS + 1;

pub fn router() -> Router<Runtime> {
    Router::new().route("/shc", post(receive).layer(DefaultBodyLimit::max(MAX_BODY)))
}

fn bad(message: &str) -> Fail {
    Fail::Api(ApiError::new(StatusCode::BAD_REQUEST, "bad_request", message))
}

/// Where an issuer's keys are fetched from: the issuer's own URL under the connector egress guard
/// ([`shc::public`]). A debug build that runs with dev auth may also map a test issuer origin to a
/// local server (`Settings::dev_shc_issuers`, `OWNHEALTH_DEV_SHC_ISSUERS=https://issuer.example=
/// http://127.0.0.1:9177`, comma separated), so the receiver can be tried end to end with an issuer
/// we generate. Only a whole origin matches: the mapped prefix must be followed by `/`.
fn issuer_route(dev_auth: bool, map: Option<&str>) -> impl Fn(&str) -> (String, Egress) + Send + Sync + use<> {
    let local: Vec<(String, String)> = if cfg!(debug_assertions) && dev_auth {
        map.unwrap_or_default()
            .split(',')
            .filter_map(|pair| pair.split_once('='))
            .map(|(from, to)| (from.trim().to_string(), to.trim().to_string()))
            .collect()
    } else {
        Vec::new()
    };
    move |url: &str| {
        for (from, to) in &local {
            if let Some(rest) = url.strip_prefix(from.as_str())
                && rest.starts_with('/')
            {
                let egress = Egress {
                    allow_private: true,
                    https_only: false,
                    ..Egress::connector()
                };
                return (format!("{to}{rest}"), egress);
            }
        }
        shc::public(url)
    }
}

async fn text_field(field: axum::extract::multipart::Field<'_>) -> R<String> {
    let bytes = field_bytes(field, MAX_FIELD).await?;
    String::from_utf8(bytes).map_err(|_| bad("A text field is not UTF-8."))
}

async fn field_bytes(mut field: axum::extract::multipart::Field<'_>, cap: usize) -> R<Vec<u8>> {
    let mut buf = Vec::new();
    while let Some(chunk) = field
        .chunk()
        .await
        .map_err(|_| bad("There was an error parsing the body"))?
    {
        if buf.len() + chunk.len() > cap {
            return Err(bad("A field is too large."));
        }
        buf.extend_from_slice(&chunk);
    }
    Ok(buf)
}

async fn receive(State(rt): State<Runtime>, caller: Caller, req: Request) -> R<Json<Value>> {
    let principal = &caller.0;
    let mut form = Multipart::from_request(req, &rt)
        .await
        .map_err(|_| bad("The body is not a form."))?;
    let (mut profile, mut text, mut file, mut approved) = (None, None, None, Vec::new());
    let (mut passcode, mut held) = (None, Vec::new());
    while let Some(field) = form
        .next_field()
        .await
        .map_err(|_| bad("There was an error parsing the body"))?
    {
        match field.name() {
            Some("profile") => profile = Some(text_field(field).await?),
            Some("text") => text = Some(text_field(field).await?),
            Some("approve") => {
                approved.push(text_field(field).await?.trim().to_string());
                if approved.len() > MAX_APPROVED {
                    return Err(bad("Too many approve fields."));
                }
            }
            Some("passcode") => passcode = Some(Passcode(text_field(field).await?)),
            Some("held") => {
                held.push(held_field(&text_field(field).await?)?);
                if held.len() > shl_receive::MAX_FILES {
                    return Err(bad("Too many held fields."));
                }
            }
            Some("file") => {
                let name = field.file_name().map(str::to_string);
                file = Some((name, field_bytes(field, MAX_BODY).await?));
            }
            _ => {
                field_bytes(field, MAX_FIELD).await?;
            }
        }
    }
    let profile = profile.ok_or_else(|| bad("The profile field is missing."))?;
    if let (Some(t), None) = (&text, &file)
        && shl_receive::is_link(t)
    {
        let form = LinkForm {
            text: t,
            approved: &approved,
            passcode: passcode.as_ref().map(|p| p.0.as_str()),
            held,
        };
        return receive_link(&rt, &caller, &profile, form).await;
    }
    let (filename, bytes) = match (text, file) {
        (Some(text), None) => (None, text.into_bytes()),
        (None, Some(file)) => file,
        _ => return Err(bad("Send either text or file.")),
    };

    // the permission comes first, so a person who may not import never makes us fetch anything; the
    // connection goes back to the pool before any issuer is contacted
    let chosen = {
        let mut conn = rt.pool.acquire().await?;
        caller
            .require(&mut conn, &profile, None, Some("import"))
            .await?
            .profile
            .id
    };
    let read = {
        let bytes = bytes.clone();
        tokio::task::spawn_blocking(move || shc::read(&bytes))
            .await
            .map_err(|e| Error::Other(e.into()))?
    };
    let input = read.map_err(|refusal| refused(&refusal, None))?;
    let route = issuer_route(rt.settings.dev_auth, rt.settings.dev_shc_issuers.as_deref());
    let cards = match shc::check(&input, &approved, &route, chrono::Utc::now()).await {
        Checked::NeedsApproval(asks) => return Ok(Json(asks_json(&asks))),
        Checked::Refused { refusal, iss } => return Err(refused(&refusal, iss)),
        Checked::Verified(cards) => cards,
    };
    let received = Received {
        bytes: &bytes,
        filename: filename.as_deref(),
        input: &input,
        approved: &approved,
    };
    let mut tx = rt.pool.begin().await?;
    // checked again in the transaction that stores, for the same profile: the grant may have changed
    // during the fetches, and a name could now resolve to another profile
    let perm = caller
        .require(&mut tx, &chosen.to_string(), None, Some("import"))
        .await?;
    let got = shc::store(&mut tx, &rt.blobs, &perm.profile, &received, cards).await?;
    let mut audit = AuditEvent::new(principal.actor_kind(), "ui", "import");
    audit.actor_id = principal.user_id;
    audit.tool = Some("POST /api/shc".into());
    audit.profile_id = Some(perm.profile.id);
    audit.resource_type = Some("document".into());
    audit.resource_ids = Some(vec![got.document_id.to_string()]);
    write_audit(&mut tx, &audit).await?;
    tx.commit().await?;
    Ok(Json(imported_json(&got)))
}

/// A link's passcode as the form sent it. Its Debug never shows it.
struct Passcode(String);

impl std::fmt::Debug for Passcode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Passcode(..)")
    }
}

/// A `held` field: `<sha256> <content type>` (the content type may be empty).
fn held_field(v: &str) -> R<Held> {
    let (sha, content_type) = v.trim().split_once(' ').unwrap_or((v.trim(), ""));
    let sha = ownhealth_core::blobs::sha256_hex(sha).map_err(|_| bad("A held field is not a stored file."))?;
    Ok(Held {
        sha,
        content_type: content_type.trim().to_string(),
    })
}

/// The link fields of one form.
struct LinkForm<'a> {
    text: &'a str,
    approved: &'a [String],
    passcode: Option<&'a str>,
    held: Vec<Held>,
}

/// One SMART Health Link: the host question, the fetch with the passcode, the cards' issuer
/// questions, then the import, in a transaction that checks the permission again.
async fn receive_link(rt: &Runtime, caller: &Caller, profile: &str, form: LinkForm<'_>) -> R<Json<Value>> {
    let chosen = {
        let mut conn = rt.pool.acquire().await?;
        caller
            .require(&mut conn, profile, None, Some("import"))
            .await?
            .profile
            .id
    };
    let now = chrono::Utc::now();
    let link = shl_receive::parse(form.text, now).map_err(|r| shl_refused(&r))?;
    if !form.approved.contains(&link.host) {
        return Ok(Json(json!({
            "status": "needs_approval",
            "hosts": [{"host": link.host, "issuers": []}],
            "link": link_json(&link),
        })));
    }
    let route = issuer_route(rt.settings.dev_auth, rt.settings.dev_shc_issuers.as_deref());
    let held = if form.held.is_empty() {
        shl_receive::fetch(&link, form.passcode, &rt.blobs, chosen, &route)
            .await?
            .map_err(|r| shl_refused(&r))?
    } else {
        form.held
    };
    let opened = {
        let (link, held, blobs) = (link.clone(), held.clone(), rt.blobs.clone());
        tokio::task::spawn_blocking(move || shl_receive::open(&link, &held, &blobs, chosen))
            .await
            .map_err(|e| Error::Other(e.into()))?
            .map_err(stopped)?
    };
    let cards = match shl_receive::check(&opened, form.approved, &route, now).await {
        CardsChecked::NeedsApproval(asks) => {
            let mut v = asks_json(&asks);
            v["link"] = link_json(&link);
            v["held"] = json!(
                held.iter()
                    .map(|h| json!({"sha": h.sha, "content_type": h.content_type}))
                    .collect::<Vec<_>>()
            );
            return Ok(Json(v));
        }
        CardsChecked::Stopped(stop) => return Err(stopped(stop)),
        CardsChecked::Verified(cards) => cards,
    };
    let mut tx = rt.pool.begin().await?;
    let perm = caller
        .require(&mut tx, &chosen.to_string(), None, Some("import"))
        .await?;
    let got = shl_receive::store(&mut tx, &rt.blobs, &perm.profile, &link, &opened, cards, form.approved).await?;
    let mut audit = AuditEvent::new(caller.0.actor_kind(), "ui", "import");
    audit.actor_id = caller.0.user_id;
    audit.tool = Some("POST /api/shc".into());
    audit.profile_id = Some(perm.profile.id);
    audit.resource_type = Some("document".into());
    audit.resource_ids = Some(got.documents.iter().map(|d| d.document_id.to_string()).collect());
    write_audit(&mut tx, &audit).await?;
    tx.commit().await?;
    Ok(Json(link_imported_json(&link, &got)))
}

fn shl_refused(r: &shl_receive::Refusal) -> Fail {
    Fail::Api(
        ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, "shl_refused", r.to_string())
            .with("reason", json!(r.code()))
            .with("detail", json!(r.detail())),
    )
}

fn stopped(stop: Stop) -> Fail {
    match stop {
        Stop::Link(r) => shl_refused(&r),
        Stop::Card { refusal, iss } => refused(&refusal, iss),
    }
}

/// What the person sees of a link before they approve it. The label is the sender's text.
fn link_json(link: &Link) -> Value {
    json!({"host": link.host, "label": link.label, "expires": link.exp, "passcode": link.needs_passcode(),
        "direct": link.direct()})
}

fn link_imported_json(link: &Link, got: &shl_receive::Imported) -> Value {
    json!({
        "status": "imported",
        "link": link_json(link),
        "document_id": got.documents.first().map(|d| d.document_id),
        "created": got.documents.iter().any(|d| d.created),
        "identity": got.identity,
        "cards": got.cards.iter().map(card_json).collect::<Vec<_>>(),
        "summary": got.summary.to_value(),
        "documents": got.documents.iter().map(|d| json!({"document_id": d.document_id, "created": d.created,
            "content_type": d.content_type})).collect::<Vec<_>>(),
        "not_imported": got.not_imported.iter().map(|(t, r)| json!({"content_type": t, "reason": r}))
            .collect::<Vec<_>>(),
    })
}

fn refused(refusal: &Refusal, iss: Option<String>) -> Fail {
    Fail::Api(
        ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, "shc_refused", refusal.to_string())
            .with("reason", json!(refusal.code()))
            .with("iss", json!(iss))
            .with("detail", json!(refusal.detail())),
    )
}

fn asks_json(asks: &[Ask]) -> Value {
    json!({
        "status": "needs_approval",
        "hosts": asks.iter().map(|a| json!({"host": a.host, "issuers": a.issuers})).collect::<Vec<_>>(),
    })
}

fn imported_json(got: &Imported) -> Value {
    json!({
        "status": "imported",
        "document_id": got.document_id,
        "created": got.created,
        "identity": got.identity,
        "cards": got.cards.iter().map(card_json).collect::<Vec<_>>(),
        "summary": got.summary.to_value(),
    })
}

fn card_json(c: &shc::Card) -> Value {
    json!({
        "iss": c.iss, "host": c.host, "issued": c.issued, "expires": c.expires, "types": c.types,
        "name": c.name, "birth_date": c.birth_date,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const MAP: &str = "https://issuer.labnorte.example=http://127.0.0.1:1";

    /// The dev issuer map applies only with dev auth, and only to the issuer origin itself.
    #[test]
    fn the_dev_issuer_map_needs_dev_auth_and_matches_whole_origins() {
        let jwks = "https://issuer.labnorte.example/.well-known/jwks.json";
        let (url, egress) = issuer_route(false, Some(MAP))(jwks);
        assert_eq!(
            (url.as_str(), egress.allow_private),
            (jwks, false),
            "mapped without dev auth"
        );
        let (url, egress) = issuer_route(true, Some(MAP))(jwks);
        // a release build (`cargo test --release`) has no dev map
        if cfg!(debug_assertions) {
            assert_eq!(
                (url.as_str(), egress.allow_private, egress.https_only),
                ("http://127.0.0.1:1/.well-known/jwks.json", true, false)
            );
        } else {
            assert_eq!((url.as_str(), egress.allow_private), (jwks, false));
        }
        let evil = "https://issuer.labnorte.example.evil/.well-known/jwks.json";
        let (url, egress) = issuer_route(true, Some(MAP))(evil);
        assert_eq!(
            (url.as_str(), egress.allow_private),
            (evil, false),
            "a longer host matched the prefix"
        );
        let (url, _) = issuer_route(true, None)(jwks);
        assert_eq!(url, jwks);
    }

    #[test]
    fn an_imported_answer_says_whose_card_it_is() {
        let card = shc::Card {
            iss: "https://issuer.labnorte.example".into(),
            host: "issuer.labnorte.example".into(),
            jws_sha: "ab".into(),
            name: Some("Carmen Ortega".into()),
            birth_date: Some("1955-01-30".into()),
            kid: "k".into(),
            issued: chrono::DateTime::from_timestamp(1_790_000_000, 0).unwrap_or_default(),
            expires: None,
            types: vec!["https://smarthealth.cards#health-card".into()],
            bundle: json!({}),
        };
        let got = Imported {
            document_id: uuid::Uuid::nil(),
            created: true,
            cards: vec![card],
            summary: ownhealth_core::ingest::ImportSummary::new("d"),
            identity: "mismatch".into(),
        };
        let v = imported_json(&got);
        assert_eq!(v["identity"], "mismatch");
        assert_eq!(
            v["cards"][0],
            json!({"iss": "https://issuer.labnorte.example", "host": "issuer.labnorte.example",
                   "issued": "2026-09-21T14:13:20Z", "expires": null,
                   "types": ["https://smarthealth.cards#health-card"],
                   "name": "Carmen Ortega", "birth_date": "1955-01-30"})
        );
    }
}
