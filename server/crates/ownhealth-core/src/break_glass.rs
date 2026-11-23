//! Break-glass (security-privacy.md section 6.5, migration 1260): a relative the owner named reads the
//! owner's records in an emergency, after a delay the owner can cancel in.
//!
//! ```text
//! requested --(delay passed AND an owner calendar feed served it)--> active --(14 days)--> expired
//!     |                                                                  |
//!     +-- cancelled (owner, or the contact removed)                       +-- revoked (owner)
//! ```
//!
//! - The owner names a contact and the categories the contact would read (`CATEGORIES`, never
//!   `read:sensitive`, `read:genome_raw` or a write). Naming one needs an owner calendar feed.
//! - The contact asks with a reason. The request opens once `activates_at` has passed and an owner
//!   calendar feed has served it (`owner_feed_at`), at the contact's next request (`settle`, called by
//!   the REST caller) or at the sweep, whichever comes first, and stays open for `OPEN_DAYS`.
//! - An open request is added to a browser session's grant by `access.rs`, and only while a REST
//!   request records its uses (`recording`, `arm`). Tokens, OAuth consents, calendar feeds and share
//!   links read without it. The contact's own `profile_grant` row is never touched.
//! - Every step writes an audit row and tells the owner through `notification_outbox`: the request,
//!   a cancel, the activation, each REST request that read through the request, the expiry and a
//!   revoke.

use std::collections::BTreeSet;
use std::future::Future;
use std::sync::{Arc, Mutex, PoisonError};

use chrono::{DateTime, Utc};
use serde::Serialize;
use sqlx::{Connection, PgConnection, PgPool};
use uuid::Uuid;

use crate::audit::{AuditEvent, write_audit};
use crate::error::{Error, Result};

pub const STATUSES: [&str; 5] = ["requested", "cancelled", "active", "expired", "revoked"];
/// What a contact reads by default.
pub const DEFAULT_CATEGORIES: [&str; 3] = ["read:summary", "read:records", "read:labs"];
/// What a contact may be given: reads only, never `read:sensitive` or `read:genome_raw`.
pub const CATEGORIES: [&str; 7] = [
    "read:summary",
    "read:labs",
    "read:vitals",
    "read:wearables",
    "read:records",
    "read:genome",
    "read:documents_text",
];
pub const DEFAULT_DELAY_HOURS: i32 = 48;
pub const MIN_DELAY_HOURS: i32 = 12;
pub const MAX_DELAY_HOURS: i32 = 168;
/// How long an open request lasts.
pub const OPEN_DAYS: i32 = 14;
/// `notification_outbox.kind`, as the migration's CHECK lists it.
pub const OUTBOX_KINDS: [&str; 6] = [
    "break_glass.request",
    "break_glass.cancel",
    "break_glass.activate",
    "break_glass.use",
    "break_glass.expire",
    "break_glass.revoke",
];

// ------------------------------------------------------------- recording uses ----

/// One read through an open request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Use {
    pub request_id: Uuid,
    pub profile_id: Uuid,
    pub contact: Uuid,
}

struct Armed {
    user_id: Uuid,
    pool: PgPool,
    route: String,
}

/// The uses of one REST request, written by `flush` before its answer leaves.
#[derive(Default)]
pub struct Recorder {
    armed: Mutex<Option<Armed>>,
    uses: Mutex<Vec<Use>>,
}

tokio::task_local! {
    static RECORDER: Arc<Recorder>;
}

/// Run `f` with a recorder in scope (the REST layer wraps every request in it).
pub async fn recording<F: Future>(f: F) -> (F::Output, Arc<Recorder>) {
    let rec = Arc::new(Recorder::default());
    let out = RECORDER.scope(rec.clone(), f).await;
    (out, rec)
}

/// The REST caller of this request is `user_id`, a person in the browser: from here on an open request
/// of theirs counts, and each use is written to `pool`. Outside `recording` it does nothing, so the
/// union never applies there.
pub fn arm(user_id: Uuid, pool: PgPool, route: String) {
    let _ = RECORDER.try_with(|r| {
        *r.armed.lock().unwrap_or_else(PoisonError::into_inner) = Some(Armed { user_id, pool, route });
    });
}

/// Whether an open request of `user_id` may be added to their grant in this task.
pub fn armed_for(user_id: Uuid) -> bool {
    RECORDER
        .try_with(|r| {
            r.armed
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .as_ref()
                .is_some_and(|a| a.user_id == user_id)
        })
        .unwrap_or(false)
}

/// Note one read through an open request (access.rs, when the request widened a permission).
pub fn record(u: Use) {
    let _ = RECORDER.try_with(|r| r.uses.lock().unwrap_or_else(PoisonError::into_inner).push(u));
}

/// Write the uses of one REST request: one audit row and one outbox row per request read through,
/// in a transaction of their own (a repeatable read route cannot link an audit row), before the answer
/// is sent. An error here must stop the answer.
pub async fn flush(rec: &Recorder) -> Result<usize> {
    let uses: Vec<Use> = std::mem::take(&mut *rec.uses.lock().unwrap_or_else(PoisonError::into_inner));
    if uses.is_empty() {
        return Ok(0);
    }
    let (pool, route) = {
        let armed = rec.armed.lock().unwrap_or_else(PoisonError::into_inner);
        let a = armed
            .as_ref()
            .ok_or_else(|| Error::Other(anyhow::anyhow!("a break-glass use outside a REST caller")))?;
        (a.pool.clone(), a.route.clone())
    };
    let mut seen = BTreeSet::new();
    let mut tx = pool.begin().await?;
    for u in uses {
        if !seen.insert(u.request_id) {
            continue;
        }
        let mut e = event(
            "user",
            "ui",
            "break_glass.use",
            Some(u.contact),
            u.profile_id,
            u.request_id,
        );
        e.tool = Some(route.clone());
        write_audit(&mut tx, &e).await?;
        notify(&mut tx, u.profile_id, "break_glass.use", u.request_id, Some(u.contact)).await?;
    }
    tx.commit().await?;
    Ok(seen.len())
}

// ------------------------------------------------------------------ the union ----

/// An open request of `user` on `profile` (or on any profile): (request id, profile id, categories).
pub async fn active(
    conn: &mut PgConnection,
    user: Uuid,
    profile: Option<Uuid>,
) -> Result<Vec<(Uuid, Uuid, Vec<String>)>> {
    Ok(sqlx::query_as(
        "select id, profile_id, categories from break_glass_request where contact_user_id = $1 \
         and status = 'active' and expires_at > now() and ($2::uuid is null or profile_id = $2) \
         order by profile_id",
    )
    .bind(user)
    .bind(profile)
    .fetch_all(conn)
    .await?)
}

/// Whether `user` reaches any of `profiles` only through emergency access: an open request, or a
/// break-glass grant row. Token creation and OAuth consent refuse those (the caller already found no
/// ordinary grant on them).
pub async fn emergency_only(conn: &mut PgConnection, user: Uuid, profiles: &[Uuid]) -> Result<bool> {
    if profiles.is_empty() {
        return Ok(false);
    }
    Ok(sqlx::query_scalar(
        "select exists (select 1 from break_glass_request where contact_user_id = $1 and profile_id = any($2) \
         and status = 'active' and expires_at > now()) \
         or exists (select 1 from profile_grant where user_id = $1 and profile_id = any($2) and break_glass)",
    )
    .bind(user)
    .bind(profiles)
    .fetch_one(conn)
    .await?)
}

// ------------------------------------------------------------------ the steps ----

fn event(kind: &str, via: &str, action: &str, actor: Option<Uuid>, profile: Uuid, request: Uuid) -> AuditEvent {
    let mut e = AuditEvent::new(kind, via, action);
    e.actor_id = actor;
    e.tool = Some("break_glass".into());
    e.profile_id = Some(profile);
    e.resource_type = Some("break_glass_request".into());
    e.resource_ids = Some(vec![request.to_string()]);
    e
}

/// Tell every owner of the profile.
pub async fn notify(
    conn: &mut PgConnection,
    profile: Uuid,
    kind: &str,
    request: Uuid,
    actor: Option<Uuid>,
) -> Result<u64> {
    if !OUTBOX_KINDS.contains(&kind) {
        return Err(Error::Invalid(format!("unknown notification kind {kind}")));
    }
    Ok(sqlx::query(
        "insert into notification_outbox (user_id, profile_id, kind, request_id, actor_id) \
         select g.user_id, g.profile_id, $2, $3, $4 from profile_grant g where g.profile_id = $1 \
         and g.role = 'owner' and not g.break_glass and (g.expires_at is null or g.expires_at > now())",
    )
    .bind(profile)
    .bind(kind)
    .bind(request)
    .bind(actor)
    .execute(conn)
    .await?
    .rows_affected())
}

/// Whether the profile has a live calendar feed made by `owner` (or, with None, by any of its owners).
pub async fn owner_feed_live(conn: &mut PgConnection, profile: Uuid, owner: Option<Uuid>) -> Result<bool> {
    Ok(sqlx::query_scalar(
        "select exists (select 1 from calendar_feed f join app_user u on u.id = f.created_by \
         join profile_grant g on g.user_id = f.created_by and g.profile_id = f.profile_id \
         where f.profile_id = $1 and ($2::uuid is null or f.created_by = $2) and f.revoked_at is null \
         and u.disabled_at is null and g.role = 'owner' and not g.break_glass \
         and (g.expires_at is null or g.expires_at > now()))",
    )
    .bind(profile)
    .bind(owner)
    .fetch_one(conn)
    .await?)
}

/// One request as the routes show it.
#[derive(Debug, Clone, PartialEq, Serialize, sqlx::FromRow)]
pub struct Request {
    pub id: Uuid,
    pub profile_id: Uuid,
    pub contact_user_id: Uuid,
    pub categories: Vec<String>,
    pub reason: String,
    pub status: String,
    pub requested_at: DateTime<Utc>,
    pub activates_at: DateTime<Utc>,
    pub owner_feed_at: Option<DateTime<Utc>>,
    pub activated_at: Option<DateTime<Utc>>,
    pub expires_at: Option<DateTime<Utc>>,
    pub ended_at: Option<DateTime<Utc>>,
}

pub const REQUEST_COLS: &str = "id, profile_id, contact_user_id, categories, reason, status, requested_at, \
                                activates_at, owner_feed_at, activated_at, expires_at, ended_at";

pub async fn get_request(conn: &mut PgConnection, id: Uuid) -> Result<Option<Request>> {
    Ok(sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "select {REQUEST_COLS} from break_glass_request where id = $1"
    )))
    .bind(id)
    .fetch_optional(conn)
    .await?)
}

/// `contact` asks for access to `profile`, which must have named them. The categories and the delay
/// are the contact row's at this moment. `NotFound` when they are not a contact (the same for a profile
/// that does not exist), `Review` (409) when a request of theirs is already open.
pub async fn request(conn: &mut PgConnection, contact: Uuid, profile: Uuid, reason: &str) -> Result<Request> {
    let mut tx = conn.begin().await?;
    let row: Option<(Vec<String>, i32)> = sqlx::query_as(
        "select c.categories, c.delay_hours from break_glass_contact c join app_user u on u.id = c.contact_user_id \
         where c.profile_id = $1 and c.contact_user_id = $2 and u.disabled_at is null for update of c",
    )
    .bind(profile)
    .bind(contact)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((categories, delay)) = row else {
        return Err(Error::NotFound(
            "No emergency access was set up for you on this profile.".into(),
        ));
    };
    let open: bool = sqlx::query_scalar(
        "select exists (select 1 from break_glass_request where profile_id = $1 and contact_user_id = $2 \
         and status in ('requested', 'active'))",
    )
    .bind(profile)
    .bind(contact)
    .fetch_one(&mut *tx)
    .await?;
    if open {
        return Err(Error::Review(
            "A request of yours on this profile is already open.".into(),
        ));
    }
    let r: Request = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "insert into break_glass_request (profile_id, contact_user_id, categories, reason, activates_at) \
         values ($1, $2, $3, $4, now() + make_interval(hours => $5)) returning {REQUEST_COLS}"
    )))
    .bind(profile)
    .bind(contact)
    .bind(&categories)
    .bind(reason)
    .bind(delay)
    .fetch_one(&mut *tx)
    .await?;
    let mut e = event("user", "ui", "break_glass.request", Some(contact), profile, r.id);
    e.args_summary = Some(format!("delay_hours={delay};categories={}", categories.join(",")));
    write_audit(&mut tx, &e).await?;
    notify(&mut tx, profile, "break_glass.request", r.id, Some(contact)).await?;
    tx.commit().await?;
    Ok(r)
}

/// End one request of `profile`: `cancel` a requested one, `revoke` an active one. `by` is the person
/// who ended it. `Review` (409) when it is not in the state the step needs; `NotFound` when the
/// profile has no such request.
pub async fn end(conn: &mut PgConnection, profile: Uuid, id: Uuid, by: Uuid, step: End) -> Result<Request> {
    let mut tx = conn.begin().await?;
    let (from, to, action) = step.parts();
    let found: Option<String> =
        sqlx::query_scalar("select status from break_glass_request where id = $1 and profile_id = $2 for update")
            .bind(id)
            .bind(profile)
            .fetch_optional(&mut *tx)
            .await?;
    let status = found.ok_or_else(|| Error::NotFound("No such request.".into()))?;
    // an active request whose time ran out, not swept yet, has expired: it cannot be revoked
    let ran_out: bool =
        sqlx::query_scalar("select coalesce(expires_at <= now(), false) from break_glass_request where id = $1")
            .bind(id)
            .fetch_one(&mut *tx)
            .await?;
    if status != from || ran_out {
        return Err(Error::Review(format!(
            "This request is {status}; only a {from} request can be ended this way."
        )));
    }
    let r = finish(&mut tx, id, to, action, Some(by)).await?;
    tx.commit().await?;
    Ok(r)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum End {
    Cancel,
    Revoke,
}

impl End {
    fn parts(self) -> (&'static str, &'static str, &'static str) {
        match self {
            End::Cancel => ("requested", "cancelled", "break_glass.cancel"),
            End::Revoke => ("active", "revoked", "break_glass.revoke"),
        }
    }
}

/// Move one locked request to `to`, with its audit row and its outbox row.
async fn finish(conn: &mut PgConnection, id: Uuid, to: &str, action: &str, by: Option<Uuid>) -> Result<Request> {
    let r: Request = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "update break_glass_request set status = $2, ended_at = now(), ended_by = $3 where id = $1 \
         returning {REQUEST_COLS}"
    )))
    .bind(id)
    .bind(to)
    .bind(by)
    .fetch_one(&mut *conn)
    .await?;
    let kind = if by.is_some() { "user" } else { "system" };
    let via = if by.is_some() { "ui" } else { "job" };
    write_audit(&mut *conn, &event(kind, via, action, by, r.profile_id, r.id)).await?;
    notify(&mut *conn, r.profile_id, action, r.id, by).await?;
    Ok(r)
}

/// The contact row is gone (`remove_contact`) or the owner ends everything: cancel a requested request
/// and revoke an active one of this contact on this profile.
pub async fn end_open(conn: &mut PgConnection, profile: Uuid, contact: Uuid, by: Uuid) -> Result<Vec<Request>> {
    let open: Vec<(Uuid, String)> = sqlx::query_as(
        "select id, status from break_glass_request where profile_id = $1 and contact_user_id = $2 \
         and status in ('requested', 'active') for update",
    )
    .bind(profile)
    .bind(contact)
    .fetch_all(&mut *conn)
    .await?;
    let mut out = Vec::new();
    for (id, status) in open {
        let (_, to, action) = if status == "active" {
            End::Revoke.parts()
        } else {
            End::Cancel.parts()
        };
        out.push(finish(&mut *conn, id, to, action, Some(by)).await?);
    }
    Ok(out)
}

/// What `settle` moved.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Settled {
    pub activated: Vec<Uuid>,
    pub expired: Vec<Uuid>,
}

const DUE_ACTIVATE: &str =
    "status = 'requested' and owner_feed_at is not null and activates_at <= coalesce($1::timestamptz, now())";
const DUE_EXPIRE: &str = "status = 'active' and expires_at <= coalesce($1::timestamptz, now())";

/// Activate every request whose delay has passed and that an owner calendar feed has served, and
/// expire every open one whose time ran out, each with its audit row and its outbox row. `at` is the
/// clock (None: the database's); tests pass their own. The REST caller runs it before every request
/// of a person, and the sweep every minute.
pub async fn settle(conn: &mut PgConnection, at: Option<DateTime<Utc>>) -> Result<Settled> {
    let due: bool = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "select exists (select 1 from break_glass_request where ({DUE_ACTIVATE}) or ({DUE_EXPIRE}))"
    )))
    .bind(at)
    .fetch_one(&mut *conn)
    .await?;
    if !due {
        return Ok(Settled::default());
    }
    let mut tx = conn.begin().await?;
    let activated: Vec<(Uuid, Uuid)> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "update break_glass_request set status = 'active', activated_at = coalesce($1::timestamptz, now()), \
         expires_at = coalesce($1::timestamptz, now()) + make_interval(days => $2) \
         where {DUE_ACTIVATE} returning id, profile_id"
    )))
    .bind(at)
    .bind(OPEN_DAYS)
    .fetch_all(&mut *tx)
    .await?;
    for (id, profile) in &activated {
        write_audit(
            &mut tx,
            &event("system", "job", "break_glass.activate", None, *profile, *id),
        )
        .await?;
        notify(&mut tx, *profile, "break_glass.activate", *id, None).await?;
    }
    let expired: Vec<(Uuid, Uuid)> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "update break_glass_request set status = 'expired', ended_at = coalesce($1::timestamptz, now()) \
         where {DUE_EXPIRE} returning id, profile_id"
    )))
    .bind(at)
    .fetch_all(&mut *tx)
    .await?;
    for (id, profile) in &expired {
        write_audit(
            &mut tx,
            &event("system", "job", "break_glass.expire", None, *profile, *id),
        )
        .await?;
        notify(&mut tx, *profile, "break_glass.expire", *id, None).await?;
    }
    tx.commit().await?;
    Ok(Settled {
        activated: activated.into_iter().map(|r| r.0).collect(),
        expired: expired.into_iter().map(|r| r.0).collect(),
    })
}

// ------------------------------------------------------------ the calendar feed ----

/// A requested request as an owner calendar feed shows it: (id, requested_at, activates_at, contact
/// name).
pub async fn feed_requests(
    conn: &mut PgConnection,
    profile: Uuid,
) -> Result<Vec<(Uuid, DateTime<Utc>, DateTime<Utc>, String)>> {
    Ok(sqlx::query_as(
        "select r.id, r.requested_at, r.activates_at, u.display_name from break_glass_request r \
         join app_user u on u.id = r.contact_user_id where r.profile_id = $1 and r.status = 'requested' \
         order by r.requested_at, r.id",
    )
    .bind(profile)
    .fetch_all(conn)
    .await?)
}

/// An owner calendar feed served these requests: activation may follow once their delay has passed.
pub async fn mark_fed(conn: &mut PgConnection, ids: &[Uuid]) -> Result<()> {
    sqlx::query(
        "update break_glass_request set owner_feed_at = clock_timestamp() where id = any($1) \
         and status = 'requested' and owner_feed_at is null",
    )
    .bind(ids)
    .execute(conn)
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_contact_never_gets_sensitive_raw_genome_or_a_write() {
        for c in CATEGORIES {
            assert!(crate::access::is_scope(c), "{c}");
            assert!(
                !["read:sensitive", "read:genome_raw", "write:propose"].contains(&c),
                "{c}"
            );
        }
        assert!(DEFAULT_CATEGORIES.iter().all(|c| CATEGORIES.contains(c)));
    }

    #[tokio::test]
    async fn outside_a_recording_nothing_is_armed() {
        let u = Uuid::new_v4();
        assert!(!armed_for(u));
        let ((), rec) = recording(async move {
            assert!(!armed_for(u));
            record(Use {
                request_id: u,
                profile_id: u,
                contact: u,
            });
        })
        .await;
        // a use with no armed caller has nowhere to go: flush refuses rather than drop it
        assert!(flush(&rec).await.is_err());
    }
}
