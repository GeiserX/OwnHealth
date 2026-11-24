use super::*;
use axum::{
    Json, Router,
    extract::{Form, Path, Query, Request, State},
    http::{HeaderMap, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{delete, get, post, put},
};
use chrono::{Duration, Utc};
use ownhealth_core::{Runtime, demo::create_rivera_family, ids::token_hex, testing::TestDb};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

const SPEC: &str = include_str!("../../../../../fixtures/openwearables/openapi.json");
const DISABLED: [&str; 10] = [
    "strava",
    "garmin",
    "fitbit",
    "google_health",
    "samsung",
    "suunto",
    "ultrahuman",
    "sensorbio",
    "health_connect",
    "apple",
];

struct Fake {
    key: String,
    password: String,
    bearer: String,
    deleted: Vec<Uuid>,
    fail_delete: bool,
    disabled: Vec<String>,
    cursors: Vec<Option<String>>,
    connections: Value,
    t: String,
}

type Shared = Arc<Mutex<Fake>>;

async fn auth(State(s): State<Shared>, req: Request, next: Next) -> Response {
    let authorized = {
        let s = s.lock().expect("fake state");
        req.headers()
            .get("x-open-wearables-api-key")
            .and_then(|v| v.to_str().ok())
            == Some(&s.key)
    };
    if authorized {
        next.run(req).await
    } else {
        StatusCode::UNAUTHORIZED.into_response()
    }
}

async fn login(State(s): State<Shared>, Form(form): Form<HashMap<String, String>>) -> Response {
    let s = s.lock().expect("fake state");
    if form
        .get("username")
        .is_some_and(|name| name == "operator@example.invalid")
        && form.get("password") == Some(&s.password)
    {
        return Json(json!({"access_token": s.bearer, "token_type": "bearer"})).into_response();
    }
    StatusCode::UNAUTHORIZED.into_response()
}

fn provider(name: &str, enabled: bool) -> Value {
    json!({"provider": name, "name": name, "has_cloud_api": true, "is_enabled": enabled, "icon_url": ""})
}

async fn providers() -> Json<Vec<Value>> {
    Json(
        DISABLED
            .into_iter()
            .chain(["polar", "oura", "whoop", "withings", "nightscout", "future_vendor"])
            .map(|p| provider(p, true))
            .collect(),
    )
}

async fn disable(State(s): State<Shared>, Path(p): Path<String>, h: HeaderMap, Json(v): Json<Value>) -> Response {
    let mut s = s.lock().expect("fake state");
    if h.get("authorization").and_then(|v| v.to_str().ok()) != Some(&format!("Bearer {}", s.bearer)) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    assert_eq!(v, json!({"is_enabled": false}));
    s.disabled.push(p.clone());
    Json(provider(&p, false)).into_response()
}

async fn user(Path(id): Path<Uuid>) -> Json<Value> {
    Json(json!({"id": id, "created_at": Utc::now()}))
}

async fn remove(State(s): State<Shared>, Path(id): Path<Uuid>) -> Response {
    {
        let mut state = s.lock().expect("fake state");
        if state.fail_delete {
            return StatusCode::SERVICE_UNAVAILABLE.into_response();
        }
        state.deleted.push(id);
    }
    user(Path(id)).await.into_response()
}

async fn connections(State(s): State<Shared>, Path(user): Path<Uuid>) -> Json<Vec<Value>> {
    let s = s.lock().expect("fake state");
    Json(
        s.connections
            .as_array()
            .expect("connection list")
            .iter()
            .map(|v| {
                json!({"user_id": user, "provider": v[0], "status": v[1], "id": Uuid::nil(),
            "last_synced_at": s.t, "created_at": s.t, "updated_at": s.t})
            })
            .collect(),
    )
}

fn page(data: Vec<Value>, cursor: Option<&str>, has_more: bool) -> Json<Value> {
    Json(json!({"data": data, "pagination": {"next_cursor": cursor, "has_more": has_more}, "metadata": {}}))
}

async fn series(State(s): State<Shared>, Query(q): Query<HashMap<String, String>>) -> Json<Value> {
    assert!(q.contains_key("start_time") && q.contains_key("end_time"));
    let mut s = s.lock().expect("fake state");
    if q.get("resolution").is_some_and(|r| r == "1day") {
        let t = s.t.parse::<chrono::DateTime<Utc>>().expect("fixture timestamp");
        return page(
            vec![json!({"type": "heart_rate_variability_rmssd", "timestamp": t,
            "value": 48.0, "unit": "ms", "source": {"provider": "polar"}, "computed_by": "provider",
            "zone_offset": "-03:30", "period_end": t + Duration::days(1)})],
            None,
            false,
        );
    }
    s.cursors.push(q.get("cursor").cloned());
    let row = |p: &str, metric: &str, v: f64, unit: &str, window: Option<i64>| {
        json!({"timestamp": s.t, "zone_offset": "-03:30", "type": metric, "value": v,
            "unit": unit, "source": {"provider": p}, "window_seconds": window})
    };
    if let Some(cursor) = q.get("cursor") {
        assert_eq!(cursor, "next+opaque/=");
        page(
            vec![
                row("garmin", "steps", 30., "count", None),
                row("polar", "heart_rate_variability_rmssd", 17., "ms", Some(60)),
                row("whoop", "workout_effort_score", 8., "score", None),
                row("nightscout", "steps", 10., "count", None),
            ],
            None,
            false,
        )
    } else {
        page(
            vec![
                row("strava", "heart_rate", 78., "bpm", None),
                row("oura", "steps", 27., "count", None),
            ],
            Some("next+opaque/="),
            true,
        )
    }
}

async fn scores(State(s): State<Shared>) -> Json<Value> {
    let s = s.lock().expect("fake state");
    page(
        vec![
            json!({"id": "recovery", "category": "recovery", "recorded_at": s.t, "provider": "oura",
                     "value": 81, "qualifier": "Good", "components": {"rest": {"value": 20, "unit": "score"}}}),
            json!({"id": "garmin", "category": "body_battery", "recorded_at": s.t, "provider": "garmin",
                     "value": 64}),
            json!({"id": "internal", "category": "sleep", "recorded_at": s.t,
                     "provider": "internal", "value": 72}),
        ],
        None,
        false,
    )
}

struct Env {
    _db: TestDb,
    _dir: tempfile::TempDir,
    ctx: JobContext,
    fake: Shared,
    base: Url,
    profile: Uuid,
    user: Uuid,
}

impl Env {
    async fn new() -> anyhow::Result<Self> {
        let db = TestDb::new().await?;
        let dir = tempfile::tempdir()?;
        let rt = Runtime::with_pool(db.settings(dir.path())?, db.pool.clone())?;
        let mut conn = db.pool.acquire().await?;
        let family = create_rivera_family(&mut conn, "Rivera").await?;
        let fake = Arc::new(Mutex::new(Fake {
            key: token_hex(20),
            password: token_hex(24),
            bearer: token_hex(20),
            deleted: vec![],
            fail_delete: false,
            disabled: vec![],
            cursors: vec![],
            connections: json!([["polar", "active"], ["oura", "active"], ["whoop", "active"]]),
            t: (Utc::now() - Duration::hours(1)).to_rfc3339(),
        }));
        let spec: Value = serde_json::from_str(SPEC)?;
        let route = |tail: &str, method: &str| {
            let p = format!("/api/v1{tail}");
            assert!(spec["paths"][&p][method].is_object());
            p
        };
        let data = Router::new()
            .route(&route("/users/{user_id}", "get"), get(user))
            .route(&route("/users/{user_id}", "delete"), delete(remove))
            .route(&route("/users/{user_id}/connections", "get"), get(connections))
            .route(&route("/users/{user_id}/timeseries", "get"), get(series))
            .route(&route("/users/{user_id}/health-scores", "get"), get(scores))
            .route_layer(middleware::from_fn_with_state(fake.clone(), auth));
        let app = data
            .route(&route("/auth/login", "post"), post(login))
            .route(&route("/oauth/providers", "get"), get(providers))
            .route(&route("/oauth/providers/{provider}", "put"), put(disable))
            .with_state(fake.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let base = Url::parse(&format!("http://{}", listener.local_addr()?))?;
        tokio::spawn(async { axum::serve(listener, app).await.expect("serve fixture") });
        Ok(Self {
            _db: db,
            _dir: dir,
            ctx: rt.job_context(),
            fake,
            base,
            profile: family.profile("sam"),
            user: Uuid::new_v4(),
        })
    }

    async fn seed_source(&self) -> anyhow::Result<Uuid> {
        let mut tx = self.ctx.pool.begin().await?;
        let household: Uuid = sqlx::query_scalar("select household_id from profile where id = $1")
            .bind(self.profile)
            .fetch_one(&mut *tx)
            .await?;
        let key = SecretString::new(&self.fake.lock().expect("fake state").key);
        let secret = ownhealth_core::secrets::store_secret(
            &mut tx,
            &ownhealth_core::secrets::SecretBox::from_settings(&self.ctx.settings)?,
            household,
            "openwearables_api_key",
            None,
            &key,
        )
        .await?;
        let source = ownhealth_core::sources::create_source(
            &mut tx,
            &ownhealth_core::sources::NewSource {
                profile_id: Some(self.profile),
                poll_seconds: Some(900),
                ..ownhealth_core::sources::NewSource::new(household, "openwearables", "Open Wearables")
            },
        )
        .await?;
        sqlx::query("insert into openwearables_user (user_id, source_id, secret_id) values ($1, $2, $3)")
            .bind(self.user)
            .bind(source)
            .bind(secret)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(source)
    }

    async fn setup(&self) -> anyhow::Result<Uuid> {
        let (password, key) = {
            let s = self.fake.lock().expect("fake state");
            (SecretString::new(&s.password), SecretString::new(&s.key))
        };
        Ok(setup(
            &self.ctx,
            &self.base,
            self.profile,
            self.user,
            "operator@example.invalid",
            &password,
            &key,
        )
        .await?)
    }
}

#[tokio::test]
async fn setup_disables_every_unlisted_provider_and_seals_the_key() -> anyhow::Result<()> {
    let env = Env::new().await?;
    let source = env.setup().await?;
    assert_eq!(env.setup().await?, source);
    let expected = DISABLED
        .into_iter()
        .chain(["nightscout", "future_vendor"])
        .collect::<std::collections::BTreeSet<_>>();
    let key = {
        let state = env.fake.lock().expect("fake state");
        assert_eq!(
            state
                .disabled
                .iter()
                .map(String::as_str)
                .collect::<std::collections::BTreeSet<_>>(),
            expected
        );
        state.key.clone()
    };
    let (config, ciphertext): (Value, Vec<u8>) = sqlx::query_as(
        "select s.config, k.ciphertext from source s join openwearables_user w on w.source_id = s.id join secret k on k.id = w.secret_id where s.id = $1 and k.purpose = 'openwearables_api_key'"
    ).bind(source).fetch_one(&env.ctx.pool).await?;
    assert!(!config.to_string().contains(&key));
    assert!(!ciphertext.windows(key.len()).any(|bytes| bytes == key.as_bytes()));
    Ok(())
}

#[tokio::test]
async fn pages_rows_filters_providers_and_keeps_hrv_windows_and_scores() -> anyhow::Result<()> {
    let env = Env::new().await?;
    let id = env.seed_source().await?;
    let out = poll(&env.ctx, &env.base, id).await?;
    assert_eq!(out.skipped.get("provider_strava"), Some(&1));
    assert_eq!(out.skipped.get("provider_garmin"), Some(&2));
    assert_eq!(out.skipped.get("provider_other"), Some(&2));
    assert_eq!(
        env.fake.lock().expect("fake state").cursors,
        [None, Some("next+opaque/=".into())]
    );
    let actual: Vec<(String, String, Option<i16>, Option<i32>)> = sqlx::query_as(
        "select s.provider, w.metric, w.tz_offset_min, extract(epoch from w.t_end - w.t)::int
         from wearable_sample w join wearable_source s on s.id = w.source_id
         where w.profile_id = $1 order by s.provider, w.v",
    )
    .bind(env.profile)
    .fetch_all(&env.ctx.pool)
    .await?;
    assert_eq!(
        actual,
        [
            ("oura".into(), "steps".into(), Some(-210), None),
            ("polar".into(), "hrv_rmssd".into(), Some(-210), Some(60)),
            ("polar".into(), "hrv_rmssd".into(), Some(-210), Some(86400))
        ]
    );
    let actual: Vec<(String, String)> =
        sqlx::query_as("select provider, category from vendor_score where profile_id = $1 order by provider")
            .bind(env.profile)
            .fetch_all(&env.ctx.pool)
            .await?;
    assert_eq!(
        actual,
        [("oura".into(), "recovery".into()), ("whoop".into(), "activity".into())]
    );
    let digest: String = sqlx::query_scalar("select encode(blob_sha256, 'hex') from source_item where source_id = $1")
        .bind(id)
        .fetch_one(&env.ctx.pool)
        .await?;
    let bytes = env.ctx.blobs.get_bytes(BlobOwner::Profile(env.profile), &digest)?;
    let bundle: Value = serde_json::from_slice(&bytes)?;
    assert_eq!(bundle["raw"].as_array().map(Vec::len), Some(3));
    assert_eq!(bundle["daily"].as_array().map(Vec::len), Some(1));
    assert_eq!(bundle["scores"].as_array().map(Vec::len), Some(1));
    let rerun = poll(&env.ctx, &env.base, id).await?;
    assert_eq!(rerun.new, 0);
    assert!(rerun.duplicate >= 5);
    Ok(())
}

#[tokio::test]
async fn expired_and_revoked_rows_are_left_out_and_mark_reconsent() -> anyhow::Result<()> {
    let env = Env::new().await?;
    let id = env.seed_source().await?;
    env.fake.lock().expect("fake state").connections =
        json!([["polar", "expired"], ["oura", "revoked"], ["whoop", "active"]]);
    let out = poll(&env.ctx, &env.base, id).await?;
    assert_eq!(out.skipped.get("connection_expired"), Some(&2));
    assert_eq!(out.skipped.get("connection_revoked"), Some(&2));
    assert!(
        sqlx::query_scalar::<_, bool>("select reconsent_needed_at is not null from source where id = $1")
            .bind(id)
            .fetch_one(&env.ctx.pool)
            .await?
    );
    env.fake.lock().expect("fake state").connections =
        json!([["polar", "active"], ["oura", "active"], ["whoop", "active"]]);
    poll(&env.ctx, &env.base, id).await?;
    assert!(
        !sqlx::query_scalar::<_, bool>("select reconsent_needed_at is not null from source where id = $1")
            .bind(id)
            .fetch_one(&env.ctx.pool)
            .await?
    );
    Ok(())
}

#[tokio::test]
async fn a_deleted_profiles_last_user_is_removed_next_cycle() -> anyhow::Result<()> {
    let env = Env::new().await?;
    let id = env.seed_source().await?;
    assert_eq!(reconcile(&env.ctx, &env.base).await?, 0);
    // Profile deletion removes the profile's source before the profile.
    sqlx::query("delete from source where id = $1")
        .bind(id)
        .execute(&env.ctx.pool)
        .await?;
    schedule_reconcile(&env.ctx).await?;
    let queued: i64 = sqlx::query_scalar("select count(*) from job where kind = 'openwearables.reconcile'")
        .fetch_one(&env.ctx.pool)
        .await?;
    assert_eq!(queued, 1);
    env.fake.lock().expect("fake state").fail_delete = true;
    assert!(reconcile(&env.ctx, &env.base).await.is_err());
    assert_eq!(tombstones(&env.ctx.pool).await?, [env.user]);
    env.fake.lock().expect("fake state").fail_delete = false;
    assert_eq!(reconcile(&env.ctx, &env.base).await?, 1);
    assert_eq!(reconcile(&env.ctx, &env.base).await?, 0);
    assert_eq!(env.fake.lock().expect("fake state").deleted, [env.user]);
    Ok(())
}
