//! Lane E: `api/status.py`. `/profiles/{p}/status`, `/profiles/{p}/dashboard`, `GET /rules`.
//!
//! Empty until lane E lands it; unknown /api paths answer 404 `not_found`.

use axum::Router;
use ownhealth_core::Runtime;

pub fn router() -> Router<Runtime> {
    Router::new()
}
