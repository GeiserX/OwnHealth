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
    collections::{BTreeSet, HashMap},
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
/// Rows the fake sends per page, so every read pages.
const FAKE_PAGE: usize = 2;

/// A stored sample: provider, device, metric, zone offset and window seconds.
type StoredSample = (String, String, String, Option<i16>, Option<i32>);

/// One request the fake answered: the route's last segment and its query pairs.
type Asked = (String, Vec<(String, String)>);

struct Fake {
    key: String,
    password: String,
    bearer: String,
    deleted: Vec<Uuid>,
    /// What DELETE /users answers; None is 200 with the user.
    delete_status: Option<StatusCode>,
    disabled: Vec<String>,
    /// A provider whose disable PUT answers that it is still on.
    keep_on: Option<String>,
    connections: Value,
    /// Time-series rows, each with the `resolution` it is served at.
    rows: Vec<Value>,
    scores: Vec<Value>,
    /// A sidecar that ignores `provider=` when asked for this provider: it sends every provider's rows.
    ignore_filter_for: Option<String>,
    /// Time-series requests that start at or after this instant answer 503.
    fail_from: Option<DateTime<Utc>>,
    /// The time series sends one cursor again and again.
    repeat_cursor: bool,
    /// SQL run once, keyed by route ("put" at the first disable PUT, "scores" at the first score read).
    hooks: HashMap<&'static str, (PgPool, String)>,
    /// Whether each hook's SQL succeeded.
    hooked: Vec<(&'static str, bool)>,
    asked: Vec<Asked>,
    t: DateTime<Utc>,
}

type Shared = Arc<Mutex<Fake>>;

async fn run_hook(s: &Shared, name: &'static str) {
    let hook = s.lock().expect("fake state").hooks.remove(name);
    if let Some((pool, sql)) = hook {
        let ok = sqlx::query(sqlx::AssertSqlSafe(sql)).execute(&pool).await.is_ok();
        s.lock().expect("fake state").hooked.push((name, ok));
    }
}

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
    run_hook(&s, "put").await;
    let mut s = s.lock().expect("fake state");
    if h.get("authorization").and_then(|v| v.to_str().ok()) != Some(&format!("Bearer {}", s.bearer)) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    assert_eq!(v, json!({"is_enabled": false}));
    s.disabled.push(p.clone());
    let still_on = s.keep_on.as_deref() == Some(p.as_str());
    Json(provider(&p, still_on)).into_response()
}

async fn user(Path(id): Path<Uuid>) -> Json<Value> {
    Json(json!({"id": id, "created_at": Utc::now()}))
}

async fn remove(State(s): State<Shared>, Path(id): Path<Uuid>) -> Response {
    {
        let mut state = s.lock().expect("fake state");
        state.deleted.push(id);
        if let Some(code) = state.delete_status {
            return code.into_response();
        }
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

fn page(data: Vec<Value>, cursor: Option<String>, has_more: bool) -> Json<Value> {
    Json(json!({"data": data, "pagination": {"next_cursor": cursor, "has_more": has_more}, "metadata": {}}))
}

fn one<'a>(q: &'a [(String, String)], name: &str) -> Option<&'a str> {
    q.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
}

fn instant(v: &str) -> DateTime<Utc> {
    v.parse().expect("an RFC 3339 instant")
}

/// The rows a sidecar holds for this request's provider in [start, end), one page after `offset`.
fn serve(all: &[Value], q: &[(String, String)], ignore: Option<&str>, at: &str, offset: usize) -> (Vec<Value>, bool) {
    let (start, end) = (
        instant(one(q, "start_time").or(one(q, "start_date")).expect("start")),
        instant(one(q, "end_time").or(one(q, "end_date")).expect("end")),
    );
    let wanted = one(q, "provider").expect("a provider filter");
    let types: Vec<&str> = q
        .iter()
        .filter(|(k, _)| k == "types")
        .map(|(_, v)| v.as_str())
        .collect();
    let resolution = one(q, "resolution");
    let rows: Vec<Value> = all
        .iter()
        .filter(|row| {
            let t = instant(row[at].as_str().expect("row time"));
            // a score names its provider itself; a sample in its source
            let provider = row["provider"].as_str().or(row["source"]["provider"].as_str());
            (ignore == Some(wanted) || provider == Some(wanted))
                && t >= start
                && t < end
                && resolution.is_none_or(|r| row["resolution"] == r)
                && (types.is_empty() || types.contains(&row["type"].as_str().unwrap_or_default()))
        })
        .cloned()
        .collect();
    let has_more = rows.len() > offset + FAKE_PAGE;
    (rows.into_iter().skip(offset).take(FAKE_PAGE).collect(), has_more)
}

async fn series(State(s): State<Shared>, Query(q): Query<Vec<(String, String)>>) -> Response {
    let mut s = s.lock().expect("fake state");
    s.asked.push(("timeseries".into(), q.clone()));
    if s.fail_from
        .is_some_and(|from| instant(one(&q, "start_time").expect("start")) >= from)
    {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    // Cursors carry characters a query must escape.
    let offset = one(&q, "cursor").map_or(0, |c| {
        c.trim_start_matches("c+")
            .trim_end_matches("/=")
            .parse()
            .expect("fake cursor")
    });
    let (data, has_more) = serve(&s.rows, &q, s.ignore_filter_for.as_deref(), "timestamp", offset);
    let next = if s.repeat_cursor { 0 } else { offset + FAKE_PAGE };
    let has_more = has_more || s.repeat_cursor;
    page(data, has_more.then(|| format!("c+{next}/=")), has_more).into_response()
}

async fn scores(State(s): State<Shared>, Query(q): Query<Vec<(String, String)>>) -> Json<Value> {
    run_hook(&s, "scores").await;
    let mut s = s.lock().expect("fake state");
    s.asked.push(("health-scores".into(), q.clone()));
    let offset = one(&q, "offset").map_or(0, |o| o.parse().expect("offset"));
    let (data, has_more) = serve(&s.scores, &q, s.ignore_filter_for.as_deref(), "recorded_at", offset);
    page(data, None, has_more)
}

fn sample(resolution: &str, provider: &str, series: &str, value: f64, unit: &str, t: DateTime<Utc>) -> Value {
    json!({"resolution": resolution, "timestamp": t.with_timezone(&FixedOffset::west_opt(210 * 60).expect("offset")),
        "zone_offset": "-03:30", "type": series, "value": value, "unit": unit, "source": {"provider": provider},
        "computed_by": if resolution == "1day" { Some("provider") } else { None }})
}

struct Env {
    _db: TestDb,
    _dir: tempfile::TempDir,
    ctx: JobContext,
    fake: Shared,
    base: Url,
    profile: Uuid,
    other_profile: Uuid,
    user: Uuid,
}

impl Env {
    async fn new() -> anyhow::Result<Self> {
        let db = TestDb::new().await?;
        let dir = tempfile::tempdir()?;
        let rt = Runtime::with_pool(db.settings(dir.path())?, db.pool.clone())?;
        let mut conn = db.pool.acquire().await?;
        let family = create_rivera_family(&mut conn, "Rivera").await?;
        let t = Utc::now() - Duration::hours(1);
        let mut hrv = sample("1day", "polar", "heart_rate_variability_rmssd", 48.0, "ms", t);
        hrv["period_end"] = json!(t + Duration::days(1));
        let mut spot = sample("raw", "polar", "heart_rate_variability_rmssd", 17.0, "ms", t);
        spot["window_seconds"] = json!(60);
        let mut ow_steps = sample("1day", "oura", "steps", 9000.0, "count", t);
        ow_steps["computed_by"] = json!("open_wearables");
        let fake = Arc::new(Mutex::new(Fake {
            key: token_hex(20),
            password: token_hex(24),
            bearer: token_hex(20),
            deleted: vec![],
            delete_status: None,
            disabled: vec![],
            keep_on: None,
            connections: json!([["polar", "active"], ["oura", "active"], ["whoop", "active"]]),
            rows: vec![
                sample("raw", "strava", "heart_rate", 78., "bpm", t),
                sample("raw", "oura", "steps", 27., "count", t),
                sample("raw", "garmin", "steps", 30., "count", t),
                spot,
                sample("raw", "whoop", "workout_effort_score", 8., "score", t),
                sample("raw", "nightscout", "steps", 10., "count", t),
                sample("raw", "polar", "latitude", 40.4, "deg", t),
                hrv,
                ow_steps,
            ],
            scores: vec![
                json!({"id": "recovery", "category": "recovery", "recorded_at": t, "provider": "oura",
                     "value": 81, "qualifier": "Good", "components": {"rest": {"value": 20, "unit": "score"}}}),
                json!({"id": "garmin", "category": "body_battery", "recorded_at": t, "provider": "garmin", "value": 64}),
                json!({"id": "internal", "category": "sleep", "recorded_at": t, "provider": "internal", "value": 72}),
                json!({"id": "relabelled", "category": "sleep", "recorded_at": t, "provider": "oura",
                     "source": {"provider": "polar"}, "value": 70}),
            ],
            ignore_filter_for: None,
            fail_from: None,
            repeat_cursor: false,
            hooks: HashMap::new(),
            hooked: vec![],
            asked: vec![],
            t,
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
            other_profile: family.profile("alex"),
            user: Uuid::new_v4(),
        })
    }

    fn fake(&self) -> std::sync::MutexGuard<'_, Fake> {
        self.fake.lock().expect("fake state")
    }

    async fn seed_source(&self) -> anyhow::Result<Uuid> {
        let mut tx = self.ctx.pool.begin().await?;
        let household: Uuid = sqlx::query_scalar("select household_id from profile where id = $1")
            .bind(self.profile)
            .fetch_one(&mut *tx)
            .await?;
        let key = SecretString::new(&self.fake().key);
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

    async fn setup_as(&self, profile: Uuid, password: &str, key: &str) -> Result<Uuid> {
        setup(
            &self.ctx,
            &self.base,
            profile,
            self.user,
            "operator@example.invalid",
            &SecretString::new(password),
            &SecretString::new(key),
        )
        .await
    }

    async fn setup(&self) -> anyhow::Result<Uuid> {
        let (password, key) = {
            let s = self.fake();
            (s.password.clone(), s.key.clone())
        };
        Ok(self.setup_as(self.profile, &password, &key).await?)
    }

    /// The vendor's cursor: where its last stored day ended.
    async fn mark(&self, source: Uuid, provider: &str) -> anyhow::Result<Option<DateTime<Utc>>> {
        let cursor: Option<Value> = sqlx::query_scalar("select cursor from source where id = $1")
            .bind(source)
            .fetch_one(&self.ctx.pool)
            .await?;
        Ok(super::mark(cursor.as_ref(), provider))
    }

    /// The start of the first time-series request for `provider` since request number `after`.
    fn first_start(&self, after: usize, provider: &str) -> Option<DateTime<Utc>> {
        self.fake().asked[after..]
            .iter()
            .find(|(part, q)| part == "timeseries" && one(q, "provider") == Some(provider))
            .map(|(_, q)| instant(one(q, "start_time").expect("start")))
    }

    async fn count(&self, sql: &str) -> anyhow::Result<i64> {
        Ok(sqlx::query_scalar(sqlx::AssertSqlSafe(sql.to_string()))
            .bind(self.profile)
            .fetch_one(&self.ctx.pool)
            .await?)
    }
}

#[test]
fn the_allowlist_is_the_policy_files_open_wearables_lines() {
    let via_ow: BTreeSet<&str> = providers::policy()
        .providers
        .iter()
        .filter(|(_, p)| p.path == "Open Wearables" && p.ingest == providers::Ingest::Allow)
        .map(|(name, _)| name.as_str())
        .collect();
    assert_eq!(via_ow, PROVIDERS.into_iter().collect());
}

#[tokio::test]
async fn setup_switches_off_every_unlisted_provider_holds_no_lock_while_asking_and_seals_the_key() -> anyhow::Result<()>
{
    let env = Env::new().await?;
    // While setup talks to the sidecar, a profile delete must not wait behind it.
    env.fake().hooks.insert(
        "put",
        (
            env.ctx.pool.clone(),
            format!("select id from profile where id = '{}' for update nowait", env.profile),
        ),
    );
    let source = env.setup().await?;
    assert_eq!(env.fake().hooked, [("put", true)]);
    assert_eq!(env.setup().await?, source);
    let expected = DISABLED
        .into_iter()
        .chain(["nightscout", "future_vendor"])
        .collect::<BTreeSet<_>>();
    let key = {
        let state = env.fake();
        assert_eq!(
            state.disabled.iter().map(String::as_str).collect::<BTreeSet<_>>(),
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
async fn setup_refuses_a_provider_the_sidecar_keeps_on() -> anyhow::Result<()> {
    let env = Env::new().await?;
    env.fake().keep_on = Some("garmin".into());
    let err = env.setup().await.expect_err("garmin stayed on");
    assert!(err.to_string().contains("did not switch off garmin"), "{err}");
    assert_eq!(env.count("select count(*) from source where profile_id = $1").await?, 0);
    Ok(())
}

#[tokio::test]
async fn setup_names_the_failing_step_and_refuses_another_profiles_user() -> anyhow::Result<()> {
    let env = Env::new().await?;
    let (password, key) = {
        let s = env.fake();
        (s.password.clone(), s.key.clone())
    };
    let err = env
        .setup_as(env.profile, &password, "wrong-key")
        .await
        .expect_err("wrong key");
    assert!(
        err.text().starts_with("ow_http: User check with the API key"),
        "{}",
        err.text()
    );
    let err = env
        .setup_as(env.profile, "wrong", &key)
        .await
        .expect_err("wrong password");
    assert!(err.text().starts_with("ow_http: Developer sign-in"), "{}", err.text());

    env.setup().await?;
    let switched = env.fake().disabled.len();
    let err = env
        .setup_as(env.other_profile, &password, &key)
        .await
        .expect_err("linked to Sam");
    assert!(err.text().contains("linked to another profile"), "{}", err.text());
    // Refused before any request: no provider was switched again.
    assert_eq!(env.fake().disabled.len(), switched);
    let profiles: Vec<Uuid> = sqlx::query_scalar(
        "select s.profile_id from openwearables_user w join source s on s.id = w.source_id where w.user_id = $1",
    )
    .bind(env.user)
    .fetch_all(&env.ctx.pool)
    .await?;
    assert_eq!(profiles, [env.profile]);
    Ok(())
}

#[tokio::test]
async fn the_operator_names_a_profile_by_id_or_short_name() -> anyhow::Result<()> {
    let env = Env::new().await?;
    assert_eq!(profile_id(&env.ctx.pool, "sam").await?, env.profile);
    assert_eq!(profile_id(&env.ctx.pool, &env.profile.to_string()).await?, env.profile);
    let err = profile_id(&env.ctx.pool, "nobody").await.expect_err("no such profile");
    assert!(err.text().contains("No profile is named nobody"), "{}", err.text());
    Ok(())
}

#[tokio::test]
async fn each_vendor_is_read_by_day_for_mapped_series_and_lands_with_its_hrv_window_scores_and_device()
-> anyhow::Result<()> {
    let env = Env::new().await?;
    let id = env.seed_source().await?;
    // The sidecar sends every provider's rows when asked for Polar's.
    env.fake().ignore_filter_for = Some("polar".into());
    let out = poll(&env.ctx, &env.base, id).await?;
    assert_eq!(out.skipped.get("provider_strava"), Some(&1));
    assert_eq!(out.skipped.get("provider_garmin"), Some(&2), "{:?}", out.skipped);
    // nightscout and the internal score; the score Oura sent under Polar's name
    assert_eq!(out.skipped.get("provider_other"), Some(&3), "{:?}", out.skipped);
    assert_eq!(out.skipped.get("provider_mismatch"), Some(&5), "{:?}", out.skipped);
    assert_eq!(
        out.skipped.get("computed_by_open_wearables"),
        Some(&1),
        "{:?}",
        out.skipped
    );
    assert!(out.problems.is_empty());

    let types: BTreeSet<&str> = series_types().into_iter().collect();
    {
        let fake = env.fake();
        let mut asked = BTreeSet::new();
        for (part, q) in &fake.asked {
            let provider = one(q, "provider").expect("a provider filter");
            asked.insert(provider.to_string());
            let start = instant(one(q, "start_time").or(one(q, "start_date")).expect("start"));
            let end = instant(one(q, "end_time").or(one(q, "end_date")).expect("end"));
            assert!(end - start <= Duration::days(1), "{part}: {q:?}");
            if part == "timeseries" {
                let sent: BTreeSet<&str> = q
                    .iter()
                    .filter(|(k, _)| k == "types")
                    .map(|(_, v)| v.as_str())
                    .collect();
                assert_eq!(sent, types);
            }
        }
        assert_eq!(asked, BTreeSet::from(["oura".into(), "polar".into(), "whoop".into()]));
        // The cursor pages, escaped.
        assert!(fake.asked.iter().any(|(_, q)| one(q, "cursor") == Some("c+2/=")));
    }
    assert!(types.contains("heart_rate") && types.contains("workout_effort_score") && !types.contains("latitude"));

    let actual: Vec<StoredSample> = sqlx::query_as(
        "select s.provider, s.device, w.metric, w.tz_offset_min, extract(epoch from w.t_end - w.t)::int
         from wearable_sample w join wearable_source s on s.id = w.source_id
         where w.profile_id = $1 order by s.provider, w.v",
    )
    .bind(env.profile)
    .fetch_all(&env.ctx.pool)
    .await?;
    assert_eq!(
        actual,
        [
            ("oura".into(), "Oura Ring".into(), "steps".into(), Some(-210), None),
            ("polar".into(), "Polar".into(), "hrv_rmssd".into(), Some(-210), Some(60)),
            (
                "polar".into(),
                "Polar Nightly Recharge".into(),
                "hrv_rmssd".into(),
                Some(-210),
                Some(86400)
            )
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
    // Each stored row is remembered once, in the bundle of its vendor's day.
    let items: Vec<String> =
        sqlx::query_scalar("select encode(blob_sha256, 'hex') from source_item where source_id = $1 order by 1")
            .bind(id)
            .fetch_all(&env.ctx.pool)
            .await?;
    assert_eq!(items.len(), 5);
    let mut rows = 0;
    for digest in items.iter().collect::<BTreeSet<_>>() {
        let bundle: Value = serde_json::from_slice(&env.ctx.blobs.get_bytes(BlobOwner::Profile(env.profile), digest)?)?;
        rows += ["daily", "raw", "scores"]
            .iter()
            .map(|k| bundle[k].as_array().map_or(0, Vec::len))
            .sum::<usize>();
    }
    assert_eq!(rows, 5);
    Ok(())
}

#[tokio::test]
async fn a_poll_reads_seven_days_again_but_stores_only_rows_never_seen() -> anyhow::Result<()> {
    let env = Env::new().await?;
    let id = env.seed_source().await?;
    poll(&env.ctx, &env.base, id).await?;
    let until = env.mark(id, "polar").await?.expect("Polar's cursor");
    let items = "select count(*) from source_item where profile_id = $1";
    let blobs = "select count(distinct blob_sha256) from source_item where profile_id = $1";
    let (stored, bundles) = (env.count(items).await?, env.count(blobs).await?);

    let asked = env.fake().asked.len();
    let rerun = poll(&env.ctx, &env.base, id).await?;
    assert_eq!(
        env.first_start(asked, "polar"),
        Some(until - Duration::days(OVERLAP_DAYS))
    );
    assert_eq!(rerun.new, 0);
    assert!(rerun.duplicate >= 4, "{rerun:?}");
    assert_eq!((env.count(items).await?, env.count(blobs).await?), (stored, bundles));

    let late = Utc::now() - Duration::minutes(10);
    env.fake()
        .rows
        .push(sample("raw", "polar", "heart_rate", 61., "bpm", late));
    let next = poll(&env.ctx, &env.base, id).await?;
    assert_eq!(next.new, 1);
    assert_eq!(
        (env.count(items).await?, env.count(blobs).await?),
        (stored + 1, bundles + 1)
    );
    let digest: String = sqlx::query_scalar(
        "select encode(blob_sha256, 'hex') from source_item where profile_id = $1 order by seen_at desc limit 1",
    )
    .bind(env.profile)
    .fetch_one(&env.ctx.pool)
    .await?;
    let bundle: Value = serde_json::from_slice(&env.ctx.blobs.get_bytes(BlobOwner::Profile(env.profile), &digest)?)?;
    assert_eq!(bundle["raw"].as_array().map(Vec::len), Some(1));
    assert_eq!(bundle["raw"][0]["value"], json!(61.0));
    assert_eq!(bundle["daily"].as_array().map(Vec::len), Some(0));
    Ok(())
}

#[tokio::test]
async fn a_long_window_keeps_each_stored_day_when_a_later_day_fails() -> anyhow::Result<()> {
    let env = Env::new().await?;
    let id = env.seed_source().await?;
    let now = Utc::now();
    {
        let mut fake = env.fake();
        fake.connections = json!([["polar", "active"]]);
        fake.rows = [5, 3, 1]
            .into_iter()
            .map(|d| {
                sample(
                    "raw",
                    "polar",
                    "heart_rate",
                    60. + d as f64,
                    "bpm",
                    now - Duration::days(d),
                )
            })
            .collect();
        fake.scores.clear();
        fake.fail_from = Some(now - Duration::days(4));
    }
    let err = poll(&env.ctx, &env.base, id)
        .await
        .expect_err("the sidecar fails from day -4");
    assert_eq!(err.text(), "ow_http: Open Wearables HTTP 503");
    let stored = "select count(*) from wearable_sample where profile_id = $1 and metric = 'hr'";
    assert_eq!(env.count(stored).await?, 1, "the day before the failure is kept");
    let mark = env.mark(id, "polar").await?.expect("the cursor moved");
    assert!(
        mark > now - Duration::days(5) && mark <= Utc::now() - Duration::days(4),
        "{mark}"
    );

    env.fake().fail_from = None;
    let asked = env.fake().asked.len();
    poll(&env.ctx, &env.base, id).await?;
    assert_eq!(
        env.first_start(asked, "polar"),
        Some(mark - Duration::days(OVERLAP_DAYS))
    );
    assert_eq!(env.count(stored).await?, 3);

    env.fake().repeat_cursor = true;
    let err = poll(&env.ctx, &env.base, id).await.expect_err("the cursor repeats");
    assert!(
        err.text().starts_with("PermanentJobError: repeated_page: "),
        "{}",
        err.text()
    );
    Ok(())
}

#[tokio::test]
async fn an_ended_connection_keeps_its_own_cursor_while_the_others_move() -> anyhow::Result<()> {
    let env = Env::new().await?;
    let id = env.seed_source().await?;
    poll(&env.ctx, &env.base, id).await?;
    let (oura, polar, whoop) = (
        env.mark(id, "oura").await?.expect("Oura"),
        env.mark(id, "polar").await?.expect("Polar"),
        env.mark(id, "whoop").await?.expect("WHOOP"),
    );

    env.fake().connections = json!([["polar", "active"], ["oura", "revoked"], ["whoop", "expired"]]);
    let asked = env.fake().asked.len();
    let out = poll(&env.ctx, &env.base, id).await?;
    assert_eq!(
        out.problems,
        [RunProblem {
            code: CONNECTION_ENDED.into(),
            message: "oura, whoop".into()
        }]
    );
    assert_eq!(env.mark(id, "oura").await?, Some(oura));
    assert_eq!(env.mark(id, "whoop").await?, Some(whoop));
    assert!(env.mark(id, "polar").await?.expect("Polar") > polar);
    assert!(
        env.fake().asked[asked..]
            .iter()
            .all(|(_, q)| one(q, "provider") == Some("polar"))
    );
    let reconsent = "select count(*) from source s where s.profile_id = $1 and s.reconsent_needed_at is not null";
    assert_eq!(env.count(reconsent).await?, 1);

    env.fake().connections = json!([["polar", "active"], ["oura", "active"], ["whoop", "active"]]);
    let asked = env.fake().asked.len();
    let out = poll(&env.ctx, &env.base, id).await?;
    assert!(out.problems.is_empty());
    assert_eq!(
        env.first_start(asked, "oura"),
        Some(oura - Duration::days(OVERLAP_DAYS))
    );
    assert_eq!(env.count(reconsent).await?, 0);
    Ok(())
}

#[tokio::test]
async fn a_key_rotation_while_a_day_is_read_writes_nothing() -> anyhow::Result<()> {
    let env = Env::new().await?;
    let id = env.seed_source().await?;
    let mut tx = env.ctx.pool.begin().await?;
    let household: Uuid = sqlx::query_scalar("select household_id from profile where id = $1")
        .bind(env.profile)
        .fetch_one(&mut *tx)
        .await?;
    let rotated = store_secret(
        &mut tx,
        &SecretBox::from_settings(&env.ctx.settings)?,
        household,
        PURPOSE,
        None,
        &SecretString::new("rotated"),
    )
    .await?;
    tx.commit().await?;
    env.fake().connections = json!([["oura", "active"]]);
    env.fake().hooks.insert(
        "scores",
        (
            env.ctx.pool.clone(),
            format!(
                "update openwearables_user set secret_id = '{rotated}' where user_id = '{}'",
                env.user
            ),
        ),
    );
    poll(&env.ctx, &env.base, id).await?;
    assert_eq!(env.fake().hooked, [("scores", true)]);
    for table in ["wearable_sample", "vendor_score", "source_item"] {
        assert_eq!(
            env.count(&format!("select count(*) from {table} where profile_id = $1"))
                .await?,
            0,
            "{table}"
        );
    }
    assert_eq!(env.mark(id, "oura").await?, None);
    Ok(())
}

#[tokio::test]
async fn a_deleted_profiles_user_is_removed_by_the_reconcile_job_even_when_an_answer_was_lost() -> anyhow::Result<()> {
    let env = Env::new().await?;
    env.seed_source().await?;
    schedule_reconcile(&env.ctx).await?;
    let queued = "select count(*) from job where kind = 'openwearables.reconcile' and $1::uuid is not null";
    assert_eq!(env.count(queued).await?, 0);

    let mut tx = env.ctx.pool.begin().await?;
    ownhealth_auth::household::delete_profile(&mut tx, env.profile).await?;
    tx.commit().await?;
    assert_eq!(tombstones(&env.ctx.pool).await?, [env.user]);
    schedule_reconcile(&env.ctx).await?;
    assert_eq!(env.count(queued).await?, 1);
    let job = jobs::claim(&env.ctx.pool, "core", "test", Some(&[RECONCILE.to_string()][..]))
        .await?
        .expect("the queued reconcile job");

    env.fake().delete_status = Some(StatusCode::SERVICE_UNAVAILABLE);
    assert!(reconcile_job(&env.ctx, &env.base, &job).await.is_err());
    assert_eq!(tombstones(&env.ctx.pool).await?, [env.user]);
    // The sidecar deleted the user, but the answer was lost: the retry reads 404.
    env.fake().delete_status = Some(StatusCode::NOT_FOUND);
    assert_eq!(
        reconcile_job(&env.ctx, &env.base, &job).await?,
        Some(json!({"deleted": true}))
    );
    assert!(tombstones(&env.ctx.pool).await?.is_empty());
    assert_eq!(
        reconcile_job(&env.ctx, &env.base, &job).await?,
        Some(json!({"deleted": false}))
    );
    assert_eq!(env.fake().deleted, [env.user, env.user]);
    let left: i64 = sqlx::query_scalar("select count(*) from secret where purpose = 'openwearables_api_key'")
        .fetch_one(&env.ctx.pool)
        .await?;
    assert_eq!(left, 0);
    Ok(())
}
