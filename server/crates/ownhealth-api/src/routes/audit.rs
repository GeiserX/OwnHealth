//! Lane G: `api/audit.py`. The audit log viewer: who (person or token) read or changed what, never
//! the payloads.
//!
//! A person sees every event on a profile they manage (owner, or guardian of an unclaimed profile),
//! and their own actions everywhere, including what their tokens did. A viewer of someone else's
//! profile does not see who else read it.

use axum::Router;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::routing::get;
use chrono::{DateTime, Utc};
use ownhealth_auth::household::can_manage;
use ownhealth_core::Runtime;
use ownhealth_core::access::accessible_profiles;
use ownhealth_core::pyjson::pydantic_datetime_utc;
use serde_json::{Value, json};
use uuid::Uuid;

use super::auth::{R, detail, ok};
use crate::extract::{Caller, SessionCaller};
use crate::validate;

#[derive(sqlx::FromRow)]
struct Row {
    id: i64,
    ts: DateTime<Utc>,
    actor_kind: String,
    via: String,
    action: String,
    tool: Option<String>,
    resource_type: Option<String>,
    resource_ids: Option<Vec<String>>,
    src_ids: Option<Vec<String>>,
    args_summary: Option<String>,
    rows_out: Option<i32>,
    bytes_out: Option<i32>,
    result_kind: Option<String>,
    withheld: Option<Value>,
    user_name: Option<String>,
    token_label: Option<String>,
    client: Option<String>,
    profile_slug: Option<String>,
}

/// The name a log row shows for its actor.
fn actor_label(kind: &str, user_name: Option<&str>, token_label: Option<&str>, client: Option<&str>) -> String {
    match kind {
        // a row the server wrote with no token (a replayed OAuth code) still names its app
        "token" => format!(
            "{} (token)",
            token_label
                .or(client.filter(|c| !c.is_empty()))
                .unwrap_or("deleted token")
        ),
        "system" => "OwnHealth".into(),
        _ => user_name.unwrap_or("unknown").to_string(),
    }
}

async fn audit_log(State(rt): State<Runtime>, SessionCaller(p): SessionCaller, Query(q): Query<validate::Params>) -> R {
    let mut e = validate::Errors::new();
    let limit = e.int("query", &q, "limit", 100, Some(1), Some(500));
    let before = validate::param(&q, "before").map(|_| e.int("query", &q, "before", 0, Some(1), None));
    e.done()?;
    let mut tx = rt.pool.begin().await?;
    let managed: Vec<Uuid> = accessible_profiles(&mut tx, &p)
        .await?
        .iter()
        .filter(|perm| can_manage(perm))
        .map(|perm| perm.profile.id)
        .collect();
    let profile = validate::param(&q, "profile").filter(|s| !s.is_empty());
    // One channel only ('mcp', 'ui', 'api'): what agents did is not buried under page views.
    let via = validate::param(&q, "via").filter(|s| !s.is_empty()).map(str::to_owned);
    if let Some(v) = via.as_deref()
        && !["mcp", "ui", "api"].contains(&v)
    {
        return Err(detail(
            StatusCode::UNPROCESSABLE_ENTITY,
            "bad_input",
            &format!("Unknown via '{v}': use mcp, ui or api."),
        ));
    }
    let pid = match profile {
        Some(r) => Some(Caller(p.clone()).require(&mut tx, r, None, None).await?.profile.id),
        None => None,
    };
    let rows: Vec<Row> = sqlx::query_as(
        "select e.id, e.ts, e.actor_kind, e.via, e.action, e.tool, e.resource_type, e.resource_ids, e.src_ids, \
         e.args_summary, e.rows_out, e.bytes_out, e.result_kind, e.withheld, u.display_name as user_name, \
         t.label as token_label, e.client, p.slug as profile_slug \
         from audit_event e left join app_user u on u.id = e.actor_id \
         left join api_token t on t.id = e.token_id left join profile p on p.id = e.profile_id \
         where (e.profile_id = any($1) or e.actor_id = $2) and ($3::uuid is null or e.profile_id = $3) \
         and ($4::bigint is null or e.id < $4) and ($6::text is null or e.via = $6) order by e.id desc limit $5",
    )
    .bind(&managed)
    .bind(p.user_id)
    .bind(pid)
    .bind(before)
    .bind(limit)
    .bind(via)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    let items: Vec<Value> = rows
        .into_iter()
        .map(|r| {
            let actor = actor_label(
                &r.actor_kind,
                r.user_name.as_deref(),
                r.token_label.as_deref(),
                r.client.as_deref(),
            );
            json!({
                "id": r.id,
                "ts": pydantic_datetime_utc(&r.ts),
                "actor": actor,
                "actor_kind": r.actor_kind,
                "via": r.via,
                "action": r.action,
                "tool": r.tool,
                "profile": r.profile_slug,
                "resource_type": r.resource_type,
                "resource_ids": r.resource_ids.unwrap_or_default(),
                "src_ids": r.src_ids.unwrap_or_default(),
                "args_summary": r.args_summary,
                "rows_out": r.rows_out,
                "bytes_out": r.bytes_out,
                "result_kind": r.result_kind,
                "withheld": r.withheld,
            })
        })
        .collect();
    let next = if items.len() as i64 == limit {
        items.last().map_or(Value::Null, |i| i["id"].clone())
    } else {
        Value::Null
    };
    ok(json!({"items": items, "next_before": next}))
}

/// The state of the whole log, for every signed-in person: how far the hash chain holds, the first
/// entry that does not, and whether the head the last backup anchored outside the database is still
/// in it (ownhealth-ops `audit_status`). Counts and verdicts only, never a hash, a path or an entry.
/// One full check runs at a time and a young verdict is reused (`audit_status_shared`), so loads in
/// parallel cannot make the database read the whole log over and over.
///
/// No audit row: it reads no profile's data, only the verdict on the log, and `GET /api/audit` and
/// `/api/audit/who`, which read the log itself, write none either.
async fn audit_chain(State(rt): State<Runtime>, SessionCaller(_p): SessionCaller) -> R {
    ok(ownhealth_ops::audit_status_shared(&rt.settings, &rt.pool).await?)
}

#[derive(sqlx::FromRow)]
struct WhoRow {
    actor_kind: String,
    via: String,
    events: i64,
    first_ts: DateTime<Utc>,
    last_ts: DateTime<Utc>,
    user_name: Option<String>,
    token_label: Option<String>,
    client: Option<String>,
}

/// Who saw one profile's data: one row per actor and channel, with how many log entries it has on
/// that profile and when the first and the last were. The same rows as the log: everyone on a
/// profile the person manages, only their own actions on a profile they just view, and nothing on a
/// profile they hold no grant on (404, as everywhere).
async fn audit_who(State(rt): State<Runtime>, SessionCaller(p): SessionCaller, Query(q): Query<validate::Params>) -> R {
    let Some(slug) = validate::param(&q, "profile").filter(|s| !s.is_empty()) else {
        return Err(detail(
            StatusCode::UNPROCESSABLE_ENTITY,
            "bad_input",
            "Say which profile: ?profile=<slug>.",
        ));
    };
    let mut tx = rt.pool.begin().await?;
    let perm = Caller(p.clone()).require(&mut tx, slug, None, None).await?;
    let pid = perm.profile.id;
    let everyone = can_manage(&perm);
    let rows: Vec<WhoRow> = sqlx::query_as(
        "select e.actor_kind, e.via, count(*) as events, min(e.ts) as first_ts, max(e.ts) as last_ts, \
         min(u.display_name) as user_name, min(t.label) as token_label, min(e.client) as client \
         from audit_event e left join app_user u on u.id = e.actor_id left join api_token t on t.id = e.token_id \
         where e.profile_id = $1 and ($2 or e.actor_id = $3) \
         group by e.actor_kind, e.actor_id, e.token_id, e.client, e.via order by max(e.id) desc",
    )
    .bind(pid)
    .bind(everyone)
    .bind(p.user_id)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    let items: Vec<Value> = rows
        .into_iter()
        .map(|r| {
            json!({
                "actor": actor_label(&r.actor_kind, r.user_name.as_deref(), r.token_label.as_deref(), r.client.as_deref()),
                "actor_kind": r.actor_kind,
                "via": r.via,
                "events": r.events,
                "first_ts": pydantic_datetime_utc(&r.first_ts),
                "last_ts": pydantic_datetime_utc(&r.last_ts),
            })
        })
        .collect();
    // "own": the person only views this profile, so the rows are their own actions on it.
    let scope = if everyone { "everyone" } else { "own" };
    ok(json!({"profile": perm.profile.slug, "scope": scope, "items": items}))
}

pub fn router() -> Router<Runtime> {
    Router::new()
        .route("/audit", get(audit_log))
        .route("/audit/chain", get(audit_chain))
        .route("/audit/who", get(audit_who))
}
