//! Lane B: `api/documents.py`. `/profiles/{p}/documents`, `/documents/{id}` (file, page PNGs, words, assign, retract).
//!
//! Empty until lane B lands it; unknown /api paths answer 404 `not_found`.

use axum::Router;
use ownhealth_core::Runtime;

pub fn router() -> Router<Runtime> {
    Router::new()
}
