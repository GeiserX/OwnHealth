//! Lane G (v0.2): the source registry over REST (`core::sources`) and the health endpoint per source
//! (ROADMAP v0.2 Ops: "last successful sync" for alerting). Owner or guardian of the household; the
//! secrets inside a source are write-only (never returned).
//!
//! | Route | What |
//! |---|---|
//! | `GET /inboxes` | the household's sources with their health |
//! | `POST /inboxes` | `{kind, label, profile_id?, config, secret?, poll_seconds?}`; a secret is sealed (`core::secrets`) |
//! | `GET /inboxes/{id}`, `PATCH /inboxes/{id}`, `DELETE /inboxes/{id}` | one source |
//! | `GET /inboxes/{id}/runs?limit=` | recent runs and their errors |
//! | `POST /inboxes/{id}/token` | a new push token, shown once (step-up) |
//! | `POST /inboxes/{id}/sync` | poll now |
//! | `GET /inboxes/health` | `[{id, kind, label, enabled, last_success_at, last_error_at, last_error, consecutive_failures, stale}]` |

use axum::Router;
use axum::routing::{get, post};
use ownhealth_core::{Error, Runtime};

use crate::Caller;
use crate::error::ApiError;

// Resolves the caller first, so an anonymous request gets 401 before the 501.
async fn not_yet(_caller: Caller) -> ApiError {
    Error::NotImplemented("inbox registry routes (lane G)").into()
}

pub fn router() -> Router<Runtime> {
    Router::new()
        .route("/inboxes", get(not_yet).post(not_yet))
        .route("/inboxes/health", get(not_yet))
        .route("/inboxes/{id}", get(not_yet).patch(not_yet).delete(not_yet))
        .route("/inboxes/{id}/runs", get(not_yet))
        .route("/inboxes/{id}/token", post(not_yet))
        .route("/inboxes/{id}/sync", post(not_yet))
}
