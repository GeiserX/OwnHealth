//! SMART Health Cards receive (slice 22) over REST; the work is in `ownhealth_ingest_structured::shc`.
//!
//! ```text
//! POST /api/shc   multipart: profile, then text (pasted shc:/ text) or file (a .smart-health-card
//!                 file or a photo of the QR code), and one approve field per issuer host the
//!                 person said yes to
//! ```
//!
//! - 200 `{"status": "needs_approval", "issuers": [{"iss", "host"}]}`: nothing was fetched or stored.
//!   The person approves the hosts and the same form is sent again with `approve`.
//! - 200 `{"status": "imported", "document_id", "created", "cards": [{"iss", "issued", "expires",
//!   "types"}], "summary"}`.
//! - 422 `{"detail": {"code": "shc_refused", "reason", "message", "iss", "detail"}}`: nothing was
//!   stored. `reason` is [`shc::Refusal::code`], `detail` its date, host or missing part.

use axum::extract::{DefaultBodyLimit, FromRequest, Multipart, Request, State};
use axum::http::StatusCode;
use axum::routing::post;
use axum::{Json, Router};
use ownhealth_core::audit::{AuditEvent, write_audit};
use ownhealth_core::net::Egress;
use ownhealth_core::{Error, Runtime};
use ownhealth_ingest_structured::shc::{self, Outcome, Received};
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

/// Where an issuer's keys are fetched from: the issuer's own URL under the connector egress guard
/// ([`shc::public`]). A debug build that runs with dev auth may also map a test issuer origin to a
/// local server, `OWNHEALTH_DEV_SHC_ISSUERS=https://issuer.example=http://127.0.0.1:9177` (comma
/// separated), so the receiver can be tried end to end with an issuer we generate. A release build
/// never reads it.
fn issuer_route(dev_auth: bool) -> impl Fn(&str) -> (String, Egress) + Send + Sync {
    let local: Vec<(String, String)> = if cfg!(debug_assertions) && dev_auth {
        std::env::var("OWNHEALTH_DEV_SHC_ISSUERS")
            .unwrap_or_default()
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

    let mut tx = rt.pool.begin().await?;
    // the permission comes first, so a person who may not import never makes us fetch anything
    let perm = caller.require(&mut tx, &profile, None, Some("import")).await?;
    let read = {
        let bytes = bytes.clone();
        tokio::task::spawn_blocking(move || shc::read(&bytes))
            .await
            .map_err(|e| Error::Other(e.into()))?
    };
    let outcome = match read {
        Err(refusal) => Outcome::Refused { refusal, iss: None },
        Ok(input) => {
            let received = Received {
                bytes: &bytes,
                filename: filename.as_deref(),
                input: &input,
                approved: &approved,
            };
            let route = issuer_route(rt.settings.dev_auth);
            shc::receive(&mut tx, &rt.blobs, &perm.profile, &received, &route, chrono::Utc::now()).await?
        }
    };
    let out = match outcome {
        Outcome::NeedsApproval(issuers) => json!({
            "status": "needs_approval",
            "issuers": issuers.iter().map(|i| json!({"iss": i.iss, "host": i.host})).collect::<Vec<_>>(),
        }),
        Outcome::Refused { refusal, iss } => {
            return Err(Fail::Api(
                ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, "shc_refused", refusal.to_string())
                    .with("reason", json!(refusal.code()))
                    .with("iss", json!(iss))
                    .with("detail", json!(refusal.detail())),
            ));
        }
        Outcome::Imported(got) => {
            let mut audit = AuditEvent::new(principal.actor_kind(), "ui", "import");
            audit.actor_id = principal.user_id;
            audit.tool = Some("POST /api/shc".into());
            audit.profile_id = Some(perm.profile.id);
            audit.resource_type = Some("document".into());
            audit.resource_ids = Some(vec![got.document_id.to_string()]);
            write_audit(&mut tx, &audit).await?;
            tx.commit().await?;
            json!({
                "status": "imported",
                "document_id": got.document_id,
                "created": got.created,
                "cards": got.cards.iter().map(|c| json!({
                    "iss": c.iss, "issued": c.issued, "expires": c.expires, "types": c.types,
                })).collect::<Vec<_>>(),
                "summary": got.summary.to_value(),
            })
        }
    };
    Ok(Json(out))
}
