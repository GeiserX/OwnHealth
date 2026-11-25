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
use ownhealth_api::routes::imaging_proxy::{self, Imaging};
use ownhealth_api::routes::ow_proxy;
use ownhealth_core::config::{DEV_SECRET_KEY, Settings};
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
        .merge(ow_proxy::proxy_router(&rt.settings))
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

/// Open the break-glass requests whose delay has passed and that an owner calendar feed has carried,
/// and end the ones whose time ran out (`break_glass::settle`), every minute until `stop`: a person's
/// next request does the same, so this tells the owner even when nobody comes back.
pub fn break_glass_sweeper(rt: &Runtime, mut stop: watch::Receiver<bool>) -> JoinHandle<()> {
    let rt = rt.clone();
    tokio::spawn(async move {
        let mut every = tokio::time::interval(std::time::Duration::from_secs(60));
        loop {
            tokio::select! {
                _ = every.tick() => {
                    let settled = async {
                        let mut conn = rt.pool.acquire().await?;
                        ownhealth_core::break_glass::settle(&mut conn, None).await
                    };
                    if let Err(e) = settled.await {
                        tracing::warn!(error = %e, "could not settle break-glass requests");
                    }
                }
                _ = stop.changed() => return,
            }
        }
    })
}

/// With imaging on: queues an `imaging.sweep` at the start (after a restore, it finds the recorded
/// studies Orthanc lacks and queues their rebuild) and then every hour, so Orthanc holds only what the
/// record names (slice 43). An enqueue that fails is tried again after a minute.
pub fn imaging_sweeper(rt: &Runtime, stop: watch::Receiver<bool>) -> JoinHandle<()> {
    imaging_sweeper_every(
        rt,
        stop,
        std::time::Duration::from_secs(ownhealth_imaging::sweep::EVERY_SECONDS),
        std::time::Duration::from_secs(SWEEP_ENQUEUE_RETRY_SECONDS),
    )
}

/// How long the imaging sweeper waits before it tries again after it could not queue a sweep.
pub const SWEEP_ENQUEUE_RETRY_SECONDS: u64 = 60;

/// [`imaging_sweeper`], queueing a sweep every `period`, and `retry` after an enqueue that failed.
pub fn imaging_sweeper_every(
    rt: &Runtime,
    mut stop: watch::Receiver<bool>,
    period: std::time::Duration,
    retry: std::time::Duration,
) -> JoinHandle<()> {
    let rt = rt.clone();
    tokio::spawn(async move {
        let mut every = tokio::time::interval(period);
        loop {
            tokio::select! {
                _ = every.tick() => {
                    let queued = async {
                        let mut conn = rt.pool.acquire().await?;
                        ownhealth_imaging::sweep::enqueue(&mut conn).await
                    };
                    if let Err(e) = queued.await {
                        tracing::warn!(error = %e, "could not queue the imaging sweep; trying again soon");
                        // the next tick comes after `retry`, then the period again
                        every.reset_after(retry);
                    }
                }
                _ = stop.changed() => return,
            }
        }
    })
}

/// The wearable_sample partition job (`ownhealth_ops::partition`) an hour after the start and every
/// hour after, until `stop`: past OWNHEALTH_PARTITION_ROWS it turns the table into monthly
/// partitions, and then makes next month's partition ahead of time. Below the threshold a run reads
/// the row estimate and changes nothing. When the app's role does not own the table (the compose
/// files), it warns that the operator runs `ownhealth partition`: once, again after a run that ended
/// otherwise, and once a day while nobody does. A `needs_maintenance` run (too many future-dated
/// samples to move, which no later run resolves by itself) warns on the same schedule.
pub fn partition_job(rt: &Runtime, mut stop: watch::Receiver<bool>) -> JoinHandle<()> {
    let rt = rt.clone();
    tokio::spawn(async move {
        let hour = std::time::Duration::from_secs(3600);
        let mut every = tokio::time::interval_at(tokio::time::Instant::now() + hour, hour);
        let mut owner_due = OwnerDue::default();
        loop {
            tokio::select! {
                _ = every.tick() => {
                    let opts = ownhealth_ops::partition::Options::new(rt.settings.partition_rows);
                    match ownhealth_ops::partition::run(&rt.pool, &opts).await {
                        Ok(r) => {
                            let state = r["state"].as_str();
                            let warn = owner_due.warn(state);
                            match state {
                                Some("converted") => tracing::info!(report = %r, "wearable_sample is partitioned by month"),
                                Some("needs_owner") if warn => {
                                    tracing::warn!(report = %r, "wearable_sample needs the partition job: run `ownhealth partition` as the database owner");
                                }
                                Some("needs_maintenance") if warn => {
                                    tracing::warn!(report = %r, "wearable_sample partitioning waits on future-dated samples: retract the files that hold the samples the report dates, or delete them when pushes wrote them with no file (deploy/README.md, needs_maintenance), then run `ownhealth partition`");
                                }
                                Some("busy") => tracing::info!(report = %r, "the wearable_sample partition job will try again"),
                                _ if r["made"].as_array().is_some_and(|m| !m.is_empty()) => {
                                    tracing::info!(report = %r, "made wearable_sample partitions ahead of time");
                                }
                                _ => {}
                            }
                        }
                        Err(e) => tracing::warn!(error = %e, "the wearable_sample partition job failed"),
                    }
                }
                _ = stop.changed() => return,
            }
        }
    })
}

/// When the hourly partition run warns that a person must act (`needs_owner` or
/// `needs_maintenance`): at the first run of a stretch in that state, then once a day (every 24th
/// run) while the stretch lasts. A run that ends in another state ends the stretch, so a month missed
/// later warns again.
#[derive(Debug, Default)]
struct OwnerDue {
    /// The state of the stretch, and its runs since the last warning.
    quiet: Option<(&'static str, u32)>,
}

impl OwnerDue {
    const EVERY: u32 = 24;
    const STATES: [&'static str; 2] = ["needs_owner", "needs_maintenance"];

    /// Whether a run that ended in `state` warns.
    fn warn(&mut self, state: Option<&str>) -> bool {
        let Some(state) = Self::STATES.into_iter().find(|s| state == Some(*s)) else {
            self.quiet = None;
            return false;
        };
        match self.quiet {
            Some((s, n)) if s == state && n + 1 < Self::EVERY => {
                self.quiet = Some((s, n + 1));
                false
            }
            _ => {
                self.quiet = Some((state, 0));
                true
            }
        }
    }
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
        w.push(partition_job(rt, rx.clone()));
        w
    } else {
        Vec::new()
    };
    if rt.settings.run_jobs && rt.settings.orthanc.is_some() {
        workers.push(imaging_sweeper(rt, rx.clone()));
    }
    workers.push(break_glass_sweeper(rt, rx.clone()));
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
    // the vendor profiles drive the connector token code: a bad file stops the start and names its line
    ownhealth_core::connector_profiles::check().map_err(anyhow::Error::msg)?;
    // Imaging is on when OWNHEALTH_ORTHANC_URL is set (Settings reads it; a URL without a password
    // stops the start). The viewer proxy and the DICOM import job use the same Orthanc.
    let orthanc = settings.orthanc.clone();
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

#[cfg(test)]
mod tests {
    use super::OwnerDue;

    /// The first needs_owner warns, the next ones of the same stretch stay quiet; a run that ends
    /// otherwise (the owner ran the job) starts a new stretch, and a stretch that lasts a day warns
    /// again.
    #[test]
    fn needs_owner_warns_again_after_another_state_and_once_a_day() {
        let mut d = OwnerDue::default();
        assert!(d.warn(Some("needs_owner")));
        assert!(!d.warn(Some("needs_owner")));
        assert!(!d.warn(Some("partitioned")));
        assert!(d.warn(Some("needs_owner")), "a month missed later warns again");
        assert!(!d.warn(Some("below_threshold")));
        assert!(d.warn(Some("needs_owner")));
        let quiet = (0..23).filter(|_| d.warn(Some("needs_owner"))).count();
        assert_eq!(quiet, 0);
        assert!(d.warn(Some("needs_owner")), "the 25th hourly run warns again");
    }

    /// A group of future-dated samples too large to move never resolves by itself, so the hourly
    /// run of an owner-role app reports needs_maintenance every hour: it warns like needs_owner, once
    /// per stretch and once a day. A change from one state to the other warns at once.
    #[test]
    fn needs_maintenance_warns_once_a_stretch_and_once_a_day() {
        let mut d = OwnerDue::default();
        assert!(d.warn(Some("needs_maintenance")));
        let quiet = (0..23).filter(|_| d.warn(Some("needs_maintenance"))).count();
        assert_eq!(quiet, 0);
        assert!(d.warn(Some("needs_maintenance")), "the 25th hourly run warns again");
        assert!(d.warn(Some("needs_owner")), "another state warns at once");
        assert!(d.warn(Some("needs_maintenance")));
        assert!(!d.warn(Some("needs_maintenance")));
        assert!(!d.warn(Some("busy")));
        assert!(d.warn(Some("needs_maintenance")), "a stretch that ended warns again");
    }
}
