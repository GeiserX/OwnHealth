//! Lane B (v0.2): the push inboxes over REST (`ownhealth_inbox::{paperless, share}`).
//!
//! | Route | Auth | What |
//! |---|---|---|
//! | `POST /ingest/paperless` | `X-OwnHealth-Secret: oh_ing_...` of a `paperless` source | `{"doc_url": ...}` -> `{"outcome": ...}` |
//! | `POST /ingest/share` | session (the PWA share target, Android) | multipart `files`, `profile` -> `{"items": [...]}` |
//! | `POST /ingest/shortcut` | `Authorization: Bearer oh_ing_...` of a `shortcut` source | multipart `file` (the iOS Shortcut) |
//!
//! ingestion.md 3 names `/api/ingest/upload` for the Shortcut; that route takes sessions only, so the
//! token-bound upload has its own path (RUST-CONTRACTS.md section 10).

use axum::Router;
use axum::routing::post;
use ownhealth_core::{Error, Runtime};

use crate::error::ApiError;
use crate::{Caller, SourceCaller};

// Each stub authenticates as its route will (401 first), then answers 501.
async fn not_yet(_caller: Caller) -> ApiError {
    Error::NotImplemented("ingest routes (lane B)").into()
}

async fn not_yet_push(_source: SourceCaller) -> ApiError {
    Error::NotImplemented("ingest routes (lane B)").into()
}

pub fn router() -> Router<Runtime> {
    Router::new()
        .route("/ingest/paperless", post(not_yet_push))
        .route("/ingest/share", post(not_yet))
        .route("/ingest/shortcut", post(not_yet_push))
}
