//! The assembled app: probes, the SPA fallback, /api and /mcp routing, startup steps.

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use ownhealth::app::{build_app, check_secret_key, start};
use ownhealth_core::Runtime;
use ownhealth_core::testing::TestDb;
use serde_json::{Value, json};
use tower::ServiceExt;

type R = anyhow::Result<()>;

async fn call(app: &axum::Router, req: Request<Body>) -> anyhow::Result<(StatusCode, Vec<u8>, Option<String>)> {
    let res = app.clone().oneshot(req).await?;
    let status = res.status();
    let cache = res
        .headers()
        .get("cache-control")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let body = to_bytes(res.into_body(), 1 << 20).await?.to_vec();
    Ok((status, body, cache))
}

fn get(path: &str) -> anyhow::Result<Request<Body>> {
    Ok(Request::get(path).body(Body::empty())?)
}

fn json_of(b: &[u8]) -> Value {
    serde_json::from_slice(b).unwrap_or(Value::Null)
}

async fn runtime(db: &TestDb, dir: &std::path::Path, ui: bool) -> anyhow::Result<Runtime> {
    let mut s = db.settings(dir)?;
    let static_dir = dir.join("static");
    std::fs::create_dir_all(&static_dir)?;
    if ui {
        std::fs::write(static_dir.join("index.html"), "<!doctype html><title>OwnHealth</title>")?;
        std::fs::create_dir_all(static_dir.join("_app"))?;
        std::fs::write(static_dir.join("_app/app.js"), "console.log(1)")?;
        std::fs::create_dir_all(static_dir.join("_app/immutable"))?;
        std::fs::write(static_dir.join("_app/immutable/a1b2.js"), "x")?;
        std::fs::write(static_dir.join("_app/version.json"), "{}")?;
        std::fs::write(static_dir.join("service-worker.js"), "self")?;
    }
    s.static_dir = Some(static_dir);
    Runtime::with_pool(s, db.pool.clone())
}

#[tokio::test]
async fn probes_answer_like_python() -> R {
    let db = TestDb::empty().await?;
    let dir = tempfile::tempdir()?;
    let rt = runtime(&db, dir.path(), true).await?;
    let app = build_app(rt.clone());
    let (s, b, _) = call(&app, get("/healthz")?).await?;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(
        json_of(&b),
        json!({"status": "ok", "version": env!("CARGO_PKG_VERSION")})
    );
    let (s, b, _) = call(&app, get("/readyz")?).await?;
    assert_eq!(s, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(json_of(&b)["detail"]["code"], "migrations_pending");
    ownhealth_core::migrate::migrate_embedded(&db.pool).await?;
    let (s, b, _) = call(&app, get("/readyz")?).await?;
    assert_eq!((s, json_of(&b)), (StatusCode::OK, json!({"status": "ready"})));
    std::fs::remove_dir_all(rt.settings.blobs_path())?;
    let (s, b, _) = call(&app, get("/readyz")?).await?;
    assert_eq!(
        (s, json_of(&b)["detail"]["code"].clone()),
        (StatusCode::SERVICE_UNAVAILABLE, json!("blob_dir_missing"))
    );
    Ok(())
}

/// No page of the UI may be framed: a sibling subdomain the SameSite=Lax cookie lets through could
/// otherwise overlay the OAuth consent screen and trick the person into approving a client.
#[tokio::test]
async fn no_ui_page_can_be_framed() -> R {
    let db = TestDb::empty().await?;
    let dir = tempfile::tempdir()?;
    let app = build_app(runtime(&db, dir.path(), true).await?);
    for path in ["/", "/oauth/consent", "/_app/app.js"] {
        let res = app.clone().oneshot(get(path)?).await?;
        assert_eq!(res.status(), StatusCode::OK, "{path}");
        let h = |name: &str| {
            res.headers()
                .get(name)
                .and_then(|v| v.to_str().ok())
                .map(str::to_string)
        };
        assert_eq!(
            h("content-security-policy").as_deref(),
            Some("frame-ancestors 'none'"),
            "{path}"
        );
        assert_eq!(h("x-frame-options").as_deref(), Some("DENY"), "{path}");
        assert_eq!(h("x-content-type-options").as_deref(), Some("nosniff"), "{path}");
    }
    Ok(())
}

#[tokio::test]
async fn the_ui_falls_back_to_index_but_api_and_mcp_paths_do_not() -> R {
    let db = TestDb::new().await?;
    let dir = tempfile::tempdir()?;
    let app = build_app(runtime(&db, dir.path(), true).await?);
    let (s, b, cache) = call(&app, get("/biomarkers/ldl")?).await?;
    assert!(s == StatusCode::OK && b.starts_with(b"<!doctype html>") && cache.as_deref() == Some("no-cache"));
    let (s, b, _) = call(&app, get("/_app/app.js")?).await?;
    assert!(s == StatusCode::OK && b == b"console.log(1)");
    let (s, b, _) = call(&app, get("/../../etc/passwd")?).await?;
    assert!(
        s == StatusCode::OK && b.starts_with(b"<!doctype html>"),
        "no path traversal"
    );
    for path in ["/api", "/api/nope", "/mcp/x"] {
        let (s, b, _) = call(&app, get(path)?).await?;
        assert_eq!(
            (s, json_of(&b)["detail"]["code"].clone()),
            (StatusCode::NOT_FOUND, json!("not_found")),
            "{path}"
        );
    }
    // 0.1.1 has no /metrics: the SPA answers it like any other non-API path
    let (s, _, _) = call(&app, get("/metrics")?).await?;
    assert_eq!(s, StatusCode::OK, "/metrics is the SPA");

    let bare = build_app(runtime(&db, tempfile::tempdir()?.path(), false).await?);
    let (s, b, _) = call(&bare, get("/")?).await?;
    assert_eq!(
        (s, json_of(&b)["detail"]["code"].clone()),
        (StatusCode::SERVICE_UNAVAILABLE, json!("ui_not_built"))
    );
    Ok(())
}

/// Hashed chunks stay cached for a year, the worker and version file are checked on every load, a
/// missing build file is a plain 404 (never the app's HTML), and no API answer is kept by a cache.
#[tokio::test]
async fn files_carry_the_right_cache_and_missing_files_are_404() -> R {
    let db = TestDb::new().await?;
    let dir = tempfile::tempdir()?;
    let app = build_app(runtime(&db, dir.path(), true).await?);
    let (s, _, cache) = call(&app, get("/_app/immutable/a1b2.js")?).await?;
    assert_eq!(
        (s, cache.as_deref()),
        (StatusCode::OK, Some("public, max-age=31536000, immutable"))
    );
    for path in ["/service-worker.js", "/_app/version.json"] {
        let (s, _, cache) = call(&app, get(path)?).await?;
        assert_eq!((s, cache.as_deref()), (StatusCode::OK, Some("no-cache")), "{path}");
    }
    for path in ["/_app/immutable/gone.js", "/_app/nothing", "/icons/missing.png"] {
        let (s, b, _) = call(&app, get(path)?).await?;
        assert!(
            s == StatusCode::NOT_FOUND && !b.starts_with(b"<!doctype html>"),
            "{path}"
        );
    }
    let (s, _, cache) = call(&app, get("/timeline")?).await?;
    assert_eq!((s, cache.as_deref()), (StatusCode::OK, Some("no-cache")));
    for path in ["/api/me", "/api/nope"] {
        let (_, _, cache) = call(&app, get(path)?).await?;
        assert_eq!(cache.as_deref(), Some("no-store"), "{path}");
    }
    Ok(())
}

#[tokio::test]
async fn mcp_answers_initialize_at_slash_mcp() -> R {
    let db = TestDb::new().await?;
    let dir = tempfile::tempdir()?;
    let app = build_app(runtime(&db, dir.path(), true).await?);
    // every /mcp request, initialize included, needs a PAT (test_mcp_protocol.py)
    let mut conn = db.pool.acquire().await?;
    let fam = ownhealth_core::demo::create_rivera_family(&mut conn, "Rivera").await?;
    let tok = ownhealth_auth::tokens::create_token(
        &mut conn,
        &ownhealth_auth::tokens::TokenIn {
            user_id: fam.user("alex"),
            label: "t",
            profile_ids: vec![fam.profile("alex")],
            scopes: vec!["read:summary".into()],
            default_profile: None,
            days: 30,
            identity_mode: "full",
            lang: "en",
            units: "si",
        },
    )
    .await?;
    drop(conn);
    let init = json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
        "protocolVersion": "2025-11-25", "capabilities": {}, "clientInfo": {"name": "t", "version": "1"}}});
    let req = Request::post("/mcp")
        .header("host", "127.0.0.1")
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .header("authorization", format!("Bearer {}", tok.plaintext))
        .body(Body::from(init.to_string()))?;
    let (s, b, _) = call(&app, req).await?;
    assert_eq!(s, StatusCode::OK, "{}", String::from_utf8_lossy(&b));
    let v = json_of(&b);
    assert_eq!(v["result"]["serverInfo"]["name"], "OwnHealth");
    assert_eq!(v["result"]["protocolVersion"], "2025-11-25");
    Ok(())
}

#[tokio::test]
async fn startup_migrates_and_seeds_the_demo_once() -> R {
    let db = TestDb::empty().await?;
    let dir = tempfile::tempdir()?;
    let mut s = db.settings(dir.path())?;
    s.run_migrations = true;
    s.demo = true;
    let rt = Runtime::with_pool(s, db.pool.clone())?;
    start(&rt).await?.stop().await;
    start(&rt).await?.stop().await;
    let n: i64 = sqlx::query_scalar("select count(*) from household where name = 'Rivera (demo)'")
        .fetch_one(&db.pool)
        .await?;
    assert_eq!(n, 1);
    let profiles: i64 = sqlx::query_scalar("select count(*) from profile")
        .fetch_one(&db.pool)
        .await?;
    assert_eq!(profiles, 4);
    Ok(())
}

#[test]
fn the_dev_secret_key_is_refused_outside_dev_and_demo() -> R {
    let mut env = std::collections::HashMap::new();
    env.insert(
        "OWNHEALTH_DATABASE_URL".to_string(),
        "postgresql://x@localhost/x".to_string(),
    );
    let s = ownhealth_core::config::Settings::from(&env)?;
    assert!(check_secret_key(&s).is_err());
    assert!(
        check_secret_key(&ownhealth_core::config::Settings {
            dev_auth: true,
            ..s.clone()
        })
        .is_ok()
    );
    assert!(
        check_secret_key(&ownhealth_core::config::Settings {
            secret_key: "k".repeat(64),
            ..s
        })
        .is_ok()
    );
    Ok(())
}

#[tokio::test]
async fn ow_paths_outside_the_get_allowlist_never_fall_through_to_the_ui() -> R {
    let db = TestDb::empty().await?;
    let dir = tempfile::tempdir()?;
    let app = build_app(runtime(&db, dir.path(), true).await?);
    for path in [
        "/ow",
        "/ow/",
        "/ow/api/v1/users",
        "/ow/api/v1/oauth/providers",
        "/ow/api/v1/oauth/whoop/authorize",
        "/ow/api/v1/oauth/strava/callback",
        "/ow/api/v1/oauth/apple_health/callback",
        "/ow/api/v1/oauth/WHOOP/callback",
        "/ow/api/v1/oauth/whoop/callback/",
        "/ow/api/v1/oauth/whoop%2Fextra/callback",
        "/ow/api/v1/oauth/success?provider=garmin",
    ] {
        assert_eq!(call(&app, get(path)?).await?.0, StatusCode::NOT_FOUND, "{path}");
    }
    for method in ["HEAD", "POST", "OPTIONS", "PUT", "PATCH", "DELETE", "CONNECT", "TRACE"] {
        let req = Request::builder()
            .method(method)
            .uri("/ow/api/v1/oauth/whoop/callback")
            .body(Body::empty())?;
        assert_eq!(call(&app, req).await?.0, StatusCode::METHOD_NOT_ALLOWED, "{method}");
    }
    Ok(())
}

#[tokio::test]
async fn ow_callback_passes_the_query_only_and_keeps_the_result_under_ow() -> R {
    use axum::http::{Method, Uri, header};
    use axum::response::IntoResponse;
    use axum::routing::get as on_get;
    use ownhealth_core::config::Settings;
    use std::collections::HashMap;

    let fake = axum::Router::new()
        .route(
            "/api/v1/oauth/whoop/callback",
            on_get(|req: Request<Body>| async move {
                let (parts, body) = req.into_parts();
                if parts.method != Method::GET
                    || parts.uri.query() != Some("code=abc%2Bdef&state=xyz")
                    || [
                        header::AUTHORIZATION,
                        header::COOKIE,
                        header::ORIGIN,
                        header::REFERER,
                        header::FORWARDED,
                    ]
                    .iter()
                    .any(|name| parts.headers.contains_key(name))
                    || parts.headers.contains_key("x-open-wearables-api-key")
                    || !to_bytes(body, 1024).await.is_ok_and(|bytes| bytes.is_empty())
                {
                    return StatusCode::BAD_REQUEST.into_response();
                }
                (
                    StatusCode::SEE_OTHER,
                    [
                        (header::LOCATION, "/api/v1/oauth/success?provider=whoop&user_id=demo"),
                        (header::SET_COOKIE, "fake=sidecar; Path=/"),
                    ],
                )
                    .into_response()
            }),
        )
        .route(
            "/api/v1/oauth/success",
            on_get(|uri: Uri| async move { axum::Json(json!({"success": true, "query": uri.query()})) }),
        )
        .route(
            "/api/v1/oauth/error",
            on_get(|| async { axum::Json(json!({"success": false})) }),
        )
        .route(
            "/api/v1/oauth/oura/callback",
            on_get(|| async { (StatusCode::SEE_OTHER, [(header::LOCATION, "/api/v1/users")]) }),
        )
        .route("/api/v1/users", on_get(|| async { "private upstream route" }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let origin = format!("http://{}", listener.local_addr()?);
    let task = tokio::spawn(async move { axum::serve(listener, fake).await });
    let db = TestDb::empty().await?;
    let dir = tempfile::tempdir()?;
    let settings = db.settings(dir.path())?;
    let overrides = HashMap::from([
        ("OWNHEALTH_OW_URL".into(), origin),
        ("OWNHEALTH_DATA_DIR".into(), settings.data_dir.display().to_string()),
        ("OWNHEALTH_SECRETS_KEY".into(), settings.secrets_key.unwrap_or_default()),
    ]);
    let app = build_app(Runtime::with_pool(Settings::from(&overrides)?, db.pool.clone())?);
    let req = Request::get("/ow/api/v1/oauth/whoop/callback?code=abc%2Bdef&state=xyz")
        .header(header::AUTHORIZATION, "Bearer synthetic")
        .header(header::COOKIE, "synthetic=x")
        .header(header::ORIGIN, "https://ownhealth.invalid")
        .header(header::REFERER, "https://ownhealth.invalid/connections")
        .header(header::FORWARDED, "for=192.0.2.1")
        .header("x-open-wearables-api-key", "synthetic")
        .body(Body::from("must not forward"))?;
    let res = app.clone().oneshot(req).await?;
    assert_eq!(res.status(), StatusCode::SEE_OTHER);
    assert_eq!(res.headers()[header::CACHE_CONTROL], "no-store");
    assert_eq!(res.headers()[header::REFERRER_POLICY], "no-referrer");
    assert!(!res.headers().contains_key(header::SET_COOKIE));
    let location = res.headers()[header::LOCATION].to_str()?;
    assert_eq!(location, "/ow/api/v1/oauth/success?provider=whoop&user_id=demo");
    let (status, bytes, _) = call(&app, get(location)?).await?;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        json_of(&bytes),
        json!({"success": true, "query": "provider=whoop&user_id=demo"})
    );
    let (status, bytes, _) = call(&app, get("/ow/api/v1/oauth/error?message=declined")?).await?;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json_of(&bytes), json!({"success": false}));
    for path in [
        "/ow/api/v1/oauth/success?provider=whoop&provider=garmin",
        "/ow/api/v1/oauth/success?provider=garmin",
        "/ow/api/v1/oauth/success",
        "/ow/api/v1/oauth/error?provider=strava",
    ] {
        assert_eq!(call(&app, get(path)?).await?.0, StatusCode::NOT_FOUND, "{path}");
    }
    assert_eq!(
        call(&app, get("/ow/api/v1/oauth/oura/callback")?).await?.0,
        StatusCode::BAD_GATEWAY
    );
    task.abort();
    Ok(())
}
