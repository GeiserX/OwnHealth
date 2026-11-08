//! Lane C: `api/records.py`. `/profiles/{p}/records/{kind}` (list, add, correct, retract) and manual observations.
//!
//! Empty until lane C lands it; unknown /api paths answer 404 `not_found`.

use axum::Router;
use ownhealth_core::Runtime;

pub fn router() -> Router<Runtime> {
    Router::new()
}
