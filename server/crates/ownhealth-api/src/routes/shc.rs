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

use axum::extract::{DefaultBodyLimit, FromRequest, Multipart, Request, State};
use axum::http::StatusCode;
use axum::routing::post;
use axum::{Json, Router};
use ownhealth_core::audit::{AuditEvent, write_audit};
use ownhealth_core::net::Egress;
use ownhealth_core::{Error, Runtime};
use ownhealth_ingest_structured::shc::{self, Ask, Checked, Imported, Received, Refusal};
use serde_json::{Value, json};

use crate::Caller;
use crate::error::ApiError;
use crate::routes::auth::{Fail, R};

/// A photo of a QR code, a card file or pasted text: 20 MiB is a large phone photo.
const MAX_BODY: usize = 20 * 1024 * 1024;
/// A text field (the profile, a host, pasted text) holds at most 1 MiB.
const MAX_FIELD: usize = 1024 * 1024;
/// At most this many `approve` fields.
const MAX_APPROVED: usize = shc::MAX_CARDS;

pub fn router() -> Router<Runtime> {
    Router::new().route("/shc", post(receive).layer(DefaultBodyLimit::max(MAX_BODY)))
}

fn bad(message: &str) -> Fail {
    Fail::Api(ApiError::new(StatusCode::BAD_REQUEST, "bad_request", message))
}

/// The dev issuer map, `OWNHEALTH_DEV_SHC_ISSUERS=https://issuer.example=http://127.0.0.1:9177`
/// (comma separated). Only a debug build has it.
#[cfg(debug_assertions)]
fn dev_map() -> Option<String> {
    std::env::var("OWNHEALTH_DEV_SHC_ISSUERS").ok()
}

/// A release build has no dev issuer map.
#[cfg(not(debug_assertions))]
fn dev_map() -> Option<String> {
    None
}

/// Where an issuer's keys are fetched from: the issuer's own URL under the connector egress guard
/// ([`shc::public`]). A debug build that runs with dev auth may also map a test issuer origin to a
/// local server ([`dev_map`]), so the receiver can be tried end to end with an issuer we generate.
/// Only a whole origin matches: the mapped prefix must be followed by `/`.
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
    let (filename, bytes) = match (text, file) {
        (Some(text), None) => (None, text.into_bytes()),
        (None, Some(file)) => file,
        _ => return Err(bad("Send either text or file.")),
    };

    // the permission comes first, so a person who may not import never makes us fetch anything; the
    // connection goes back to the pool before any issuer is contacted
    {
        let mut conn = rt.pool.acquire().await?;
        caller.require(&mut conn, &profile, None, Some("import")).await?;
    }
    let read = {
        let bytes = bytes.clone();
        tokio::task::spawn_blocking(move || shc::read(&bytes))
            .await
            .map_err(|e| Error::Other(e.into()))?
    };
    let input = read.map_err(|refusal| refused(&refusal, None))?;
    let map = dev_map();
    let route = issuer_route(rt.settings.dev_auth, map.as_deref());
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
    // checked again in the transaction that stores: the grant may have changed during the fetches
    let perm = caller.require(&mut tx, &profile, None, Some("import")).await?;
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
        "cards": got.cards.iter().map(|c| json!({
            "iss": c.iss, "host": c.host, "issued": c.issued, "expires": c.expires, "types": c.types,
            "name": c.name, "birth_date": c.birth_date,
        })).collect::<Vec<_>>(),
        "summary": got.summary.to_value(),
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
        assert_eq!(
            (url.as_str(), egress.allow_private, egress.https_only),
            ("http://127.0.0.1:1/.well-known/jwks.json", true, false)
        );
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
