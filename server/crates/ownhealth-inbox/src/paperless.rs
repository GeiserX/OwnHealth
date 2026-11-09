//! Lane B: Paperless-ngx (ingestion.md 3.1). The person's Paperless workflow ("Document Added", tag
//! `health`) posts `{"doc_url": "{{doc_url}}"}` to `POST /api/ingest/paperless` with the source's push
//! token in `X-OwnHealth-Secret`. We parse the id from `doc_url`, never trust the body otherwise, and
//! pull `/api/documents/<id>/download/?original=true` with the source's API token (secret purpose
//! `paperless_token`) through `core::net::get` (`Egress::home_service`). No usable text layer: also
//! pull the archived PDF and treat its layer as OCR (`word_layer = 'paperless_ocr'`). The Paperless id
//! is `source_ref` and the per-source dedup key; we tag the document `ownhealth-imported` in
//! Paperless. Polling fallback every 15 min: `tags__id__in=<health>&modified__gt=<cursor>`.
//!
//! Source `config` (kind `paperless`): `{base_url, health_tag, allow_private?: bool,
//! profile_by_tag?: {tag: profile_id}}`; `source.profile_id` is the default profile.

use ownhealth_core::Runtime;
use ownhealth_core::arrive::Arrived;
use ownhealth_core::error::{Error, Result};
use ownhealth_core::jobs::{JobContext, JobError};
use ownhealth_core::sources::{RunOutcome, Source};
use serde_json::Value;

pub const DEFAULT_POLL_SECONDS: i32 = 900;
pub const IMPORTED_TAG: &str = "ownhealth-imported";

/// The Paperless document id in a `doc_url` (`https://paperless.example/documents/123/details`).
pub fn document_id_from_url(_doc_url: &str) -> Option<i64> {
    None
}

/// The webhook body of one "Document Added": pull the original and arrive it.
pub async fn handle_webhook(_rt: &Runtime, _source: &Source, _body: &Value) -> Result<Arrived> {
    Err(Error::NotImplemented("paperless::handle_webhook"))
}

/// The `paperless` poller (the fallback when Paperless cannot reach us).
pub async fn poll_paperless(_ctx: JobContext, _source: Source) -> std::result::Result<RunOutcome, JobError> {
    Err(Error::NotImplemented("paperless::poll_paperless").into())
}

pub fn register() {
    ownhealth_core::sources::register_poller("paperless", |ctx, source| Box::pin(poll_paperless(ctx, source)));
}
