//! Lane A: `api/observations.py`. `/profiles/{p}/observations`, `/profiles/{p}/trend`, `POST /aliases`.
//!
//! Empty until lane A lands it; unknown /api paths answer 404 `not_found`.

use axum::Router;
use ownhealth_core::Runtime;

pub fn router() -> Router<Runtime> {
    Router::new()
}
