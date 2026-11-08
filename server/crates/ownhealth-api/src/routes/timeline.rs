//! Lane I: `api/timeline.py`. `GET /profiles/{p}/timeline?types=&from=&to=&lang=` (owned by lane H in Python; I here).
//!
//! Empty until lane I lands it; unknown /api paths answer 404 `not_found`.

use axum::Router;
use ownhealth_core::Runtime;

pub fn router() -> Router<Runtime> {
    Router::new()
}
