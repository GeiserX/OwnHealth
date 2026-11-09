//! Lane E (v0.2): the HTTP side of the built-in OAuth 2.1 authorization server (mcp.md 2.2,
//! security-privacy.md 5). The domain logic (codes, consents, tokens, the CIMD fetch) is
//! `ownhealth_auth::oauth`; these handlers speak the protocol:
//!
//! - `GET /.well-known/oauth-authorization-server` (RFC 8414): issuer = OWNHEALTH_BASE_URL,
//!   `authorization_endpoint`, `token_endpoint`, `code_challenge_methods_supported: ["S256"]`,
//!   `client_id_metadata_document_supported: true`, `authorization_response_iss_parameter_supported:
//!   true`, `scopes_supported`; no `registration_endpoint` (no DCR).
//! - `GET /oauth/authorize`: checks the client (CIMD), `redirect_uri`, `response_type=code`, PKCE S256,
//!   `resource` = our canonical `/mcp` URI (RFC 8707), then sends the browser to the UI consent screen.
//! - `POST /oauth/token`: `authorization_code` (PKCE verifier, same `resource`) and `refresh_token`;
//!   access tokens are `api_token` rows of kind `oauth` with `audience` set.
//!
//! The protected-resource metadata (`auth::protected_resource_metadata`) gains
//! `authorization_servers: [issuer]` in the same lane. Already merged into `router_with`; until lane E
//! lands, every route answers 501 `not_implemented`.

use axum::Router;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use ownhealth_core::Runtime;
use serde_json::json;

async fn not_yet() -> Response {
    (
        StatusCode::NOT_IMPLEMENTED,
        axum::Json(json!({"error": "temporarily_unavailable",
                          "error_description": "OAuth is not implemented yet in this build."})),
    )
        .into_response()
}

/// The authorization server routes (outside `/api`).
pub fn router() -> Router<Runtime> {
    Router::new()
        .route("/.well-known/oauth-authorization-server", get(not_yet))
        .route("/oauth/authorize", get(not_yet))
        .route("/oauth/token", post(not_yet))
}
