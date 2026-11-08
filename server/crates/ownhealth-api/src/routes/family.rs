//! Lane G: `api/family.py`. Household, grants, locks, sensitive switches, relatives, invites, deletions.
//!
//! Empty until lane G lands it; unknown /api paths answer 404 `not_found`.

use axum::Router;
use ownhealth_core::Runtime;

pub fn router() -> Router<Runtime> {
    Router::new()
}
