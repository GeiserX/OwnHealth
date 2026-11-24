use axum::extract::State;
use axum::http::{StatusCode, header};
use axum::response::IntoResponse;
use axum::routing::{get, put};
use axum::{Json, Router};
use chrono::{DateTime, Utc};
use ownhealth_auth::household::is_household_owner;
use ownhealth_core::Runtime;
use ownhealth_core::access::Principal;
use ownhealth_core::audit::{AuditEvent, write_audit};
use ownhealth_core::langs;
use ownhealth_mcp::tools::{SESSION_REPORT_SETTING, session_report_enabled};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::PgConnection;
use uuid::Uuid;

use super::auth::Body;
use crate::extract::SessionCaller;
use crate::{ApiError, ApiResult};

const ADMIN_ONLY: &[(&str, &str)] = &[
    (
        "en",
        "Only the household owner can view the session report or change consent.",
    ),
    (
        "es",
        "Solo la persona titular del hogar puede ver el informe de sesiones o cambiar el consentimiento.",
    ),
];

/// Messages for the wording lint.
pub fn phrases() -> Value {
    langs::per_lang(|lang| json!(langs::pick(ADMIN_ONLY, lang)))
}

#[derive(Serialize, Deserialize)]
struct Consent {
    enabled: bool,
}

#[derive(Serialize, Deserialize)]
struct ToolStats {
    tool: String,
    calls: i64,
    not_assessed_calls: i64,
    not_assessed_n: i64,
    budget_cuts: i64,
    denied: i64,
    errors: i64,
    arg_bytes: i64,
}

#[derive(Serialize, sqlx::FromRow)]
struct Session {
    started_at: DateTime<Utc>,
    ended_at: DateTime<Utc>,
    tools: sqlx::types::Json<Vec<ToolStats>>,
}

#[derive(Serialize)]
struct Report {
    enabled: bool,
    sessions: Vec<Session>,
}

async fn owner(conn: &mut PgConnection, p: &Principal) -> ApiResult<(Uuid, Uuid)> {
    let (Some(household_id), Some(uid)) = (p.household_id, p.user_id) else {
        return Err(ApiError::unauthenticated());
    };
    if !is_household_owner(conn, household_id, uid).await? {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "forbidden",
            langs::pick(ADMIN_ONLY, &p.lang),
        ));
    }
    Ok((household_id, uid))
}

async fn get_report(State(rt): State<Runtime>, SessionCaller(p): SessionCaller) -> ApiResult<impl IntoResponse> {
    let mut tx = rt.pool.begin().await?;
    let (household_id, uid) = owner(&mut tx, &p).await?;
    let enabled = session_report_enabled(&mut tx, Some(household_id)).await?;
    let sessions = if enabled {
        // An unrecorded tool call breaks the run without being part of either session. The audit
        // id gives calls with the same timestamp a stable order. Group by token internally only.
        sqlx::query_as(
            r#"with ordered as (
                select e.id, e.ts, e.token_id, e.tool, e.result_kind, e.not_assessed_n, e.arg_bytes,
                       lag(e.ts) over by_token as prev_at,
                       lag(e.arg_bytes is not null and e.not_assessed_n is not null, 1, false)
                           over by_token as prev_recorded
                from audit_event e join app_user u on u.id = e.actor_id
                where u.household_id = $1 and e.via = 'mcp' and e.actor_kind = 'token'
                      and e.token_id is not null and e.tool is not null and e.action like 'tools/call:%'
                window by_token as (partition by e.token_id order by e.ts, e.id)
            ), numbered as (
                select *, sum(case when not prev_recorded or ts - prev_at >= interval '30 minutes'
                                   then 1 else 0 end)
                    over (partition by token_id order by ts, id) as session_no
                from ordered
                where arg_bytes is not null and not_assessed_n is not null
            ), recent as (
                select token_id, session_no, min(ts) as started_at, max(ts) as ended_at, max(id) as last_id
                from numbered group by token_id, session_no
                order by ended_at desc, last_id desc limit 50
            ), stats as (
                select token_id, session_no, tool, count(*) as calls,
                       count(*) filter (where not_assessed_n > 0) as not_assessed_calls,
                       sum(not_assessed_n) as not_assessed_n,
                       count(*) filter (where result_kind = 'budget_cut') as budget_cuts,
                       count(*) filter (where result_kind = 'denied') as denied,
                       count(*) filter (where result_kind = 'error') as errors,
                       sum(arg_bytes) as arg_bytes
                from numbered join recent using (token_id, session_no)
                group by token_id, session_no, tool
            )
            select started_at, ended_at,
                   jsonb_agg(to_jsonb(stats) - array['token_id', 'session_no'] order by tool) as tools
            from recent join stats using (token_id, session_no)
            group by started_at, ended_at, last_id
            order by ended_at desc, last_id desc"#,
        )
        .bind(household_id)
        .fetch_all(&mut *tx)
        .await?
    } else {
        Vec::new()
    };
    write_audit(
        &mut tx,
        &AuditEvent {
            actor_id: Some(uid),
            tool: Some("GET /api/mcp/session-report".into()),
            ..AuditEvent::new("user", "ui", "read")
        },
    )
    .await?;
    tx.commit().await?;
    Ok((
        [(header::CACHE_CONTROL, "no-store")],
        Json(Report { enabled, sessions }),
    ))
}

async fn put_consent(
    State(rt): State<Runtime>,
    SessionCaller(p): SessionCaller,
    Body(consent): Body<Consent>,
) -> ApiResult<Json<Consent>> {
    let mut tx = rt.pool.begin().await?;
    let (household_id, uid) = owner(&mut tx, &p).await?;
    sqlx::query(
        "insert into household_setting (household_id, key, value) values ($1, $2, 'false'::jsonb) \
         on conflict (household_id, key) do nothing",
    )
    .bind(household_id)
    .bind(SESSION_REPORT_SETTING)
    .execute(&mut *tx)
    .await?;
    let changed = sqlx::query(
        "update household_setting set value = $3 \
         where household_id = $1 and key = $2 and value is distinct from $3",
    )
    .bind(household_id)
    .bind(SESSION_REPORT_SETTING)
    .bind(json!(consent.enabled))
    .execute(&mut *tx)
    .await?;
    if changed.rows_affected() > 0 {
        write_audit(
            &mut tx,
            &AuditEvent {
                actor_id: Some(uid),
                tool: Some("PUT /api/mcp/session-report/consent".into()),
                args_summary: Some(format!("{SESSION_REPORT_SETTING}={}", consent.enabled)),
                ..AuditEvent::new("user", "ui", "consent_change")
            },
        )
        .await?;
    }
    tx.commit().await?;
    Ok(Json(consent))
}

pub fn router() -> Router<Runtime> {
    Router::new()
        .route("/mcp/session-report", get(get_report))
        .route("/mcp/session-report/consent", put(put_consent))
}
