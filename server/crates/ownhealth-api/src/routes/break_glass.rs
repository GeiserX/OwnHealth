//! Break-glass (migration 1260, `ownhealth_core::break_glass`): emergency access for a relative the
//! owner named, after a delay the owner can cancel in, and the in-app notices that tell the owner.
//!
//! ```text
//! the owner (manage_grants on {p})
//! GET    /api/profiles/{p}/break-glass                              contacts, requests, calendar_feed
//! PUT    /api/profiles/{p}/break-glass/contacts/{user}  + step-up   {categories?, delay_hours?}
//! DELETE /api/profiles/{p}/break-glass/contacts/{user}              also ends their open request
//! POST   /api/profiles/{p}/break-glass/requests/{id}/cancel         a requested one
//! POST   /api/profiles/{p}/break-glass/requests/{id}/revoke         an active one
//! the contact
//! GET    /api/break-glass                                           the profiles that named them
//! POST   /api/break-glass/requests                      + step-up   {profile, reason}
//! anyone logged in
//! GET    /api/notifications                                         {items, unread}
//! POST   /api/notifications/read                                    {ids?} -> {read}
//! ```
//!
//! Naming a contact needs a live calendar feed of the owner on the profile, and a request needs one of
//! an owner: the feed is how the owner hears of a request, and a request opens only after a feed has
//! served it (409 `no_calendar_feed`). `record_uses` wraps every REST request and writes the reads made
//! through an open request (one audit row and one outbox row each) before the answer leaves.

use axum::Router;
use axum::extract::{Path, Request, State};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use chrono::{DateTime, Utc};
use ownhealth_auth::sessions;
use ownhealth_core::Runtime;
use ownhealth_core::audit::{AuditEvent, write_audit};
use ownhealth_core::break_glass::{
    self as bg, CATEGORIES, DEFAULT_CATEGORIES, DEFAULT_DELAY_HOURS, End, MAX_DELAY_HOURS, MIN_DELAY_HOURS,
    REQUEST_COLS,
};
use ownhealth_core::pyjson::pydantic_datetime_utc;
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::PgConnection;
use uuid::Uuid;

use super::auth::{Body, Check, R, answer, ok};
use crate::error::ApiError;
use crate::extract::Caller;
use crate::validate::uuid_path;

/// The layer around every REST route: the reads made through an open request are written before the
/// answer leaves; when they cannot be, the answer is an error instead.
pub async fn record_uses(req: Request, next: Next) -> Response {
    let (res, rec) = bg::recording(next.run(req)).await;
    match bg::flush(&rec).await {
        Ok(_) => res,
        Err(e) => {
            tracing::error!(error = %e, "a break-glass read could not be recorded");
            ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, "internal", "Internal error.").into_response()
        }
    }
}

fn ts(t: &Option<DateTime<Utc>>) -> Value {
    t.as_ref().map_or(Value::Null, |t| json!(pydantic_datetime_utc(t)))
}

#[derive(sqlx::FromRow)]
struct Listed {
    #[sqlx(flatten)]
    r: bg::Request,
    contact_name: String,
}

fn request_out(l: &Listed, now: DateTime<Utc>) -> Value {
    let r = &l.r;
    json!({
        "id": r.id,
        "profile_id": r.profile_id,
        "contact": {"user_id": r.contact_user_id, "display_name": l.contact_name},
        "reason": r.reason,
        "categories": r.categories,
        "status": r.status,
        "requested_at": pydantic_datetime_utc(&r.requested_at),
        "activates_at": pydantic_datetime_utc(&r.activates_at),
        "owner_feed_at": ts(&r.owner_feed_at),
        "activated_at": ts(&r.activated_at),
        "expires_at": ts(&r.expires_at),
        "ended_at": ts(&r.ended_at),
        // the delay has passed, but no owner calendar feed has served the request yet
        "waiting_for_calendar": r.status == "requested" && r.owner_feed_at.is_none() && r.activates_at <= now,
    })
}

fn listed_sql(filter: &str) -> String {
    let cols: Vec<String> = REQUEST_COLS.split(',').map(|c| format!("r.{}", c.trim())).collect();
    format!(
        "select {}, u.display_name as contact_name from break_glass_request r \
         join app_user u on u.id = r.contact_user_id where {filter}",
        cols.join(", ")
    )
}

async fn db_now(conn: &mut PgConnection) -> R<DateTime<Utc>> {
    Ok(sqlx::query_scalar("select now()").fetch_one(conn).await?)
}

async fn one(conn: &mut PgConnection, id: Uuid) -> R<Value> {
    let l: Listed = sqlx::query_as(sqlx::AssertSqlSafe(listed_sql("r.id = $1")))
        .bind(id)
        .fetch_one(&mut *conn)
        .await?;
    let now = db_now(conn).await?;
    Ok(request_out(&l, now))
}

fn no_feed(whose: &str) -> ApiError {
    ApiError::new(
        StatusCode::CONFLICT,
        "no_calendar_feed",
        format!(
            "Emergency access needs {whose} calendar feed on this profile: it is how the owner hears of a request."
        ),
    )
}

// ------------------------------------------------------------------- the owner ----

type ContactRow = (Uuid, String, Vec<String>, i32, DateTime<Utc>);

fn contact_out(c: &ContactRow) -> Value {
    json!({"user_id": c.0, "display_name": c.1, "categories": c.2, "delay_hours": c.3,
           "created_at": pydantic_datetime_utc(&c.4)})
}

const CONTACT_COLS: &str = "c.contact_user_id, u.display_name, c.categories, c.delay_hours, c.created_at";

async fn owner_view(State(rt): State<Runtime>, caller: Caller, Path(p): Path<String>) -> R {
    let mut tx = rt.pool.begin().await?;
    let perm = caller.require(&mut tx, &p, None, Some("manage_grants")).await?;
    let contacts: Vec<ContactRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "select {CONTACT_COLS} from break_glass_contact c join app_user u on u.id = c.contact_user_id \
         where c.profile_id = $1 order by u.display_name, c.contact_user_id"
    )))
    .bind(perm.profile.id)
    .fetch_all(&mut *tx)
    .await?;
    let requests: Vec<Listed> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "{} order by r.requested_at desc, r.id limit 100",
        listed_sql("r.profile_id = $1")
    )))
    .bind(perm.profile.id)
    .fetch_all(&mut *tx)
    .await?;
    let feed = bg::owner_feed_live(&mut tx, perm.profile.id, caller.0.user_id).await?;
    let now = db_now(&mut tx).await?;
    tx.commit().await?;
    ok(json!({
        "contacts": contacts.iter().map(contact_out).collect::<Vec<_>>(),
        "requests": requests.iter().map(|l| request_out(l, now)).collect::<Vec<_>>(),
        "calendar_feed": feed,
        "defaults": {"categories": DEFAULT_CATEGORIES, "delay_hours": DEFAULT_DELAY_HOURS,
                     "min_delay_hours": MIN_DELAY_HOURS, "max_delay_hours": MAX_DELAY_HOURS,
                     "allowed": CATEGORIES},
    }))
}

#[derive(Deserialize)]
struct ContactIn {
    categories: Option<Vec<String>>,
    delay_hours: Option<i32>,
}

fn audit_contact(user: Option<Uuid>, profile: Uuid, contact: Uuid, action: &str, args: String) -> AuditEvent {
    let mut e = AuditEvent::new("user", "ui", action);
    e.actor_id = user;
    e.tool = Some("break_glass".into());
    e.profile_id = Some(profile);
    e.resource_type = Some("break_glass_contact".into());
    e.resource_ids = Some(vec![contact.to_string()]);
    e.args_summary = Some(args);
    e
}

/// Name a contact, or change what a named one would read. Widening access, so it needs step-up.
async fn set_contact(
    State(rt): State<Runtime>,
    caller: Caller,
    Path((p, user)): Path<(String, String)>,
    Body(b): Body<ContactIn>,
) -> R {
    let mut check = Check::body();
    for c in b.categories.iter().flatten() {
        check.literal("categories", Some(c), &CATEGORIES);
    }
    check.done()?;
    let contact = uuid_path("user", &user)?;
    let mut categories: Vec<String> = b
        .categories
        .unwrap_or_else(|| DEFAULT_CATEGORIES.iter().map(|c| c.to_string()).collect());
    categories.sort();
    categories.dedup();
    if categories.is_empty() {
        return Err(ownhealth_core::Error::Invalid("choose at least one category".into()).into());
    }
    let delay = b.delay_hours.unwrap_or(DEFAULT_DELAY_HOURS);
    if !(MIN_DELAY_HOURS..=MAX_DELAY_HOURS).contains(&delay) {
        return Err(ownhealth_core::Error::Invalid(format!(
            "delay_hours must be {MIN_DELAY_HOURS} to {MAX_DELAY_HOURS}"
        ))
        .into());
    }
    let mut tx = rt.pool.begin().await?;
    let perm = caller.require(&mut tx, &p, None, Some("manage_grants")).await?;
    sessions::require_step_up(&mut tx, &caller.0, sessions::STEP_UP_SECONDS).await?;
    // a login of this household, open, and not someone who already owns the profile
    let fits: bool = sqlx::query_scalar(
        "select exists (select 1 from app_user u where u.id = $1 and u.household_id = $2 and u.disabled_at is null \
         and not exists (select 1 from profile_grant g where g.user_id = u.id and g.profile_id = $3 \
         and g.role = 'owner'))",
    )
    .bind(contact)
    .bind(perm.profile.household_id)
    .bind(perm.profile.id)
    .fetch_one(&mut *tx)
    .await?;
    if !fits {
        return Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "unknown_user",
            "No such person in this household, or they already own this profile.",
        )
        .into());
    }
    if !bg::owner_feed_live(&mut tx, perm.profile.id, caller.0.user_id).await? {
        return Err(no_feed("your").into());
    }
    sqlx::query(
        "insert into break_glass_contact (profile_id, contact_user_id, categories, delay_hours, created_by) \
         values ($1, $2, $3, $4, $5) on conflict (profile_id, contact_user_id) do update set \
         categories = excluded.categories, delay_hours = excluded.delay_hours",
    )
    .bind(perm.profile.id)
    .bind(contact)
    .bind(&categories)
    .bind(delay)
    .bind(caller.0.user_id)
    .execute(&mut *tx)
    .await?;
    let e = audit_contact(
        caller.0.user_id,
        perm.profile.id,
        contact,
        "break_glass.contact",
        format!("categories={};delay_hours={delay}", categories.join(",")),
    );
    write_audit(&mut tx, &e).await?;
    let row: ContactRow = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "select {CONTACT_COLS} from break_glass_contact c join app_user u on u.id = c.contact_user_id \
         where c.profile_id = $1 and c.contact_user_id = $2"
    )))
    .bind(perm.profile.id)
    .bind(contact)
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    ok(contact_out(&row))
}

/// Remove a contact; their open request ends with it (cancelled, or revoked when active).
async fn remove_contact(State(rt): State<Runtime>, caller: Caller, Path((p, user)): Path<(String, String)>) -> R {
    let contact = uuid_path("user", &user)?;
    let mut tx = rt.pool.begin().await?;
    let perm = caller.require(&mut tx, &p, None, Some("manage_grants")).await?;
    let by = caller.0.user_id.ok_or_else(ApiError::unauthenticated)?;
    let gone = sqlx::query("delete from break_glass_contact where profile_id = $1 and contact_user_id = $2")
        .bind(perm.profile.id)
        .bind(contact)
        .execute(&mut *tx)
        .await?
        .rows_affected();
    if gone == 0 {
        return Err(ApiError::not_found().into());
    }
    let ended = bg::end_open(&mut tx, perm.profile.id, contact, by).await?;
    let e = audit_contact(
        Some(by),
        perm.profile.id,
        contact,
        "break_glass.contact_remove",
        String::new(),
    );
    write_audit(&mut tx, &e).await?;
    tx.commit().await?;
    ok(json!({"removed": true, "ended": ended.iter().map(|r| r.id).collect::<Vec<_>>()}))
}

async fn end(rt: Runtime, caller: Caller, p: String, id: String, step: End) -> R {
    let id = uuid_path("id", &id)?;
    let mut tx = rt.pool.begin().await?;
    let perm = caller.require(&mut tx, &p, None, Some("manage_grants")).await?;
    let by = caller.0.user_id.ok_or_else(ApiError::unauthenticated)?;
    let r = bg::end(&mut tx, perm.profile.id, id, by, step).await?;
    let out = one(&mut tx, r.id).await?;
    tx.commit().await?;
    ok(out)
}

/// Taking access away needs no step-up.
async fn cancel(State(rt): State<Runtime>, caller: Caller, Path((p, id)): Path<(String, String)>) -> R {
    end(rt, caller, p, id, End::Cancel).await
}

async fn revoke(State(rt): State<Runtime>, caller: Caller, Path((p, id)): Path<(String, String)>) -> R {
    end(rt, caller, p, id, End::Revoke).await
}

// ----------------------------------------------------------------- the contact ----

type Named = (Uuid, String, String, Vec<String>, i32);

async fn mine(State(rt): State<Runtime>, caller: Caller) -> R {
    let user = caller.0.user_id.ok_or_else(ApiError::unauthenticated)?;
    let mut conn = rt.pool.acquire().await?;
    let named: Vec<Named> = sqlx::query_as(
        "select p.id, p.slug, p.display_name, c.categories, c.delay_hours from break_glass_contact c \
         join profile p on p.id = c.profile_id where c.contact_user_id = $1 order by p.slug",
    )
    .bind(user)
    .fetch_all(&mut *conn)
    .await?;
    let last: Vec<Listed> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "select distinct on (r.profile_id) * from ({}) r order by r.profile_id, r.requested_at desc",
        listed_sql("r.contact_user_id = $1")
    )))
    .bind(user)
    .fetch_all(&mut *conn)
    .await?;
    let now = db_now(&mut conn).await?;
    let items: Vec<Value> = named
        .iter()
        .map(|n| {
            let request = last
                .iter()
                .find(|l| l.r.profile_id == n.0)
                .map_or(Value::Null, |l| request_out(l, now));
            json!({"profile": {"id": n.0, "slug": n.1, "display_name": n.2},
                   "categories": n.3, "delay_hours": n.4, "request": request})
        })
        .collect();
    ok(json!({"items": items}))
}

#[derive(Deserialize)]
struct RequestIn {
    profile: String,
    reason: String,
}

/// Ask for access. Step-up, so a session someone else picked up cannot ask.
async fn ask(State(rt): State<Runtime>, caller: Caller, Body(b): Body<RequestIn>) -> R {
    let reason = b.reason.trim().to_string();
    Check::body().len("reason", Some(&reason), 1, 500).done()?;
    let profile = uuid_path("profile", &b.profile)?;
    let user = caller.0.user_id.ok_or_else(ApiError::unauthenticated)?;
    let mut tx = rt.pool.begin().await?;
    sessions::require_step_up(&mut tx, &caller.0, sessions::STEP_UP_SECONDS).await?;
    let named: bool = sqlx::query_scalar(
        "select exists (select 1 from break_glass_contact where profile_id = $1 and contact_user_id = $2)",
    )
    .bind(profile)
    .bind(user)
    .fetch_one(&mut *tx)
    .await?;
    if !named {
        return Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "not_a_contact",
            "No emergency access was set up for you on this profile.",
        )
        .into());
    }
    if !bg::owner_feed_live(&mut tx, profile, None).await? {
        return Err(no_feed("an owner").into());
    }
    let r = bg::request(&mut tx, user, profile, &reason).await?;
    let out = one(&mut tx, r.id).await?;
    tx.commit().await?;
    Ok(answer(StatusCode::CREATED, out, &[]))
}

// ---------------------------------------------------------------- the outbox ----

type Notice = (
    Uuid,
    String,
    Uuid,
    String,
    String,
    Option<Uuid>,
    Option<Uuid>,
    Option<String>,
    DateTime<Utc>,
    Option<DateTime<Utc>>,
);

async fn notices(State(rt): State<Runtime>, caller: Caller) -> R {
    let user = caller.0.user_id.ok_or_else(ApiError::unauthenticated)?;
    let mut conn = rt.pool.acquire().await?;
    let rows: Vec<Notice> = sqlx::query_as(
        "select n.id, n.kind, p.id, p.slug, p.display_name, n.request_id, n.actor_id, a.display_name, \
         n.created_at, n.read_at from notification_outbox n join profile p on p.id = n.profile_id \
         left join app_user a on a.id = n.actor_id where n.user_id = $1 \
         order by n.created_at desc, n.id limit 200",
    )
    .bind(user)
    .fetch_all(&mut *conn)
    .await?;
    let unread: i64 =
        sqlx::query_scalar("select count(*) from notification_outbox where user_id = $1 and read_at is null")
            .bind(user)
            .fetch_one(&mut *conn)
            .await?;
    let items: Vec<Value> = rows
        .iter()
        .map(|n| {
            json!({"id": n.0, "kind": n.1,
                   "profile": {"id": n.2, "slug": n.3, "display_name": n.4},
                   "request_id": n.5, "actor": n.6.map(|id| json!({"user_id": id, "display_name": n.7})),
                   "created_at": pydantic_datetime_utc(&n.8), "read_at": ts(&n.9)})
        })
        .collect();
    ok(json!({"items": items, "unread": unread}))
}

#[derive(Deserialize)]
struct ReadIn {
    ids: Option<Vec<Uuid>>,
}

/// Mark the caller's notices read: the ones named, or all.
async fn mark_read(State(rt): State<Runtime>, caller: Caller, Body(b): Body<ReadIn>) -> R {
    let user = caller.0.user_id.ok_or_else(ApiError::unauthenticated)?;
    let mut conn = rt.pool.acquire().await?;
    let n = sqlx::query(
        "update notification_outbox set read_at = now() where user_id = $1 and read_at is null \
         and ($2::uuid[] is null or id = any($2))",
    )
    .bind(user)
    .bind(b.ids)
    .execute(&mut *conn)
    .await?
    .rows_affected();
    ok(json!({"read": n}))
}

pub fn router() -> Router<Runtime> {
    Router::new()
        .route("/profiles/{p}/break-glass", get(owner_view))
        .route(
            "/profiles/{p}/break-glass/contacts/{user}",
            put(set_contact).delete(remove_contact),
        )
        .route("/profiles/{p}/break-glass/requests/{id}/cancel", post(cancel))
        .route("/profiles/{p}/break-glass/requests/{id}/revoke", post(revoke))
        .route("/break-glass", get(mine))
        .route("/break-glass/requests", post(ask))
        .route("/notifications", get(notices))
        .route("/notifications/read", post(mark_read))
}
