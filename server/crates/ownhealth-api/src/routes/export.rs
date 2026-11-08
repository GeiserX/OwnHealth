//! Lane G: `api/export.py`. `POST /profiles/{p}/export` (step-up; answers the zip).
//!
//! Empty until lane G lands it; unknown /api paths answer 404 `not_found`.

use axum::Router;
use ownhealth_core::Runtime;

pub fn router() -> Router<Runtime> {
    Router::new()
}
