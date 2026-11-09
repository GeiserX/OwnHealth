//! `ocr.document` (lane C), payload `{document_id, pages: [i32], why}` on the `ocr` queue
//! (`core::arrive::enqueue_ocr`). Per page: render at OCR_DPI (`ownhealth_ingest_pdf::render_page_png`
//! or the image itself), `engine::read_page`, store the words as the page's word layer (same JSON as
//! the text layer, `word_layer = 'ocr'`, one `ocr_run` row), the pHash (`core::dedup`), then run the
//! extractor chain on the OCR page model and route every row with its grounding. Pages OCR cannot read
//! still end as one `pending_document` item: nothing is dropped. `document.ocr_state` moves
//! queued -> running -> done | failed.

use ownhealth_core::arrive::OCR_JOB;
use ownhealth_core::error::Error;
use ownhealth_core::jobs::{self, Job, JobContext};

async fn run(_ctx: JobContext, _job: Job) -> jobs::HandlerResult {
    Err(Error::NotImplemented("ocr.document").into())
}

pub fn register() {
    jobs::register_handler(OCR_JOB, |ctx, job| Box::pin(run(ctx, job)));
}
