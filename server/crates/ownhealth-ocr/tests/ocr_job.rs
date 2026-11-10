//! `ocr.document` end to end with a stand-in reader: the page words come from the slip's own text
//! layer (what a perfect OCR would read), so these tests pin the routing, not the engine. The engine
//! itself is measured by `exit_criteria.rs` (feature `engine`) and the benchmark.
mod eval;
#[path = "../../ownhealth-ingest-pdf/tests/support/mod.rs"]
mod support;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use ownhealth_core::acceptance::DEFAULT_THRESHOLD;
use ownhealth_ingest_pdf::textlayer::read_pdf;
use ownhealth_ingest_pdf::{ImportOptions, import_pdf_opts};
use ownhealth_ocr::{ENGINE, OcrError, OcrPage, OcrWord, Reader, use_reader};
use serde_json::Value;
use sqlx::PgConnection;
use support::env::{Env, env, fake_normalizer};
use support::generate;
use uuid::Uuid;

static TURN: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// A reader that "reads" the words of `pdf`'s text layer, page by page, at confidence `conf`.
fn text_reader(pdf: &[u8], conf: f32, calls: Arc<AtomicUsize>) -> Reader {
    let model = read_pdf(pdf).expect("a PDF");
    let pages: Vec<OcrPage> = model
        .pages
        .iter()
        .map(|p| OcrPage {
            page_no: p.page_no,
            width_pt: p.width,
            height_pt: p.height,
            words: p
                .visible()
                .iter()
                .map(|w| OcrWord {
                    text: w.text.clone(),
                    bbox: w.bbox(),
                    conf,
                })
                .collect(),
            engine: ENGINE.into(),
            engine_version: "test".into(),
            seconds: 0.0,
        })
        .collect();
    Arc::new(move |_rgb, _w, _h, _dpi, page_no| {
        calls.fetch_add(1, Ordering::SeqCst);
        pages
            .iter()
            .find(|p| p.page_no == page_no)
            .cloned()
            .ok_or_else(|| OcrError::Engine(format!("no page {page_no}")))
    })
}

fn reader_on(pdf: &[u8], conf: f32) -> Arc<AtomicUsize> {
    let calls = Arc::new(AtomicUsize::new(0));
    use_reader(Some((text_reader(pdf, conf, calls.clone()), "test-models".into())));
    calls
}

async fn setup() -> anyhow::Result<Env> {
    let e = env().await?;
    ownhealth_ocr::register();
    fake_normalizer();
    Ok(e)
}

/// Store `data` for `person`, run `import.pdf` with the `ocr` profile on, then every queued OCR job.
async fn import_with_ocr(e: &Env, c: &mut PgConnection, data: &[u8], mime: &str, person: &str) -> anyhow::Result<Uuid> {
    let ext = if mime == "application/pdf" { "pdf" } else { "jpg" };
    let doc = e.store(c, data, person, mime, &format!("scan.{ext}")).await?;
    let summary = import_pdf_opts(
        &mut *c,
        &e.blobs,
        doc,
        false,
        ImportOptions {
            threshold: DEFAULT_THRESHOLD,
            ocr: true,
        },
    )
    .await?;
    assert_eq!(summary.reasons.get("ocr_queued"), Some(&1), "{summary:?}");
    assert_eq!(state(c, doc).await?, "queued");
    assert_eq!(
        open_pending(c, doc).await?,
        0,
        "a queued scan files no pending_document"
    );
    run_ocr_queue(e).await?;
    Ok(doc)
}

async fn run_ocr_queue(e: &Env) -> anyhow::Result<()> {
    let ctx = e.job_context();
    let kinds = [ownhealth_core::arrive::OCR_JOB.to_string()];
    while let Some(job) = ownhealth_core::jobs::run_one(&ctx, "ocr", Some(&kinds)).await? {
        let (status, error): (String, Option<String>) =
            sqlx::query_as("select state, last_error from job where id = $1")
                .bind(job.id)
                .fetch_one(&e.db.pool)
                .await?;
        assert_eq!(status, "done", "ocr.document job {} ended {status}: {error:?}", job.id);
    }
    Ok(())
}

async fn state(c: &mut PgConnection, doc: Uuid) -> anyhow::Result<String> {
    Ok(sqlx::query_scalar("select ocr_state from document where id = $1")
        .bind(doc)
        .fetch_one(c)
        .await?)
}

async fn open_pending(c: &mut PgConnection, doc: Uuid) -> anyhow::Result<i64> {
    Ok(sqlx::query_scalar(
        "select count(*) from review_item where document_id = $1 and kind = 'pending_document' and status = 'open'",
    )
    .bind(doc)
    .fetch_one(c)
    .await?)
}

fn scan(pdf: &[u8]) -> Vec<u8> {
    // the stand-in reader ignores the pixels: a coarse raster keeps the test fast
    generate::scanned(pdf, 36.0)
}

#[tokio::test]
async fn every_row_of_a_scan_gets_exactly_one_outcome() -> anyhow::Result<()> {
    let _turn = TURN.lock().await;
    let e = setup().await?;
    let mut c = e.conn().await?;
    let ln = generate::labnorte_3p();
    let (pdf, truth) = generate::render(&ln, None);
    let calls = reader_on(&pdf, 0.995);
    let doc = import_with_ocr(&e, &mut c, &scan(&pdf), "application/pdf", "alex").await?;
    assert_eq!(calls.load(Ordering::SeqCst), 3);

    let o = eval::outcomes(&mut c, "labnorte", doc, &truth).await?;
    assert!(o.truth >= 30, "{o:?}");
    assert!(
        o.dropped.is_empty() && o.duplicated.is_empty() && o.pending.is_empty(),
        "{o:?}"
    );
    assert_eq!(o.accepted + o.review, o.truth, "{o:?}");
    assert_eq!(o.tp, o.truth, "a perfect reading finds every row: {o:?}");
    assert!(o.false_auto.is_empty(), "{o:?}");
    // a matched template and a sure recognizer: OCR rows can auto-accept (at 0.98, not 0.95)
    assert!(o.accepted > 0, "{o:?}");

    assert_eq!(state(&mut c, doc).await?, "done");
    assert_eq!(open_pending(&mut c, doc).await?, 0);
    let pages: Vec<(i32, String, bool)> = sqlx::query_as(
        "select page_no, word_layer, text_layer_ok from document_page where document_id = $1 order by page_no",
    )
    .bind(doc)
    .fetch_all(&mut *c)
    .await?;
    assert_eq!(pages, [1, 2, 3].map(|n| (n, "ocr".to_string(), false)));
    let runs: Vec<(String, i32, String)> =
        sqlx::query_as("select status, word_count, models_sha256 from ocr_run where document_id = $1 order by page_no")
            .bind(doc)
            .fetch_all(&mut *c)
            .await?;
    assert_eq!(runs.len(), 3);
    assert!(
        runs.iter()
            .all(|(s, n, m)| s == "done" && *n > 20 && m == "test-models"),
        "{runs:?}"
    );
    let layers: Vec<(Option<String>, Option<String>)> = sqlx::query_as(
        "select c.word_layer, c.grounding from candidate_row c join extraction_run r on r.id = c.extraction_run_id \
         where r.document_id = $1",
    )
    .bind(doc)
    .fetch_all(&mut *c)
    .await?;
    assert_eq!(layers.len(), o.truth);
    assert!(
        layers
            .iter()
            .all(|(l, g)| l.as_deref() == Some("ocr") && g.as_deref() == Some("grounded")),
        "{layers:?}"
    );
    let engine: String = sqlx::query_scalar("select engine from extraction_run where document_id = $1")
        .bind(doc)
        .fetch_one(&mut *c)
        .await?;
    assert_eq!(engine, ENGINE);

    // the job again (a retry, a second enqueue): nothing is read or routed twice
    let again = ownhealth_ocr::ocr_document(&e.job_context(), doc, &[1, 2, 3]).await?;
    assert_eq!(again["pages_read"], 0, "{again}");
    assert_eq!(calls.load(Ordering::SeqCst), 3, "no page is read twice");
    assert_eq!(state(&mut c, doc).await?, "done");
    let o2 = eval::outcomes(&mut c, "labnorte", doc, &truth).await?;
    assert_eq!((o2.extracted, o2.duplicated.len()), (o.extracted, 0));
    use_reader(None);
    Ok(())
}

#[tokio::test]
async fn an_unsure_recognizer_sends_every_row_to_review() -> anyhow::Result<()> {
    let _turn = TURN.lock().await;
    let e = setup().await?;
    let mut c = e.conn().await?;
    let ln = generate::labnorte_3p();
    let (pdf, truth) = generate::render(&ln, None);
    reader_on(&pdf, 0.62);
    let doc = import_with_ocr(&e, &mut c, &scan(&pdf), "application/pdf", "alex").await?;
    let o = eval::outcomes(&mut c, "labnorte", doc, &truth).await?;
    assert_eq!((o.accepted, o.review), (0, o.truth), "{o:?}");
    let confs: Vec<f32> = sqlx::query_scalar(
        "select c.row_conf from candidate_row c join extraction_run r on r.id = c.extraction_run_id \
         where r.document_id = $1",
    )
    .bind(doc)
    .fetch_all(&mut *c)
    .await?;
    assert!(
        !confs.is_empty() && confs.iter().all(|c| *c <= 0.62 + 1e-6),
        "{confs:?}"
    );
    use_reader(None);
    Ok(())
}

#[tokio::test]
async fn a_photo_is_read_like_a_scan() -> anyhow::Result<()> {
    let _turn = TURN.lock().await;
    let e = setup().await?;
    let mut c = e.conn().await?;
    let lay = generate::layouts().remove(0);
    let (pdf, truth) = generate::render(&lay, None);
    reader_on(&pdf, 0.99);
    let page = eval::render_pages(&pdf, 50.0).remove(0);
    let mut jpeg = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg, 80).encode_image(&page)?;
    let person = truth["person"].as_str().unwrap_or("alex");
    let doc = import_with_ocr(&e, &mut c, &jpeg, "image/jpeg", person).await?;
    let o = eval::outcomes(&mut c, "photo", doc, &truth).await?;
    assert!(o.truth > 5, "{o:?}");
    assert!(
        o.dropped.is_empty() && o.duplicated.is_empty() && o.pending.is_empty(),
        "{o:?}"
    );
    assert_eq!(o.tp, o.truth, "{o:?}");
    // the image's own classification came from the OCR words
    let doc_type: Option<String> = sqlx::query_scalar("select doc_type from document where id = $1")
        .bind(doc)
        .fetch_one(&mut *c)
        .await?;
    assert_eq!(doc_type.as_deref(), Some("lab_report"));
    use_reader(None);
    Ok(())
}

#[tokio::test]
async fn a_photo_gets_a_page_fingerprint_that_matches_the_pdf_it_shows() -> anyhow::Result<()> {
    let _turn = TURN.lock().await;
    let e = setup().await?;
    let mut c = e.conn().await?;
    let lay = generate::layouts().remove(0);
    let (pdf, truth) = generate::render(&lay, None);
    let person = truth["person"].as_str().unwrap_or("alex");
    let original = e.store(&mut c, &pdf, person, "application/pdf", "informe.pdf").await?;
    ownhealth_ingest_pdf::import_pdf(&mut c, &e.blobs, original, false).await?;

    // the phone's photo of the same page: the image is hashed as its one page (dedup level 2)
    reader_on(&pdf, 0.99);
    let page = eval::render_pages(&pdf, 50.0).remove(0);
    let mut jpeg = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg, 80).encode_image(&page)?;
    let photo = import_with_ocr(&e, &mut c, &jpeg, "image/jpeg", person).await?;
    use_reader(None);
    let hash = |doc: Uuid| {
        sqlx::query_scalar::<_, Option<Vec<u8>>>(
            "select phash from document_page where document_id = $1 and page_no = 1",
        )
        .bind(doc)
    };
    let mine = hash(photo).fetch_one(&mut *c).await?;
    let theirs = hash(original).fetch_one(&mut *c).await?;
    let bits = |h: &Option<Vec<u8>>| {
        h.as_deref()
            .and_then(|b| <[u8; 8]>::try_from(b).ok())
            .map(u64::from_be_bytes)
    };
    let (Some(a), Some(b)) = (bits(&mine), bits(&theirs)) else {
        anyhow::bail!("no page fingerprint: photo {mine:?}, pdf {theirs:?}");
    };
    assert!((a ^ b).count_ones() <= 6, "pHash distance {}", (a ^ b).count_ones());
    // the same report with its sample id: linked as an edition, which wins over "probably the same"
    let (near, supersedes): (Option<Uuid>, Option<Uuid>) =
        sqlx::query_as("select near_duplicate_of, supersedes_document from document where id = $1")
            .bind(photo)
            .fetch_one(&mut *c)
            .await?;
    assert!(
        near == Some(original) || supersedes == Some(original),
        "{near:?} {supersedes:?}"
    );
    Ok(())
}

#[tokio::test]
async fn without_an_engine_the_scan_waits_for_an_agent() -> anyhow::Result<()> {
    let _turn = TURN.lock().await;
    use_reader(None);
    let e = setup().await?;
    let mut c = e.conn().await?;
    let ln = generate::labnorte_3p();
    let (pdf, _) = generate::render(&ln, None);
    // the test settings point the models at a directory that does not exist
    let doc = import_with_ocr(&e, &mut c, &scan(&pdf), "application/pdf", "alex").await?;
    assert_eq!(state(&mut c, doc).await?, "failed");
    let items: Vec<Value> = sqlx::query_scalar(
        "select payload from review_item where document_id = $1 and kind = 'pending_document' and status = 'open'",
    )
    .bind(doc)
    .fetch_all(&mut *c)
    .await?;
    assert_eq!(items.len(), 1, "{items:?}");
    assert_eq!(items[0]["why"], "ocr_failed");
    assert_eq!(items[0]["pages"], serde_json::json!([1, 2, 3]));
    let errors: Vec<Option<String>> =
        sqlx::query_scalar("select error from ocr_run where document_id = $1 and status = 'failed'")
            .bind(doc)
            .fetch_all(&mut *c)
            .await?;
    assert_eq!(errors.len(), 3);
    assert!(
        errors.iter().all(|e| e
            .as_deref()
            .is_some_and(|e| e.contains("OCR engine") || e.contains("model"))),
        "{errors:?}"
    );
    let rows: i64 = sqlx::query_scalar(
        "select count(*) from candidate_row c join extraction_run r on r.id = c.extraction_run_id where r.document_id = $1",
    )
    .bind(doc)
    .fetch_one(&mut *c)
    .await?;
    assert_eq!(rows, 0);
    Ok(())
}

#[tokio::test]
async fn a_good_text_layer_is_never_read_again() -> anyhow::Result<()> {
    let _turn = TURN.lock().await;
    let e = setup().await?;
    let mut c = e.conn().await?;
    let lay = generate::layouts().remove(0);
    let (pdf, _) = generate::render(&lay, None);
    let calls = reader_on(&pdf, 0.99);
    let doc = e.store_pdf(&mut c, &pdf, "slip.pdf").await?;
    ownhealth_ingest_pdf::import_pdf(&mut c, &e.blobs, doc, false).await?;
    let before: i64 = sqlx::query_scalar("select count(*) from candidate_row")
        .fetch_one(&mut *c)
        .await?;
    // a stray job naming a page with a good text layer
    let out = ownhealth_ocr::ocr_document(&e.job_context(), doc, &[1]).await?;
    assert_eq!(out["pages_read"], 0);
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    let after: i64 = sqlx::query_scalar("select count(*) from candidate_row")
        .fetch_one(&mut *c)
        .await?;
    assert_eq!(before, after);
    let layer: String = sqlx::query_scalar("select word_layer from document_page where document_id = $1")
        .bind(doc)
        .fetch_one(&mut *c)
        .await?;
    assert_eq!(layer, "text_layer");
    use_reader(None);
    Ok(())
}

#[tokio::test]
async fn instructions_hidden_in_the_image_never_become_values() -> anyhow::Result<()> {
    let _turn = TURN.lock().await;
    let e = setup().await?;
    let mut c = e.conn().await?;
    let (_, pdf, _) = eval::ocr_injection_fixture();
    reader_on(&pdf, 0.99);
    let doc = import_with_ocr(&e, &mut c, &scan(&pdf), "application/pdf", "alex").await?;
    eval::assert_injection_contained(&mut c, doc).await?;
    use_reader(None);
    Ok(())
}

#[tokio::test]
async fn the_demo_has_one_scanned_page_waiting_for_ocr() -> anyhow::Result<()> {
    let _turn = TURN.lock().await;
    let e = setup().await?;
    let mut c = e.conn().await?;
    let lay = generate::layouts().remove(0);
    let (pdf, _) = generate::render(&lay, None);
    let report = e.store(&mut c, &pdf, "lucia", "application/pdf", "lucia.pdf").await?;
    for ocr in [false, true] {
        let mut settings = e.settings.clone();
        settings.ocr = ocr;
        let mut ctx = ownhealth_core::demo::DemoContext {
            settings,
            blobs: e.blobs.clone(),
            family: e.fam.clone(),
            as_of: chrono::NaiveDate::from_ymd_opt(2026, 9, 30).expect("a date"),
            documents: [("lucia:2026-09-14".to_string(), report)].into_iter().collect(),
            src_ids: Default::default(),
        };
        ownhealth_ocr::demo::seed_demo(&mut c, &mut ctx).await?;
        let (doc, state): (Uuid, String) = sqlx::query_as(
            "select id, ocr_state from document where mime = 'image/png' and profile_id = $1 order by created_at desc limit 1",
        )
        .bind(e.fam.profile("lucia"))
        .fetch_one(&mut *c)
        .await?;
        if ocr {
            assert_eq!(state, "queued");
            let queued: i64 = sqlx::query_scalar("select count(*) from job where queue = 'ocr' and state = 'queued'")
                .fetch_one(&mut *c)
                .await?;
            assert_eq!(queued, 1);
        } else {
            assert_eq!(state, "off");
            assert_eq!(open_pending(&mut c, doc).await?, 1);
        }
        sqlx::query("delete from review_item where document_id = $1")
            .bind(doc)
            .execute(&mut *c)
            .await?;
        sqlx::query("delete from document_source where document_id = $1")
            .bind(doc)
            .execute(&mut *c)
            .await?;
        sqlx::query("delete from document where id = $1")
            .bind(doc)
            .execute(&mut *c)
            .await?;
    }
    Ok(())
}

#[tokio::test]
async fn turning_ocr_on_later_reads_the_scans_that_waited() -> anyhow::Result<()> {
    let _turn = TURN.lock().await;
    let e = setup().await?;
    let mut c = e.conn().await?;
    let ln = generate::labnorte_3p();
    let (pdf, _) = generate::render(&ln, None);
    let calls = reader_on(&pdf, 0.99);
    // two scans that arrived while OCR was off: one imported by 0.3.0 (`off`), one by 0.2.0, whose
    // documents the migration left at `not_needed`
    let mut docs = Vec::new();
    for (name, dpi) in [("scan-a.pdf", 36.0), ("scan-b.pdf", 40.0)] {
        let bytes = generate::scanned(&pdf, dpi);
        let doc = e.store(&mut c, &bytes, "alex", "application/pdf", name).await?;
        let off = ImportOptions {
            threshold: DEFAULT_THRESHOLD,
            ocr: false,
        };
        import_pdf_opts(&mut c, &e.blobs, doc, false, off).await?;
        assert_eq!(
            (state(&mut c, doc).await?, open_pending(&mut c, doc).await?),
            ("off".into(), 1)
        );
        docs.push(doc);
    }
    sqlx::query("update document set ocr_state = 'not_needed' where id = $1")
        .bind(docs[1])
        .execute(&mut *c)
        .await?;
    // the worker starts with OCR on: every waiting scan is queued once, however often it starts
    assert_eq!(ownhealth_ocr::job::backfill(&e.db.pool).await?, 2);
    assert_eq!(ownhealth_ocr::job::backfill(&e.db.pool).await?, 0);
    let jobs: Vec<Value> = sqlx::query_scalar("select payload from job where queue = 'ocr' and kind = $1")
        .bind(ownhealth_core::arrive::OCR_JOB)
        .fetch_all(&mut *c)
        .await?;
    assert_eq!(jobs.len(), 2, "{jobs:?}");
    assert!(
        jobs.iter().all(|j| j["pages"] == serde_json::json!([1, 2, 3])),
        "{jobs:?}"
    );
    for doc in &docs {
        assert_eq!(state(&mut c, *doc).await?, "queued");
        assert_eq!(
            open_pending(&mut c, *doc).await?,
            0,
            "the queued scan no longer waits for an agent"
        );
    }
    run_ocr_queue(&e).await?;
    assert_eq!(calls.load(Ordering::SeqCst), 6);
    for doc in &docs {
        assert_eq!(state(&mut c, *doc).await?, "done");
    }
    use_reader(None);
    Ok(())
}

/// A reader that hands back `words` for page 1 (made-up lines of prose, no table) and fails every
/// page in `fail`; any other page is read from `pdf`'s text layer.
fn mixed_reader(pdf: &[u8], prose_on_1: bool, fail: &'static [i32]) {
    let calls = Arc::new(AtomicUsize::new(0));
    let text = text_reader(pdf, 0.99, calls);
    let prose = [
        "Informe de alta hospitalaria",
        "Servicio de Medicina Interna",
        "Evolución favorable durante el ingreso.",
        "Se recomienda reposo relativo y control en dos semanas.",
    ];
    let read: Reader = Arc::new(move |rgb, w, h, dpi, page_no| {
        if fail.contains(&page_no) {
            return Err(OcrError::Engine(format!("page {page_no} unreadable")));
        }
        if prose_on_1 && page_no == 1 {
            let mut words = Vec::new();
            for (i, line) in prose.iter().enumerate() {
                let top = 120.0 + 30.0 * i as f64;
                let mut x = 72.0;
                for t in line.split_whitespace() {
                    let x1 = x + 7.0 * t.chars().count() as f64;
                    words.push(OcrWord {
                        text: t.into(),
                        bbox: [x, top, x1, top + 12.0],
                        conf: 0.99,
                    });
                    x = x1 + 5.0;
                }
            }
            return Ok(OcrPage {
                page_no,
                width_pt: 595.0,
                height_pt: 842.0,
                words,
                engine: ENGINE.into(),
                engine_version: "test".into(),
                seconds: 0.0,
            });
        }
        text(rgb, w, h, dpi, page_no)
    });
    use_reader(Some((read, "test-models".into())));
}

async fn pending_items(c: &mut PgConnection, doc: Uuid) -> anyhow::Result<Vec<Value>> {
    Ok(sqlx::query_scalar(
        "select payload from review_item where document_id = $1 and kind = 'pending_document' and status = 'open'",
    )
    .bind(doc)
    .fetch_all(c)
    .await?)
}

#[tokio::test]
async fn a_scan_that_is_not_a_lab_report_and_holds_no_table_waits_after_ocr() -> anyhow::Result<()> {
    let _turn = TURN.lock().await;
    let e = setup().await?;
    let mut c = e.conn().await?;
    let lay = generate::layouts().remove(0);
    let (pdf, _) = generate::render(&lay, None);
    let one_page = scan(&pdf);
    mixed_reader(&pdf, true, &[]);
    let doc = import_with_ocr(&e, &mut c, &one_page, "application/pdf", "alex").await?;
    use_reader(None);
    let doc_type: Option<String> = sqlx::query_scalar("select doc_type from document where id = $1")
        .bind(doc)
        .fetch_one(&mut *c)
        .await?;
    assert_ne!(doc_type.as_deref(), Some("lab_report"));
    let items = pending_items(&mut c, doc).await?;
    assert_eq!(items.len(), 1, "{items:?}");
    assert_eq!(items[0]["why"], "no_table");
    assert_eq!(items[0]["pages"], serde_json::json!([1]));
    Ok(())
}

#[tokio::test]
async fn a_page_ocr_cannot_read_waits_as_ocr_failed_next_to_the_pages_it_read() -> anyhow::Result<()> {
    let _turn = TURN.lock().await;
    let e = setup().await?;
    let mut c = e.conn().await?;
    let ln = generate::labnorte_3p();
    let (pdf, _) = generate::render(&ln, None);
    mixed_reader(&pdf, false, &[3]);
    let doc = import_with_ocr(&e, &mut c, &scan(&pdf), "application/pdf", "alex").await?;
    let items = pending_items(&mut c, doc).await?;
    assert_eq!(items.len(), 1, "{items:?}");
    assert_eq!(items[0]["why"], "ocr_failed", "{items:?}");
    assert_eq!(items[0]["pages"], serde_json::json!([3]), "{items:?}");
    use_reader(None);
    Ok(())
}

#[tokio::test]
async fn pages_without_a_table_and_pages_ocr_failed_on_share_one_item() -> anyhow::Result<()> {
    let _turn = TURN.lock().await;
    let e = setup().await?;
    let mut c = e.conn().await?;
    let ln = generate::labnorte_3p();
    let (pdf, _) = generate::render(&ln, None);
    mixed_reader(&pdf, true, &[2, 3]);
    let doc = import_with_ocr(&e, &mut c, &scan(&pdf), "application/pdf", "alex").await?;
    use_reader(None);
    let items = pending_items(&mut c, doc).await?;
    assert_eq!(items.len(), 1, "{items:?}");
    assert_eq!(items[0]["pages"], serde_json::json!([1, 2, 3]), "{items:?}");
    assert_eq!(
        items[0]["page_why"],
        serde_json::json!({"1": "no_table", "2": "ocr_failed", "3": "ocr_failed"}),
        "{items:?}"
    );
    Ok(())
}

#[tokio::test]
async fn a_scan_moved_to_another_profile_is_read_again_for_it() -> anyhow::Result<()> {
    let _turn = TURN.lock().await;
    let e = setup().await?;
    let mut c = e.conn().await?;
    let ln = generate::labnorte_3p();
    let (pdf, truth) = generate::render(&ln, None);
    let calls = reader_on(&pdf, 0.995);
    let doc = import_with_ocr(&e, &mut c, &scan(&pdf), "application/pdf", "alex").await?;
    let o = eval::outcomes(&mut c, "labnorte", doc, &truth).await?;
    assert_eq!(o.accepted + o.review, o.truth, "{o:?}");

    let sam = e.fam.profile("sam");
    ownhealth_ingest_pdf::assign_document(&mut c, doc, sam, e.fam.user("alex"), None).await?;
    // the forced import.pdf the move queued, with the `ocr` profile on
    let s = import_pdf_opts(
        &mut c,
        &e.blobs,
        doc,
        true,
        ImportOptions {
            threshold: DEFAULT_THRESHOLD,
            ocr: true,
        },
    )
    .await?;
    assert_eq!(s.reasons.get("ocr_queued"), Some(&1), "{s:?}");
    run_ocr_queue(&e).await?;
    assert_eq!(calls.load(Ordering::SeqCst), 6, "every page is read again");
    use_reader(None);

    let live: i64 = sqlx::query_scalar(
        "select count(*) from observation where document_id = $1 and profile_id = $2 and status in ('current', 'provisional')",
    )
    .bind(doc)
    .bind(sam)
    .fetch_one(&mut *c)
    .await?;
    let waiting: i64 = sqlx::query_scalar(
        "select count(*) from review_item where document_id = $1 and profile_id = $2 and kind = 'candidate_row' \
         and status = 'open'",
    )
    .bind(doc)
    .bind(sam)
    .fetch_one(&mut *c)
    .await?;
    assert_eq!(
        usize::try_from(live + waiting)?,
        o.truth,
        "every row is stored or waits for sam"
    );
    assert_eq!(state(&mut c, doc).await?, "done");
    Ok(())
}

#[tokio::test]
async fn a_photo_moved_to_another_profile_is_read_again_for_it() -> anyhow::Result<()> {
    let _turn = TURN.lock().await;
    let e = setup().await?;
    let mut c = e.conn().await?;
    let lay = generate::layouts().remove(0);
    let (pdf, truth) = generate::render(&lay, None);
    let calls = reader_on(&pdf, 0.99);
    let page = eval::render_pages(&pdf, 50.0).remove(0);
    let mut jpeg = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg, 80).encode_image(&page)?;
    let person = truth["person"].as_str().unwrap_or("alex");
    let doc = import_with_ocr(&e, &mut c, &jpeg, "image/jpeg", person).await?;
    let o = eval::outcomes(&mut c, "photo", doc, &truth).await?;
    assert_eq!(o.accepted + o.review, o.truth, "{o:?}");

    let to = e.fam.profile(if person == "sam" { "alex" } else { "sam" });
    ownhealth_ingest_pdf::assign_document(&mut c, doc, to, e.fam.user("alex"), None).await?;
    let s = import_pdf_opts(
        &mut c,
        &e.blobs,
        doc,
        true,
        ImportOptions {
            threshold: DEFAULT_THRESHOLD,
            ocr: true,
        },
    )
    .await?;
    assert_eq!(s.reasons.get("ocr_queued"), Some(&1), "{s:?}");
    run_ocr_queue(&e).await?;
    assert_eq!(calls.load(Ordering::SeqCst), 2, "the photo is read again");
    use_reader(None);

    let live: i64 = sqlx::query_scalar(
        "select count(*) from observation where document_id = $1 and profile_id = $2 and status in ('current', 'provisional')",
    )
    .bind(doc)
    .bind(to)
    .fetch_one(&mut *c)
    .await?;
    let waiting: i64 = sqlx::query_scalar(
        "select count(*) from review_item where document_id = $1 and profile_id = $2 and kind = 'candidate_row' \
         and status = 'open'",
    )
    .bind(doc)
    .bind(to)
    .fetch_one(&mut *c)
    .await?;
    assert_eq!(
        usize::try_from(live + waiting)?,
        o.truth,
        "every row is stored or waits for the new profile"
    );
    assert_eq!(state(&mut c, doc).await?, "done");
    Ok(())
}

/// Dedup level 2 positive control: once OCR has read a scan's words, a scan of a report we hold
/// matches it on its page fingerprints, and a scan of another report from the same template does not.
/// (On import the edition link wins over the "probably the same" flag; this pins the match under it.)
#[tokio::test]
async fn a_scan_of_a_report_we_hold_matches_it_once_ocr_reads_it() -> anyhow::Result<()> {
    let _turn = TURN.lock().await;
    let e = setup().await?;
    let mut c = e.conn().await?;
    let lay = generate::layouts().remove(0);
    let (pdf, truth) = generate::render(&lay, None);
    let person = truth["person"].as_str().unwrap_or("alex");
    let original = e.store(&mut c, &pdf, person, "application/pdf", "informe.pdf").await?;
    ownhealth_ingest_pdf::import_pdf(&mut c, &e.blobs, original, false).await?;

    reader_on(&pdf, 0.99);
    let same = import_with_ocr(&e, &mut c, &scan(&pdf), "application/pdf", person).await?;
    let near = ownhealth_core::dedup::find_near_duplicate(&mut c, same).await?;
    assert_eq!(near.map(|n| n.document_id), Some(original));

    // next month's report from the same lab: the same layout, other values, date and sample
    let (next, _) = generate::render(
        &generate::Layout {
            seed: lay.seed + 7,
            collected: "2026-10-14",
            sample: "S-0002".into(),
            ..lay.clone()
        },
        None,
    );
    reader_on(&next, 0.99);
    let other = import_with_ocr(&e, &mut c, &scan(&next), "application/pdf", person).await?;
    use_reader(None);
    let near = ownhealth_core::dedup::find_near_duplicate(&mut c, other).await?;
    assert_eq!(near.map(|n| n.document_id), None, "another report is not the same");
    Ok(())
}
