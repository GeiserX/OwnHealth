//! The benchmark harness (lane C; ROADMAP v0.2 "Benchmark harness", ingestion.md 13): pages/s and row
//! F1 per engine on the scanned and photographed eval set, written as a JSON report and the table
//! published in docs. Run with `cargo run -p ownhealth-ocr --features engine --example bench`.

use serde::Serialize;

/// One engine's numbers on the eval set.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct BenchReport {
    pub engine: String,
    pub engine_version: String,
    pub pages: usize,
    pub mean_seconds_per_page: f64,
    pub pages_per_second: f64,
    pub row_precision: f64,
    pub row_recall: f64,
    pub row_f1: f64,
    pub dropped: usize,
    pub duplicated: usize,
}
