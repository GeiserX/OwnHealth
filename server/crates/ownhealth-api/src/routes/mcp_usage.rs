//! Lane F: `api/mcp_usage.py`. `GET /tokens/{id}/usage`.
//!
//! Empty until lane F lands it; unknown /api paths answer 404 `not_found`.

use axum::Router;
use ownhealth_core::Runtime;

pub fn router() -> Router<Runtime> {
    Router::new()
}
