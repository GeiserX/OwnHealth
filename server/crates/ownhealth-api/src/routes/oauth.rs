//! Lane E (v0.2): what the UI needs for OAuth clients (`ownhealth_auth::oauth`). The protocol
//! endpoints themselves (`/.well-known/oauth-authorization-server`, `/oauth/authorize`,
//! `/oauth/token`) are `ownhealth_mcp::oauth`, outside `/api`.
//!
//! | Route | What |
//! |---|---|
//! | `GET /oauth/requests/{id}` | the pending authorization the consent screen shows: client name and CIMD URL, scopes (es/en), the caller's profiles |
//! | `POST /oauth/requests/{id}/approve` | `{scopes, profile_ids, default_profile?, identity_mode, days?}`; step-up; -> `{redirect}` |
//! | `POST /oauth/requests/{id}/deny` | -> `{redirect}` with `error=access_denied` |
//! | `GET /oauth/consents` | the caller's approved clients and their tokens |
//! | `DELETE /oauth/consents/{id}` | revoke (no step-up: it only narrows access) |

use axum::Router;
use axum::routing::{delete, get, post};
use ownhealth_core::{Error, Runtime};

use crate::Caller;
use crate::error::ApiError;

// Resolves the caller first, so an anonymous request gets 401 before the 501.
async fn not_yet(_caller: Caller) -> ApiError {
    Error::NotImplemented("oauth routes (lane E)").into()
}

pub fn router() -> Router<Runtime> {
    Router::new()
        .route("/oauth/requests/{id}", get(not_yet))
        .route("/oauth/requests/{id}/approve", post(not_yet))
        .route("/oauth/requests/{id}/deny", post(not_yet))
        .route("/oauth/consents", get(not_yet))
        .route("/oauth/consents/{id}", delete(not_yet))
}
