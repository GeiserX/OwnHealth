//! Lane E: `api/drug_check.py`. `GET /profiles/{p}/drug-check?drug=&lang=`.
//!
//! Empty until lane E lands it; unknown /api paths answer 404 `not_found`.

use axum::Router;
use ownhealth_core::Runtime;

pub fn router() -> Router<Runtime> {
    Router::new()
}
