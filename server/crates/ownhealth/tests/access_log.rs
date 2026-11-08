//! The access log. Its own test binary: tracing caches callsite interest
//! globally, so a test that installs a thread-local subscriber races with any
//! other test in the same binary that hits the same callsite first.

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use ownhealth::app::build_app;
use ownhealth_core::Runtime;
use ownhealth_core::testing::TestDb;
use tower::ServiceExt;

/// A writer the test reads back.
#[derive(Clone, Default)]
struct Captured(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

impl std::io::Write for Captured {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        if let Ok(mut v) = self.0.lock() {
            v.extend_from_slice(b);
        }
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[tokio::test]
async fn one_access_line_per_request_without_the_query() -> anyhow::Result<()> {
    let db = TestDb::empty().await?;
    let dir = tempfile::tempdir()?;
    let mut s = db.settings(dir.path())?;
    let static_dir = dir.path().join("static");
    std::fs::create_dir_all(&static_dir)?;
    std::fs::write(static_dir.join("index.html"), "<!doctype html><title>OwnHealth</title>")?;
    s.static_dir = Some(static_dir);
    let app = build_app(Runtime::with_pool(s, db.pool.clone())?);

    let out = Captured::default();
    let writer = out.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(move || writer.clone())
        .with_ansi(false)
        .with_max_level(tracing::Level::INFO)
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);
    tracing::callsite::rebuild_interest_cache();

    let res = app
        .oneshot(Request::get("/healthz?x=secret").body(Body::empty())?)
        .await?;
    assert_eq!(res.status(), StatusCode::OK);
    to_bytes(res.into_body(), 1 << 20).await?;

    let text = String::from_utf8(out.0.lock().map(|v| v.clone()).unwrap_or_default())?;
    let lines: Vec<&str> = text.lines().filter(|l| l.contains("ownhealth::access")).collect();
    assert_eq!(lines.len(), 1, "{text}");
    for want in ["GET", "/healthz", "status=200", "latency_ms="] {
        assert!(lines[0].contains(want), "{want} missing: {}", lines[0]);
    }
    assert!(!text.contains("secret"), "{text}");
    Ok(())
}
