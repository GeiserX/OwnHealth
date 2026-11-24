use axum::Router;
use axum::body::{Body, to_bytes};
use chrono::{DateTime, TimeDelta, Utc};
use http::{Method, Request, StatusCode, header};
use ownhealth_auth::sessions::{COOKIE, create_session};
use ownhealth_core::Runtime;
use ownhealth_core::audit::{AuditEvent, write_audit_at};
use ownhealth_core::demo::{Family, create_rivera_family};
use ownhealth_core::testing::TestDb;
use serde_json::{Value, json};
use tower::ServiceExt;
use uuid::Uuid;

const REPORT: &str = "/mcp/session-report";
const CONSENT: &str = "/mcp/session-report/consent";

struct World {
    db: TestDb,
    _dir: tempfile::TempDir,
    app: Router,
    family: Family,
    alex: String,
    sam: String,
}

impl World {
    async fn new() -> anyhow::Result<Self> {
        let db = TestDb::new().await?;
        let dir = tempfile::tempdir()?;
        let rt = Runtime::with_pool(db.settings(dir.path())?, db.pool.clone())?;
        let mut c = db.pool.acquire().await?;
        let family = create_rivera_family(&mut c, "Rivera report test").await?;
        let alex = create_session(&mut c, family.user("alex"), None, None, "passkey").await?;
        let sam = create_session(&mut c, family.user("sam"), None, None, "passkey").await?;
        Ok(Self {
            db,
            _dir: dir,
            app: ownhealth_api::router().with_state(rt),
            family,
            alex,
            sam,
        })
    }

    async fn req(&self, method: Method, path: &str, session: &str, body: Value) -> anyhow::Result<(StatusCode, Value)> {
        let req = Request::builder()
            .method(method)
            .uri(path)
            .header(header::COOKIE, format!("{COOKIE}={session}"))
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::HOST, "127.0.0.1:8080")
            .header(header::ORIGIN, "http://127.0.0.1:8080")
            .body(Body::from(body.to_string()))?;
        let res = self.app.clone().oneshot(req).await?;
        let status = res.status();
        let raw = to_bytes(res.into_body(), 1 << 20).await?;
        Ok((status, serde_json::from_slice(&raw).unwrap_or(Value::Null)))
    }

    async fn report(&self, session: &str) -> anyhow::Result<(StatusCode, Value)> {
        self.req(Method::GET, REPORT, session, Value::Null).await
    }

    async fn consent(&self, session: &str, enabled: bool) -> anyhow::Result<(StatusCode, Value)> {
        self.req(Method::PUT, CONSENT, session, json!({ "enabled": enabled }))
            .await
    }
}

#[tokio::test]
async fn only_the_household_admin_can_read_or_change_consent_and_each_change_is_audited() -> anyhow::Result<()> {
    let w = World::new().await?;
    assert_eq!(
        w.report(&w.alex).await?,
        (StatusCode::OK, json!({"enabled": false, "sessions": []}))
    );
    assert_eq!(w.consent(&w.sam, true).await?.0, StatusCode::FORBIDDEN);
    assert_eq!(
        w.consent(&w.alex, true).await?,
        (StatusCode::OK, json!({"enabled": true}))
    );
    let (status, forbidden) = w.report(&w.sam).await?;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(forbidden["detail"]["code"], "forbidden");
    assert!(forbidden.get("sessions").is_none());
    assert!(forbidden.get("enabled").is_none());
    w.consent(&w.alex, true).await?;
    let changed: Vec<Option<String>> = sqlx::query_scalar(
        "select args_summary from audit_event where actor_id = $1 and action = 'consent_change' \
         and tool = 'PUT /api/mcp/session-report/consent' order by id",
    )
    .bind(w.family.user("alex"))
    .fetch_all(&w.db.pool)
    .await?;
    assert_eq!(changed, [Some("mcp_session_report=true".to_string())]);
    let reads: i64 = sqlx::query_scalar(
        "select count(*) from audit_event where actor_id = $1 and action = 'read' \
         and tool = 'GET /api/mcp/session-report'",
    )
    .bind(w.family.user("alex"))
    .fetch_one(&w.db.pool)
    .await?;
    assert_eq!(reads, 1, "one audit row for the one report the owner read");
    Ok(())
}

fn call(actor: Uuid, token: Uuid, tool: &str, result: &str, measures: Option<(i32, i32)>) -> AuditEvent {
    AuditEvent {
        actor_id: Some(actor),
        token_id: Some(token),
        tool: Some(tool.into()),
        client: Some("SyntheticTokenLabel".into()),
        args_summary: Some("SyntheticQuestion".into()),
        result_kind: Some(result.into()),
        not_assessed_n: measures.map(|(n, _)| n),
        arg_bytes: measures.map(|(_, bytes)| bytes),
        ..AuditEvent::new("token", "mcp", &format!("tools/call:{tool}"))
    }
}

fn on(profile: Uuid, mut event: AuditEvent) -> AuditEvent {
    event.profile_id = Some(profile);
    event
}

#[tokio::test]
async fn sessions_split_at_thirty_minutes_and_summarise_only_eligible_calls() -> anyhow::Result<()> {
    let w = World::new().await?;
    let alex = w.family.user("alex");
    let sam = w.family.user("sam");
    let a = Uuid::new_v4();
    let b = Uuid::new_v4();
    let foreign = Uuid::new_v4();
    let mut c = w.db.pool.acquire().await?;
    let other = create_rivera_family(&mut c, "Another Rivera household").await?;
    let start = DateTime::parse_from_rfc3339("2026-11-23T12:00:00Z")?.with_timezone(&Utc);
    for (minutes, event) in [
        (0, call(alex, a, "get_genome_findings", "ok", Some((2, 14)))),
        (0, call(alex, a, "get_genome_findings", "budget_cut", Some((1, 18)))),
        (
            2,
            on(
                w.family.profile("alex"),
                call(sam, b, "get_health_status", "ok", Some((0, 14))),
            ),
        ),
        (
            3,
            call(other.user("alex"), foreign, "get_health_status", "ok", Some((99, 99))),
        ),
        (29, call(alex, a, "search_observations", "denied", Some((0, 7)))),
        (59, call(alex, a, "get_genome_findings", "ok", Some((0, 21)))),
        (60, call(alex, a, "get_genome_findings", "ok", None)),
        (61, call(alex, a, "get_genome_findings", "error", Some((0, 45)))),
    ] {
        write_audit_at(&mut c, &event, Some(start + TimeDelta::minutes(minutes))).await?;
    }
    drop(c);
    assert_eq!(w.consent(&w.alex, true).await?.0, StatusCode::OK);
    let (status, report) = w.report(&w.alex).await?;
    assert_eq!(status, StatusCode::OK, "{report}");
    assert_eq!(
        report,
        json!({
            "enabled": true,
            "sessions": [
                {"started_at": "2026-11-23T13:01:00Z", "ended_at": "2026-11-23T13:01:00Z", "tools": [
                    {"tool": "get_genome_findings", "calls": 1, "not_assessed_calls": 0, "not_assessed_n": 0, "budget_cuts": 0, "denied": 0, "errors": 1, "arg_bytes": 45}
                ]},
                {"started_at": "2026-11-23T12:59:00Z", "ended_at": "2026-11-23T12:59:00Z", "tools": [
                    {"tool": "get_genome_findings", "calls": 1, "not_assessed_calls": 0, "not_assessed_n": 0, "budget_cuts": 0, "denied": 0, "errors": 0, "arg_bytes": 21}
                ]},
                {"started_at": "2026-11-23T12:00:00Z", "ended_at": "2026-11-23T12:29:00Z", "tools": [
                    {"tool": "get_genome_findings", "calls": 2, "not_assessed_calls": 2, "not_assessed_n": 3, "budget_cuts": 1, "denied": 0, "errors": 0, "arg_bytes": 32},
                    {"tool": "search_observations", "calls": 1, "not_assessed_calls": 0, "not_assessed_n": 0, "budget_cuts": 0, "denied": 1, "errors": 0, "arg_bytes": 7}
                ]},
                {"started_at": "2026-11-23T12:02:00Z", "ended_at": "2026-11-23T12:02:00Z", "tools": [
                    {"tool": "get_health_status", "calls": 1, "not_assessed_calls": 0, "not_assessed_n": 0, "budget_cuts": 0, "denied": 0, "errors": 0, "arg_bytes": 14}
                ]}
            ]
        })
    );
    assert!(!report.to_string().contains("Synthetic"));
    assert_eq!(w.report(&w.sam).await?.0, StatusCode::FORBIDDEN);
    assert_eq!(w.consent(&w.alex, false).await?.0, StatusCode::OK);
    assert_eq!(
        w.report(&w.alex).await?,
        (StatusCode::OK, json!({"enabled": false, "sessions": []}))
    );
    Ok(())
}

#[tokio::test]
async fn the_report_shows_only_calls_the_owner_sees_in_the_audit_log() -> anyhow::Result<()> {
    let w = World::new().await?;
    let alex = w.family.user("alex");
    let sam = w.family.user("sam");
    let token = Uuid::new_v4();
    let mut c = w.db.pool.acquire().await?;
    let start = DateTime::parse_from_rfc3339("2026-11-23T12:00:00Z")?.with_timezone(&Utc);
    for (minutes, event) in [
        // sam's own profile, which alex does not manage: hidden from alex's audit log
        (
            0,
            on(
                w.family.profile("sam"),
                call(sam, token, "get_genome_findings", "ok", Some((7, 30))),
            ),
        ),
        // no profile at all, made by sam's token: hidden too
        (1, call(sam, token, "list_profiles", "ok", Some((0, 2)))),
        // alex's profile: alex manages it, so alex sees it
        (
            2,
            on(
                w.family.profile("alex"),
                call(sam, token, "get_health_status", "ok", Some((1, 14))),
            ),
        ),
        // alex's own token: alex sees every call it made
        (3, call(alex, Uuid::new_v4(), "list_profiles", "ok", Some((0, 2)))),
    ] {
        write_audit_at(&mut c, &event, Some(start + TimeDelta::minutes(minutes))).await?;
    }
    drop(c);
    assert_eq!(w.consent(&w.alex, true).await?.0, StatusCode::OK);
    let (status, audit) = w.req(Method::GET, "/audit?via=mcp", &w.alex, Value::Null).await?;
    assert_eq!(status, StatusCode::OK, "{audit}");
    let mut seen: Vec<&str> = audit["items"]
        .as_array()
        .map(|a| a.iter().filter_map(|i| i["tool"].as_str()).collect())
        .unwrap_or_default();
    seen.sort_unstable();
    assert_eq!(seen, ["get_health_status", "list_profiles"], "{audit}");
    let (status, report) = w.report(&w.alex).await?;
    assert_eq!(status, StatusCode::OK, "{report}");
    assert_eq!(
        report["sessions"],
        json!([
            {"started_at": "2026-11-23T12:03:00Z", "ended_at": "2026-11-23T12:03:00Z", "tools": [
                {"tool": "list_profiles", "calls": 1, "not_assessed_calls": 0, "not_assessed_n": 0, "budget_cuts": 0, "denied": 0, "errors": 0, "arg_bytes": 2}
            ]},
            {"started_at": "2026-11-23T12:02:00Z", "ended_at": "2026-11-23T12:02:00Z", "tools": [
                {"tool": "get_health_status", "calls": 1, "not_assessed_calls": 1, "not_assessed_n": 1, "budget_cuts": 0, "denied": 0, "errors": 0, "arg_bytes": 14}
            ]}
        ])
    );
    Ok(())
}

#[tokio::test]
async fn the_report_keeps_the_newest_fifty_sessions() -> anyhow::Result<()> {
    let w = World::new().await?;
    let alex = w.family.user("alex");
    let token = Uuid::new_v4();
    let mut c = w.db.pool.acquire().await?;
    let start = DateTime::parse_from_rfc3339("2026-11-01T00:00:00Z")?.with_timezone(&Utc);
    for i in 0..51 {
        let event = call(alex, token, "list_profiles", "ok", Some((0, i + 1)));
        write_audit_at(&mut c, &event, Some(start + TimeDelta::minutes(31 * i64::from(i)))).await?;
    }
    drop(c);
    assert_eq!(w.consent(&w.alex, true).await?.0, StatusCode::OK);
    let (status, report) = w.report(&w.alex).await?;
    assert_eq!(status, StatusCode::OK, "{report}");
    let bytes: Vec<i64> = report["sessions"]
        .as_array()
        .map(|a| a.iter().filter_map(|s| s["tools"][0]["arg_bytes"].as_i64()).collect())
        .unwrap_or_default();
    assert_eq!(bytes, (2..=51).rev().collect::<Vec<i64>>());
    Ok(())
}
