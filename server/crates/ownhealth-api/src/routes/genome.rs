//! Lane D: `api/genome.py`. `/profiles/{p}/genome`, `/genome/variant`, `/genome/consent`.
//!
//! Empty until lane D lands it; unknown /api paths answer 404 `not_found`.

use axum::Router;
use ownhealth_core::Runtime;

pub fn router() -> Router<Runtime> {
    Router::new()
}
