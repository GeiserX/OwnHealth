//! v0.2 dedup level 2 inside `import.pdf` (ingestion.md 4): every page gets a pHash next to its text
//! simhash, and a document whose pages match an earlier document of the profile is still imported,
//! then filed as one `near_duplicate` review item ("probably the same as document X"). A page is
//! compared on its words: a scan without them is never matched on its pHash alone.
mod support;

use ownhealth_ingest_pdf::import_pdf;
use support::env::{env, fake_normalizer, items};
use support::generate::{self, layouts, render, scanned};
use uuid::Uuid;

type R = anyhow::Result<()>;

async fn near_of(c: &mut sqlx::PgConnection, doc: Uuid) -> anyhow::Result<Option<Uuid>> {
    Ok(
        sqlx::query_scalar("select near_duplicate_of from document where id = $1")
            .bind(doc)
            .fetch_one(c)
            .await?,
    )
}

#[tokio::test]
async fn a_scan_is_never_called_the_same_on_its_picture_alone() -> R {
    fake_normalizer();
    let e = env().await?;
    let mut c = e.conn().await?;
    let ls = layouts();
    let (text_pdf, _) = render(&ls[0], None);
    // next month's report from the same lab: the same page layout, other values, date and sample
    let next = render(
        &generate::Layout {
            seed: ls[0].seed + 7,
            collected: "2026-10-14",
            sample: "S-0002".into(),
            ..ls[0].clone()
        },
        None,
    )
    .0;

    let original = e.store_pdf(&mut c, &text_pdf, "informe.pdf").await?;
    import_pdf(&mut c, &e.blobs, original, false).await?;
    let fp: (Option<Vec<u8>>, Option<i64>) =
        sqlx::query_as("select phash, text_simhash from document_page where document_id = $1 and page_no = 1")
            .bind(original)
            .fetch_one(&mut *c)
            .await?;
    assert!(fp.0.is_some() && fp.1.is_some(), "{fp:?}");

    // a scan has a pHash and no words: a page of the same template looks the same whatever values it
    // holds, so the pHash alone never decides (the OCR pass compares the words)
    let first = e
        .store_pdf(&mut c, &scanned(&text_pdf, 100.0), "escaneo-septiembre.pdf")
        .await?;
    import_pdf(&mut c, &e.blobs, first, false).await?;
    let second = e
        .store_pdf(&mut c, &scanned(&next, 100.0), "escaneo-octubre.pdf")
        .await?;
    let s = import_pdf(&mut c, &e.blobs, second, false).await?;
    for scan in [second, first] {
        let ph: Option<Vec<u8>> =
            sqlx::query_scalar("select phash from document_page where document_id = $1 and page_no = 1")
                .bind(scan)
                .fetch_one(&mut *c)
                .await?;
        assert!(ph.is_some(), "the scan still gets its pHash");
        assert_eq!(near_of(&mut c, scan).await?, None, "{}", s.to_value());
        assert!(items(&mut c, scan, Some("near_duplicate")).await?.is_empty());
        // still processed: the scan waits for extraction like any other
        assert_eq!(items(&mut c, scan, Some("pending_document")).await?.len(), 1);
    }
    Ok(())
}

#[tokio::test]
async fn an_edition_of_an_earlier_report_is_linked_not_flagged() -> R {
    fake_normalizer();
    let e = env().await?;
    let mut c = e.conn().await?;
    let ls = layouts();
    let (pdf, _) = render(&ls[0], None);
    let reprint = render(
        &generate::Layout {
            meta: vec![("Subject", "reimpresión")],
            ..ls[0].clone()
        },
        None,
    )
    .0;
    let first = e.store_pdf(&mut c, &pdf, "a.pdf").await?;
    import_pdf(&mut c, &e.blobs, first, false).await?;
    let second = e.store_pdf(&mut c, &reprint, "b.pdf").await?;
    let s = import_pdf(&mut c, &e.blobs, second, false).await?;
    // read as an edition of the first, nothing changed: the rows are linked, no review at all
    assert_eq!((s.queued, s.skipped), (0, 12), "{}", s.to_value());
    assert!(items(&mut c, second, None).await?.is_empty());
    assert_eq!(near_of(&mut c, second).await?, None);
    Ok(())
}
