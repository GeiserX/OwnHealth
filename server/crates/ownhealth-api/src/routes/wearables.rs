//! Lane C: `api/wearables.py`. `/profiles/{p}/wearables/summary`, `/profiles/{p}/wearables/{metric}`.
//!
//! Empty until lane C lands it; unknown /api paths answer 404 `not_found`.

use axum::Router;
use ownhealth_core::Runtime;

pub fn router() -> Router<Runtime> {
    Router::new()
}
