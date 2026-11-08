//! Lane I: `api/review.py`. `GET /review`, `/review/{id}` and its accept, reject, reopen, dismiss; `POST /review/bulk-accept`.
//!
//! Empty until lane I lands it; unknown /api paths answer 404 `not_found`.

use axum::Router;
use ownhealth_core::Runtime;

pub fn router() -> Router<Runtime> {
    Router::new()
}
