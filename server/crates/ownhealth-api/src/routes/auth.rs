//! Lane G: `api/auth.py`. Passkeys, passwords, recovery, OIDC, invites, step-up, logout (`/auth/*`).
//!
//! Empty until lane G lands it; unknown /api paths answer 404 `not_found`.

use axum::Router;
use ownhealth_core::Runtime;

pub fn router() -> Router<Runtime> {
    Router::new()
}
