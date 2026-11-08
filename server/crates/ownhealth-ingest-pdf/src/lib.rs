//! Lane B: lab PDFs (`ingest/pdf/`, ingestion.md 5-7, 9). pdfium (see SPIKES.md) reads the text
//! layer; vendor templates are read unchanged from `backend/src/ownhealth/ingest/pdf/vendors/`.
//! Review kinds it files: `candidate_row`, `amended_report`, `document_identity`, `pending_document`.
//!
//! Every function below is a stub until lane B lands it: it returns `Error::NotImplemented`.

use futures::future::BoxFuture;
use ownhealth_core::blobs::BlobStore;
use ownhealth_core::ingest::ImportSummary;
use ownhealth_core::jobs::{self, Job, JobContext, JobError};
use ownhealth_core::{Error, Result};
use serde::{Deserialize, Serialize};
use sqlx::PgConnection;
use uuid::Uuid;

/// One word of a page's text layer. bbox is `[x0, top, x1, bottom]` in PDF points, origin top-left.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Word {
    pub text: String,
    pub bbox: [f64; 4],
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub font: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bold: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hidden: Option<bool>,
}

pub async fn import_pdf(
    _conn: &mut PgConnection,
    _blobs: &BlobStore,
    _document_id: Uuid,
    _force: bool,
) -> Result<ImportSummary> {
    Err(Error::NotImplemented("lane B: ingest_pdf::import_pdf"))
}

pub async fn page_words(
    _conn: &mut PgConnection,
    _blobs: &BlobStore,
    _document_id: Uuid,
    _page_no: i32,
) -> Result<Vec<Word>> {
    Err(Error::NotImplemented("lane B: ingest_pdf::page_words"))
}

/// A PNG of one page (CPU work: call it from `spawn_blocking`).
pub fn render_page_png(_blobs: &BlobStore, _blob_sha256: &str, _page_no: i32, _scale: f64) -> Result<Vec<u8>> {
    Err(Error::NotImplemented("lane B: ingest_pdf::render_page_png"))
}

/// Where `name` and `value` are printed together on the page, or None.
pub async fn ground_value(
    _conn: &mut PgConnection,
    _blobs: &BlobStore,
    _document_id: Uuid,
    _page_no: i32,
    _name: &str,
    _value: &str,
) -> Result<Option<[f64; 4]>> {
    Err(Error::NotImplemented("lane B: ingest_pdf::ground_value"))
}

fn import_pdf_job(_ctx: JobContext, _job: Job) -> BoxFuture<'static, jobs::HandlerResult> {
    Box::pin(async { Err(JobError::from(Error::NotImplemented("lane B: job import.pdf"))) })
}

/// Register lane B's job handler (`import.pdf`), accept handlers and demo hook (order 200).
pub fn register() {
    jobs::register_handler("import.pdf", import_pdf_job);
}
