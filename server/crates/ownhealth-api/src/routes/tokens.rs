//! Lane G: `api/tokens.py`. `GET /tokens`, `POST /tokens` (plaintext once), `DELETE /tokens/{id}`.
//!
//! Empty until lane G lands it; unknown /api paths answer 404 `not_found`.

use axum::Router;
use ownhealth_core::Runtime;

pub fn router() -> Router<Runtime> {
    Router::new()
}
