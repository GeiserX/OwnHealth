//! Lane A (v0.2): email routing over REST (`ownhealth_inbox::email`): the allowlist, PDF password
//! rules and the quarantine. Owner or guardian. Releasing a message and adding a sender widen what
//! flows in, so both need step-up; deleting does not.
//!
//! | Route | What |
//! |---|---|
//! | `GET /inboxes/quarantine?status=held` | `[{id, source_id, sender, subject: untrusted, attachments, reason, status, received_at}]` |
//! | `POST /inboxes/quarantine/{id}/release` | `{profile_id, allow_sender: bool}` -> `{document_ids}` |
//! | `DELETE /inboxes/quarantine/{id}` | delete a held message |
//! | `GET /inboxes/{id}/senders`, `POST /inboxes/{id}/senders` | sender rules of a mailbox |
//! | `DELETE /senders/{id}` | one rule |
//! | `POST /senders/{id}/pdf-passwords` | `{password, label?}` (sealed; never returned) |
//! | `DELETE /pdf-passwords/{id}` | one password rule |

use axum::Router;
use axum::routing::{delete, get, post};
use ownhealth_core::{Error, Runtime};

use crate::Caller;
use crate::error::ApiError;

// Resolves the caller first, so an anonymous request gets 401 before the 501.
async fn not_yet(_caller: Caller) -> ApiError {
    Error::NotImplemented("inbox routes (lane A)").into()
}

pub fn router() -> Router<Runtime> {
    Router::new()
        .route("/inboxes/quarantine", get(not_yet))
        .route("/inboxes/quarantine/{id}", delete(not_yet))
        .route("/inboxes/quarantine/{id}/release", post(not_yet))
        .route("/inboxes/{id}/senders", get(not_yet).post(not_yet))
        .route("/senders/{id}", delete(not_yet))
        .route("/senders/{id}/pdf-passwords", post(not_yet))
        .route("/pdf-passwords/{id}", delete(not_yet))
}
