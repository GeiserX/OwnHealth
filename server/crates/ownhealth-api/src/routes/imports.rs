//! Lane I: `api/imports.py`. `POST /imports` (multipart `profile`, `files[]`) and `GET /jobs?ids=`.
//!
//! Empty until lane I lands it; unknown /api paths answer 404 `not_found`.

use axum::Router;
use ownhealth_core::Runtime;

pub fn router() -> Router<Runtime> {
    Router::new()
}
