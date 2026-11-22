//! Imaging (v1.0, compose profile `imaging`): the OHIF viewer and the DICOMweb proxy in front of Orthanc.
//!
//! Orthanc knows nothing of profiles, so nobody reaches it directly; the app is the boundary.
//!
//! 1. A person opens one study: `POST /api/profiles/{p}/imaging/studies/{uid}/open`, from a browser
//!    session (or dev auth) with `read:records` on the profile. Access tokens never get a link. The answer
//!    is a viewer link that carries a capability: 32 random bytes, valid for one hour, bound to that
//!    session and to that one study. Links live in memory only, found by the token's hash; opening the
//!    same study again in the same session gives the same live link back (a new one in its last 10
//!    minutes, unless the server is full), and one session holds at most [`LINKS_PER_HOLDER`] (its
//!    oldest goes first). The first open of a study in a session writes one `imaging.open` audit row.
//!
//!    Orthanc keys a study by PatientID plus StudyInstanceUID, so two patients' studies can share a UID,
//!    and its DICOMweb plugin then serves both under `/studies/{uid}`. The link is therefore bound to
//!    the Orthanc study the record names (`imaging_study.orthanc_id`): at the open, and again before
//!    proxied reads (at most [`ORTHANC_RECHECK`] apart), Orthanc's `/tools/lookup` must find exactly one
//!    study under the UID, with that id. Anything else passes nothing.
//! 2. The viewer (OHIF, served by Orthanc's plugin) runs under `/ohif/t/<capability>/`. Every `/ohif`
//!    answer carries [`VIEWER_CSP`]: `sandbox allow-scripts` puts the page in an opaque origin, so the
//!    session cookie never reaches its requests and it cannot read `/api`, and `connect-src 'self'` keeps
//!    it on this server. Small shims in its `app-config.js` let OHIF run there (no storage, no service
//!    worker, workers only from `blob:` URLs), and its DICOMweb roots point at `/dicom-web/t/<capability>`.
//! 3. The proxy answers CORS for `Origin: null`, without credentials, and allows GET only. Every request
//!    is checked again: the capability is live, its session is live, the session can still read the
//!    profile (`read:records`), and the study is still in that profile, not retracted, and still the
//!    same Orthanc study. A viewer request that passes every check counts as activity for the session
//!    (at most once a minute; a refused one does not), so a long read does not go idle; the link's own
//!    hour still bounds it. The study UID comes from the path, or from a search that names exactly that
//!    one UID. WADO-URI, a bare study list, root searches, Orthanc ids (`/instances/{id}`,
//!    `ohif-dicom-json`) and whole-study or whole-series downloads are refused. Every answer carries [`DICOMWEB_CSP`], so one opened as a page runs nothing,
//!    and one that is not an image or DICOM is a download.
//!
//!    Orthanc's absolute URLs in its JSON answers are turned into links through the capability. The
//!    part headers inside a multipart answer (`Content-Location` of each frame) are passed as they come
//!    and still name Orthanc's internal address (`http://orthanc:8042` in compose); the viewer cannot
//!    use it, because `connect-src 'self'` keeps it on this server and Orthanc publishes no port.

use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::extract::{Path, Query, Request, State};
use axum::http::request::Parts;
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, post};
use axum::{Extension, Json, Router};
use ownhealth_auth::principal::dev_principal;
use ownhealth_auth::sessions::IDLE_MINUTES;
use ownhealth_core::access::{Principal, PrincipalKind, effective_permission};
use ownhealth_core::audit::{AuditEvent, hash_arg, write_audit};
use ownhealth_core::config::Env;
use ownhealth_core::{Error, Runtime};
use serde_json::{Value, json};
use sqlx::PgConnection;
use uuid::Uuid;

use crate::error::{ApiError, ApiResult};
use crate::extract::Caller;

/// How long a viewer link works.
pub const CAPABILITY_TTL: Duration = Duration::from_secs(3600);
/// The audit action of the first open of a study in a session.
pub const AUDIT_ACTION: &str = "imaging.open";
/// More open links than this (after expired ones are dropped) refuses new ones.
const MAX_LINKS: usize = 10_000;
/// Live links one session (or dev user) holds; a new one drops that holder's oldest, so one person
/// cannot use up [`MAX_LINKS`] for everyone.
pub const LINKS_PER_HOLDER: usize = 50;
/// A second open of the same study reuses the live link while it has at least this long left.
const REUSE_LEFT: Duration = Duration::from_secs(600);
/// How long a link's check against Orthanc's `/tools/lookup` stands before a proxied read asks again.
pub const ORTHANC_RECHECK: Duration = Duration::from_secs(5);
/// Orthanc's JSON answers (QIDO, metadata) are read whole to rewrite its URLs, and the rewrite copies
/// them twice; a series of thousands of instances stays far below this.
const MAX_JSON_BYTES: usize = 64 << 20;
/// A viewer request counts as session activity at most this often.
const TOUCH_EVERY_SECS: i32 = 60;

/// Every `/ohif` answer, refusals included. `sandbox allow-scripts` (no `allow-same-origin`) runs the
/// viewer in an opaque origin. OHIF needs inline scripts, `eval` (its WebAssembly codecs) and workers
/// from `blob:` URLs; `connect-src 'self'` keeps every fetch on this server, where only its link works.
pub const VIEWER_CSP: &str = "sandbox allow-scripts; default-src 'none'; \
    script-src 'self' 'unsafe-inline' 'unsafe-eval' blob:; worker-src blob:; \
    style-src 'self' 'unsafe-inline'; img-src 'self' data: blob:; font-src 'self' data:; \
    connect-src 'self'; media-src 'self' blob:; manifest-src 'self'; frame-ancestors 'self'; \
    base-uri 'none'; form-action 'none'";

/// Every `/dicom-web` answer. The viewer reads them with fetch, where a CSP does nothing; a person (or a
/// script in the viewer) who opens one as a page gets a document with no rights at all.
pub const DICOMWEB_CSP: &str = "sandbox; default-src 'none'";

/// Where Orthanc is. `OWNHEALTH_ORTHANC_URL` (Orthanc's root, `http://orthanc:8042` in compose) turns
/// imaging on; the password comes from `OWNHEALTH_ORTHANC_PASSWORD_FILE` (a docker secret) or
/// `OWNHEALTH_ORTHANC_PASSWORD`, the user from `OWNHEALTH_ORTHANC_USER` (default `ownhealth`).
#[derive(Clone)]
pub struct OrthancConfig {
    pub url: String,
    pub user: String,
    pub password: String,
}

impl fmt::Debug for OrthancConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OrthancConfig")
            .field("url", &self.url)
            .field("user", &self.user)
            .field("password", &"***")
            .finish()
    }
}

impl OrthancConfig {
    /// None when `OWNHEALTH_ORTHANC_URL` is unset or empty (imaging off). A URL without a password,
    /// or one that is not http(s), is an error, so a half-configured install does not start.
    pub fn from_env(env: &dyn Env) -> Result<Option<OrthancConfig>, Error> {
        let var = |name: &str| env.var(name).map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
        let Some(url) = var("OWNHEALTH_ORTHANC_URL") else {
            return Ok(None);
        };
        let url = url.trim_end_matches('/').to_string();
        let scheme_ok = url.starts_with("http://") || url.starts_with("https://");
        if !scheme_ok || url.contains(['?', '#', '@']) {
            return Err(Error::Invalid(format!(
                "OWNHEALTH_ORTHANC_URL must be Orthanc's http(s) root, for example http://orthanc:8042, not {url}"
            )));
        }
        let password = match var("OWNHEALTH_ORTHANC_PASSWORD_FILE") {
            Some(path) => std::fs::read_to_string(&path)
                .map_err(|e| Error::Invalid(format!("OWNHEALTH_ORTHANC_PASSWORD_FILE={path}: {e}")))?
                .trim()
                .to_string(),
            None => var("OWNHEALTH_ORTHANC_PASSWORD").unwrap_or_default(),
        };
        if password.is_empty() {
            return Err(Error::Invalid(
                "OWNHEALTH_ORTHANC_URL is set but Orthanc's password is not: set OWNHEALTH_ORTHANC_PASSWORD_FILE \
                 or OWNHEALTH_ORTHANC_PASSWORD."
                    .into(),
            ));
        }
        let user = var("OWNHEALTH_ORTHANC_USER").unwrap_or_else(|| "ownhealth".into());
        Ok(Some(OrthancConfig { url, user, password }))
    }
}

/// Who a link belongs to. A dev-auth link works only while dev auth is on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Holder {
    Session(Uuid),
    Dev(Uuid),
}

impl Holder {
    /// The audit marker: one `imaging.open` row per study per holder.
    fn marker(self) -> String {
        match self {
            Holder::Session(id) => hash_arg(&format!("session:{id}")),
            Holder::Dev(user) => hash_arg(&format!("dev:{user}")),
        }
    }
}

#[derive(Debug, Clone)]
struct Link {
    /// The capability itself, so a second open of the study in the session can hand it back.
    token: String,
    holder: Holder,
    profile_id: Uuid,
    study_uid: String,
    /// The one Orthanc study the record named at the open.
    orthanc_id: String,
    expires: Instant,
    /// When Orthanc last confirmed that the UID names that study only.
    checked: Instant,
}

/// The imaging state of one app: the Orthanc settings (None: imaging off) and the open viewer links.
pub struct Imaging {
    orthanc: Option<OrthancConfig>,
    http: reqwest::Client,
    ttl: Duration,
    recheck: Duration,
    links: Mutex<HashMap<Vec<u8>, Link>>,
}

impl Imaging {
    pub fn new(orthanc: Option<OrthancConfig>) -> Arc<Imaging> {
        Imaging::with_ttl(orthanc, CAPABILITY_TTL)
    }

    /// `new` with another link lifetime (tests).
    pub fn with_ttl(orthanc: Option<OrthancConfig>, ttl: Duration) -> Arc<Imaging> {
        Imaging::with_times(orthanc, ttl, ORTHANC_RECHECK)
    }

    /// `new` with another link lifetime and Orthanc recheck interval (tests: zero asks every time).
    pub fn with_times(orthanc: Option<OrthancConfig>, ttl: Duration, recheck: Duration) -> Arc<Imaging> {
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .read_timeout(Duration::from_secs(60))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap_or_default();
        Arc::new(Imaging {
            orthanc,
            http,
            ttl,
            recheck,
            links: Mutex::default(),
        })
    }

    fn orthanc(&self) -> Result<&OrthancConfig, ApiError> {
        self.orthanc.as_ref().ok_or_else(|| {
            ApiError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "imaging_off",
                "Imaging is off: it needs the imaging compose profile and OWNHEALTH_ORTHANC_URL.",
            )
        })
    }

    /// A link for `holder` to the study: the holder's live one when it has [`REUSE_LEFT`] or more,
    /// else a new one (dropping the holder's oldest past [`LINKS_PER_HOLDER`]). Returns the token and
    /// the time it has left, and whether the link is new. When the server holds [`MAX_LINKS`]
    /// already, the holder's live link to the study is handed back however little it has left, and
    /// None when there is none.
    fn mint(
        &self,
        holder: Holder,
        profile_id: Uuid,
        study_uid: &str,
        orthanc_id: &str,
    ) -> Option<(String, Duration, bool)> {
        let mut links = self.links.lock().ok()?;
        let now = Instant::now();
        links.retain(|_, l| l.expires > now);
        let live = links
            .values()
            .filter(|l| {
                l.holder == holder
                    && l.profile_id == profile_id
                    && l.study_uid == study_uid
                    && l.orthanc_id == orthanc_id
            })
            .max_by_key(|l| l.expires)
            .map(|l| (l.token.clone(), l.expires - now, false));
        if live.as_ref().is_some_and(|(_, left, _)| *left >= REUSE_LEFT) {
            return live;
        }
        let mut held: Vec<(Instant, Vec<u8>)> = links
            .iter()
            .filter(|(_, l)| l.holder == holder)
            .map(|(k, l)| (l.expires, k.clone()))
            .collect();
        if held.len() >= LINKS_PER_HOLDER {
            held.sort();
            for (_, key) in held.iter().take(held.len() + 1 - LINKS_PER_HOLDER) {
                links.remove(key);
            }
        }
        if links.len() >= MAX_LINKS {
            return live;
        }
        let token = ownhealth_auth::token_urlsafe(32);
        links.insert(
            ownhealth_auth::sha256(&token),
            Link {
                token: token.clone(),
                holder,
                profile_id,
                study_uid: study_uid.to_string(),
                orthanc_id: orthanc_id.to_string(),
                expires: now + self.ttl,
                checked: now,
            },
        );
        Some((token, self.ttl, true))
    }

    /// Takes back a new link whose open did not complete.
    fn revoke(&self, token: &str) {
        if let Ok(mut links) = self.links.lock() {
            links.remove(&ownhealth_auth::sha256(token));
        }
    }

    /// Orthanc confirmed the link's study again just now.
    fn mark_checked(&self, token: &str) {
        if let Ok(mut links) = self.links.lock()
            && let Some(l) = links.get_mut(&ownhealth_auth::sha256(token))
        {
            l.checked = Instant::now();
        }
    }

    fn lookup(&self, token: &str) -> Option<Link> {
        if !is_token(token) {
            return None;
        }
        let key = ownhealth_auth::sha256(token);
        let mut links = self.links.lock().ok()?;
        match links.get(&key) {
            Some(l) if l.expires > Instant::now() => Some(l.clone()),
            Some(_) => {
                links.remove(&key);
                None
            }
            None => None,
        }
    }
}

/// The `/api` part, nested at `/api` with every area.
pub fn router() -> Router<Runtime> {
    Router::new().route("/profiles/{p}/imaging/studies/{uid}/open", post(open))
}

/// `/ohif` and `/dicom-web`, at the top level. The rest of both trees answers a refusal, never the UI.
pub fn viewer_router() -> Router<Runtime> {
    Router::new()
        .route("/ohif", any(ohif))
        .route("/ohif/", any(ohif))
        .route("/ohif/{*rest}", any(ohif))
        .route("/dicom-web", any(dicomweb))
        .route("/dicom-web/", any(dicomweb))
        .route("/dicom-web/{*rest}", any(dicomweb))
}

fn imaging_of(ext: Option<&Extension<Arc<Imaging>>>) -> Result<&Imaging, ApiError> {
    let imaging = ext
        .map(|Extension(i)| i.as_ref())
        .ok_or_else(|| ApiError::new(StatusCode::SERVICE_UNAVAILABLE, "imaging_off", "Imaging is off."))?;
    imaging.orthanc()?;
    Ok(imaging)
}

fn refused(code: &str, message: impl Into<String>) -> ApiError {
    ApiError::new(StatusCode::FORBIDDEN, code, message)
}

fn link_invalid() -> ApiError {
    refused(
        "viewer_link_invalid",
        "This viewer link is not valid any more. Open the study again in OwnHealth.",
    )
}

fn study_not_available() -> ApiError {
    refused("study_not_available", "This study is not available to you.")
}

fn retracted() -> ApiError {
    refused("study_retracted", "This study was retracted.")
}

fn not_bound() -> ApiError {
    refused(
        "study_not_bound",
        "OwnHealth cannot tell this study apart in the image store, so the viewer shows none of it.",
    )
}

fn not_in_link() -> ApiError {
    refused("study_not_in_link", "This viewer link opens one study only.")
}

fn dicomweb_refused(why: &str) -> ApiError {
    refused(
        "dicomweb_refused",
        format!("The viewer may not make this request: {why}"),
    )
}

fn method_not_allowed() -> ApiError {
    ApiError::new(
        StatusCode::METHOD_NOT_ALLOWED,
        "method_not_allowed",
        "The viewer only reads (GET).",
    )
}

fn orthanc_down() -> ApiError {
    ApiError::new(
        StatusCode::BAD_GATEWAY,
        "orthanc_unavailable",
        "Orthanc did not answer.",
    )
}

/// `POST /api/profiles/{p}/imaging/studies/{uid}/open`: a viewer link for one study.
async fn open(
    State(rt): State<Runtime>,
    imaging: Option<Extension<Arc<Imaging>>>,
    caller: Caller,
    Path((p, uid)): Path<(String, String)>,
) -> ApiResult<Json<Value>> {
    let imaging = imaging_of(imaging.as_ref())?;
    let principal = &caller.0;
    // Caller already refuses access tokens; a link is for a person in the browser only.
    let holder = match (principal.kind, principal.session_id, principal.user_id) {
        (PrincipalKind::Session, Some(session), _) => Holder::Session(session),
        (PrincipalKind::Dev, _, Some(user)) => Holder::Dev(user),
        _ => {
            return Err(refused(
                "forbidden",
                "Only a person logged in to the web app opens the viewer.",
            ));
        }
    };
    if !is_uid(&uid) {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid",
            "This is not a DICOM StudyInstanceUID.",
        ));
    }
    // A short read first. Orthanc's answer can take up to a minute, and nothing in the database is
    // held while it comes: no connection, no transaction.
    let (profile_id, orthanc_id) = {
        let mut conn = rt.pool.acquire().await?;
        let perm = caller.require(&mut conn, &p, Some("read:records"), None).await?;
        let profile_id = perm.profile.id;
        (profile_id, bound_study(&mut conn, profile_id, &uid).await?)
    };
    bind_check(imaging, &uid, &orthanc_id).await?;
    // Then the same checks again in the audit transaction: the record must still name the Orthanc
    // study that Orthanc just confirmed.
    let mut tx = rt.pool.begin().await?;
    let perm = caller.require(&mut tx, &p, Some("read:records"), None).await?;
    if perm.profile.id != profile_id || bound_study(&mut tx, profile_id, &uid).await? != orthanc_id {
        return Err(not_bound());
    }
    // One audit row per study per session: the lock makes two opens at once write one row.
    let marker = holder.marker();
    sqlx::query("select pg_advisory_xact_lock(hashtext($1))")
        .bind(format!("{AUDIT_ACTION}:{marker}:{profile_id}:{uid}"))
        .execute(&mut *tx)
        .await?;
    let seen: bool = sqlx::query_scalar(
        "select exists (select 1 from audit_event where action = $1 and actor_id = $2 and profile_id = $3 \
         and resource_ids = array[$4]::text[] and args_summary = $5)",
    )
    .bind(AUDIT_ACTION)
    .bind(principal.user_id)
    .bind(profile_id)
    .bind(&uid)
    .bind(&marker)
    .fetch_one(&mut *tx)
    .await?;
    let event = (!seen).then(|| {
        let mut e = AuditEvent::new(principal.actor_kind(), "ui", AUDIT_ACTION);
        e.actor_id = principal.user_id;
        e.profile_id = Some(profile_id);
        e.resource_type = Some("imaging_study".into());
        e.resource_ids = Some(vec![uid.clone()]);
        e.args_summary = Some(marker);
        e
    });
    // The link first: when the server is full, the 503 rolls the transaction back and logs no open.
    // A new link whose audit row does not commit is taken back.
    let (token, left, fresh) = imaging.mint(holder, profile_id, &uid, &orthanc_id).ok_or_else(|| {
        ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "too_many_viewer_links",
            "Too many viewer links are open. Try again later.",
        )
    })?;
    if let Err(e) = commit_open(tx, event.as_ref()).await {
        if fresh {
            imaging.revoke(&token);
        }
        return Err(e);
    }
    Ok(Json(json!({
        "viewer_url": format!("/ohif/t/{token}/viewer?StudyInstanceUIDs={uid}"),
        "expires_in": left.as_secs(),
    })))
}

/// The open's audit row (None: the session opened the study before), then the commit.
async fn commit_open(mut tx: sqlx::Transaction<'_, sqlx::Postgres>, event: Option<&AuditEvent>) -> ApiResult<()> {
    if let Some(e) = event {
        write_audit(&mut tx, e).await?;
    }
    tx.commit().await?;
    Ok(())
}

/// The Orthanc id the profile's record of the study names, or why the study does not open.
async fn bound_study(conn: &mut PgConnection, profile_id: Uuid, uid: &str) -> Result<String, ApiError> {
    match study_row(conn, profile_id, uid).await? {
        None => Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "study_not_found",
            "This profile has no study with that UID.",
        )),
        Some((s, _)) if s == "retracted" => Err(retracted()),
        Some((_, None)) => Err(not_bound()),
        Some((_, Some(id))) => Ok(id),
    }
}

/// The study's status and Orthanc id in that profile (`study_instance_uid` is unique).
async fn study_row(
    conn: &mut PgConnection,
    profile_id: Uuid,
    uid: &str,
) -> Result<Option<(String, Option<String>)>, sqlx::Error> {
    sqlx::query_as("select status, orthanc_id from imaging_study where profile_id = $1 and study_instance_uid = $2")
        .bind(profile_id)
        .bind(uid)
        .fetch_optional(conn)
        .await
}

/// The principal of a live session, by id: the same rules as `sessions::resolve_session` (not revoked,
/// not expired, not idle, user not disabled), and whether its `last_seen_at` is over a minute old. It
/// writes nothing: only a request that passes every check counts as activity ([`touch`]).
async fn live_session(conn: &mut PgConnection, session_id: Uuid) -> Result<Option<(Principal, bool)>, sqlx::Error> {
    let row: Option<(Uuid, Uuid, bool)> = sqlx::query_as(
        "select s.user_id, u.household_id, s.last_seen_at < now() - make_interval(secs => $3) \
         from user_session s join app_user u on u.id = s.user_id \
         where s.id = $1 and s.revoked_at is null and s.expires_at > now() \
         and s.last_seen_at > now() - make_interval(mins => $2) and u.disabled_at is null",
    )
    .bind(session_id)
    .bind(IDLE_MINUTES as i32)
    .bind(f64::from(TOUCH_EVERY_SECS))
    .fetch_optional(&mut *conn)
    .await?;
    Ok(row.map(|(user, household, stale)| {
        let principal = Principal {
            session_id: Some(session_id),
            ..Principal::session(user, household)
        };
        (principal, stale)
    }))
}

/// A viewer request that passed every check counts as activity for its session: it refreshes
/// `last_seen_at`, but at most once a minute, so a long read does not go idle and a busy viewer does
/// not write a row per frame. A refused request refreshes nothing. The session's 12-hour end and the
/// link's hour still hold.
async fn touch(rt: &Runtime, session: Option<Uuid>) -> Result<(), ApiError> {
    let Some(session_id) = session else {
        return Ok(());
    };
    sqlx::query(
        "update user_session set last_seen_at = now() \
         where id = $1 and last_seen_at < now() - make_interval(secs => $2)",
    )
    .bind(session_id)
    .bind(f64::from(TOUCH_EVERY_SECS))
    .execute(&rt.pool)
    .await?;
    Ok(())
}

/// A link that passed [`authorize`], and the session to refresh once the request passes its own
/// checks too (None: nothing to refresh, or refreshed within the minute).
struct Authorized {
    link: Link,
    touch: Option<Uuid>,
}

/// Every check a viewer request passes, each time.
async fn authorize(rt: &Runtime, imaging: &Imaging, token: &str) -> Result<Authorized, ApiError> {
    let link = imaging.lookup(token).ok_or_else(link_invalid)?;
    let mut conn = rt.pool.acquire().await?;
    let (principal, touch) = match link.holder {
        Holder::Session(id) => live_session(&mut conn, id)
            .await?
            .map(|(p, stale)| (p, stale.then_some(id))),
        Holder::Dev(user) if rt.settings.dev_auth => dev_principal(&mut conn)
            .await?
            .filter(|p| p.user_id == Some(user))
            .map(|p| (p, None)),
        Holder::Dev(_) => None,
    }
    .ok_or_else(link_invalid)?;
    match effective_permission(&mut conn, &principal, link.profile_id).await {
        Ok(perm) if perm.has("read:records") => {}
        Ok(_) | Err(Error::AccessDenied { .. }) => return Err(study_not_available()),
        Err(e) => return Err(e.into()),
    }
    match study_row(&mut conn, link.profile_id, &link.study_uid).await? {
        None => Err(study_not_available()),
        Some((s, _)) if s == "retracted" => Err(retracted()),
        Some((_, id)) if id.as_deref() != Some(link.orthanc_id.as_str()) => Err(study_not_available()),
        Some(_) => Ok(Authorized { link, touch }),
    }
}

/// Orthanc's `/tools/lookup` must find exactly one study under `uid`, and it must be `orthanc_id`.
async fn bind_check(imaging: &Imaging, uid: &str, orthanc_id: &str) -> Result<(), ApiError> {
    let orthanc = imaging.orthanc()?;
    let upstream = imaging
        .http
        .post(format!("{}/tools/lookup", orthanc.url))
        .basic_auth(&orthanc.user, Some(&orthanc.password))
        .body(uid.to_string())
        .send()
        .await
        .map_err(|_| orthanc_down())?;
    check_orthanc_auth(&upstream)?;
    if !upstream.status().is_success() {
        return Err(orthanc_down());
    }
    let found: Vec<Value> =
        serde_json::from_slice(&read_limited(upstream, 1 << 20).await?).map_err(|_| orthanc_down())?;
    let studies: Vec<&str> = found
        .iter()
        .filter(|r| r["Type"] == "Study")
        .filter_map(|r| r["ID"].as_str())
        .collect();
    if studies != [orthanc_id] {
        tracing::warn!(
            studies = studies.len(),
            "Orthanc does not hold exactly the recorded study under its UID: the viewer shows none of it"
        );
        return Err(not_bound());
    }
    Ok(())
}

/// `/ohif/*`: the viewer's files from Orthanc's OHIF plugin, under a live link only.
async fn ohif(State(rt): State<Runtime>, imaging: Option<Extension<Arc<Imaging>>>, req: Request) -> Response {
    let (parts, _) = req.into_parts();
    let mut res = match viewer_file(&rt, imaging.as_ref(), &parts).await {
        Ok(r) => r,
        Err(e) => refusal_page(
            &e,
            page_lang(&parts.headers, &rt.settings.default_locale),
            &rt.settings.base_url,
        ),
    };
    let h = res.headers_mut();
    h.insert(header::CONTENT_SECURITY_POLICY, HeaderValue::from_static(VIEWER_CSP));
    // The sandboxed page loads its own files cross-origin (its origin is opaque).
    h.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, HeaderValue::from_static("null"));
    h.insert("cross-origin-resource-policy", HeaderValue::from_static("cross-origin"));
    common_headers(h);
    res
}

async fn viewer_file(rt: &Runtime, ext: Option<&Extension<Arc<Imaging>>>, req: &Parts) -> Result<Response, ApiError> {
    if req.method != Method::GET {
        return Err(method_not_allowed());
    }
    let imaging = imaging_of(ext)?;
    let (token, file) = req
        .uri
        .path()
        .strip_prefix("/ohif/t/")
        .and_then(|r| r.split_once('/'))
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::NOT_FOUND,
                "viewer_link_missing",
                "The viewer opens from a study in OwnHealth.",
            )
        })?;
    if !is_viewer_file(file) {
        return Err(refused("viewer_file_refused", "This is not a file of the viewer."));
    }
    let ok = authorize(rt, imaging, token).await?;
    touch(rt, ok.touch).await?;
    let orthanc = imaging.orthanc()?;
    let upstream = imaging
        .http
        .get(format!("{}/ohif/{file}", orthanc.url))
        .basic_auth(&orthanc.user, Some(&orthanc.password))
        .send()
        .await
        .map_err(|_| orthanc_down())?;
    check_orthanc_auth(&upstream)?;
    if file == "app-config.js" && upstream.status().is_success() {
        let plugin = read_limited(upstream, MAX_JSON_BYTES).await?;
        let js = app_config(&plugin, &rt.settings.base_url, token);
        return Ok(answer(StatusCode::OK, "text/javascript; charset=utf-8", js));
    }
    Ok(passthrough(upstream))
}

/// `/dicom-web/*`: DICOMweb reads for the link's study, from Orthanc.
async fn dicomweb(State(rt): State<Runtime>, imaging: Option<Extension<Arc<Imaging>>>, req: Request) -> Response {
    let (parts, _) = req.into_parts();
    let mut res = if parts.method == Method::OPTIONS {
        // A CORS preflight carries no credentials and is answered alike for any path.
        let mut r = StatusCode::NO_CONTENT.into_response();
        let h = r.headers_mut();
        h.insert(header::ACCESS_CONTROL_ALLOW_METHODS, HeaderValue::from_static("GET"));
        h.insert(header::ACCESS_CONTROL_ALLOW_HEADERS, HeaderValue::from_static("accept"));
        h.insert(header::ACCESS_CONTROL_MAX_AGE, HeaderValue::from_static("600"));
        r
    } else {
        match proxy(&rt, imaging.as_ref(), &parts).await {
            Ok(r) => r,
            Err(e) => e.into_response(),
        }
    };
    let has_body = res.status() != StatusCode::NO_CONTENT;
    let h = res.headers_mut();
    // CORS for the sandboxed viewer, never with credentials.
    h.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, HeaderValue::from_static("null"));
    h.insert(header::CONTENT_SECURITY_POLICY, HeaderValue::from_static(DICOMWEB_CSP));
    if has_body && !inline_type(h.get(header::CONTENT_TYPE)) {
        h.insert(header::CONTENT_DISPOSITION, HeaderValue::from_static("attachment"));
    }
    common_headers(h);
    if res.status() == StatusCode::METHOD_NOT_ALLOWED {
        res.headers_mut().insert(header::ALLOW, HeaderValue::from_static("GET"));
    }
    res
}

async fn proxy(rt: &Runtime, ext: Option<&Extension<Arc<Imaging>>>, req: &Parts) -> Result<Response, ApiError> {
    if req.method != Method::GET {
        return Err(method_not_allowed());
    }
    let imaging = imaging_of(ext)?;
    let (token, rest) = req
        .uri
        .path()
        .strip_prefix("/dicom-web/t/")
        .and_then(|r| r.split_once('/'))
        .ok_or_else(ApiError::not_found)?;
    let Authorized { link, touch: session } = authorize(rt, imaging, token).await?;
    let Query(query) = Query::<Vec<(String, String)>>::try_from_uri(&req.uri)
        .map_err(|_| dicomweb_refused("the query does not parse."))?;
    let target = dicomweb_target(rest, &query, &link.study_uid)?;
    if link.checked.elapsed() >= imaging.recheck {
        bind_check(imaging, &link.study_uid, &link.orthanc_id).await?;
        imaging.mark_checked(token);
    }
    // Every check passed: only now does the request count as activity.
    touch(rt, session).await?;
    let orthanc = imaging.orthanc()?;
    let mut call = imaging
        .http
        .get(format!("{}/dicom-web/{target}", orthanc.url))
        .basic_auth(&orthanc.user, Some(&orthanc.password));
    if let Some(accept) = req
        .headers
        .get(header::ACCEPT)
        .filter(|v| v.len() <= 512 && v.to_str().is_ok())
    {
        call = call.header(header::ACCEPT, accept.clone());
    }
    let upstream = call.send().await.map_err(|_| orthanc_down())?;
    check_orthanc_auth(&upstream)?;
    let json = upstream
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|ct| ct.starts_with("application/dicom+json") || ct.starts_with("application/json"));
    if !json {
        return Ok(passthrough(upstream));
    }
    // Orthanc writes absolute URLs (RetrieveURL, BulkDataURI) from the Host it was called with: turn
    // them into links through this capability.
    let status = upstream.status();
    let ctype = upstream.headers().get(header::CONTENT_TYPE).cloned();
    let body = read_limited(upstream, MAX_JSON_BYTES).await?;
    let from = format!("\"{}/dicom-web/", orthanc.url);
    let to = format!("\"{}/dicom-web/t/{token}/", rt.settings.base_url.trim_end_matches('/'));
    let body = match String::from_utf8(body) {
        Ok(text) => text.replace(&from, &to).into_bytes(),
        Err(e) => e.into_bytes(),
    };
    let mut res = Response::new(Body::from(body));
    *res.status_mut() = status;
    if let Some(ct) = ctype {
        res.headers_mut().insert(header::CONTENT_TYPE, ct);
    }
    Ok(res)
}

/// The Orthanc path and query for a viewer request, or why it is refused. The study comes from the path
/// (and must be the link's), or from a search that names exactly that one UID.
fn dicomweb_target(rest: &str, query: &[(String, String)], study_uid: &str) -> Result<String, ApiError> {
    let path_chars_ok = rest
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'/' | b',' | b'-'));
    if rest.is_empty() || rest.len() > 1024 || !path_chars_ok {
        return Err(dicomweb_refused("this path."));
    }
    let segments: Vec<&str> = rest.split('/').collect();
    let search = match segments.as_slice() {
        ["studies"] => true,
        ["studies", study, tail @ ..] => {
            if !is_uid(study) {
                return Err(dicomweb_refused("studies are named by their StudyInstanceUID."));
            }
            if *study != study_uid {
                return Err(not_in_link());
            }
            if !study_tail_ok(tail) {
                return Err(dicomweb_refused("this path."));
            }
            false
        }
        _ => return Err(dicomweb_refused("this path.")),
    };
    let mut named = 0;
    for (key, value) in query {
        match key.as_str() {
            "StudyInstanceUID" | "0020000D" => {
                if !is_uid(value) {
                    return Err(dicomweb_refused(
                        "a search names one StudyInstanceUID, no list or wildcard.",
                    ));
                }
                if value != study_uid {
                    return Err(not_in_link());
                }
                named += 1;
            }
            "SeriesInstanceUID" | "0020000E" | "SOPInstanceUID" | "00080018" if is_uid(value) => {}
            "includefield" | "limit" | "offset" | "fuzzymatching" | "quality" | "viewport" | "window"
                if value.len() <= 512
                    && value
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b',' | b'.' | b'-')) => {}
            _ => return Err(dicomweb_refused(&format!("the query parameter {key}."))),
        }
    }
    if search && named != 1 {
        return Err(dicomweb_refused("a study search names exactly one StudyInstanceUID."));
    }
    if query.is_empty() {
        return Ok(rest.to_string());
    }
    let encoded: Vec<String> = query
        .iter()
        .map(|(k, v)| format!("{}={}", query_encode(k), query_encode(v)))
        .collect();
    Ok(format!("{rest}?{}", encoded.join("&")))
}

/// Types a browser may show inline from `/dicom-web`: images, video, DICOM, JSON and DICOMweb's
/// multipart. Anything else (an encapsulated PDF, text) is a download.
fn inline_type(ctype: Option<&HeaderValue>) -> bool {
    let ct = ctype
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    [
        "image/",
        "video/",
        "application/dicom",
        "application/json",
        "multipart/related",
    ]
    .iter()
    .any(|p| ct.starts_with(p))
}

/// What may follow `studies/{uid}/`: the reads OHIF makes (metadata, searches inside the study, frames,
/// rendered images, thumbnails, bulk data) and one instance. Never a whole study or series download.
fn study_tail_ok(tail: &[&str]) -> bool {
    match tail {
        ["metadata" | "series" | "rendered" | "thumbnail"] => true,
        ["series", series, rest @ ..] if is_uid(series) => match rest {
            ["metadata" | "instances" | "rendered" | "thumbnail"] => true,
            ["instances", instance, rest @ ..] if is_uid(instance) => match rest {
                [] | ["metadata" | "rendered" | "thumbnail"] => true,
                ["frames", frames] | ["frames", frames, "rendered" | "thumbnail"] => is_frame_list(frames),
                ["bulk", tags @ ..] => !tags.is_empty() && tags.len() <= 16 && tags.iter().all(|t| is_bulk_part(t)),
                _ => false,
            },
            _ => false,
        },
        _ => false,
    }
}

/// A DICOM UID: digits in dot-separated parts, at most 64 characters. Rules out `..`, lists and wildcards.
fn is_uid(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.split('.')
            .all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()))
}

fn is_frame_list(s: &str) -> bool {
    s.len() <= 512
        && s.split(',')
            .all(|n| !n.is_empty() && n.len() <= 6 && n.bytes().all(|b| b.is_ascii_digit()))
}

/// A bulk data path part: a tag (8 hex digits) or an item index.
fn is_bulk_part(s: &str) -> bool {
    (s.len() == 8 && s.bytes().all(|b| b.is_ascii_hexdigit()))
        || (!s.is_empty() && s.len() <= 6 && s.bytes().all(|b| b.is_ascii_digit()))
}

/// A capability: 32 random bytes, base64url without padding.
fn is_token(s: &str) -> bool {
    s.len() == 43 && s.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
}

/// A file of the viewer: plain names, no dot segments. Empty is the viewer's index page.
fn is_viewer_file(file: &str) -> bool {
    file.is_empty()
        || (file.len() <= 256
            && file.split('/').all(|part| {
                !part.is_empty()
                    && !part.starts_with('.')
                    && part
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_'))
            }))
}

fn query_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Orthanc refusing our own account is a setup error, not the person's: say so and log it.
fn check_orthanc_auth(upstream: &reqwest::Response) -> Result<(), ApiError> {
    if matches!(upstream.status(), StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN) {
        tracing::error!(
            status = upstream.status().as_u16(),
            "Orthanc refused OwnHealth's account: check its password"
        );
        return Err(ApiError::new(
            StatusCode::BAD_GATEWAY,
            "orthanc_refused",
            "Orthanc refused OwnHealth's account.",
        ));
    }
    Ok(())
}

async fn read_limited(mut upstream: reqwest::Response, max: usize) -> Result<Vec<u8>, ApiError> {
    let mut out = Vec::new();
    while let Some(chunk) = upstream.chunk().await.map_err(|_| orthanc_down())? {
        if out.len() + chunk.len() > max {
            return Err(ApiError::new(
                StatusCode::BAD_GATEWAY,
                "orthanc_answer_too_large",
                "Orthanc's answer is too large.",
            ));
        }
        out.extend_from_slice(&chunk);
    }
    Ok(out)
}

/// Orthanc's answer as it comes, streamed, with its status and content type only: none of its other
/// headers (its own isolation and auth headers) reach the viewer.
fn passthrough(upstream: reqwest::Response) -> Response {
    let status = upstream.status();
    let ctype = upstream.headers().get(header::CONTENT_TYPE).cloned();
    let chunks = futures::stream::unfold(Some(upstream), |state| async move {
        let mut upstream = state?;
        match upstream.chunk().await {
            Ok(Some(bytes)) => Some((Ok(bytes), Some(upstream))),
            Ok(None) => None,
            Err(e) => Some((Err(e), None)),
        }
    });
    let mut res = Response::new(Body::from_stream(chunks));
    *res.status_mut() = status;
    if let Some(ct) = ctype {
        res.headers_mut().insert(header::CONTENT_TYPE, ct);
    }
    res
}

fn answer(status: StatusCode, content_type: &'static str, body: Vec<u8>) -> Response {
    let mut res = Response::new(Body::from(body));
    *res.status_mut() = status;
    res.headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    res
}

/// Health data in every answer: never cached, never sniffed, no referrer.
fn common_headers(h: &mut HeaderMap) {
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    h.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    h.insert(header::REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
}

/// The language of a page under `/ohif`, picked as the app picks it with nothing saved: the browser's
/// first language, Spanish when it is Spanish, else English; with no `Accept-Language`, the server's
/// default (`OWNHEALTH_DEFAULT_LOCALE`). A sandboxed page cannot read the app's saved choice.
fn page_lang(headers: &HeaderMap, default: &str) -> Lang {
    let first = headers
        .get(header::ACCEPT_LANGUAGE)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(',').next())
        .map(|t| t.trim().to_ascii_lowercase())
        .filter(|t| !t.is_empty());
    match first.as_deref().unwrap_or(default) {
        t if t.starts_with("es") => Lang::Es,
        _ => Lang::En,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Lang {
    En,
    Es,
}

/// The words of every refusal page, (code, English, Spanish). A code not here gets the last line.
const REFUSALS: &[(&str, &str, &str)] = &[
    (
        "viewer_link_invalid",
        "This viewer link is not valid any more. A link works for one hour, and only while you stay signed in to OwnHealth; opening the study there again gives a new one.",
        "Este enlace del visor ya no sirve. Un enlace dura una hora, y solo mientras sigues con la sesión iniciada en OwnHealth; al abrir allí el estudio otra vez se crea uno nuevo.",
    ),
    (
        "viewer_link_missing",
        "The viewer opens from a study in OwnHealth.",
        "El visor se abre desde un estudio en OwnHealth.",
    ),
    (
        "viewer_file_refused",
        "This is not a file of the viewer.",
        "Esto no es un archivo del visor.",
    ),
    (
        "study_not_available",
        "This study is not available to you.",
        "Este estudio no está disponible para ti.",
    ),
    (
        "study_retracted",
        "This study was retracted.",
        "Este estudio se retiró.",
    ),
    (
        "imaging_off",
        "Imaging is off on this server.",
        "Las imágenes médicas están desactivadas en este servidor.",
    ),
    ("method_not_allowed", "The viewer only reads.", "El visor solo lee."),
    (
        "",
        "The viewer cannot open this study now.",
        "El visor no puede abrir este estudio ahora.",
    ),
];

/// Every text a refusal page can show (the wording lint reads them).
pub fn phrases() -> Value {
    let pages: Vec<Value> = REFUSALS
        .iter()
        .map(|(code, en, es)| json!({"code": code, "en": en, "es": es}))
        .collect();
    json!({"pages": pages, "back": [BACK.0, BACK.1]})
}

/// The link back to the app, (English, Spanish).
const BACK: (&str, &str) = ("Back to OwnHealth", "Volver a OwnHealth");

fn html_escape(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            '<' => "&lt;".to_string(),
            '>' => "&gt;".to_string(),
            '&' => "&amp;".to_string(),
            '"' => "&quot;".to_string(),
            '\'' => "&#39;".to_string(),
            c => c.to_string(),
        })
        .collect()
}

/// A refusal under `/ohif` is a page too, sandboxed like the viewer, in the person's language, with a
/// link back to the app (a sandboxed page may still navigate itself).
fn refusal_page(e: &ApiError, lang: Lang, base_url: &str) -> Response {
    let (_, en, es) = REFUSALS
        .iter()
        .find(|(code, _, _)| *code == e.code)
        .or_else(|| REFUSALS.last())
        .copied()
        .unwrap_or(("", "", ""));
    let (tag, message, back) = match lang {
        Lang::Es => ("es", es, BACK.1),
        Lang::En => ("en", en, BACK.0),
    };
    let home = html_escape(&format!("{}/", base_url.trim_end_matches('/')));
    let html = format!(
        "<!doctype html><html lang=\"{tag}\"><meta charset=\"utf-8\"><title>OwnHealth</title>\
         <body style=\"background:#0f1115;color:#e6e8ee;font:15px system-ui;padding:2rem\"><p>{}</p>\
         <p><a href=\"{home}\" style=\"color:#8ab4ff\">{back}</a></p></body></html>",
        html_escape(message)
    );
    answer(e.status, "text/html; charset=utf-8", html.into_bytes())
}

/// The plugin's `app-config.js` with the sandbox shims before it and our data source after it.
fn app_config(plugin: &[u8], base_url: &str, token: &str) -> Vec<u8> {
    let root = format!("{}/dicom-web/t/{token}", base_url.trim_end_matches('/'));
    let basename = format!("/ohif/t/{token}/");
    let js_string = |s: &str| serde_json::to_string(s).unwrap_or_else(|_| "\"\"".into());
    let tail = CONFIG_OVERRIDE
        .replace("__ROOT__", &js_string(&root))
        .replace("__BASENAME__", &js_string(&basename));
    let mut out = Vec::with_capacity(SANDBOX_SHIMS.len() + plugin.len() + tail.len());
    out.extend_from_slice(SANDBOX_SHIMS.as_bytes());
    out.extend_from_slice(plugin);
    out.extend_from_slice(tail.as_bytes());
    out
}

/// Runs before OHIF. In the opaque origin, reading cookies, storage or the service worker throws, and a
/// worker cannot start from (or `importScripts`) a server URL. These shims give OHIF an empty cookie,
/// in-memory storage and no service worker, and start each worker from a `blob:` URL that loads the real
/// script over CORS. They give it nothing it could not reach already.
const SANDBOX_SHIMS: &str = r#"/* OwnHealth: the viewer runs sandboxed, in an opaque origin. */
;(function () {
  'use strict';
  function memoryStorage() {
    var items = new Map();
    return {
      getItem: function (k) { k = String(k); return items.has(k) ? items.get(k) : null; },
      setItem: function (k, v) { items.set(String(k), String(v)); },
      removeItem: function (k) { items.delete(String(k)); },
      clear: function () { items.clear(); },
      key: function (i) { var keys = Array.from(items.keys()); return i < keys.length ? keys[i] : null; },
      get length() { return items.size; }
    };
  }
  function define(target, name, value) {
    try {
      Object.defineProperty(target, name, { configurable: true, get: function () { return value; }, set: function () {} });
    } catch (e) { /* the browser's own stays */ }
  }
  // Runs inside each worker: scripts load over CORS and run from blob: URLs, and the worker reports the
  // real script URL as its location, so relative chunk and codec paths still resolve.
  function workerPrelude(base) {
    try { Object.defineProperty(self, 'location', { configurable: true, get: function () { return new URL(base); } }); } catch (e) {}
    var load = self.importScripts.bind(self);
    self.importScripts = function () {
      for (var i = 0; i < arguments.length; i++) {
        var url = new URL(String(arguments[i]), base).href;
        var xhr = new XMLHttpRequest();
        xhr.open('GET', url, false);
        xhr.send();
        if (xhr.status !== 200) { throw new DOMException('Could not load ' + url, 'NetworkError'); }
        load(URL.createObjectURL(new Blob([xhr.responseText + '\n//# sourceURL=' + url], { type: 'text/javascript' })));
      }
    };
    self.importScripts(base);
  }
  var NativeWorker = window.Worker;
  if (NativeWorker) {
    var SandboxWorker = function (url, options) {
      var href = new URL(String(url), document.baseURI).href;
      if (href.indexOf('blob:') === 0 || href.indexOf('data:') === 0) { return new NativeWorker(url, options); }
      var source = options && options.type === 'module'
        ? 'import ' + JSON.stringify(href) + ';'
        : '(' + workerPrelude.toString() + ')(' + JSON.stringify(href) + ');';
      return new NativeWorker(URL.createObjectURL(new Blob([source], { type: 'text/javascript' })), options);
    };
    SandboxWorker.prototype = NativeWorker.prototype;
    window.Worker = SandboxWorker;
  }
  define(document, 'cookie', '');
  define(window, 'localStorage', memoryStorage());
  define(window, 'sessionStorage', memoryStorage());
  define(navigator, 'serviceWorker', {
    getRegistrations: function () { return Promise.resolve([]); },
    getRegistration: function () { return Promise.resolve(undefined); },
    register: function () { return Promise.reject(new Error('The viewer has no service worker.')); },
    addEventListener: function () {},
    removeEventListener: function () {},
    ready: new Promise(function () {})
  });
})();
"#;

/// Runs after the plugin's config: DICOMweb only through this link, and no study list.
const CONFIG_OVERRIDE: &str = r#"
/* OwnHealth: this viewer reads one study, through its own link. */
;(function () {
  'use strict';
  var root = __ROOT__;
  var config = (window.config = window.config || {});
  config.routerBasename = __BASENAME__;
  config.showStudyList = false;
  config.dataSources = [
    {
      namespace: '@ohif/extension-default.dataSourcesModule.dicomweb',
      sourceName: 'dicomweb',
      configuration: {
        friendlyName: 'OwnHealth',
        name: 'ownhealth',
        wadoUriRoot: root,
        qidoRoot: root,
        wadoRoot: root,
        qidoSupportsIncludeField: false,
        supportsReject: false,
        supportsStow: false,
        imageRendering: 'wadors',
        thumbnailRendering: 'wadors',
        enableStudyLazyLoad: true,
        supportsFuzzyMatching: false,
        supportsWildcard: false,
        staticWado: true,
        singlepart: 'bulkdata,video',
        acceptHeader: ['multipart/related; type=application/octet-stream; transfer-syntax=*'],
        bulkDataURI: { enabled: true, relativeResolution: 'studies' }
      }
    }
  ];
  config.defaultDataSourceName = 'dicomweb';
  // The sandbox forgets OHIF's "seen" flags (storage is in memory), so turn the investigational-use
  // banner and the onboarding tour off instead of showing them on every open.
  config.investigationalUseDialog = { option: 'never' };
  var tours = { 'ohif.tours': { $set: [] } };
  var cs = config.customizationService;
  config.customizationService = Array.isArray(cs) ? cs.concat([tours]) : [cs || {}, tours];
})();
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uids_are_digits_and_single_dots() {
        assert!(is_uid("1.2.826.0.1.3680043.10.999.1"));
        for bad in [
            "",
            "1..2",
            ".1.2",
            "1.2.",
            "1.2.*",
            "1,2",
            "1\\2",
            "abc",
            "../1",
            &"1".repeat(65),
        ] {
            assert!(!is_uid(bad), "{bad}");
        }
    }

    #[test]
    fn viewer_files_have_no_dot_segments() {
        assert!(is_viewer_file(""));
        assert!(is_viewer_file("viewer"));
        assert!(is_viewer_file("assets/favicon-16x16.png"));
        assert!(is_viewer_file("app.bundle.cf7306f0850f48da4eeb.js"));
        for bad in [
            "..",
            "../app/explorer.html",
            "a//b",
            "a/./b",
            ".env",
            "a%2Fb",
            "a\\b",
            "/abs",
        ] {
            assert!(!is_viewer_file(bad), "{bad}");
        }
    }

    #[test]
    fn the_query_is_rebuilt_encoded() {
        let q = vec![
            ("includefield".to_string(), "00081030,00080060".to_string()),
            ("StudyInstanceUID".to_string(), "1.2.3".to_string()),
        ];
        assert_eq!(
            dicomweb_target("studies", &q, "1.2.3").map_err(|e| e.code),
            Ok("studies?includefield=00081030%2C00080060&StudyInstanceUID=1.2.3".to_string())
        );
    }

    #[test]
    fn the_config_override_names_the_link_and_hides_the_study_list() {
        let js = String::from_utf8(app_config(b"window.config = {};", "https://h.test/", "T")).unwrap_or_default();
        assert!(js.starts_with("/* OwnHealth: the viewer runs sandboxed"));
        assert!(js.contains("var root = \"https://h.test/dicom-web/t/T\";"));
        assert!(js.contains("config.routerBasename = \"/ohif/t/T/\";"));
        assert!(js.contains("config.showStudyList = false;"));
        // The sandbox forgets OHIF's "seen" flags, so the banner and the tour are off, not shown each time.
        assert!(js.contains("config.investigationalUseDialog = { option: 'never' };"));
        assert!(js.contains("'ohif.tours': { $set: [] }"));
    }

    fn holder(n: u128) -> Holder {
        Holder::Session(Uuid::from_u128(n))
    }

    #[test]
    fn a_second_open_of_the_study_reuses_the_live_link() {
        let imaging = Imaging::new(None);
        let p = Uuid::from_u128(7);
        let (a, left, fresh) = imaging.mint(holder(1), p, "1.2.3", "o1").unwrap_or_default();
        assert_eq!((left, fresh), (CAPABILITY_TTL, true));
        assert_eq!(
            imaging
                .mint(holder(1), p, "1.2.3", "o1")
                .map(|(t, _, fresh)| (t, fresh)),
            Some((a.clone(), false))
        );
        // Another session, another study or another Orthanc study: a new link.
        assert_ne!(
            imaging.mint(holder(2), p, "1.2.3", "o1").map(|(t, ..)| t),
            Some(a.clone())
        );
        assert_ne!(
            imaging.mint(holder(1), p, "1.2.4", "o1").map(|(t, ..)| t),
            Some(a.clone())
        );
        assert_ne!(
            imaging.mint(holder(1), p, "1.2.3", "o2").map(|(t, ..)| t),
            Some(a.clone())
        );
        // A link close to its end is not handed out again.
        let short = Imaging::with_ttl(None, REUSE_LEFT - Duration::from_secs(1));
        let (b, ..) = short.mint(holder(1), p, "1.2.3", "o1").unwrap_or_default();
        assert_ne!(short.mint(holder(1), p, "1.2.3", "o1").map(|(t, ..)| t), Some(b));
    }

    #[test]
    fn a_full_server_still_hands_back_a_live_link_near_its_end() {
        // Every link here has less than REUSE_LEFT left, so a second open would make a new one.
        let imaging = Imaging::with_ttl(None, REUSE_LEFT - Duration::from_secs(1));
        let p = Uuid::from_u128(7);
        let (a, ..) = imaging.mint(holder(1), p, "1.2.3", "o1").unwrap_or_default();
        let mut n = 2;
        while imaging
            .mint(holder(n as u128 / 40 + 2), p, &format!("1.2.{n}"), "o")
            .is_some()
        {
            n += 1;
            assert!(n <= MAX_LINKS + 1, "the server never filled up");
        }
        // Full: the same open gets its live link back, not a refusal; a new study still waits.
        assert_eq!(
            imaging
                .mint(holder(1), p, "1.2.3", "o1")
                .map(|(t, _, fresh)| (t, fresh)),
            Some((a.clone(), false))
        );
        assert!(imaging.mint(holder(1), p, "1.2.4", "o1").is_none());
        // An open whose audit row does not commit takes its new link back.
        assert!(imaging.lookup(&a).is_some());
        imaging.revoke(&a);
        assert!(imaging.lookup(&a).is_none());
    }

    #[test]
    fn one_person_cannot_use_up_the_links_of_everyone() {
        let imaging = Imaging::new(None);
        let p = Uuid::from_u128(7);
        let mut first = String::new();
        let mut last = String::new();
        for i in 0..MAX_LINKS + 50 {
            let (t, ..) = imaging.mint(holder(1), p, &format!("1.2.{i}"), "o").unwrap_or_default();
            assert!(!t.is_empty(), "link {i} refused");
            if i == 0 {
                first = t.clone();
            }
            last = t;
        }
        let held = imaging.links.lock().map(|l| l.len()).unwrap_or_default();
        assert_eq!(held, LINKS_PER_HOLDER);
        assert!(imaging.lookup(&first).is_none(), "the oldest link went first");
        assert!(imaging.lookup(&last).is_some());
        // Everyone else still gets a link.
        assert!(imaging.mint(holder(2), p, "1.2.3", "o").is_some());
    }

    #[test]
    fn the_orthanc_check_stands_for_seconds_only() {
        // A study that arrives under the UID can pass until the next check, so the time stays short.
        assert!(ORTHANC_RECHECK <= Duration::from_secs(10), "{ORTHANC_RECHECK:?}");
    }

    #[test]
    fn a_json_answer_is_capped_well_below_the_memory_of_a_small_box() {
        // The proxy buffers it and the URL rewrite copies it twice, with many requests in flight.
        assert!((32 << 20..=64 << 20).contains(&MAX_JSON_BYTES), "{MAX_JSON_BYTES}");
    }

    #[test]
    fn a_refusal_page_picks_the_language_as_the_app_does() {
        let h = |v: &str| {
            let mut m = HeaderMap::new();
            m.insert(
                header::ACCEPT_LANGUAGE,
                HeaderValue::from_str(v).unwrap_or(HeaderValue::from_static("")),
            );
            m
        };
        assert_eq!(page_lang(&h("es-ES,en;q=0.8"), "en"), Lang::Es);
        assert_eq!(page_lang(&h("en-US,es;q=0.8"), "es"), Lang::En);
        assert_eq!(page_lang(&h("de"), "es"), Lang::En);
        assert_eq!(page_lang(&HeaderMap::new(), "es"), Lang::Es);
        assert_eq!(page_lang(&HeaderMap::new(), "en"), Lang::En);
        // Every refusal has both languages, and the fallback is last.
        assert!(
            REFUSALS
                .iter()
                .all(|(_, en, es)| !en.is_empty() && !es.is_empty() && en != es)
        );
        assert_eq!(REFUSALS.last().map(|r| r.0), Some(""));
    }
}
