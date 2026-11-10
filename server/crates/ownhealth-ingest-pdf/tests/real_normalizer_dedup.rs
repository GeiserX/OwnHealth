//! Dedup level 3 with lane A's real normalizer, which flags a value already accepted from another
//! document as `duplicate` by itself. That flag must not stop the same fact from being linked.
//! A test binary of its own, because the normalizer hook is process-wide.
mod support;

use ownhealth_core::demo_pdf::{LabReport, LabRow, lab_report_pdf};
use ownhealth_ingest_pdf::import_pdf;
use support::env::env;

fn labnorte(report_id: &str, rows: &[(&str, &str, &str, &str)]) -> Vec<u8> {
    lab_report_pdf(&LabReport {
        lab: "LabNorte",
        title: "Informe de laboratorio",
        patient: "RIVERA, ALEX",
        birth: "14/05/1979",
        collected: "14/09/2026 08:15",
        report_id,
        pages: vec![rows.iter().map(|(n, v, u, r)| LabRow::new(n, v, u, r)).collect()],
    })
    .0
}

#[tokio::test]
async fn the_same_fact_again_is_linked_with_the_real_normalizer() -> anyhow::Result<()> {
    ownhealth_ingest_pdf::use_normalizer(None);
    let e = env().await?;
    let mut c = e.conn().await?;
    let glucose = ("Glucosa", "102", "mg/dL", "70 - 110");
    let a = e.store_pdf(&mut c, &labnorte("LN-R1", &[glucose]), "a.pdf").await?;
    import_pdf(&mut c, &e.blobs, a, false).await?;
    let rows = [
        glucose,
        ("Creatinina", "0,9", "mg/dL", "0,7 - 1,3"),
        ("Urea", "32", "mg/dL", "15 - 45"),
    ];
    let b = e.store_pdf(&mut c, &labnorte("LN-R2", &rows), "b.pdf").await?;
    let s = import_pdf(&mut c, &e.blobs, b, false).await?;
    assert_eq!(s.reasons.get("duplicate"), Some(&1), "{}", s.to_value());
    let glucose_rows: i64 =
        sqlx::query_scalar("select count(*) from observation where alias_group = 'glucose' and status = 'current'")
            .fetch_one(&mut *c)
            .await?;
    assert_eq!(glucose_rows, 1);
    let links: i64 = sqlx::query_scalar("select count(*) from observation_link where document_id = $1")
        .bind(b)
        .fetch_one(&mut *c)
        .await?;
    assert_eq!(links, 1, "the second report is cited as one more source");
    Ok(())
}
