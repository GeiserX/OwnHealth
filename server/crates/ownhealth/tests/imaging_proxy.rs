//! v1.0 imaging: the viewer and the DICOMweb proxy in front of Orthanc (architecture.md section 4).
//!
//! Orthanc knows nothing of profiles, so the app is the boundary. A browser session opens one study
//! and gets a capability link; the viewer under it runs sandboxed in an opaque origin; every DICOMweb
//! read is checked again against the session, the profile's `read:records` and the study. Each refusal
//! has its own test, and each one checks that Orthanc never saw the request. A fake Orthanc stands in
//! for the real one: it builds absolute URLs from the Host header it gets, as Orthanc does.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::extract::Request;
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use ownhealth::app::build_app_with;
use ownhealth_api::routes::imaging_proxy::{AUDIT_ACTION, DICOMWEB_CSP, Imaging, OrthancConfig, VIEWER_CSP};
use ownhealth_auth::sessions;
use ownhealth_auth::tokens::{TokenIn, create_token};
use ownhealth_core::Runtime;
use ownhealth_core::access::all_scopes;
use ownhealth_core::demo::{Family, create_rivera_family};
use ownhealth_core::testing::TestDb;
use serde_json::{Value, json};
use tower::ServiceExt;
use uuid::Uuid;

type R = anyhow::Result<()>;

const BASE: &str = "https://health.test";
const PASSWORD: &str = "synthetic-orthanc-password";
const ALEX_CT: &str = "1.2.826.0.1.3680043.10.999.1";
const ALEX_SERIES: &str = "1.2.826.0.1.3680043.10.999.1.1";
const ALEX_INSTANCE: &str = "1.2.826.0.1.3680043.10.999.1.1.1";
const CARMEN_MR: &str = "1.2.826.0.1.3680043.10.999.2";
const LUCIA_XR: &str = "1.2.826.0.1.3680043.10.999.3";
const SAM_US: &str = "1.2.826.0.1.3680043.10.999.4";
const ORTHANC_ID: &str = "522ef0ce-f5570074-52b2faa2-781ae81c-c87098aa";

/// One request the fake Orthanc received.
#[derive(Debug, Clone)]
struct Seen {
    method: Method,
    uri: String,
    authorization: Option<String>,
    cookie: bool,
    accept: Option<String>,
}

/// The Orthanc studies by StudyInstanceUID: Orthanc keys a study by PatientID and UID, so one UID can
/// name two studies (two patients).
type Studies = Arc<Mutex<HashMap<String, Vec<String>>>>;

#[derive(Clone)]
struct FakeOrthanc {
    url: String,
    /// Every request but the UID lookups: what could carry image data.
    seen: Arc<Mutex<Vec<Seen>>>,
    /// The bodies of `POST /tools/lookup`.
    lookups: Arc<Mutex<Vec<String>>>,
    studies: Studies,
    /// While a test holds this for writing, `/tools/lookup` waits before it answers.
    pause: Arc<tokio::sync::RwLock<()>>,
}

impl FakeOrthanc {
    async fn start() -> anyhow::Result<FakeOrthanc> {
        let seen: Arc<Mutex<Vec<Seen>>> = Arc::default();
        let lookups: Arc<Mutex<Vec<String>>> = Arc::default();
        let studies: Studies = Arc::default();
        let pause: Arc<tokio::sync::RwLock<()>> = Arc::default();
        let (log, looked, known, paused) = (seen.clone(), lookups.clone(), studies.clone(), pause.clone());
        let app = Router::new().fallback(move |req: Request| {
            let (log, looked, known, paused) = (log.clone(), looked.clone(), known.clone(), paused.clone());
            async move {
                if req.method() == Method::POST && req.uri().path() == "/tools/lookup" {
                    let answer = fake_lookup(&looked, &known, req).await;
                    let _wait = paused.read().await;
                    return answer;
                }
                fake_answer(&log, req)
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let url = format!("http://{}", listener.local_addr()?);
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        Ok(FakeOrthanc {
            url,
            seen,
            lookups,
            studies,
            pause,
        })
    }

    fn seen(&self) -> Vec<Seen> {
        self.seen.lock().map(|s| s.clone()).unwrap_or_default()
    }

    fn lookups(&self) -> Vec<String> {
        self.lookups.lock().map(|s| s.clone()).unwrap_or_default()
    }

    /// One more Orthanc study under `uid`.
    fn add_study(&self, uid: &str, orthanc_id: &str) {
        if let Ok(mut s) = self.studies.lock() {
            s.entry(uid.to_string()).or_default().push(orthanc_id.to_string());
        }
    }

    fn clear(&self) {
        if let Ok(mut s) = self.seen.lock() {
            s.clear();
        }
    }
}

/// `POST /tools/lookup` as Orthanc answers it: every resource whose DICOM UID is the body.
async fn fake_lookup(
    looked: &Mutex<Vec<String>>,
    known: &Mutex<HashMap<String, Vec<String>>>,
    req: Request,
) -> Response {
    let body = to_bytes(req.into_body(), 1 << 16).await.unwrap_or_default();
    let uid = String::from_utf8_lossy(&body).trim().to_string();
    if let Ok(mut l) = looked.lock() {
        l.push(uid.clone());
    }
    let ids = known
        .lock()
        .map(|k| k.get(&uid).cloned().unwrap_or_default())
        .unwrap_or_default();
    let found: Vec<Value> = ids
        .iter()
        .map(|id| json!({"ID": id, "Path": format!("/studies/{id}"), "Type": "Study"}))
        .collect();
    let mut r = (StatusCode::OK, Value::Array(found).to_string()).into_response();
    r.headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static("application/json"));
    r
}

fn fake_answer(log: &Mutex<Vec<Seen>>, req: Request) -> Response {
    let h = req.headers();
    let text = |name: header::HeaderName| h.get(name).and_then(|v| v.to_str().ok()).map(str::to_string);
    if let Ok(mut s) = log.lock() {
        s.push(Seen {
            method: req.method().clone(),
            uri: req.uri().to_string(),
            authorization: text(header::AUTHORIZATION),
            cookie: h.contains_key(header::COOKIE),
            accept: text(header::ACCEPT),
        });
    }
    let host = text(header::HOST).unwrap_or_default();
    let path = req.uri().path().to_string();
    if req.method() != Method::GET {
        return (StatusCode::OK, "stored").into_response(); // a write that got through would show here
    }
    let json = |v: Value| {
        let mut r = (StatusCode::OK, v.to_string()).into_response();
        r.headers_mut()
            .insert(header::CONTENT_TYPE, HeaderValue::from_static("application/dicom+json"));
        r
    };
    if path == "/ohif/app-config.js" {
        let mut r = (
            StatusCode::OK,
            "window.config = { routerBasename: '/ohif/', dataSources: [] };",
        )
            .into_response();
        r.headers_mut()
            .insert(header::CONTENT_TYPE, HeaderValue::from_static("text/javascript"));
        return r;
    }
    if path.starts_with("/ohif/") {
        let mut r = (StatusCode::OK, "<!doctype html><title>OHIF Viewer</title>").into_response();
        let hm = r.headers_mut();
        hm.insert(header::CONTENT_TYPE, HeaderValue::from_static("text/html"));
        hm.insert("cross-origin-resource-policy", HeaderValue::from_static("same-origin"));
        hm.insert("cross-origin-embedder-policy", HeaderValue::from_static("require-corp"));
        hm.insert("cross-origin-opener-policy", HeaderValue::from_static("same-origin"));
        return r;
    }
    if path == "/dicom-web/studies" {
        return json(json!([{
            "0020000D": {"vr": "UI", "Value": [ALEX_CT]},
            "00081190": {"vr": "UR", "Value": [format!("http://{host}/dicom-web/studies/{ALEX_CT}")]}
        }]));
    }
    if path.ends_with("/metadata") {
        return json(json!([{
            "00080018": {"vr": "UI", "Value": [ALEX_INSTANCE]},
            "7FE00010": {"vr": "OW", "BulkDataURI": format!(
                "http://{host}/dicom-web/studies/{ALEX_CT}/series/{ALEX_SERIES}/instances/{ALEX_INSTANCE}/bulk/7fe00010")}
        }]));
    }
    if path.ends_with("/rendered") && path.contains("/instances/") {
        // An encapsulated PDF, rendered: bytes the uploader chose.
        let mut r = (StatusCode::OK, "%PDF-1.4\n<html><script>alert(1)</script>").into_response();
        r.headers_mut()
            .insert(header::CONTENT_TYPE, HeaderValue::from_static("application/pdf"));
        return r;
    }
    if path.contains("/frames/") {
        let mut r = (StatusCode::OK, "--b\r\n\r\nPIXELS\r\n--b--").into_response();
        r.headers_mut().insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("multipart/related; type=\"application/octet-stream\"; boundary=b"),
        );
        return r;
    }
    json(json!([]))
}

struct World {
    db: TestDb,
    _dir: tempfile::TempDir,
    app: Router,
    fam: Family,
    orthanc: FakeOrthanc,
}

impl World {
    async fn new() -> anyhow::Result<World> {
        World::with(Duration::from_secs(3600), true).await
    }

    async fn with(ttl: Duration, imaging_on: bool) -> anyhow::Result<World> {
        // Orthanc is asked again on every proxied request (no cache), so a change shows at once.
        World::with_times(ttl, imaging_on, Duration::ZERO).await
    }

    /// `with`, and a check against Orthanc that stands for `recheck` (production: `ORTHANC_RECHECK`).
    async fn with_times(ttl: Duration, imaging_on: bool, recheck: Duration) -> anyhow::Result<World> {
        let db = TestDb::new().await?;
        let dir = tempfile::tempdir()?;
        let mut settings = db.settings(dir.path())?;
        settings.dev_auth = false;
        settings.base_url = BASE.into();
        let rt = Runtime::with_pool(settings, db.pool.clone())?;
        let fam = {
            let mut c = db.pool.acquire().await?;
            create_rivera_family(&mut c, "Rivera").await?
        };
        let orthanc = FakeOrthanc::start().await?;
        let config = imaging_on.then(|| OrthancConfig {
            url: orthanc.url.clone(),
            user: "ownhealth".into(),
            password: PASSWORD.into(),
        });
        let app = build_app_with(rt, Imaging::with_times(config, ttl, recheck)?);
        let w = World {
            db,
            _dir: dir,
            app,
            fam,
            orthanc,
        };
        w.add_study("alex", ALEX_CT, "current").await?;
        w.add_study("carmen", CARMEN_MR, "current").await?;
        w.add_study("lucia", LUCIA_XR, "current").await?;
        w.add_study("sam", SAM_US, "current").await?;
        Ok(w)
    }

    /// A study in `slug`'s records, held by Orthanc as `orthanc-<slug>`.
    async fn add_study(&self, slug: &str, uid: &str, status: &str) -> R {
        let profile = self.fam.profile(slug);
        let orthanc_id = format!("orthanc-{slug}");
        self.orthanc.add_study(uid, &orthanc_id);
        let src = format!("src_dicom_{}", slug);
        sqlx::query(
            "insert into src (id, profile_id, kind, actor_kind, label) values ($1, $2, 'dicom', 'system', 'synthetic CD') \
             on conflict do nothing",
        )
        .bind(&src)
        .bind(profile)
        .execute(&self.db.pool)
        .await?;
        sqlx::query(
            "insert into imaging_study (profile_id, study_instance_uid, orthanc_id, modality, status, src_id, fhir) \
             values ($1, $2, $3, 'CT', $4, $5, '{}')",
        )
        .bind(profile)
        .bind(uid)
        .bind(&orthanc_id)
        .bind(status)
        .bind(&src)
        .execute(&self.db.pool)
        .await?;
        Ok(())
    }

    /// A logged-in browser: the `oh_session` cookie value.
    async fn session(&self, slug: &str) -> anyhow::Result<String> {
        let mut c = self.db.pool.acquire().await?;
        Ok(sessions::create_session_with(
            &mut c,
            self.fam.user(slug),
            None,
            Some("cargo test"),
            "passkey",
            Some(true),
        )
        .await?)
    }

    async fn call(&self, req: Request<Body>) -> anyhow::Result<(StatusCode, HeaderMap, Vec<u8>)> {
        let res = self.app.clone().oneshot(req).await?;
        let (status, headers) = (res.status(), res.headers().clone());
        let body = to_bytes(res.into_body(), 1 << 22).await?.to_vec();
        Ok((status, headers, body))
    }

    /// `POST /api/profiles/{slug}/imaging/studies/{uid}/open` from our own origin.
    async fn open_with(&self, auth: Auth<'_>, slug: &str, uid: &str) -> anyhow::Result<(StatusCode, Value)> {
        let (s, _, body) = self.call(open_request(auth, slug, uid)?).await?;
        Ok((s, serde_json::from_slice(&body).unwrap_or(Value::Null)))
    }

    /// Opens `uid` and returns the capability from the viewer link.
    async fn open(&self, cookie: &str, slug: &str, uid: &str) -> anyhow::Result<String> {
        let (s, v) = self.open_with(Auth::Cookie(cookie), slug, uid).await?;
        anyhow::ensure!(s == StatusCode::OK, "open {slug}/{uid}: {s} {v}");
        let url = v["viewer_url"].as_str().unwrap_or_default();
        let cap = url
            .strip_prefix("/ohif/t/")
            .and_then(|r| r.split_once('/'))
            .map(|(c, _)| c.to_string())
            .unwrap_or_default();
        anyhow::ensure!(
            url == format!("/ohif/t/{cap}/viewer?StudyInstanceUIDs={uid}") && cap.len() >= 43,
            "{url}"
        );
        Ok(cap)
    }

    /// A viewer request: no cookie can reach it (opaque origin), so none is sent unless `cookie` says so.
    async fn viewer_get(&self, path: &str, cookie: Option<&str>) -> anyhow::Result<(StatusCode, HeaderMap, Vec<u8>)> {
        let mut b = Request::get(path)
            .header(header::HOST, "health.test")
            .header(header::ORIGIN, "null")
            .header(
                header::ACCEPT,
                "multipart/related; type=application/octet-stream; transfer-syntax=*",
            );
        if let Some(c) = cookie {
            b = b.header(header::COOKIE, format!("{}={c}", sessions::COOKIE));
        }
        self.call(b.body(Body::empty())?).await
    }

    async fn status_of(&self, method: Method, path: &str) -> anyhow::Result<StatusCode> {
        let req = Request::builder()
            .method(method)
            .uri(path)
            .header(header::HOST, "health.test")
            .header(header::ORIGIN, "null")
            .header(header::CONTENT_TYPE, "application/dicom")
            .body(Body::from("DICM"))?;
        Ok(self.call(req).await?.0)
    }

    /// Seconds since `slug`'s sessions were last seen.
    async fn idle(&self, slug: &str) -> anyhow::Result<f64> {
        Ok(sqlx::query_scalar(
            "select extract(epoch from now() - max(last_seen_at))::float8 from user_session where user_id = $1",
        )
        .bind(self.fam.user(slug))
        .fetch_one(&self.db.pool)
        .await?)
    }

    async fn set_idle(&self, slug: &str, secs: i32) -> R {
        sqlx::query("update user_session set last_seen_at = now() - make_interval(secs => $2) where user_id = $1")
            .bind(self.fam.user(slug))
            .bind(f64::from(secs))
            .execute(&self.db.pool)
            .await?;
        Ok(())
    }

    async fn audit_rows(&self, uid: &str) -> anyhow::Result<i64> {
        Ok(sqlx::query_scalar(
            "select count(*) from audit_event where action = $1 and resource_ids = array[$2]::text[]",
        )
        .bind(AUDIT_ACTION)
        .bind(uid)
        .fetch_one(&self.db.pool)
        .await?)
    }
}

enum Auth<'a> {
    Cookie(&'a str),
    Bearer(&'a str),
}

/// `POST /api/profiles/{slug}/imaging/studies/{uid}/open` from our own origin.
fn open_request(auth: Auth<'_>, slug: &str, uid: &str) -> anyhow::Result<Request<Body>> {
    let b = Request::post(format!("/api/profiles/{slug}/imaging/studies/{uid}/open"))
        .header(header::HOST, "health.test")
        .header(header::ORIGIN, BASE);
    let b = match auth {
        Auth::Cookie(c) => b.header(header::COOKIE, format!("{}={c}", sessions::COOKIE)),
        Auth::Bearer(t) => b.header(header::AUTHORIZATION, format!("Bearer {t}")),
    };
    Ok(b.body(Body::empty())?)
}

fn hv<'h>(h: &'h HeaderMap, name: &str) -> &'h str {
    h.get(name).and_then(|v| v.to_str().ok()).unwrap_or_default()
}

fn code(body: &[u8]) -> String {
    let v: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
    v["detail"]["code"].as_str().unwrap_or_default().to_string()
}

fn dw(cap: &str, rest: &str) -> String {
    format!("/dicom-web/t/{cap}/{rest}")
}

#[tokio::test]
async fn opening_a_study_gives_a_sandboxed_viewer_and_a_proxy_for_that_study_only() -> R {
    let w = World::new().await?;
    let cookie = w.session("alex").await?;
    let cap = w.open(&cookie, "alex", ALEX_CT).await?;

    // The viewer page: sandboxed (opaque origin, so no cookie and no /api), Orthanc's isolation
    // headers replaced by ours, never cached.
    let (s, h, body) = w.viewer_get(&format!("/ohif/t/{cap}/viewer"), None).await?;
    assert_eq!(s, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    assert!(hv(&h, "content-security-policy").starts_with("sandbox allow-scripts;"));
    assert_eq!(hv(&h, "content-security-policy"), VIEWER_CSP);
    assert!(VIEWER_CSP.contains("connect-src 'self'"));
    assert_eq!(hv(&h, "cross-origin-resource-policy"), "cross-origin");
    assert_eq!(hv(&h, "cross-origin-embedder-policy"), "");
    assert_eq!(hv(&h, "referrer-policy"), "no-referrer");
    assert!(String::from_utf8_lossy(&body).contains("OHIF Viewer"));

    // app-config.js: OHIF reads DICOMweb only through this link, never the study list.
    let (s, h, body) = w.viewer_get(&format!("/ohif/t/{cap}/app-config.js"), None).await?;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(hv(&h, "cache-control"), "no-store");
    let js = String::from_utf8(body)?;
    assert!(js.contains(&format!("\"{BASE}/dicom-web/t/{cap}\"")), "{js}");
    assert!(js.contains(&format!("\"/ohif/t/{cap}/\"")), "{js}");
    assert!(js.contains("showStudyList = false"));
    assert!(js.contains("dataSourcesModule.dicomweb"));

    // QIDO for the one study, WADO-RS metadata and frames: answered with CORS for Origin null and no
    // credentials, with Orthanc's own URLs turned into links through this capability.
    let (s, h, body) = w
        .viewer_get(
            &dw(
                &cap,
                &format!("studies?StudyInstanceUID={ALEX_CT}&includefield=00081030"),
            ),
            None,
        )
        .await?;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(hv(&h, "access-control-allow-origin"), "null");
    assert_eq!(hv(&h, "access-control-allow-credentials"), "");
    assert_eq!(hv(&h, "cache-control"), "no-store");
    let text = String::from_utf8(body)?;
    assert!(
        text.contains(&format!("{BASE}/dicom-web/t/{cap}/studies/{ALEX_CT}")),
        "{text}"
    );
    assert!(!text.contains(&w.orthanc.url), "{text}");

    let meta = dw(&cap, &format!("studies/{ALEX_CT}/series/{ALEX_SERIES}/metadata"));
    let (s, _, body) = w.viewer_get(&meta, None).await?;
    assert_eq!(s, StatusCode::OK);
    let text = String::from_utf8(body)?;
    assert!(
        text.contains(&format!(
            "{BASE}/dicom-web/t/{cap}/studies/{ALEX_CT}/series/{ALEX_SERIES}/instances/{ALEX_INSTANCE}/bulk/7fe00010"
        )),
        "{text}"
    );
    let frame = dw(
        &cap,
        &format!("studies/{ALEX_CT}/series/{ALEX_SERIES}/instances/{ALEX_INSTANCE}/frames/1"),
    );
    let (s, h, body) = w.viewer_get(&frame, None).await?;
    assert_eq!(s, StatusCode::OK);
    assert!(hv(&h, "content-type").starts_with("multipart/related"));
    assert!(String::from_utf8_lossy(&body).contains("PIXELS"));

    // Orthanc saw our service account only, never the person's cookie, and the viewer's Accept.
    let seen = w.orthanc.seen();
    assert!(!seen.is_empty());
    let basic = format!("Basic {}", base64_encode(format!("ownhealth:{PASSWORD}").as_bytes()));
    for r in &seen {
        assert_eq!(r.method, Method::GET, "{r:?}");
        assert_eq!(r.authorization.as_deref(), Some(basic.as_str()), "{r:?}");
        assert!(!r.cookie, "{r:?}");
    }
    assert!(
        seen.iter()
            .any(|r| r.accept.as_deref().is_some_and(|a| a.contains("transfer-syntax=*")))
    );
    assert!(
        seen.iter()
            .any(|r| r.uri == format!("/dicom-web/studies?StudyInstanceUID={ALEX_CT}&includefield=00081030"))
    );

    // One imaging.open audit row per study per session: a second open in the same session adds none,
    // another session adds one.
    assert_eq!(w.audit_rows(ALEX_CT).await?, 1);
    w.open(&cookie, "alex", ALEX_CT).await?;
    assert_eq!(w.audit_rows(ALEX_CT).await?, 1);
    let other = w.session("alex").await?;
    w.open(&other, "alex", ALEX_CT).await?;
    assert_eq!(w.audit_rows(ALEX_CT).await?, 2);
    let row: (String, String, Option<Uuid>, Option<Uuid>) = sqlx::query_as(
        "select actor_kind, resource_type, actor_id, profile_id from audit_event where action = $1 limit 1",
    )
    .bind(AUDIT_ACTION)
    .fetch_one(&w.db.pool)
    .await?;
    assert_eq!(
        row,
        (
            "user".into(),
            "imaging_study".into(),
            Some(w.fam.user("alex")),
            Some(w.fam.profile("alex"))
        )
    );
    Ok(())
}

fn base64_encode(b: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in b.chunks(3) {
        let n = chunk
            .iter()
            .enumerate()
            .fold(0u32, |acc, (i, &x)| acc | u32::from(x) << (16 - 8 * i));
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(char::from(T[((n >> (18 - 6 * i)) & 63) as usize]));
            } else {
                out.push('=');
            }
        }
    }
    out
}

#[tokio::test]
async fn a_bearer_token_never_gets_a_viewer_link() -> R {
    let w = World::new().await?;
    let mut c = w.db.pool.acquire().await?;
    let pat = create_token(
        &mut c,
        &TokenIn {
            user_id: w.fam.user("alex"),
            label: "agent",
            profile_ids: vec![w.fam.profile("alex")],
            scopes: all_scopes().into_iter().collect(),
            default_profile: None,
            days: 30,
            identity_mode: "full",
            lang: "en",
            units: "si",
        },
    )
    .await?;
    drop(c);
    let (s, v) = w.open_with(Auth::Bearer(&pat.plaintext), "alex", ALEX_CT).await?;
    assert_eq!(s, StatusCode::FORBIDDEN, "{v}");
    assert_eq!(v["detail"]["code"], "forbidden");
    assert_eq!(w.audit_rows(ALEX_CT).await?, 0);

    // The proxy reads no Authorization header: a token in it opens nothing.
    let req = Request::get(dw("x", &format!("studies?StudyInstanceUID={ALEX_CT}")))
        .header(header::AUTHORIZATION, format!("Bearer {}", pat.plaintext))
        .body(Body::empty())?;
    let (s, _, body) = w.call(req).await?;
    assert_eq!(s, StatusCode::FORBIDDEN);
    assert_eq!(code(&body), "viewer_link_invalid");
    assert!(w.orthanc.seen().is_empty());
    Ok(())
}

#[tokio::test]
async fn a_study_outside_the_link_or_the_grant_is_refused() -> R {
    let w = World::new().await?;
    let alex = w.session("alex").await?;
    let cap = w.open(&alex, "alex", ALEX_CT).await?;
    w.orthanc.clear();
    // Alex may read Carmen's records, but this link opens Alex's CT only.
    for rest in [
        format!("studies/{CARMEN_MR}/series"),
        format!("studies/{CARMEN_MR}/series/{ALEX_SERIES}/metadata"),
        format!("studies?StudyInstanceUID={CARMEN_MR}"),
    ] {
        let (s, _, body) = w.viewer_get(&dw(&cap, &rest), None).await?;
        assert_eq!(s, StatusCode::FORBIDDEN, "{rest}");
        assert_eq!(code(&body), "study_not_in_link", "{rest}");
    }
    assert!(w.orthanc.seen().is_empty());

    // Sam has no grant on Carmen, and only `viewer` (no read:records) on Alex.
    let sam = w.session("sam").await?;
    let (s, v) = w.open_with(Auth::Cookie(&sam), "carmen", CARMEN_MR).await?;
    assert_eq!(s, StatusCode::NOT_FOUND, "{v}");
    let alex_profile = w.fam.profile("alex").to_string();
    let (s, v) = w.open_with(Auth::Cookie(&sam), &alex_profile, ALEX_CT).await?;
    assert_eq!(s, StatusCode::FORBIDDEN, "{v}");
    // A UID that is not in the profile named.
    let (s, v) = w.open_with(Auth::Cookie(&alex), "alex", CARMEN_MR).await?;
    assert_eq!(s, StatusCode::NOT_FOUND, "{v}");
    assert_eq!(v["detail"]["code"], "study_not_found");
    Ok(())
}

#[tokio::test]
async fn only_get_reaches_orthanc() -> R {
    let w = World::new().await?;
    let cookie = w.session("alex").await?;
    let cap = w.open(&cookie, "alex", ALEX_CT).await?;
    w.orthanc.clear();
    for (method, rest) in [
        (Method::POST, "studies".to_string()), // STOW-RS
        (Method::POST, format!("studies/{ALEX_CT}")),
        (Method::DELETE, format!("studies/{ALEX_CT}")),
        (Method::PUT, format!("studies/{ALEX_CT}/series/{ALEX_SERIES}")),
        (Method::PATCH, format!("studies?StudyInstanceUID={ALEX_CT}")),
    ] {
        assert_eq!(
            w.status_of(method.clone(), &dw(&cap, &rest)).await?,
            StatusCode::METHOD_NOT_ALLOWED,
            "{method} {rest}"
        );
    }
    assert_eq!(
        w.status_of(Method::POST, &format!("/ohif/t/{cap}/viewer")).await?,
        StatusCode::METHOD_NOT_ALLOWED
    );
    assert!(w.orthanc.seen().is_empty(), "{:?}", w.orthanc.seen());
    Ok(())
}

#[tokio::test]
async fn a_search_must_name_exactly_the_one_study() -> R {
    let w = World::new().await?;
    let cookie = w.session("alex").await?;
    let cap = w.open(&cookie, "alex", ALEX_CT).await?;
    w.orthanc.clear();
    for rest in [
        "studies".to_string(),
        "studies?limit=101&offset=0".to_string(),
        "studies?00100020=OH-SYNTH-Alex".to_string(), // OHIF's "other studies of this patient"
        format!("studies?PatientID=OH-SYNTH-Alex&StudyInstanceUID={ALEX_CT}"),
        format!("studies?StudyInstanceUID={ALEX_CT},{CARMEN_MR}"),
        format!("studies?StudyInstanceUID={ALEX_CT}%5C{CARMEN_MR}"),
        format!("studies?StudyInstanceUID={ALEX_CT}&StudyInstanceUID={CARMEN_MR}"),
        format!("studies?StudyInstanceUID={ALEX_CT}&0020000D={CARMEN_MR}"),
        "studies?StudyInstanceUID=1.2.826.*".to_string(),
        format!("studies/{ALEX_CT}/series?StudyInstanceUID={CARMEN_MR}"),
        format!("studies/{ALEX_CT}/series?SeriesInstanceUID=*"),
        format!("studies/{ALEX_CT}/series/{ALEX_SERIES}/instances?SOPInstanceUID=1.2.*"),
    ] {
        let (s, _, body) = w.viewer_get(&dw(&cap, &rest), None).await?;
        assert_eq!(s, StatusCode::FORBIDDEN, "{rest}");
        assert!(
            ["dicomweb_refused", "study_not_in_link"].contains(&code(&body).as_str()),
            "{rest}: {}",
            code(&body)
        );
    }
    // A list or a wildcard is refused as such, before it is compared with the link's study.
    for rest in [
        format!("studies?StudyInstanceUID={ALEX_CT},{ALEX_CT}"),
        format!("studies?StudyInstanceUID={ALEX_CT}%5C{ALEX_CT}"),
        "studies?StudyInstanceUID=*".to_string(),
    ] {
        let (s, _, body) = w.viewer_get(&dw(&cap, &rest), None).await?;
        assert_eq!(
            (s, code(&body).as_str()),
            (StatusCode::FORBIDDEN, "dicomweb_refused"),
            "{rest}"
        );
    }
    assert!(w.orthanc.seen().is_empty(), "{:?}", w.orthanc.seen());
    Ok(())
}

#[tokio::test]
async fn orthanc_ids_wado_uri_and_root_searches_are_refused() -> R {
    let w = World::new().await?;
    let cookie = w.session("alex").await?;
    let cap = w.open(&cookie, "alex", ALEX_CT).await?;
    w.orthanc.clear();
    for rest in [
        format!("studies/{ORTHANC_ID}/ohif-dicom-json"),
        format!("studies/{ALEX_CT}/ohif-dicom-json"),
        format!("studies/{ORTHANC_ID}"),
        format!("wado?requestType=WADO&studyUID={ALEX_CT}&seriesUID={ALEX_SERIES}&objectUID={ALEX_INSTANCE}"),
        format!("instances/{ORTHANC_ID}"),
        format!("instances?StudyInstanceUID={ALEX_CT}"),
        format!("series?StudyInstanceUID={ALEX_CT}"),
        "servers".to_string(),
        "info".to_string(),
        format!("studies/{ALEX_CT}/series/{ORTHANC_ID}/metadata"),
        // A whole study or series download: never, only the reads the viewer makes.
        format!("studies/{ALEX_CT}"),
        format!("studies/{ALEX_CT}/series/{ALEX_SERIES}"),
    ] {
        let (s, _, body) = w.viewer_get(&dw(&cap, &rest), None).await?;
        assert_eq!(s, StatusCode::FORBIDDEN, "{rest}");
        assert!(
            ["dicomweb_refused", "study_not_in_link"].contains(&code(&body).as_str()),
            "{rest}: {}",
            code(&body)
        );
    }
    // The viewer path reaches Orthanc's /ohif files only, never its REST API or explorer.
    for path in [
        format!("/ohif/t/{cap}/../../studies/{ORTHANC_ID}/ohif-dicom-json"),
        format!("/ohif/t/{cap}/../app/explorer.html"),
    ] {
        let (s, h, _) = w.viewer_get(&path, None).await?;
        assert_eq!(s, StatusCode::FORBIDDEN, "{path}");
        assert_eq!(hv(&h, "content-security-policy"), VIEWER_CSP);
    }
    assert!(w.orthanc.seen().is_empty(), "{:?}", w.orthanc.seen());
    Ok(())
}

#[tokio::test]
async fn a_path_traversal_uid_is_refused() -> R {
    let w = World::new().await?;
    let cookie = w.session("alex").await?;
    let cap = w.open(&cookie, "alex", ALEX_CT).await?;
    w.orthanc.clear();
    for rest in [
        format!("studies/{ALEX_CT}/../{CARMEN_MR}/series"),
        format!("studies/{ALEX_CT}/series/../../{CARMEN_MR}/series"),
        format!("studies/{ALEX_CT}/series/%2e%2e/%2e%2e/{CARMEN_MR}"),
        format!("studies/{ALEX_CT}%2F..%2F{CARMEN_MR}/series"),
        format!("studies/{ALEX_CT}/series/{ALEX_SERIES}%2F..%2F..%2F..%2Finstances"),
        format!("studies/{ALEX_CT}//series"),
        format!("studies/{ALEX_CT}/series/1..2/metadata"),
        format!("studies/{ALEX_CT}/series/.1.2/metadata"),
        format!("studies/{ALEX_CT}%5C..%5C{CARMEN_MR}"),
    ] {
        let (s, _, body) = w.viewer_get(&dw(&cap, &rest), None).await?;
        assert_eq!(s, StatusCode::FORBIDDEN, "{rest}");
        assert!(
            ["dicomweb_refused", "study_not_in_link"].contains(&code(&body).as_str()),
            "{rest}: {}",
            code(&body)
        );
    }
    let (s, v) = w.open_with(Auth::Cookie(&cookie), "alex", "1.2.826..%2F..").await?;
    assert!(s.is_client_error(), "{s} {v}");
    assert!(w.orthanc.seen().is_empty(), "{:?}", w.orthanc.seen());
    Ok(())
}

#[tokio::test]
async fn a_retracted_study_is_refused() -> R {
    let w = World::new().await?;
    let cookie = w.session("alex").await?;
    let cap = w.open(&cookie, "alex", ALEX_CT).await?;
    sqlx::query("update imaging_study set status = 'retracted' where study_instance_uid = $1")
        .bind(ALEX_CT)
        .execute(&w.db.pool)
        .await?;
    w.orthanc.clear();
    let (s, _, body) = w
        .viewer_get(&dw(&cap, &format!("studies/{ALEX_CT}/series")), None)
        .await?;
    assert_eq!(s, StatusCode::FORBIDDEN);
    assert_eq!(code(&body), "study_retracted");
    let (s, _, _) = w.viewer_get(&format!("/ohif/t/{cap}/viewer"), None).await?;
    assert_eq!(s, StatusCode::FORBIDDEN);
    let (s, v) = w.open_with(Auth::Cookie(&cookie), "alex", ALEX_CT).await?;
    assert_eq!(s, StatusCode::FORBIDDEN, "{v}");
    assert_eq!(v["detail"]["code"], "study_retracted");
    assert!(w.orthanc.seen().is_empty());
    Ok(())
}

#[tokio::test]
async fn a_deleted_profile_ends_the_link() -> R {
    let w = World::new().await?;
    let cookie = w.session("alex").await?;
    // A study removed from the records ends its link too, while the profile and the grant stay.
    let own = w.open(&cookie, "alex", ALEX_CT).await?;
    sqlx::query("delete from imaging_study where study_instance_uid = $1")
        .bind(ALEX_CT)
        .execute(&w.db.pool)
        .await?;
    w.orthanc.clear();
    let (s, _, body) = w
        .viewer_get(&dw(&own, &format!("studies/{ALEX_CT}/series")), None)
        .await?;
    assert_eq!(
        (s, code(&body).as_str()),
        (StatusCode::FORBIDDEN, "study_not_available")
    );
    assert!(w.orthanc.seen().is_empty());
    let cap = w.open(&cookie, "carmen", CARMEN_MR).await?; // Alex is her caregiver
    let series = dw(&cap, &format!("studies/{CARMEN_MR}/series"));
    assert_eq!(w.viewer_get(&series, None).await?.0, StatusCode::OK);
    let mut tx = w.db.pool.begin().await?;
    ownhealth_auth::household::delete_profile(&mut tx, w.fam.profile("carmen")).await?;
    tx.commit().await?;
    w.orthanc.clear();
    let (s, _, body) = w.viewer_get(&series, None).await?;
    assert_eq!(s, StatusCode::FORBIDDEN);
    assert_eq!(code(&body), "study_not_available");
    assert!(w.orthanc.seen().is_empty());
    Ok(())
}

#[tokio::test]
async fn an_expired_link_is_refused() -> R {
    // Long enough that the first read is well inside it on a slow runner too.
    let ttl = Duration::from_secs(2);
    let w = World::with(ttl, true).await?;
    let cookie = w.session("alex").await?;
    let cap = w.open(&cookie, "alex", ALEX_CT).await?;
    let ends = std::time::Instant::now() + ttl; // the link was made before this
    let series = dw(&cap, &format!("studies/{ALEX_CT}/series"));
    assert_eq!(w.viewer_get(&series, None).await?.0, StatusCode::OK);
    tokio::time::sleep_until((ends + Duration::from_millis(200)).into()).await;
    w.orthanc.clear();
    let (s, _, body) = w.viewer_get(&series, None).await?;
    assert_eq!(s, StatusCode::FORBIDDEN);
    assert_eq!(code(&body), "viewer_link_invalid");
    let (s, h, _) = w.viewer_get(&format!("/ohif/t/{cap}/viewer"), None).await?;
    assert_eq!(s, StatusCode::FORBIDDEN);
    assert_eq!(hv(&h, "content-security-policy"), VIEWER_CSP);
    assert!(w.orthanc.seen().is_empty());
    Ok(())
}

/// The link is bound to the session: it dies with a logout, and with the grant's `read:records`.
#[tokio::test]
async fn a_lost_grant_or_an_ended_session_ends_the_link() -> R {
    let w = World::new().await?;
    let cookie = w.session("alex").await?;
    let lucia = w.open(&cookie, "lucia", LUCIA_XR).await?;
    let alex = w.open(&cookie, "alex", ALEX_CT).await?;
    let lucia_series = dw(&lucia, &format!("studies/{LUCIA_XR}/series"));
    let alex_series = dw(&alex, &format!("studies/{ALEX_CT}/series"));
    assert_eq!(w.viewer_get(&lucia_series, None).await?.0, StatusCode::OK);
    sqlx::query("update profile_grant set categories = array['read:summary'] where user_id = $1 and profile_id = $2")
        .bind(w.fam.user("alex"))
        .bind(w.fam.profile("lucia"))
        .execute(&w.db.pool)
        .await?;
    let (s, _, body) = w.viewer_get(&lucia_series, None).await?;
    assert_eq!(s, StatusCode::FORBIDDEN);
    assert_eq!(code(&body), "study_not_available");

    assert_eq!(w.viewer_get(&alex_series, None).await?.0, StatusCode::OK);
    let mut c = w.db.pool.acquire().await?;
    assert!(sessions::revoke_session(&mut c, &cookie).await?);
    drop(c);
    let (s, _, body) = w.viewer_get(&alex_series, None).await?;
    assert_eq!(s, StatusCode::FORBIDDEN);
    assert_eq!(code(&body), "viewer_link_invalid");
    // Another live session's cookie on the request changes nothing: the proxy reads no cookie.
    let other = w.session("alex").await?;
    let (s, _, _) = w.viewer_get(&alex_series, Some(&other)).await?;
    assert_eq!(s, StatusCode::FORBIDDEN);
    Ok(())
}

/// A disabled user's links end at once.
#[tokio::test]
async fn a_disabled_user_ends_the_link() -> R {
    let w = World::new().await?;
    let cookie = w.session("alex").await?;
    let cap = w.open(&cookie, "alex", ALEX_CT).await?;
    let series = dw(&cap, &format!("studies/{ALEX_CT}/series"));
    assert_eq!(w.viewer_get(&series, None).await?.0, StatusCode::OK);
    sqlx::query("update app_user set disabled_at = now() where id = $1")
        .bind(w.fam.user("alex"))
        .execute(&w.db.pool)
        .await?;
    let (s, _, body) = w.viewer_get(&series, None).await?;
    assert_eq!(
        (s, code(&body).as_str()),
        (StatusCode::FORBIDDEN, "viewer_link_invalid")
    );
    Ok(())
}

/// A session idle for more than 30 minutes ends its links too.
#[tokio::test]
async fn an_idle_session_ends_the_link() -> R {
    let w = World::new().await?;
    let sam = w.session("sam").await?;
    let cap = w.open(&sam, "sam", SAM_US).await?;
    let series = dw(&cap, &format!("studies/{SAM_US}/series"));
    assert_eq!(w.viewer_get(&series, None).await?.0, StatusCode::OK);
    w.set_idle("sam", 31 * 60).await?;
    w.orthanc.clear();
    let (s, _, body) = w.viewer_get(&series, None).await?;
    assert_eq!(
        (s, code(&body).as_str()),
        (StatusCode::FORBIDDEN, "viewer_link_invalid")
    );
    assert!(w.orthanc.seen().is_empty());
    Ok(())
}

/// A long read: the viewer's requests count as activity (at most once a minute), so the session does
/// not go idle under a person who is still reading. The link's own hour still ends it.
#[tokio::test]
async fn reading_in_the_viewer_keeps_the_session_alive() -> R {
    let w = World::new().await?;
    let cookie = w.session("alex").await?;
    let cap = w.open(&cookie, "alex", ALEX_CT).await?;
    let series = dw(&cap, &format!("studies/{ALEX_CT}/series"));
    // 25 minutes without the app: a viewer request makes the session fresh again.
    w.set_idle("alex", 25 * 60).await?;
    assert_eq!(w.viewer_get(&series, None).await?.0, StatusCode::OK);
    let idle = w.idle("alex").await?;
    assert!(idle < 5.0, "{idle}");
    // Within a minute of the last touch, a request writes nothing.
    w.set_idle("alex", 30).await?;
    assert_eq!(w.viewer_get(&series, None).await?.0, StatusCode::OK);
    let idle = w.idle("alex").await?;
    assert!((29.0..60.0).contains(&idle), "{idle}");
    // The viewer files count too.
    w.set_idle("alex", 20 * 60).await?;
    assert_eq!(
        w.viewer_get(&format!("/ohif/t/{cap}/viewer"), None).await?.0,
        StatusCode::OK
    );
    let idle = w.idle("alex").await?;
    assert!(idle < 5.0, "{idle}");

    // A refused request is no activity: a link that authorizes nothing any more, or a request the
    // proxy refuses, leaves the session to go idle.
    let lucia = w.open(&cookie, "lucia", LUCIA_XR).await?;
    sqlx::query("update profile_grant set categories = array['read:summary'] where user_id = $1 and profile_id = $2")
        .bind(w.fam.user("alex"))
        .bind(w.fam.profile("lucia"))
        .execute(&w.db.pool)
        .await?;
    for (path, why) in [
        (dw(&lucia, &format!("studies/{LUCIA_XR}/series")), "study_not_available"),
        (format!("/ohif/t/{lucia}/viewer"), "study_not_available"),
        (dw(&cap, &format!("studies/{ALEX_CT}")), "dicomweb_refused"),
        (dw(&cap, &format!("studies/{CARMEN_MR}/series")), "study_not_in_link"),
    ] {
        w.set_idle("alex", 25 * 60).await?;
        let (s, _, body) = w.viewer_get(&path, None).await?;
        assert_eq!(s, StatusCode::FORBIDDEN, "{path}");
        if path.starts_with("/dicom-web/") {
            assert_eq!(code(&body), why, "{path}");
        }
        let idle = w.idle("alex").await?;
        assert!(
            idle >= 25.0 * 60.0,
            "{path}: a refused request refreshed the session ({idle})"
        );
    }
    // Nor does a study Orthanc cannot tell apart any more.
    w.orthanc.add_study(ALEX_CT, "orthanc-another-patient");
    w.set_idle("alex", 25 * 60).await?;
    let (s, _, body) = w.viewer_get(&series, None).await?;
    assert_eq!((s, code(&body).as_str()), (StatusCode::FORBIDDEN, "study_not_bound"));
    let idle = w.idle("alex").await?;
    assert!(idle >= 25.0 * 60.0, "study_not_bound refreshed the session ({idle})");
    Ok(())
}

#[tokio::test]
async fn every_viewer_page_is_sandboxed_even_a_refusal() -> R {
    for on in [true, false] {
        let w = World::with(Duration::from_secs(3600), on).await?;
        for path in [
            "/ohif",
            "/ohif/",
            "/ohif/viewer?StudyInstanceUIDs=1.2.3",
            "/ohif/app-config.js",
            "/ohif/t/AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA/viewer",
        ] {
            let (s, h, body) = w.viewer_get(path, None).await?;
            assert!(s.is_client_error() || s.is_server_error(), "{on} {path}: {s}");
            assert_eq!(hv(&h, "content-security-policy"), VIEWER_CSP, "{on} {path}");
            assert!(hv(&h, "content-type").starts_with("text/html"), "{on} {path}");
            assert!(
                String::from_utf8_lossy(&body).contains("<!doctype html>"),
                "{on} {path}"
            );
        }
        // The rest of /dicom-web is never Orthanc's: a 404 or 503, not the app's HTML.
        for path in ["/dicom-web", "/dicom-web/studies", "/dicom-web/t"] {
            let (s, _, body) = w.viewer_get(path, None).await?;
            assert!(s.is_client_error() || s.is_server_error(), "{on} {path}: {s}");
            assert!(!String::from_utf8_lossy(&body).contains("<html"), "{on} {path}");
        }
        assert!(w.orthanc.seen().is_empty());
    }
    Ok(())
}

#[tokio::test]
async fn with_imaging_off_nothing_opens() -> R {
    let w = World::with(Duration::from_secs(3600), false).await?;
    let cookie = w.session("alex").await?;
    let (s, v) = w.open_with(Auth::Cookie(&cookie), "alex", ALEX_CT).await?;
    assert_eq!(s, StatusCode::SERVICE_UNAVAILABLE, "{v}");
    assert_eq!(v["detail"]["code"], "imaging_off");
    let (s, _, body) = w
        .viewer_get(&dw("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA", "studies"), None)
        .await?;
    assert_eq!(s, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(code(&body), "imaging_off");
    assert_eq!(w.audit_rows(ALEX_CT).await?, 0);
    Ok(())
}

/// A preflight from the sandboxed viewer (Origin null) gets GET and Accept only, and never
/// `Access-Control-Allow-Credentials`.
#[tokio::test]
async fn the_preflight_allows_get_from_origin_null_without_credentials() -> R {
    let w = World::new().await?;
    let req = Request::builder()
        .method(Method::OPTIONS)
        .uri(dw(
            "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
            &format!("studies/{ALEX_CT}/series"),
        ))
        .header(header::ORIGIN, "null")
        .header("access-control-request-method", "GET")
        .header("access-control-request-headers", "accept")
        .body(Body::empty())?;
    let (s, h, _) = w.call(req).await?;
    assert_eq!(s, StatusCode::NO_CONTENT);
    assert_eq!(hv(&h, "access-control-allow-origin"), "null");
    assert_eq!(hv(&h, "access-control-allow-methods"), "GET");
    assert_eq!(hv(&h, "access-control-allow-credentials"), "");
    assert!(w.orthanc.seen().is_empty());
    Ok(())
}

/// Orthanc keys a study by PatientID plus StudyInstanceUID, so a second patient's study can arrive
/// under the same UID (a crafted CD, a modality that reuses UIDs). The link is bound to the one Orthanc
/// study the record names: with two under the UID, nothing passes, neither at the open nor later.
#[tokio::test]
async fn two_orthanc_studies_under_one_uid_pass_nothing() -> R {
    let w = World::new().await?;
    let cookie = w.session("alex").await?;
    let cap = w.open(&cookie, "alex", ALEX_CT).await?;
    let series = dw(&cap, &format!("studies/{ALEX_CT}/series"));
    assert_eq!(w.viewer_get(&series, None).await?.0, StatusCode::OK);

    w.orthanc.add_study(ALEX_CT, "orthanc-another-patient");
    w.orthanc.clear();
    for rest in [
        format!("studies/{ALEX_CT}/series"),
        format!("studies?StudyInstanceUID={ALEX_CT}"),
        format!("studies/{ALEX_CT}/series/{ALEX_SERIES}/instances/{ALEX_INSTANCE}/metadata"),
        format!("studies/{ALEX_CT}/series/{ALEX_SERIES}/instances/{ALEX_INSTANCE}/frames/1"),
    ] {
        let (s, _, body) = w.viewer_get(&dw(&cap, &rest), None).await?;
        assert_eq!(
            (s, code(&body).as_str()),
            (StatusCode::FORBIDDEN, "study_not_bound"),
            "{rest}"
        );
    }
    let (s, v) = w.open_with(Auth::Cookie(&cookie), "alex", ALEX_CT).await?;
    assert_eq!(s, StatusCode::FORBIDDEN, "{v}");
    assert_eq!(v["detail"]["code"], "study_not_bound");
    let other = w.session("alex").await?;
    let (s, v) = w.open_with(Auth::Cookie(&other), "alex", ALEX_CT).await?;
    assert_eq!(
        (s, v["detail"]["code"].as_str()),
        (StatusCode::FORBIDDEN, Some("study_not_bound"))
    );
    assert_eq!(w.audit_rows(ALEX_CT).await?, 1); // the refused open logged nothing
    assert!(w.orthanc.seen().is_empty(), "{:?}", w.orthanc.seen());
    // Orthanc was asked which studies carry the UID, and only that.
    assert!(w.orthanc.lookups().len() >= 4, "{:?}", w.orthanc.lookups());
    assert!(w.orthanc.lookups().iter().all(|u| u == ALEX_CT));

    // One study, but not the one the record names: refused too.
    sqlx::query("update imaging_study set orthanc_id = 'orthanc-somebody-else' where study_instance_uid = $1")
        .bind(LUCIA_XR)
        .execute(&w.db.pool)
        .await?;
    let (s, v) = w.open_with(Auth::Cookie(&cookie), "lucia", LUCIA_XR).await?;
    assert_eq!(
        (s, v["detail"]["code"].as_str()),
        (StatusCode::FORBIDDEN, Some("study_not_bound"))
    );
    // A record whose Orthanc study is not known yet opens nothing.
    sqlx::query("update imaging_study set orthanc_id = null where study_instance_uid = $1")
        .bind(LUCIA_XR)
        .execute(&w.db.pool)
        .await?;
    let (s, v) = w.open_with(Auth::Cookie(&cookie), "lucia", LUCIA_XR).await?;
    assert_eq!(
        (s, v["detail"]["code"].as_str()),
        (StatusCode::FORBIDDEN, Some("study_not_bound"))
    );
    // A record that moves to another Orthanc study after the open ends the link.
    let carmen = w.open(&cookie, "carmen", CARMEN_MR).await?;
    let carmen_series = dw(&carmen, &format!("studies/{CARMEN_MR}/series"));
    assert_eq!(w.viewer_get(&carmen_series, None).await?.0, StatusCode::OK);
    w.orthanc.add_study(CARMEN_MR, "orthanc-carmen-again");
    sqlx::query("update imaging_study set orthanc_id = 'orthanc-carmen-again' where study_instance_uid = $1")
        .bind(CARMEN_MR)
        .execute(&w.db.pool)
        .await?;
    w.orthanc.clear();
    let (s, _, body) = w.viewer_get(&carmen_series, None).await?;
    assert_eq!(s, StatusCode::FORBIDDEN);
    assert!(
        ["study_not_available", "study_not_bound"].contains(&code(&body).as_str()),
        "{}",
        code(&body)
    );
    assert!(w.orthanc.seen().is_empty(), "{:?}", w.orthanc.seen());
    Ok(())
}

/// In production a link's check against Orthanc stands for `ORTHANC_RECHECK`: reads inside that time
/// ask Orthanc nothing, and the first read after it asks again. So a second study that arrives under
/// the UID passes nothing once that time is up (here 1 s in place of 5 s).
#[tokio::test]
async fn the_orthanc_check_is_asked_again_once_its_time_is_up() -> R {
    let recheck = Duration::from_secs(1);
    let wait = recheck + Duration::from_millis(200);
    let w = World::with_times(Duration::from_secs(3600), true, recheck).await?;
    let cookie = w.session("alex").await?;
    let cap = w.open(&cookie, "alex", ALEX_CT).await?;
    let series = dw(&cap, &format!("studies/{ALEX_CT}/series"));
    assert_eq!(w.orthanc.lookups().len(), 1, "the open asks Orthanc once");
    assert_eq!(w.viewer_get(&series, None).await?.0, StatusCode::OK);
    assert_eq!(w.orthanc.lookups().len(), 1, "a read just after the open asks nothing");

    tokio::time::sleep(wait).await;
    assert_eq!(w.viewer_get(&series, None).await?.0, StatusCode::OK);
    assert_eq!(w.orthanc.lookups().len(), 2, "the first read after the time asks again");
    assert_eq!(w.viewer_get(&series, None).await?.0, StatusCode::OK);
    assert_eq!(w.orthanc.lookups().len(), 2, "and that answer stands for the next read");

    // Another patient's study arrives under the UID. A read inside the time may still pass (the
    // window the design states); the first one after it passes nothing.
    w.orthanc.add_study(ALEX_CT, "orthanc-another-patient");
    tokio::time::sleep(wait).await;
    w.orthanc.clear();
    for rest in [
        format!("studies/{ALEX_CT}/series"),
        format!("studies/{ALEX_CT}/series/{ALEX_SERIES}/instances/{ALEX_INSTANCE}/frames/1"),
    ] {
        let (s, _, body) = w.viewer_get(&dw(&cap, &rest), None).await?;
        assert_eq!(
            (s, code(&body).as_str()),
            (StatusCode::FORBIDDEN, "study_not_bound"),
            "{rest}"
        );
    }
    assert!(w.orthanc.seen().is_empty(), "{:?}", w.orthanc.seen());
    Ok(())
}

/// The open asks Orthanc with no database connection or transaction held, so a slow Orthanc cannot
/// use up the pool. When it writes the audit row, the record must still name the study Orthanc just
/// confirmed.
#[tokio::test]
async fn an_open_holds_nothing_in_the_database_while_orthanc_answers() -> R {
    let w = World::new().await?;
    let cookie = w.session("alex").await?;
    let hold = w.orthanc.pause.clone().write_owned().await;
    let (app, req) = (w.app.clone(), open_request(Auth::Cookie(&cookie), "alex", ALEX_CT)?);
    let open = tokio::spawn(async move { app.oneshot(req).await });
    for _ in 0..500 {
        if !w.orthanc.lookups().is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(w.orthanc.lookups(), [ALEX_CT], "the open is waiting on Orthanc");
    let in_transaction: i64 = sqlx::query_scalar(
        "select count(*) from pg_stat_activity where datname = current_database() \
         and state like 'idle in transaction%' and pid <> pg_backend_pid()",
    )
    .fetch_one(&w.db.pool)
    .await?;
    assert_eq!(in_transaction, 0, "a transaction stays open while Orthanc answers");
    // The record moves to another Orthanc study while Orthanc answers: the open links nothing.
    sqlx::query("update imaging_study set orthanc_id = 'orthanc-alex-again' where study_instance_uid = $1")
        .bind(ALEX_CT)
        .execute(&w.db.pool)
        .await?;
    drop(hold);
    let res = open.await??;
    let status = res.status();
    let body = to_bytes(res.into_body(), 1 << 16).await?;
    assert_eq!(
        (status, code(&body).as_str()),
        (StatusCode::FORBIDDEN, "study_not_bound")
    );
    assert_eq!(w.audit_rows(ALEX_CT).await?, 0);
    Ok(())
}

/// A top-level visit to a viewer URL in a normal tab carries the session cookie: the proxy reads it
/// nowhere and never passes it on to Orthanc.
#[tokio::test]
async fn a_cookie_on_a_viewer_request_never_reaches_orthanc() -> R {
    let w = World::new().await?;
    let cookie = w.session("alex").await?;
    let cap = w.open(&cookie, "alex", ALEX_CT).await?;
    w.orthanc.clear();
    for rest in [
        format!("studies/{ALEX_CT}/series/{ALEX_SERIES}/metadata"),
        format!("studies/{ALEX_CT}/series/{ALEX_SERIES}/instances/{ALEX_INSTANCE}/frames/1"),
    ] {
        let (s, _, _) = w.viewer_get(&dw(&cap, &rest), Some(&cookie)).await?;
        assert_eq!(s, StatusCode::OK, "{rest}");
    }
    let (s, _, _) = w.viewer_get(&format!("/ohif/t/{cap}/viewer"), Some(&cookie)).await?;
    assert_eq!(s, StatusCode::OK);
    let seen = w.orthanc.seen();
    assert_eq!(seen.len(), 3, "{seen:?}");
    assert!(seen.iter().all(|r| !r.cookie), "{seen:?}");
    Ok(())
}

/// Opening the same study again in the same session hands back the live link: a person who opens it
/// a hundred times holds one link, and the audit shows one open.
#[tokio::test]
async fn opening_a_study_again_in_the_session_reuses_its_link() -> R {
    let w = World::new().await?;
    let cookie = w.session("alex").await?;
    let cap = w.open(&cookie, "alex", ALEX_CT).await?;
    for _ in 0..3 {
        assert_eq!(w.open(&cookie, "alex", ALEX_CT).await?, cap);
    }
    let other = w.session("alex").await?;
    assert_ne!(w.open(&other, "alex", ALEX_CT).await?, cap);
    assert_ne!(w.open(&cookie, "carmen", CARMEN_MR).await?, cap);
    assert_eq!(w.audit_rows(ALEX_CT).await?, 2);
    Ok(())
}

/// What Orthanc sends back is the uploader's bytes. Opened as a page, an answer is sandboxed with no
/// rights at all, and anything that is not an image or DICOM is a download.
#[tokio::test]
async fn a_dicomweb_answer_never_runs_as_a_page_of_the_app() -> R {
    let w = World::new().await?;
    let cookie = w.session("alex").await?;
    let cap = w.open(&cookie, "alex", ALEX_CT).await?;
    let rendered = dw(
        &cap,
        &format!("studies/{ALEX_CT}/series/{ALEX_SERIES}/instances/{ALEX_INSTANCE}/rendered"),
    );
    let (s, h, _) = w.viewer_get(&rendered, None).await?;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(hv(&h, "content-type"), "application/pdf");
    assert_eq!(hv(&h, "content-security-policy"), DICOMWEB_CSP);
    assert_eq!(hv(&h, "content-disposition"), "attachment");
    assert_eq!(hv(&h, "x-content-type-options"), "nosniff");
    assert!(DICOMWEB_CSP.starts_with("sandbox;"), "{DICOMWEB_CSP}");
    // The viewer's own reads: a search (JSON) and frames (multipart) stay inline, sandboxed.
    for rest in [
        format!("studies?StudyInstanceUID={ALEX_CT}"),
        format!("studies/{ALEX_CT}/series/{ALEX_SERIES}/instances/{ALEX_INSTANCE}/frames/1"),
    ] {
        let (s, h, _) = w.viewer_get(&dw(&cap, &rest), None).await?;
        assert_eq!(s, StatusCode::OK, "{rest}");
        assert_eq!(hv(&h, "content-security-policy"), DICOMWEB_CSP, "{rest}");
        assert_eq!(hv(&h, "content-disposition"), "", "{rest}");
    }
    // Refusals and preflights carry it too.
    let (s, h, _) = w.viewer_get(&dw(&cap, "studies"), None).await?;
    assert_eq!(s, StatusCode::FORBIDDEN);
    assert_eq!(hv(&h, "content-security-policy"), DICOMWEB_CSP);
    let req = Request::builder()
        .method(Method::OPTIONS)
        .uri(dw(&cap, "studies"))
        .header(header::ORIGIN, "null")
        .body(Body::empty())?;
    let (_, h, _) = w.call(req).await?;
    assert_eq!(hv(&h, "content-security-policy"), DICOMWEB_CSP);
    Ok(())
}

/// A refusal page speaks the person's language (the browser's, as the app picks it) and links back to
/// the app, where the study opens again.
#[tokio::test]
async fn a_refusal_page_speaks_spanish_or_english_and_leads_back() -> R {
    let w = World::new().await?;
    let dead = "/ohif/t/AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA/viewer";
    for (accept, lang, words) in [
        (
            Some("es-ES,es;q=0.9,en;q=0.5"),
            "es",
            "Este enlace del visor ya no sirve",
        ),
        (Some("en-GB,en;q=0.9"), "en", "This viewer link is not valid any more"),
        (Some("fr-FR"), "en", "This viewer link is not valid any more"),
        (None, "es", "Este enlace del visor ya no sirve"), // OWNHEALTH_DEFAULT_LOCALE (es)
    ] {
        let mut b = Request::get(dead).header(header::HOST, "health.test");
        if let Some(a) = accept {
            b = b.header(header::ACCEPT_LANGUAGE, a);
        }
        let (s, h, body) = w.call(b.body(Body::empty())?).await?;
        let page = String::from_utf8(body)?;
        assert_eq!(s, StatusCode::FORBIDDEN);
        assert_eq!(hv(&h, "content-security-policy"), VIEWER_CSP);
        assert!(page.contains(&format!("<html lang=\"{lang}\"")), "{accept:?}: {page}");
        assert!(page.contains(words), "{accept:?}: {page}");
        assert!(page.contains(&format!("href=\"{BASE}/\"")), "{accept:?}: {page}");
    }
    let (_, _, body) = w
        .call(
            Request::get("/ohif/")
                .header(header::ACCEPT_LANGUAGE, "es")
                .body(Body::empty())?,
        )
        .await?;
    let page = String::from_utf8(body)?;
    assert!(
        page.contains("El visor se abre desde un estudio en OwnHealth."),
        "{page}"
    );
    Ok(())
}

#[test]
fn the_orthanc_settings_need_a_password_once_the_url_is_set() -> R {
    use std::collections::HashMap;
    let env = |pairs: &[(&str, &str)]| -> HashMap<String, String> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    };
    assert!(OrthancConfig::from_env(&env(&[]))?.is_none());
    assert!(OrthancConfig::from_env(&env(&[("OWNHEALTH_ORTHANC_URL", "")]))?.is_none());
    assert!(OrthancConfig::from_env(&env(&[("OWNHEALTH_ORTHANC_URL", "http://orthanc:8042")])).is_err());
    assert!(
        OrthancConfig::from_env(&env(&[
            ("OWNHEALTH_ORTHANC_URL", "orthanc:8042"),
            ("OWNHEALTH_ORTHANC_PASSWORD", "x")
        ]))
        .is_err()
    );
    let dir = tempfile::tempdir()?;
    let file = dir.path().join("orthanc_password");
    std::fs::write(&file, "from-the-secret\n")?;
    let c = OrthancConfig::from_env(&env(&[
        ("OWNHEALTH_ORTHANC_URL", "http://orthanc:8042/"),
        ("OWNHEALTH_ORTHANC_PASSWORD", "from-env"), // ggignore: synthetic test value
        ("OWNHEALTH_ORTHANC_PASSWORD_FILE", file.to_str().unwrap_or_default()),
    ]))?
    .ok_or_else(|| anyhow::anyhow!("no config"))?;
    assert_eq!(
        (c.url.as_str(), c.user.as_str(), c.password.as_str()),
        ("http://orthanc:8042", "ownhealth", "from-the-secret")
    );
    assert!(!format!("{c:?}").contains("from-the-secret"));
    Ok(())
}
