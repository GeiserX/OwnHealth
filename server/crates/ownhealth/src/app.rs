//! App assembly (`app.py`): REST under /api, MCP at /mcp, health probes, the UI at /,
//! and the startup steps (migrations, demo seed, job workers) with a graceful shutdown.

use std::sync::Arc;

use axum::Json;
use axum::extract::{MatchedPath, Request, State};
use axum::http::{HeaderValue, StatusCode, Uri, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Extension, Router};
use ownhealth_api::ApiError;
use ownhealth_api::routes::imaging_proxy::{self, Imaging, OrthancConfig};
use ownhealth_core::config::{DEV_SECRET_KEY, ProcessEnv, Settings};
use ownhealth_core::{Error, Runtime, demo, jobs, migrate};
use serde_json::json;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tower_http::set_header::SetResponseHeaderLayer;

use crate::ui::Ui;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// The secret key signs MCP cursors: a public default lets anyone forge offsets and as_of pins.
pub fn check_secret_key(settings: &Settings) -> anyhow::Result<()> {
    if settings.secret_key == DEV_SECRET_KEY && !(settings.dev_auth || settings.demo) {
        anyhow::bail!("OWNHEALTH_SECRET_KEY is not set: generate one with `openssl rand -hex 48`.");
    }
    Ok(())
}

/// The app with imaging off.
pub fn build_app(rt: Runtime) -> Router {
    build_app_with(rt, Imaging::off())
}

/// The app; `imaging` holds the Orthanc settings (None: imaging off) and the open viewer links.
pub fn build_app_with(rt: Runtime, imaging: Arc<Imaging>) -> Router {
    let ui = Arc::new(Ui::from_settings(rt.settings.static_dir.as_deref()));
    let web = Router::new()
        .nest(
            "/api",
            // Health answers are private and change on every import: no browser or proxy keeps
            // one unless a route says otherwise.
            ownhealth_api::router().layer(SetResponseHeaderLayer::if_not_present(
                header::CACHE_CONTROL,
                HeaderValue::from_static("no-store"),
            )),
        )
        // The imaging viewer (/ohif) and its DICOMweb proxy (/dicom-web): never the UI fallback.
        .merge(imaging_proxy::viewer_router())
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz))
        .fallback(move |uri: Uri| {
            let ui = ui.clone();
            async move { ui.serve(&uri).await }
        })
        .with_state(rt.clone());
    web.merge(ownhealth_mcp::router(rt))
        .layer(Extension(imaging))
        .layer(middleware::from_fn(access_log))
}

/// One INFO line per response: method, matched route, status, latency. Never the query string,
/// headers or bodies. `RUST_LOG=info,ownhealth::access=off` switches it off.
async fn access_log(req: Request, next: Next) -> Response {
    let method = req.method().clone();
    let path = req
        .extensions()
        .get::<MatchedPath>()
        .map_or_else(|| req.uri().path().to_string(), |m| m.as_str().to_string());
    let start = std::time::Instant::now();
    let res = next.run(req).await;
    let latency_ms = u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX);
    tracing::info!(target: "ownhealth::access", %method, %path, status = res.status().as_u16(), latency_ms, "request");
    res
}

/// Remove upload spools and decrypted copies older than an hour that a killed process left behind.
pub fn sweep_spools(rt: &Runtime) {
    match rt.blobs.sweep_tmp(std::time::Duration::from_secs(3600)) {
        Ok(0) => {}
        Ok(n) => tracing::info!(removed = n, "removed stale upload spools"),
        Err(e) => tracing::warn!(error = %e, "could not sweep upload spools"),
    }
}

/// Remove the sealed folders whose key is gone: a deleted profile's file that a process still holding
/// its key landed after the delete, and a settled held mail's or assigned document's item folder
/// (`BlobStore::sweep_unkeyed`).
pub async fn sweep_unkeyed(rt: &Runtime) {
    let swept = async {
        let mut conn = rt.pool.acquire().await?;
        rt.blobs.sweep_unkeyed(&mut conn).await.map_err(Error::from)
    };
    match swept.await {
        Ok(0) => {}
        Ok(n) => tracing::info!(removed = n, "removed sealed folders whose key is gone"),
        Err(e) => tracing::warn!(error = %e, "could not sweep sealed folders"),
    }
}

/// `sweep_spools` and `sweep_unkeyed` every 10 minutes until `stop`: a spool a killed process left
/// goes about an hour later, not only at the next start, and a sealed file landed after its key went
/// within 10 minutes.
pub fn spool_sweeper(rt: &Runtime, mut stop: watch::Receiver<bool>) -> JoinHandle<()> {
    let rt = rt.clone();
    tokio::spawn(async move {
        let mut every = tokio::time::interval(std::time::Duration::from_secs(600));
        every.tick().await; // the start already swept
        loop {
            tokio::select! {
                _ = every.tick() => {
                    sweep_spools(&rt);
                    sweep_unkeyed(&rt).await;
                }
                _ = stop.changed() => return,
            }
        }
    })
}

/// Refuse to go on when OWNHEALTH_SECRETS_KEY does not open the profile keys the database holds,
/// before anything listens, runs a job or stores a key (`keys::check_start`). A database with no
/// profile key yet passes.
pub async fn check_profile_keys(rt: &Runtime) -> Result<(), Error> {
    if let Some(kek) = rt.blobs.kek() {
        let mut conn = rt.pool.acquire().await?;
        ownhealth_core::keys::check_start(&mut conn, kek).await?;
    }
    Ok(())
}

async fn healthz() -> Json<serde_json::Value> {
    Json(json!({"status": "ok", "version": VERSION}))
}

async fn readyz(State(rt): State<Runtime>) -> Response {
    let pending = async {
        let mut conn = rt.pool.acquire().await?;
        sqlx::query("select 1").execute(&mut *conn).await?;
        migrate::pending(&mut conn, &migrate::embedded()?).await
    };
    // A full pool or an unreachable database answers 503 within seconds, not after the pool's
    // 30 s acquire timeout (the container healthcheck gives up after 5 s).
    let pending = tokio::time::timeout(std::time::Duration::from_secs(3), pending).await;
    match pending {
        // The exception class name Python reported.
        Err(_) | Ok(Err(_)) => {
            ApiError::new(StatusCode::SERVICE_UNAVAILABLE, "db_unavailable", "OperationalError").into_response()
        }
        Ok(Ok(todo)) if !todo.is_empty() => ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "migrations_pending",
            format!("pending: {todo:?}"),
        )
        .into_response(),
        Ok(Ok(_)) => match rt.blobs.health() {
            Err(missing) => ApiError::new(StatusCode::SERVICE_UNAVAILABLE, "blob_dir_missing", missing).into_response(),
            Ok(()) => Json(json!({"status": "ready"})).into_response(),
        },
    }
}

/// What `serve` started besides the HTTP server; `stop()` ends the workers and waits for them.
pub struct Background {
    stop: watch::Sender<bool>,
    workers: Vec<JoinHandle<()>>,
}

impl Background {
    pub async fn stop(self) {
        let _ = self.stop.send(true);
        for w in self.workers {
            let _ = w.await;
        }
    }
}

/// The lifespan steps before serving: migrations, the demo seed, the job workers.
pub async fn start(rt: &Runtime) -> Result<Background, Error> {
    if rt.settings.run_migrations {
        let applied = migrate::migrate_embedded(&rt.pool).await?;
        if !applied.is_empty() {
            tracing::info!(?applied, "applied migrations");
        }
    }
    check_profile_keys(rt).await?;
    sweep_spools(rt);
    sweep_unkeyed(rt).await;
    if rt.settings.demo {
        demo::seed_demo(&rt.pool, &rt.settings, &rt.blobs, false).await?;
    }
    let (stop, rx) = watch::channel(false);
    let mut workers = if rt.settings.run_jobs {
        let mut w = jobs::start_workers(&rt.job_context(), rx.clone());
        w.push(tokio::spawn(ownhealth_core::sources::scheduler_loop(
            rt.job_context(),
            rx.clone(),
        )));
        w
    } else {
        Vec::new()
    };
    workers.push(spool_sweeper(rt, rx));
    Ok(Background { stop, workers })
}

/// Resolves on SIGINT or SIGTERM.
pub async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let term = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut s) => {
                s.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let term = std::future::pending::<()>();
    tokio::select! {
        _ = ctrl_c => {}
        _ = term => {}
    }
    tracing::info!("shutting down");
}

pub async fn serve(settings: Settings) -> anyhow::Result<()> {
    check_secret_key(&settings)?;
    // Imaging is on when OWNHEALTH_ORTHANC_URL is set; a URL without a password stops the start.
    let orthanc = OrthancConfig::from_env(&ProcessEnv)?;
    let imaging_on = orthanc.is_some();
    let imaging = Imaging::new(orthanc)?;
    let addr = format!("{}:{}", settings.host, settings.port);
    let rt = Runtime::new(settings)?;
    let background = start(&rt).await?;
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    tracing::info!(%addr, version = VERSION, imaging = imaging_on, "listening");
    axum::serve(
        listener,
        build_app_with(rt, imaging).into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_signal())
    .await?;
    background.stop().await;
    Ok(())
}
