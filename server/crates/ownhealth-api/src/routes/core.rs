//! Lane I: `api/core.py`. `GET /me`, `GET /profiles`, `GET /sources/{src_id}`.
//!
//! Empty until lane I lands it; unknown /api paths answer 404 `not_found`.

use axum::Router;
use ownhealth_core::Runtime;

pub fn router() -> Router<Runtime> {
    Router::new()
}
