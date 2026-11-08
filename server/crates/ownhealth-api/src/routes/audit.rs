//! Lane G: `api/audit.py`. `GET /audit?profile=&limit=&before=`.
//!
//! Empty until lane G lands it; unknown /api paths answer 404 `not_found`.

use axum::Router;
use ownhealth_core::Runtime;

pub fn router() -> Router<Runtime> {
    Router::new()
}
