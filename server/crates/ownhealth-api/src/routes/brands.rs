//! The US endpoint directory (connectors.md 7, "Endpoint discovery";
//! ownhealth-ingest-structured `brands`): the two calls the US connection wizard makes.
//!
//! | Route | What |
//! |---|---|
//! | `POST /brands` | the wizard was opened: make the household's directory source on the first call and queue its first pull; `{created, directory}` (201 when made) |
//! | `GET /brands?q=&limit=` | `{directory, results: [{name, endpoint, state, vendor}]}`; `directory` is null and nothing is fetched before the first POST |
//!
//! An owner or guardian of the household, as for every connection. A GET never makes the source, so
//! only opening the wizard can make this install contact a US vendor.

use axum::Router;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::routing::get;
use ownhealth_auth::household::can_manage;
use ownhealth_core::access::{Principal, accessible_profiles};
use ownhealth_core::audit::{AuditEvent, write_audit};
use ownhealth_core::{Error, Runtime};
use ownhealth_ingest_structured::brands;
use serde_json::json;
use sqlx::PgConnection;
use uuid::Uuid;

use super::auth::{self, Params, R, ok};
use crate::Caller;

/// Results a search returns when the query names no limit.
const DEFAULT_LIMIT: i64 = 20;

pub fn router() -> Router<Runtime> {
    Router::new().route("/brands", get(search).post(open))
}

/// The household the caller manages (owner or guardian of one of its profiles).
async fn manager(conn: &mut PgConnection, p: &Principal) -> R<Uuid> {
    let household = p
        .household_id
        .ok_or_else(|| Error::access_denied("forbidden", "Only a person of the household manages its connections."))?;
    let perms = accessible_profiles(&mut *conn, p).await?;
    if !perms
        .iter()
        .any(|perm| perm.profile.household_id == household && can_manage(perm))
    {
        return Err(Error::access_denied("forbidden", "Only an owner or guardian manages the connections.").into());
    }
    Ok(household)
}

/// The wizard was opened: the household's directory source, made on the first call.
async fn open(State(rt): State<Runtime>, Caller(p): Caller) -> R {
    let mut tx = rt.pool.begin().await?;
    let household = manager(&mut tx, &p).await?;
    let (id, created) = brands::ensure_source(&mut tx, household, p.user_id).await?;
    if created {
        write_audit(
            &mut tx,
            &AuditEvent {
                actor_id: p.user_id,
                tool: Some("connections".into()),
                resource_type: Some("source".into()),
                resource_ids: Some(vec![id.to_string()]),
                ..AuditEvent::new("user", "ui", "source.create")
            },
        )
        .await?;
    }
    let directory = brands::directory(&mut tx, household).await?;
    tx.commit().await?;
    let status = if created { StatusCode::CREATED } else { StatusCode::OK };
    Ok(auth::answer(
        status,
        json!({"created": created, "directory": directory}),
        &[],
    ))
}

/// The directory's state and the entries whose name holds every word of `q`.
async fn search(State(rt): State<Runtime>, Caller(p): Caller, Query(q): Params) -> R {
    let limit = match q.get("limit") {
        None => DEFAULT_LIMIT,
        Some(v) => v
            .parse::<i64>()
            .ok()
            .filter(|n| (1..=brands::LIMIT_MAX).contains(n))
            .ok_or_else(|| Error::Invalid(format!("limit must be 1 to {}", brands::LIMIT_MAX)))?,
    };
    let query = q.get("q").map(String::as_str).unwrap_or_default();
    let mut tx = rt.pool.begin().await?;
    let household = manager(&mut tx, &p).await?;
    let directory = brands::directory(&mut tx, household).await?;
    let results = match &directory {
        Some(_) => brands::search(&mut tx, household, query, limit).await?,
        None => Vec::new(),
    };
    // a read of the household's directory: one audit row in the same transaction (the query names
    // a facility, not a person, and stays out of the row)
    let action = if query.trim().is_empty() { "read" } else { "search" };
    write_audit(
        &mut tx,
        &AuditEvent {
            actor_id: p.user_id,
            tool: Some("connections".into()),
            resource_type: Some("source".into()),
            resource_ids: directory.as_ref().map(|d| vec![d.source_id.to_string()]),
            ..AuditEvent::new("user", "ui", action)
        },
    )
    .await?;
    tx.commit().await?;
    ok(json!({"directory": directory, "results": results}))
}
