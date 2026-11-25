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
    /// Time-series requests that start before this instant answer 503.
    fail_before: Option<DateTime<Utc>>,
    /// A route ("login" or "user") that answers 200 with an empty object.
    malformed: Option<&'static str>,
    /// The time series sends one cursor again and again.
    repeat_cursor: bool,
    /// Time-series requests for this provider answer 200 with a body that is not a page.
    broken_for: Option<String>,
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
    if s.malformed == Some("login") {
        return Json(json!({})).into_response();
    }
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

async fn user(State(s): State<Shared>, Path(id): Path<Uuid>) -> Json<Value> {
    if s.lock().expect("fake state").malformed == Some("user") {
        return Json(json!({}));
    }
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
    user(State(s), Path(id)).await.into_response()
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
            "last_synced_at": v.get(2).cloned().unwrap_or_else(|| json!(s.t)),
            "created_at": s.t, "updated_at": s.t})
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
    let start = instant(one(&q, "start_time").expect("start"));
    if s.fail_from.is_some_and(|from| start >= from) || s.fail_before.is_some_and(|before| start < before) {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    if s.broken_for.as_deref().is_some_and(|p| one(&q, "provider") == Some(p)) {
        return "not a page".into_response();
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

/// A health score as Open Wearables 0.11 sends it: its own UUID, and no data source.
fn score(mut fields: Value) -> Value {
    fields["id"] = json!(Uuid::new_v4());
    fields["data_source_id"] = Value::Null;
    fields
}

/// The health scores the fake holds: Oura's recovery, which lands, and three that are dropped.
fn fake_scores(t: DateTime<Utc>) -> Vec<Value> {
    vec![
        score(json!({"category": "recovery", "recorded_at": t, "provider": "oura",
             "value": 81, "qualifier": "Good", "components": {"rest": {"value": 20, "unit": "score"}}})),
        score(json!({"category": "body_battery", "recorded_at": t, "provider": "garmin", "value": 64})),
        // the provider Open Wearables gives its own scores
        score(json!({"category": "sleep", "recorded_at": t, "provider": "internal", "value": 72})),
        score(json!({"category": "sleep", "recorded_at": t, "provider": "oura",
             "source": {"provider": "polar", "device_name": null}, "value": 70})),
    ]
}

/// A validator for one schema of the recorded OpenAPI document.
fn schema(name: &str) -> anyhow::Result<jsonschema::Validator> {
    let mut doc: Value = serde_json::from_str(SPEC)?;
    doc["$ref"] = json!(format!("#/components/schemas/{name}"));
    Ok(jsonschema::draft202012::options()
        .should_validate_formats(true)
        .build(&doc)?)
}

/// The users whose deletion the scheduler retries.
async fn pending(pool: &PgPool) -> anyhow::Result<Vec<Uuid>> {
    Ok(tombstones(pool).await?.into_iter().map(|(user, _)| user).collect())
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
            scores: fake_scores(t),
            ignore_filter_for: None,
            fail_from: None,
            fail_before: None,
            malformed: None,
            repeat_cursor: false,
            broken_for: None,
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
async fn a_poll_reads_seven_days_again_without_rewriting_identical_rows() -> anyhow::Result<()> {
    let env = Env::new().await?;
    let id = env.seed_source().await?;
    poll(&env.ctx, &env.base, id).await?;
    let until = env.mark(id, "polar").await?.expect("Polar's cursor");
    let items = "select count(*) from source_item where profile_id = $1";
    let blobs = "select count(distinct blob_sha256) from source_item where profile_id = $1";
    let (stored, bundles) = (env.count(items).await?, env.count(blobs).await?);
    let rollup_write = "select max(xmin::text::bigint) from wearable_rollup \
        where profile_id = $1 and metric <> 'hr'";
    let rolled_up = env.count(rollup_write).await?;

    let asked = env.fake().asked.len();
    let rerun = poll(&env.ctx, &env.base, id).await?;
    assert_eq!(
        env.first_start(asked, "polar"),
        Some(until - Duration::days(OVERLAP_DAYS))
    );
    assert_eq!(rerun.new, 0);
    assert!(rerun.duplicate >= 4, "{rerun:?}");
    assert_eq!((env.count(items).await?, env.count(blobs).await?), (stored, bundles));
    assert_eq!(env.count(rollup_write).await?, rolled_up);

    let late = Utc::now() - Duration::minutes(10);
    env.fake()
        .rows
        .push(sample("raw", "polar", "heart_rate", 61., "bpm", late));
    let next = poll(&env.ctx, &env.base, id).await?;
    assert_eq!(next.new, 1);
    assert_eq!(env.count(rollup_write).await?, rolled_up);
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
async fn a_vendor_with_an_unreadable_row_or_answer_holds_no_other_vendor_back() -> anyhow::Result<()> {
    let env = Env::new().await?;
    let id = env.seed_source().await?;
    let t = env.fake().t;
    {
        let mut fake = env.fake();
        fake.connections = json!([["oura", "active"], ["polar", "active"]]);
        let mut unreadable = sample("raw", "oura", "steps", 0., "count", t + Duration::minutes(1));
        unreadable["value"] = json!("n/a");
        fake.rows = vec![
            sample("raw", "oura", "steps", 27., "count", t),
            unreadable,
            sample("raw", "polar", "heart_rate", 61., "bpm", t),
        ];
        fake.scores.clear();
    }
    // One row Oura sent that does not read is skipped and counted, not the whole page.
    let out = poll(&env.ctx, &env.base, id).await?;
    assert!(out.problems.is_empty(), "{:?}", out.problems);
    assert_eq!(out.skipped.get("unreadable_row"), Some(&1), "{:?}", out.skipped);
    let steps = "select count(*) from wearable_sample where profile_id = $1 and metric = 'steps'";
    let hr = "select count(*) from wearable_sample where profile_id = $1 and metric = 'hr'";
    assert_eq!(env.count(steps).await?, 1);
    assert_eq!(env.count(hr).await?, 1);
    let oura = env.mark(id, "oura").await?.expect("Oura's cursor moved");
    let polar = env.mark(id, "polar").await?.expect("Polar's cursor moved");

    // An Oura answer that does not read stops Oura only; Polar, read after it, still moves.
    {
        let mut fake = env.fake();
        fake.broken_for = Some("oura".into());
        fake.connections = json!([["oura", "active"], ["polar", "active"], ["whoop", "expired"]]);
        fake.rows.push(sample(
            "raw",
            "polar",
            "heart_rate",
            62.,
            "bpm",
            Utc::now() - Duration::minutes(1),
        ));
    }
    let asked = env.fake().asked.len();
    let out = poll(&env.ctx, &env.base, id).await?;
    assert_eq!(
        out.problems,
        [
            RunProblem {
                code: CONNECTION_ENDED.into(),
                message: "whoop".into()
            },
            RunProblem {
                code: VENDOR_FAILED.into(),
                message: "oura: ow_response: Invalid Open Wearables response. / Respuesta no válida de Open Wearables."
                    .into()
            }
        ]
    );
    assert_eq!(env.mark(id, "oura").await?, Some(oura));
    assert!(env.mark(id, "polar").await?.expect("Polar") > polar);
    assert!(env.first_start(asked, "polar").is_some());
    assert_eq!(env.count(hr).await?, 2);
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
    // The poll of the old key does not count as a sync: no last success, no cursor.
    let err = poll(&env.ctx, &env.base, id)
        .await
        .expect_err("the binding changed while the day was read");
    assert!(err.text().starts_with("ow_binding_changed: "), "{}", err.text());
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
    assert_eq!(pending(&env.ctx.pool).await?, [env.user]);
    schedule_reconcile(&env.ctx).await?;
    assert_eq!(env.count(queued).await?, 1);
    let job = jobs::claim(&env.ctx.pool, "core", "test", Some(&[RECONCILE.to_string()][..]))
        .await?
        .expect("the queued reconcile job");

    env.fake().delete_status = Some(StatusCode::SERVICE_UNAVAILABLE);
    assert!(reconcile_job(&env.ctx, &env.base, &job).await.is_err());
    assert_eq!(pending(&env.ctx.pool).await?, [env.user]);
    // The sidecar deleted the user, but the answer was lost: the retry reads 404.
    env.fake().delete_status = Some(StatusCode::NOT_FOUND);
    assert_eq!(
        reconcile_job(&env.ctx, &env.base, &job).await?,
        Some(json!({"deleted": true}))
    );
    assert!(pending(&env.ctx.pool).await?.is_empty());
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

#[test]
fn the_fake_health_scores_match_the_recorded_schema() -> anyhow::Result<()> {
    let validator = schema("HealthScoreResponse")?;
    for row in fake_scores(Utc::now()) {
        let errors: Vec<String> = validator.iter_errors(&row).map(|e| e.to_string()).collect();
        assert!(errors.is_empty(), "{row}: {errors:?}");
    }
    // The check can fail: an id that is not a UUID, or no data_source_id, breaks the schema.
    let mut named = fake_scores(Utc::now()).swap_remove(0);
    named["id"] = json!("recovery");
    assert!(!validator.is_valid(&named));
    let sourceless: serde_json::Map<String, Value> = fake_scores(Utc::now())
        .swap_remove(0)
        .as_object()
        .into_iter()
        .flatten()
        .filter(|(field, _)| *field != "data_source_id")
        .map(|(field, v)| (field.clone(), v.clone()))
        .collect();
    assert!(!validator.is_valid(&Value::Object(sourceless)));
    Ok(())
}

#[tokio::test]
async fn setup_names_the_step_whose_answer_does_not_read() -> anyhow::Result<()> {
    let env = Env::new().await?;
    env.fake().malformed = Some("user");
    let err = env.setup().await.expect_err("the user answer has no id");
    assert!(err.to_string().contains("User check with the API key"), "{err}");
    env.fake().malformed = Some("login");
    let err = env.setup().await.expect_err("the sign-in answer has no token");
    assert!(err.to_string().contains("Developer sign-in"), "{err}");
    assert!(err.to_string().starts_with("ow_response: "), "{err}");
    Ok(())
}

#[tokio::test]
async fn a_sample_can_return_to_a_previous_value() -> anyhow::Result<()> {
    let env = Env::new().await?;
    let id = env.seed_source().await?;
    let t = env.fake().t;
    {
        let mut fake = env.fake();
        fake.connections = json!([["polar", "active"]]);
        fake.rows = vec![sample("raw", "polar", "heart_rate", 61., "bpm", t)];
        fake.scores.clear();
    }
    let value = "select v::int8 from wearable_sample where profile_id = $1 and metric = 'hr'";
    let average = "select v_avg::int8 from wearable_rollup where profile_id = $1 and metric = 'hr' and bucket = 'day'";
    for expected in [61, 64, 61] {
        env.fake().rows[0]["value"] = json!(expected);
        poll(&env.ctx, &env.base, id).await?;
        assert_eq!(env.count(value).await?, expected);
        assert_eq!(env.count(average).await?, expected);
    }
    assert_eq!(
        env.count("select count(*) from source_item where profile_id = $1")
            .await?,
        2
    );
    Ok(())
}

#[tokio::test]
async fn a_vendor_score_can_return_to_a_previous_value() -> anyhow::Result<()> {
    let env = Env::new().await?;
    let id = env.seed_source().await?;
    let t = env.fake().t;
    {
        let mut fake = env.fake();
        fake.connections = json!([["oura", "active"]]);
        fake.rows.clear();
        fake.scores = vec![score(json!({
            "provider": "oura", "category": "recovery", "recorded_at": t, "value": 80
        }))];
    }
    let value = "select value::int8 from vendor_score where profile_id = $1";
    for expected in [80, 87, 80] {
        env.fake().scores[0]["value"] = json!(expected);
        poll(&env.ctx, &env.base, id).await?;
        assert_eq!(env.count(value).await?, expected);
    }
    Ok(())
}

#[tokio::test]
async fn a_corrected_zone_offset_replaces_the_stored_one() -> anyhow::Result<()> {
    let env = Env::new().await?;
    let id = env.seed_source().await?;
    let t = env.fake().t;
    {
        let mut fake = env.fake();
        fake.connections = json!([["polar", "active"]]);
        fake.rows = vec![sample("raw", "polar", "heart_rate", 61., "bpm", t)];
        fake.scores.clear();
    }
    poll(&env.ctx, &env.base, id).await?;
    let tz = "select tz_offset_min::int8 from wearable_sample where profile_id = $1 and metric = 'hr'";
    assert_eq!(env.count(tz).await?, -210);
    // Polar corrects the zone only: the same instant and the same value.
    env.fake().rows[0]["zone_offset"] = json!("+01:00");
    poll(&env.ctx, &env.base, id).await?;
    assert_eq!(env.count(tz).await?, 60);
    env.fake().rows[0]["zone_offset"] = json!("-03:30");
    poll(&env.ctx, &env.base, id).await?;
    assert_eq!(env.count(tz).await?, -210);
    let rows = "select count(*) from wearable_sample where profile_id = $1";
    assert_eq!(env.count(rows).await?, 1);
    Ok(())
}

#[tokio::test]
async fn an_overlap_that_keeps_failing_holds_no_newer_day_back() -> anyhow::Result<()> {
    let env = Env::new().await?;
    let id = env.seed_source().await?;
    let t = env.fake().t;
    {
        let mut fake = env.fake();
        fake.connections = json!([["polar", "active"]]);
        fake.rows = vec![sample("raw", "polar", "heart_rate", 61., "bpm", t)];
        fake.scores.clear();
    }
    poll(&env.ctx, &env.base, id).await?;
    let mark = env.mark(id, "polar").await?.expect("Polar's cursor");
    // From now on every re-read of the days before the mark fails, while a newer day reads.
    {
        let mut fake = env.fake();
        fake.fail_before = Some(mark);
        fake.rows
            .push(sample("raw", "polar", "heart_rate", 62., "bpm", Utc::now()));
    }
    let out = poll(&env.ctx, &env.base, id).await?;
    assert_eq!(
        out.problems,
        [RunProblem {
            code: VENDOR_FAILED.into(),
            message: "polar: ow_http: Open Wearables HTTP 503".into()
        }]
    );
    let hr = "select count(*) from wearable_sample where profile_id = $1 and metric = 'hr'";
    assert_eq!(env.count(hr).await?, 2, "the newer day is stored");
    let moved = env.mark(id, "polar").await?.expect("Polar's cursor");
    assert!(moved > mark, "{moved} after {mark}");
    // The next poll tries the failed days again: its window still starts seven days before the mark.
    let asked = env.fake().asked.len();
    poll(&env.ctx, &env.base, id).await?;
    assert_eq!(
        env.first_start(asked, "polar"),
        Some(moved - Duration::days(OVERLAP_DAYS))
    );
    Ok(())
}

/// Run `poll` between `begin_run` and `finish_run`, as the source.poll job does, and read the run
/// back: its status, its problems and its error.
async fn recorded(env: &Env, id: Uuid) -> anyhow::Result<(String, Value, Option<String>)> {
    let run = sources::begin_run(&mut *env.ctx.pool.acquire().await?, id, "poll", None).await?;
    let result = poll(&env.ctx, &env.base, id).await;
    let mut conn = env.ctx.pool.acquire().await?;
    match &result {
        Ok(o) => sources::finish_run(&mut conn, run, Ok(o)).await?,
        Err(e) => sources::finish_run(&mut conn, run, Err(&e.text())).await?,
    }
    Ok(
        sqlx::query_as("select status, errors, error from source_run where id = $1")
            .bind(run)
            .fetch_one(&mut *conn)
            .await?,
    )
}

#[tokio::test]
async fn an_ended_vendor_stays_on_the_run_record_when_every_active_vendor_fails() -> anyhow::Result<()> {
    let env = Env::new().await?;
    let id = env.seed_source().await?;
    {
        let mut fake = env.fake();
        fake.connections = json!([["oura", "revoked"], ["polar", "active"]]);
        fake.fail_from = Some(Utc::now() - Duration::days(BACKFILL_DAYS + 1));
    }
    let (status, errors, error) = recorded(&env, id).await?;
    assert_eq!(status, "error");
    assert_eq!(
        errors[0],
        json!({"code": CONNECTION_ENDED, "message": "oura"}),
        "{errors}"
    );
    assert_eq!(error.as_deref(), Some("ow_http: Open Wearables HTTP 503"));
    let household: Uuid = sqlx::query_scalar("select household_id from source where id = $1")
        .bind(id)
        .fetch_one(&env.ctx.pool)
        .await?;
    let health = sources::source_health(&mut *env.ctx.pool.acquire().await?, household).await?;
    let row = health.iter().find(|h| h.id == id).expect("the source's health row");
    assert_eq!(row.last_run_problem.as_deref(), Some("ow_connection_ended: oura"));
    assert_eq!(row.last_run_status.as_deref(), Some("error"));
    assert_eq!((row.last_success_at, row.consecutive_failures), (None, 1));
    Ok(())
}

#[tokio::test]
async fn a_cleanup_the_sidecar_refuses_stops_retrying_until_the_operator_ends_it() -> anyhow::Result<()> {
    let env = Env::new().await?;
    env.seed_source().await?;
    let mut tx = env.ctx.pool.begin().await?;
    ownhealth_auth::household::delete_profile(&mut tx, env.profile).await?;
    tx.commit().await?;
    schedule_reconcile(&env.ctx).await?;
    let job = jobs::claim(&env.ctx.pool, "core", "test", Some(&[RECONCILE.to_string()][..]))
        .await?
        .expect("the queued reconcile job");

    // The stored key was revoked in Open Wearables: the sidecar answers 401 to it, every time.
    let working = token_hex(20);
    env.fake().key = working.clone();
    let err = reconcile_job(&env.ctx, &env.base, &job)
        .await
        .expect_err("the sidecar refused the key");
    assert!(err.permanent, "a retry with the same key cannot help: {}", err.text());
    let text = err.text();
    assert!(
        text.contains(&env.user.to_string()) && text.contains("openwearables-delete"),
        "{text}"
    );
    // The scheduler queues no cleanup for it again, and the dead job's problem reaches the household.
    assert!(pending(&env.ctx.pool).await?.is_empty());
    schedule_reconcile(&env.ctx).await?;
    let queued =
        "select count(*) from job where kind = 'openwearables.reconcile' and state = 'queued' and $1::uuid is not null";
    assert_eq!(env.count(queued).await?, 0);
    let household: Uuid = sqlx::query_scalar("select household_id from secret where purpose = $1")
        .bind(PURPOSE)
        .fetch_one(&env.ctx.pool)
        .await?;
    assert_eq!(job.payload["household_id"], json!(household));
    // Linking the user again names the command too.
    let password = env.fake().password.clone();
    let err = env
        .setup_as(env.other_profile, &password, &working)
        .await
        .expect_err("the user waits for the operator");
    assert!(err.text().contains("openwearables-delete"), "{}", err.text());

    // The operator runs the command with a key the sidecar takes: the user is deleted, nothing is left.
    delete_with_key(&env.ctx, &env.base, env.user, &SecretString::new(&working)).await?;
    // The refused request never reached the handler; this one did.
    assert_eq!(env.fake().deleted, [env.user]);
    let left: i64 = sqlx::query_scalar(
        "select (select count(*) from openwearables_user) + (select count(*) from secret where purpose = $1)",
    )
    .bind(PURPOSE)
    .fetch_one(&env.ctx.pool)
    .await?;
    assert_eq!(left, 0);
    Ok(())
}

#[tokio::test]
async fn the_operator_retires_a_link_the_new_sidecar_does_not_know() -> anyhow::Result<()> {
    let env = Env::new().await?;
    let id = env.seed_source().await?;
    poll(&env.ctx, &env.base, id).await?;
    let stored = "select count(*) from wearable_sample where profile_id = $1";
    let samples = env.count(stored).await?;
    // Open Wearables was set up again (a restore onto new volumes): a new key, and no old user.
    let working = token_hex(20);
    {
        let mut fake = env.fake();
        fake.key = working.clone();
        fake.delete_status = Some(StatusCode::NOT_FOUND);
    }
    delete_with_key(&env.ctx, &env.base, env.user, &SecretString::new(&working)).await?;
    let sources = "select count(*) from source where profile_id = $1 and kind = 'openwearables'";
    assert_eq!(env.count(sources).await?, 0);
    assert!(pending(&env.ctx.pool).await?.is_empty());
    let links: i64 = sqlx::query_scalar("select count(*) from openwearables_user")
        .fetch_one(&env.ctx.pool)
        .await?;
    assert_eq!(links, 0);
    // The readings it brought stay with the profile.
    assert_eq!(env.count(stored).await?, samples);
    let err = delete_with_key(&env.ctx, &env.base, env.user, &SecretString::new(&working))
        .await
        .expect_err("nothing left to delete");
    assert!(err.text().contains("knows no Open Wearables user"), "{}", err.text());
    Ok(())
}

#[test]
fn a_base_address_that_is_not_an_http_origin_is_an_error() {
    let base = Url::parse("mailto:operator@example.invalid").expect("a URL");
    let err = endpoint(&base, &["users"], &[]).expect_err("no path to extend");
    assert!(err.text().contains("not an HTTP origin"), "{}", err.text());
    assert_eq!(
        endpoint(&Url::parse("http://ow-app:8000").expect("a URL"), &["users", "u"], &[])
            .expect("an endpoint")
            .as_str(),
        "http://ow-app:8000/api/v1/users/u"
    );
}

/// Give scripts the same health/run updates as a poll completing on the injected clock.
async fn record_gap_test_run(env: &Env, source: Uuid, now: DateTime<Utc>, failed: bool) -> anyhow::Result<()> {
    let mut tx = env.ctx.pool.begin().await?;
    let run = sources::begin_run(&mut tx, source, "poll", None).await?;
    let ok = RunOutcome::default();
    sources::finish_run(&mut tx, run, if failed { Err("vendor unavailable") } else { Ok(&ok) }).await?;
    sqlx::query("update source_run set started_at = $2, finished_at = $2 where id = $1")
        .bind(run)
        .bind(now)
        .execute(&mut *tx)
        .await?;
    sqlx::query(
        "update source set last_run_at = $2,
         last_error_at = case when $3 then $2 else last_error_at end,
         last_success_at = case when $3 then last_success_at else $2 end where id = $1",
    )
    .bind(source)
    .bind(now)
    .bind(failed)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

#[tokio::test]
async fn thirty_days_of_sync_gaps_open_once_and_close_on_recovery() -> anyhow::Result<()> {
    let env = Env::new().await?;
    let ow = env.seed_source().await?;
    gaps::register();
    let registered = &env.ctx;
    let start = instant("2026-01-01T00:00:00Z");
    let household: Uuid = sqlx::query_scalar("select household_id from source where id = $1")
        .bind(ow)
        .fetch_one(&registered.pool)
        .await?;
    let mut conn = registered.pool.acquire().await?;
    let dexcom = sources::create_source(
        &mut conn,
        &NewSource {
            profile_id: Some(env.profile),
            poll_seconds: Some(900),
            ..NewSource::new(household, "dexcom", "Dexcom")
        },
    )
    .await?;
    drop(conn);

    let mut observed = BTreeMap::new();
    let mut missed = Vec::new();
    let mut false_alerts = Vec::new();
    for tick in 0..30 * 24 * 4 {
        let now = start + Duration::minutes(tick * 15);
        let hour = tick / 4;
        let failed = (250 * 4..251 * 4).contains(&tick) || tick == 300 * 4;
        let dexcom_failed = (360 * 4..361 * 4).contains(&tick);
        if tick == 300 * 4 + 1 {
            // One scheduler pass queues the retry of the poll that failed and then checks for gaps,
            // so the check runs before the retry can end. One failed poll is not a gap.
            sources::scheduler_tick_at(registered, now + Duration::seconds(10)).await?;
            let open: i64 =
                sqlx::query_scalar("select count(*) from review_item where kind = 'source_gap' and status = 'open'")
                    .fetch_one(&registered.pool)
                    .await?;
            assert_eq!(open, 0, "one failed poll opened an alert before its retry ran");
        }
        // The sidecar stays up when a vendor stops sending data or needs a new sign-in.
        if !failed && (tick % 48 == 0 || [126, 127, 198, 251].contains(&hour) && tick % 4 == 0 || tick == 300 * 4 + 1) {
            let (polar, synced) = match hour {
                48..72 => ("active", start + Duration::hours(48)),
                120..126 => ("expired", start + Duration::hours(119)),
                126 => ("active", start + Duration::hours(105)),
                _ => ("active", now),
            };
            let whoop = if (192..198).contains(&hour) {
                "revoked"
            } else {
                "active"
            };
            {
                let mut fake = env.fake();
                fake.connections = json!([["polar", polar, synced], ["whoop", whoop, now]]);
                fake.rows.clear();
                fake.scores.clear();
            }
            poll(registered, &env.base, ow).await?;
        }
        if failed || [0, 251 * 4, 300 * 4 + 1].contains(&tick) {
            record_gap_test_run(&env, ow, now, failed).await?;
        }
        if tick % 4 == 0 || (360 * 4..361 * 4).contains(&tick) {
            // Only failed Dexcom pulls are checked until its connector reports vendor freshness (slice 38).
            record_gap_test_run(&env, dexcom, now, dexcom_failed).await?;
        }

        let expected = if (68 * 4 + 1..72 * 4).contains(&tick)
            || (120..127).contains(&hour)
            || (192..198).contains(&hour)
            || (250 * 4 + 2..251 * 4).contains(&tick)
        {
            Some(ow)
        } else if (360 * 4 + 2..361 * 4).contains(&tick) {
            Some(dexcom)
        } else {
            None
        };
        if tick == 68 * 4 + 1 {
            tokio::try_join!(
                sources::scheduler_tick_at(registered, now),
                sources::scheduler_tick_at(registered, now),
            )?;
            // A concurrent scheduler may have captured its clock before the alert was due.
            sources::scheduler_tick_at(registered, now - Duration::minutes(15)).await?;
        } else {
            sources::scheduler_tick_at(registered, now).await?;
        }
        let rows: Vec<(Uuid, Uuid, Value)> =
            sqlx::query_as("select ref_id, id, payload from review_item where kind = 'source_gap' and status = 'open'")
                .fetch_all(&registered.pool)
                .await?;
        if let Some(source) = expected
            && !rows.iter().any(|(id, _, _)| *id == source)
        {
            missed.push((hour, tick % 4 * 15));
        }
        if rows.len() > usize::from(expected.is_some()) || rows.iter().any(|(id, _, _)| Some(*id) != expected) {
            false_alerts.push((hour, tick % 4 * 15));
        }
        for (source, id, payload) in rows {
            let expected_name = match hour {
                192..198 => "WHOOP",
                250 => "Open Wearables",
                360 => "Dexcom",
                _ => "Polar",
            };
            // A sidecar that does not answer needs the service checked, not the accounts reconnected.
            let advice = if expected_name == "Open Wearables" {
                [("en", "is running"), ("es", "está en marcha")]
            } else {
                [("en", "reconnect"), ("es", "volver a conectar")]
            };
            for (lang, words) in advice {
                let text = payload["message"][lang].as_str().unwrap_or_default();
                assert!(text.contains(expected_name), "{source} at {now}: {text}");
                assert!(text.contains(words), "{text}");
            }
            observed.entry(id).or_insert(tick);
        }
    }
    assert!(
        missed.is_empty(),
        "{} missed ticks, first {:?}",
        missed.len(),
        missed.first()
    );
    assert!(
        false_alerts.is_empty(),
        "{} false ticks, first {:?}",
        false_alerts.len(),
        false_alerts.first()
    );
    let mut opened: Vec<_> = observed.into_values().collect();
    opened.sort();
    assert_eq!(opened, [68 * 4 + 1, 120 * 4, 192 * 4, 250 * 4 + 2, 360 * 4 + 2]);
    let decided: Vec<DateTime<Utc>> = sqlx::query_scalar(
        "select decided_at from review_item where kind = 'source_gap' and status = 'dismissed' order by decided_at",
    )
    .fetch_all(&registered.pool)
    .await?;
    assert_eq!(
        decided,
        [72, 127, 198, 251, 361].map(|hour| start + Duration::hours(hour))
    );
    Ok(())
}

#[tokio::test]
async fn a_source_gap_ends_when_a_device_is_disabled_or_deleted() -> anyhow::Result<()> {
    let env = Env::new().await?;
    gaps::register();
    let id = env.seed_source().await?;
    env.fake().connections = json!([["polar", "expired"]]);
    poll(&env.ctx, &env.base, id).await?;

    let open = || async {
        sqlx::query_scalar::<_, Uuid>(
            "select id from review_item where kind = 'source_gap' and ref_id = $1 and status = 'open'",
        )
        .bind(id)
        .fetch_one(&env.ctx.pool)
        .await
    };
    sources::scheduler_tick(&env.ctx).await?;
    let first = open().await?;
    let mut conn = env.ctx.pool.acquire().await?;
    let user: Uuid = sqlx::query_scalar(
        "select u.id from app_user u join source s on s.household_id = u.household_id where s.id = $1 limit 1",
    )
    .bind(id)
    .fetch_one(&mut *conn)
    .await?;
    ownhealth_core::review::dismiss(&mut conn, first, user, Some("acknowledged")).await?;
    sources::scheduler_tick(&env.ctx).await?;
    assert!(
        matches!(open().await, Err(sqlx::Error::RowNotFound)),
        "the gap is still in progress"
    );
    for enabled in [false, true] {
        sources::update_source(
            &mut conn,
            id,
            &sources::SourcePatch {
                enabled: Some(enabled),
                ..sources::SourcePatch::default()
            },
        )
        .await?;
        sources::scheduler_tick(&env.ctx).await?;
        if !enabled {
            assert!(matches!(open().await, Err(sqlx::Error::RowNotFound)));
        }
    }
    assert_ne!(first, open().await?);
    assert!(sources::delete_source(&mut conn, id).await?);
    assert!(matches!(open().await, Err(sqlx::Error::RowNotFound)));
    let unavailable: i64 = sqlx::query_scalar(
        "select count(*) from review_item where ref_id = $1 and status = 'dismissed'
         and (decision_reason = 'source_unavailable' or (decision_reason = 'acknowledged' and payload ? 'ended_at'))",
    )
    .bind(id)
    .fetch_one(&mut *conn)
    .await?;
    assert_eq!(unavailable, 2);
    Ok(())
}

async fn open_gaps(env: &Env, source: Uuid) -> anyhow::Result<Vec<Uuid>> {
    Ok(sqlx::query_scalar(
        "select id from review_item where kind = 'source_gap' and ref_id = $1 and status = 'open' order by created_at",
    )
    .bind(source)
    .fetch_all(&env.ctx.pool)
    .await?)
}

/// The scheduler queues the retry of a failed poll and checks for gaps in the same pass, so the check
/// always runs before that retry ends. A single failed poll must not open an alert.
#[tokio::test]
async fn one_failed_poll_opens_no_alert_before_its_retry() -> anyhow::Result<()> {
    let env = Env::new().await?;
    let ow = env.seed_source().await?;
    gaps::register();
    let t0 = instant("2026-01-01T00:00:00Z");
    record_gap_test_run(&env, ow, t0, false).await?;
    sources::scheduler_tick_at(&env.ctx, t0).await?;
    // the next poll starts 15 min later and fails after 2 s
    let start = t0 + Duration::minutes(15);
    record_gap_test_run(&env, ow, start, true).await?;
    sqlx::query("update source_run set finished_at = $2 where source_id = $1 and started_at = $3")
        .bind(ow)
        .bind(start + Duration::seconds(2))
        .bind(start)
        .execute(&env.ctx.pool)
        .await?;
    // the first 30 s tick after last_run_at + 900 s: it queues the retry, then checks for gaps
    let check = start + Duration::minutes(15) + Duration::seconds(10);
    sources::scheduler_tick_at(&env.ctx, check).await?;
    assert!(open_gaps(&env, ow).await?.is_empty(), "one failed poll is not a gap");
    // the retry fails too: now the pull has failed for longer than the interval
    record_gap_test_run(&env, ow, check, true).await?;
    sources::scheduler_tick_at(&env.ctx, check + Duration::seconds(30)).await?;
    assert_eq!(open_gaps(&env, ow).await?.len(), 1, "a failed retry confirms the gap");
    Ok(())
}

/// A source delete that waits on a gap check holding the source row: the alert that check filed must
/// not stay open for a source that no longer exists.
#[tokio::test]
async fn a_delete_that_waits_on_a_gap_check_closes_the_alert_it_filed() -> anyhow::Result<()> {
    let env = Env::new().await?;
    let id = env.seed_source().await?;
    let household: Uuid = sqlx::query_scalar("select household_id from source where id = $1")
        .bind(id)
        .fetch_one(&env.ctx.pool)
        .await?;
    // what the gap check does: lock the source row, then file the alert
    let mut check = env.ctx.pool.begin().await?;
    sqlx::query("select 1 from source where id = $1 for no key update")
        .bind(id)
        .execute(&mut *check)
        .await?;
    let pool = env.ctx.pool.clone();
    let delete = tokio::spawn(async move {
        let mut c = pool.acquire().await?;
        anyhow::Ok(sources::delete_source(&mut c, id).await?)
    });
    tokio::time::sleep(std::time::Duration::from_millis(800)).await;
    assert!(!delete.is_finished(), "the delete waits on the check's row lock");
    sqlx::query(
        "insert into review_item (household_id, profile_id, kind, ref_table, ref_id, payload) \
         values ($1, $2, 'source_gap', 'source', $3, '{}')",
    )
    .bind(household)
    .bind(env.profile)
    .bind(id)
    .execute(&mut *check)
    .await?;
    check.commit().await?;
    assert!(delete.await??);
    assert!(
        open_gaps(&env, id).await?.is_empty(),
        "an open alert outlived its deleted source"
    );
    Ok(())
}

/// Open Wearables may report an active connection without a sync time. The last known one is kept,
/// so the staleness of a vendor keeps counting from it.
#[tokio::test]
async fn an_active_connection_without_a_sync_time_keeps_the_last_known_one() -> anyhow::Result<()> {
    let env = Env::new().await?;
    let id = env.seed_source().await?;
    let synced = instant("2026-01-01T00:00:00Z");
    for reported in [json!(synced), Value::Null] {
        env.fake().connections = json!([["polar", "active", reported]]);
        poll(&env.ctx, &env.base, id).await?;
    }
    let kept: Value =
        sqlx::query_scalar("select cursor #> '{connections,polar,last_synced_at}' from source where id = $1")
            .bind(id)
            .fetch_one(&env.ctx.pool)
            .await?;
    assert_eq!(serde_json::from_value::<Option<DateTime<Utc>>>(kept)?, Some(synced));
    Ok(())
}

/// A gap that ends because the vendor syncs again closes as recovered; one that ends because the
/// person switched the device off closes as unavailable.
#[tokio::test]
async fn an_alert_closes_with_the_reason_its_gap_ended() -> anyhow::Result<()> {
    let env = Env::new().await?;
    gaps::register();
    let id = env.seed_source().await?;
    let reason = |item: Uuid| {
        let pool = env.ctx.pool.clone();
        async move {
            sqlx::query_as::<_, (String, Option<String>)>(
                "select status, decision_reason from review_item where id = $1",
            )
            .bind(item)
            .fetch_one(&pool)
            .await
        }
    };
    env.fake().connections = json!([["polar", "expired"]]);
    poll(&env.ctx, &env.base, id).await?;
    sources::scheduler_tick(&env.ctx).await?;
    let open = open_gaps(&env, id).await?;
    let [first] = open[..] else {
        anyhow::bail!("one alert expected");
    };
    env.fake().connections = json!([["polar", "active"]]);
    poll(&env.ctx, &env.base, id).await?;
    sources::scheduler_tick(&env.ctx).await?;
    assert_eq!(
        reason(first).await?,
        ("dismissed".into(), Some("sync_recovered".into()))
    );

    env.fake().connections = json!([["polar", "expired"]]);
    poll(&env.ctx, &env.base, id).await?;
    sources::scheduler_tick(&env.ctx).await?;
    let open = open_gaps(&env, id).await?;
    let [second] = open[..] else {
        anyhow::bail!("one alert expected");
    };
    let mut conn = env.ctx.pool.acquire().await?;
    let off = sources::SourcePatch {
        enabled: Some(false),
        ..sources::SourcePatch::default()
    };
    sources::update_source(&mut conn, id, &off).await?;
    sources::scheduler_tick(&env.ctx).await?;
    assert_eq!(
        reason(second).await?,
        ("dismissed".into(), Some("source_unavailable".into()))
    );
    Ok(())
}

/// OwnHealth opens and closes a sync alert itself, so a rejected one cannot be reopened by hand.
#[tokio::test]
async fn a_rejected_sync_alert_cannot_be_reopened() -> anyhow::Result<()> {
    let env = Env::new().await?;
    gaps::register();
    let id = env.seed_source().await?;
    env.fake().connections = json!([["polar", "expired"]]);
    poll(&env.ctx, &env.base, id).await?;
    sources::scheduler_tick(&env.ctx).await?;
    let open = open_gaps(&env, id).await?;
    let [item] = open[..] else {
        anyhow::bail!("one alert expected");
    };
    let mut conn = env.ctx.pool.acquire().await?;
    let user: Uuid = sqlx::query_scalar(
        "select u.id from app_user u join source s on s.household_id = u.household_id where s.id = $1 limit 1",
    )
    .bind(id)
    .fetch_one(&mut *conn)
    .await?;
    ownhealth_core::review::reject(&mut conn, item, user, "not mine").await?;
    let refused = ownhealth_core::review::reopen(&mut conn, item, user).await;
    assert!(refused.is_err(), "a sync alert was reopened by hand");
    let status: String = sqlx::query_scalar("select status from review_item where id = $1")
        .bind(item)
        .fetch_one(&mut *conn)
        .await?;
    assert_eq!(status, "rejected");
    Ok(())
}

/// Open Wearables can drop a vendor from its connection list, as when the connection is deleted in the
/// sidecar. No data flows for that vendor any more, so it has not recovered: its alert stays open.
#[tokio::test]
async fn a_vendor_that_drops_out_of_the_report_keeps_its_alert() -> anyhow::Result<()> {
    let env = Env::new().await?;
    gaps::register();
    let id = env.seed_source().await?;
    let now = Utc::now();
    env.fake().connections = json!([["polar", "active", now - Duration::hours(22)], ["whoop", "active", now]]);
    poll(&env.ctx, &env.base, id).await?;
    sources::scheduler_tick(&env.ctx).await?;
    let open = open_gaps(&env, id).await?;
    let [item] = open[..] else {
        anyhow::bail!("one alert expected");
    };
    env.fake().connections = json!([["whoop", "active", now]]);
    poll(&env.ctx, &env.base, id).await?;
    sources::scheduler_tick(&env.ctx).await?;
    assert_eq!(
        open_gaps(&env, id).await?,
        [item],
        "a vendor missing from the report has not recovered"
    );
    Ok(())
}

/// A connection Open Wearables reports active but never synced counts its staleness from the first poll
/// that saw it, so a vendor that never sends data still raises an alert once its window passes.
#[tokio::test]
async fn a_connection_that_never_synced_alerts_once_its_window_passes() -> anyhow::Result<()> {
    let env = Env::new().await?;
    gaps::register();
    let id = env.seed_source().await?;
    let seen = Utc::now();
    env.fake().connections = json!([["polar", "active", null]]);
    poll(&env.ctx, &env.base, id).await?;
    sources::scheduler_tick_at(&env.ctx, seen + Duration::hours(19)).await?;
    assert!(open_gaps(&env, id).await?.is_empty(), "inside the 20 h Polar window");
    sources::scheduler_tick_at(&env.ctx, seen + Duration::hours(21)).await?;
    assert_eq!(
        open_gaps(&env, id).await?.len(),
        1,
        "a vendor that never synced is a gap after its window"
    );
    Ok(())
}
