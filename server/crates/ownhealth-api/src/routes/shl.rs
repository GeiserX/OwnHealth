//! SMART Health Links that the instance serves itself (security-privacy.md section 11, migration
//! 1270). The owner or a guardian shares a profile's summary; the recipient opens it with the link
//! and, when there is one, the passcode, through the public site block of deploy/Caddyfile, which
//! passes only the three public routes below.
//!
//! ```text
//! POST    /api/shl/{id}                      public: the manifest  {recipient, passcode?, embeddedLengthMax?}
//! OPTIONS /api/shl/{id}                      public: the CORS preflight of that POST
//! GET     /api/shl/{id}/file/{ticket}        public: the file (application/jose), at most 1 h after the manifest
//! GET     /api/profiles/{p}/shl              owner or guardian    the profile's links
//! POST    /api/profiles/{p}/shl              owner or guardian + step-up
//!                                            {categories?, days?, passcode?, label?, lang?} -> {link, shlink}
//! GET     /api/profiles/{p}/shl/{id}/access  owner or guardian    who opened it, newest first
//! DELETE  /api/profiles/{p}/shl/{id}         owner or guardian    revoke: the file is deleted
//! ```
//!
//! Spec: HL7 FHIR IG "SMART Health Cards and Links", Health Links Specification,
//! https://build.fhir.org/ig/HL7/smart-health-cards-and-links/links-specification.html. A comment
//! that quotes a section uses that page's heading.
//!
//! The content is the profile's IPS under the creator's categories minus the profile's locks (the
//! owner's own locks included), limited to `ownhealth_ops::shl::SHAREABLE`: never `read:sensitive`
//! or a genome scope. A viewer, a caregiver and a break-glass grant never share, and an access
//! token never reaches these routes (`Caller` refuses it). No MCP tool creates, lists or extends a
//! link (section 4.2). A link answers only while it is live, its creator may still share the
//! profile, and the creator's login is open. Each public answer writes one shl_access row (the
//! access log the owner reads) and one audit row (actor share, via share). A link answers
//! RATE_PER_HOUR times an hour; past that it says 429 with Retry-After.

use axum::Router;
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use chrono::{DateTime, Duration, Utc};
use ownhealth_auth::password::{check_password_async, hash_password_async};
use ownhealth_auth::sessions;
use ownhealth_core::Runtime;
use ownhealth_core::access::{Permission, Principal, Scopes, today_utc};
use ownhealth_core::audit::{AuditEvent, write_audit};
use ownhealth_ops::shl::{self as share, CONTENT_TYPE, LABEL_MAX, SHAREABLE, URL_MAX};
use ownhealth_rules::summary::LANGS;
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::PgConnection;
use uuid::Uuid;

use super::auth::{Body, Check, Meta, R, answer, ok};
use crate::error::ApiError;
use crate::extract::Caller;

/// "Construct a SMART Health Link Payload" leaves `exp` to the sharer; security-privacy.md section 11
/// sets 7 days by default.
pub const DEFAULT_DAYS: i64 = 7;
pub const MAX_DAYS: i64 = 365;
/// "SMART Health Link Manifest Request": the server SHALL enforce a total lifetime count of incorrect
/// passcodes. This is that count.
pub const ATTEMPTS: i32 = 5;
pub const PASSCODE_MIN: usize = 4;
pub const PASSCODE_MAX: usize = 64;
/// ".files.location links": the lifetime of a location SHALL NOT exceed one hour.
pub const LOCATION_SECONDS: i64 = 3600;
/// Answers per link per rolling hour ("Polling Manifest for Changes": 429 with Retry-After).
pub const RATE_PER_HOUR: i64 = 60;
const RECIPIENT_MAX: usize = 200;
const ROLES: [&str; 2] = ["owner", "guardian"];

fn msg(key: &str) -> &'static str {
    match key {
        "off" => "Sharing is off on this instance: OWNHEALTH_SHARE_URL is not set.",
        "long" => "OWNHEALTH_SHARE_URL is too long for a SMART Health Link (128 characters with the path).",
        "role" => "Only the profile's owner or a guardian shares it.",
        "category" => "This category is closed for you on this profile, so a link cannot carry it.",
        "recipient" => "The manifest request needs a recipient.",
        "passcode" => "The passcode is not right.",
        "rate" => "This link answers 60 times an hour.",
        _ => "No such link.",
    }
}

/// Every sentence these routes write, for the wording gate.
pub fn phrases() -> Value {
    let keys = [
        "off",
        "long",
        "role",
        "category",
        "recipient",
        "passcode",
        "rate",
        "gone",
    ];
    json!({"en": keys.iter().map(|k| (k.to_string(), json!(msg(k)))).collect::<serde_json::Map<_, _>>()})
}

// -------------------------------------------------------------- public routes ----

/// The one answer for a link that is unknown, revoked, expired, out of passcode attempts, or whose
/// creator may no longer share. "SMART Health Link Manifest Request": if the link is no longer
/// active, the server SHALL respond with a 404.
fn gone() -> ApiError {
    ApiError::new(StatusCode::NOT_FOUND, "unknown", msg("gone"))
}

/// A viewer app on another origin reads the answers (a browser, a QR reader).
fn cors(mut res: Response) -> Response {
    let h = res.headers_mut();
    h.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, HeaderValue::from_static("*"));
    h.insert(
        header::ACCESS_CONTROL_EXPOSE_HEADERS,
        HeaderValue::from_static("retry-after"),
    );
    res
}

async fn preflight() -> Response {
    let mut res = cors(StatusCode::NO_CONTENT.into_response());
    let h = res.headers_mut();
    h.insert(header::ACCESS_CONTROL_ALLOW_METHODS, HeaderValue::from_static("POST"));
    h.insert(
        header::ACCESS_CONTROL_ALLOW_HEADERS,
        HeaderValue::from_static("content-type"),
    );
    h.insert(header::ACCESS_CONTROL_MAX_AGE, HeaderValue::from_static("600"));
    res
}

/// A live link, locked to the end of the transaction.
struct Live {
    id: Uuid,
    profile_id: Uuid,
    created_by: Uuid,
    passcode_hash: Option<String>,
    attempts_left: Option<i32>,
    expires_at: DateTime<Utc>,
}

/// The live link of a manifest id, or None, with its row locked FOR UPDATE: two requests for one
/// link run one after the other, so the passcode count below holds under parallel attempts
/// ("SMART Health Link Manifest Request", design note: servers need to enforce a total lifetime
/// count of incorrect passcodes even in the face of parallel requests).
async fn live(conn: &mut PgConnection, id: &str) -> Result<Option<Live>, ApiError> {
    if !share::is_segment(id) {
        return Ok(None);
    }
    type Row = (Uuid, Uuid, Uuid, Option<String>, Option<i32>, DateTime<Utc>);
    let row: Option<Row> = sqlx::query_as(
        "select s.id, s.profile_id, s.created_by, s.passcode_hash, s.attempts_left, s.expires_at from shl s \
         join app_user u on u.id = s.created_by \
         join profile_grant g on g.user_id = s.created_by and g.profile_id = s.profile_id \
         where s.manifest_sha256 = $1 and s.revoked_at is null and s.expires_at > now() \
         and u.disabled_at is null and g.role = any($2) and not g.break_glass \
         and (g.expires_at is null or g.expires_at > now()) \
         for update of s",
    )
    .bind(share::sha256(id))
    .bind(&ROLES[..])
    .fetch_optional(&mut *conn)
    .await?;
    Ok(row.map(|r| Live {
        id: r.0,
        profile_id: r.1,
        created_by: r.2,
        passcode_hash: r.3,
        attempts_left: r.4,
        expires_at: r.5,
    }))
}

/// 429 once the link answered RATE_PER_HOUR times in the last hour.
async fn rate_limited(conn: &mut PgConnection, link: Uuid) -> Result<Option<Response>, ApiError> {
    let recent: i64 =
        sqlx::query_scalar("select count(*) from shl_access where shl_id = $1 and at > now() - interval '1 hour'")
            .bind(link)
            .fetch_one(&mut *conn)
            .await?;
    if recent < RATE_PER_HOUR {
        return Ok(None);
    }
    let mut res = ApiError::new(StatusCode::TOO_MANY_REQUESTS, "rate_limited", msg("rate")).into_response();
    res.headers_mut()
        .insert(header::RETRY_AFTER, HeaderValue::from_static("600"));
    Ok(Some(res))
}

/// One shl_access row and one audit row. Free text goes to the audit log only as a hash.
#[allow(clippy::too_many_arguments)]
async fn log_access(
    conn: &mut PgConnection,
    link: &Live,
    kind: &str,
    recipient: &str,
    meta: &Meta,
    ticket: Option<(&str, DateTime<Utc>)>,
    bytes_out: usize,
) -> Result<(), ApiError> {
    sqlx::query(
        "insert into shl_access (shl_id, kind, recipient, ip, user_agent, ticket_sha256, ticket_expires_at) \
         values ($1, $2, $3, $4::inet, $5, $6, $7)",
    )
    .bind(link.id)
    .bind(kind)
    .bind(recipient)
    .bind(meta.ip.as_deref())
    .bind(meta.user_agent.as_deref())
    .bind(ticket.map(|(t, _)| share::sha256(t)))
    .bind(ticket.map(|(_, exp)| exp))
    .execute(&mut *conn)
    .await?;
    let mut e = AuditEvent::new("share", "share", &format!("shl.{kind}"));
    e.actor_id = Some(link.created_by);
    e.ip = meta.ip.clone();
    e.user_agent = meta.user_agent.clone();
    e.tool = Some("shl".into());
    e.profile_id = Some(link.profile_id);
    e.resource_type = Some("shl".into());
    e.resource_ids = Some(vec![link.id.to_string()]);
    e.args_summary = Some(format!("recipient_sha256={}", &hex(&share::sha256(recipient))[..16]));
    e.result_kind = Some(if kind == "passcode_wrong" { "denied" } else { "ok" }.into());
    e.bytes_out = Some(i32::try_from(bytes_out).unwrap_or(i32::MAX));
    write_audit(conn, &e).await?;
    Ok(())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// The recipient as the access log keeps it: trimmed, without control characters, at most 200
/// characters. "SMART Health Link Manifest Request": recipient is required (1..1).
fn recipient(v: &Value) -> Option<String> {
    let clean: String = v.as_str()?.chars().filter(|c| !c.is_control()).collect();
    let clean = clean.trim();
    (!clean.is_empty()).then(|| clean.chars().take(RECIPIENT_MAX).collect())
}

async fn manifest(State(rt): State<Runtime>, meta: Meta, Path(id): Path<String>, body: Bytes) -> Response {
    cors(match serve_manifest(&rt, &id, &meta, &body).await {
        Ok(r) => r,
        Err(e) => e.into_response(),
    })
}

async fn serve_manifest(rt: &Runtime, id: &str, meta: &Meta, body: &[u8]) -> Result<Response, ApiError> {
    let input: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
    let Some(who) = recipient(&input["recipient"]) else {
        return Err(ApiError::new(StatusCode::BAD_REQUEST, "bad_request", msg("recipient")));
    };
    let share_url = rt.settings.share_url.clone().ok_or_else(gone)?;
    let mut tx = rt.pool.begin().await?;
    let link = live(&mut tx, id).await?.ok_or_else(gone)?;
    if let Some(res) = rate_limited(&mut tx, link.id).await? {
        return Ok(res);
    }
    if let Some(hash) = &link.passcode_hash {
        let left = link.attempts_left.unwrap_or(0);
        // the link is disabled once the lifetime count is spent: not active, so 404
        if left <= 0 {
            return Err(gone());
        }
        let given = input["passcode"].as_str().filter(|p| !p.is_empty());
        let right = match given {
            Some(p) => check_password_async(p, Some(hash.clone())).await?,
            None => false,
        };
        if !right {
            // "SMART Health Link Manifest Request": an invalid passcode gets 401 with
            // remainingAttempts. A request with no passcode guessed nothing, so it costs none.
            let remaining = if given.is_some() {
                let n: i32 = sqlx::query_scalar(
                    "update shl set attempts_left = attempts_left - 1 where id = $1 returning attempts_left",
                )
                .bind(link.id)
                .fetch_one(&mut *tx)
                .await?;
                log_access(&mut tx, &link, "passcode_wrong", &who, meta, None, 0).await?;
                n
            } else {
                left
            };
            tx.commit().await?;
            let mut res = (
                StatusCode::UNAUTHORIZED,
                axum::Json(json!({"remainingAttempts": remaining})),
            )
                .into_response();
            res.headers_mut()
                .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
            return Ok(res);
        }
    }
    // ".files.location links": dereferenced without more authentication, and short-lived: at most
    // one hour, and never past the link's own expiry
    let ticket = share::random_segment();
    let until = (Utc::now() + Duration::seconds(LOCATION_SECONDS)).min(link.expires_at);
    let last: Option<DateTime<Utc>> = sqlx::query_scalar("select max(created_at) from shl_file where shl_id = $1")
        .bind(link.id)
        .fetch_one(&mut *tx)
        .await?;
    let files: Vec<Value> = last
        .into_iter()
        .map(|at| {
            json!({
                "contentType": CONTENT_TYPE,
                "location": share::file_url(&share_url, id, &ticket),
                "lastUpdated": at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            })
        })
        .collect();
    // "SMART Health Link Manifest File": status finalized, as the file never changes (no L flag)
    let out = json!({"status": "finalized", "files": files});
    let text = out.to_string();
    log_access(
        &mut tx,
        &link,
        "manifest",
        &who,
        meta,
        Some((&ticket, until)),
        text.len(),
    )
    .await?;
    tx.commit().await?;
    Ok(([(header::CONTENT_TYPE, "application/json")], text).into_response())
}

async fn file(State(rt): State<Runtime>, meta: Meta, Path((id, ticket)): Path<(String, String)>) -> Response {
    cors(match serve_file(&rt, &id, &ticket, &meta).await {
        Ok(r) => r,
        Err(e) => e.into_response(),
    })
}

async fn serve_file(rt: &Runtime, id: &str, ticket: &str, meta: &Meta) -> Result<Response, ApiError> {
    if !share::is_segment(ticket) {
        return Err(gone());
    }
    let mut tx = rt.pool.begin().await?;
    let link = live(&mut tx, id).await?.ok_or_else(gone)?;
    let who: Option<String> = sqlx::query_scalar(
        "select recipient from shl_access where shl_id = $1 and ticket_sha256 = $2 and ticket_expires_at > now()",
    )
    .bind(link.id)
    .bind(share::sha256(ticket))
    .fetch_optional(&mut *tx)
    .await?;
    let who = who.ok_or_else(gone)?;
    if let Some(res) = rate_limited(&mut tx, link.id).await? {
        return Ok(res);
    }
    let jwe: Option<String> =
        sqlx::query_scalar("select jwe from shl_file where shl_id = $1 order by created_at desc limit 1")
            .bind(link.id)
            .fetch_optional(&mut *tx)
            .await?;
    let jwe = jwe.ok_or_else(gone)?;
    log_access(&mut tx, &link, "file", &who, meta, None, jwe.len()).await?;
    tx.commit().await?;
    // ".files.location links": content-type application/jose, the body a compact JWE
    Ok(([(header::CONTENT_TYPE, "application/jose")], jwe).into_response())
}

/// A HEAD would take one of the hour's answers and write a log row, and deliver no file.
async fn no_head() -> Response {
    (StatusCode::METHOD_NOT_ALLOWED, [(header::ALLOW, "GET")]).into_response()
}

// ------------------------------------------------------------------- managing ----

/// The profile as someone who may share it: owner or guardian, never break-glass. A viewer and a
/// caregiver are refused the same way.
async fn sharer(conn: &mut PgConnection, caller: &Caller, p: &str) -> Result<Permission, ApiError> {
    let perm = caller.require(conn, p, Some("read:summary"), None).await?;
    let grant: Option<bool> = sqlx::query_scalar(
        "select break_glass from profile_grant where user_id = $1 and profile_id = $2 and role = any($3)",
    )
    .bind(caller.0.user_id)
    .bind(perm.profile.id)
    .bind(&ROLES[..])
    .fetch_optional(&mut *conn)
    .await?;
    if grant != Some(false) {
        return Err(ownhealth_core::Error::access_denied("forbidden", msg("role")).into());
    }
    Ok(perm)
}

/// What a link of this creator may carry: their categories on the profile minus the profile's
/// locks (an owner's own locks too: a lock means "never out of the house"), within SHAREABLE.
fn open_categories(perm: &Permission) -> Scopes {
    let locks = perm.profile.locks();
    perm.scopes
        .iter()
        .filter(|s| SHAREABLE.contains(&s.as_str()) && !locks.contains(*s))
        .cloned()
        .collect()
}

#[derive(Deserialize)]
struct CreateIn {
    categories: Option<Vec<String>>,
    days: Option<i64>,
    passcode: Option<String>,
    label: Option<String>,
    lang: Option<String>,
}

type Listed = (
    Uuid,
    Option<String>,
    String,
    Option<i32>,
    Vec<String>,
    String,
    DateTime<Utc>,
    DateTime<Utc>,
    Option<DateTime<Utc>>,
    i64,
    Option<DateTime<Utc>>,
);

const COLS: &str = "s.id, s.label, s.flag, s.attempts_left, s.categories, s.lang, s.created_at, s.expires_at, \
                    s.revoked_at, (select count(*) from shl_access a where a.shl_id = s.id and a.kind = 'manifest'), \
                    (select max(a.at) from shl_access a where a.shl_id = s.id)";

fn link_out(r: &Listed) -> Value {
    json!({
        "id": r.0, "label": r.1, "passcode": r.2 == "P", "attempts_left": r.3, "categories": r.4,
        "lang": r.5, "created_at": r.6, "expires_at": r.7, "revoked_at": r.8,
        "active": r.8.is_none() && r.7 > Utc::now() && r.3 != Some(0),
        "opened": r.9, "last_access_at": r.10,
    })
}

async fn audit(
    conn: &mut PgConnection,
    p: &Principal,
    perm: &Permission,
    action: &str,
    id: Uuid,
    args: String,
) -> R<()> {
    let mut e = AuditEvent::new("user", "ui", action);
    e.actor_id = p.user_id;
    e.tool = Some("shl".into());
    e.profile_id = Some(perm.profile.id);
    e.resource_type = Some("shl".into());
    e.resource_ids = Some(vec![id.to_string()]);
    e.args_summary = Some(args);
    write_audit(conn, &e).await?;
    Ok(())
}

async fn list(State(rt): State<Runtime>, caller: Caller, Path(p): Path<String>) -> R {
    let mut conn = rt.pool.acquire().await?;
    let perm = sharer(&mut conn, &caller, &p).await?;
    let rows: Vec<Listed> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "select {COLS} from shl s where s.profile_id = $1 order by s.created_at desc, s.id"
    )))
    .bind(perm.profile.id)
    .fetch_all(&mut *conn)
    .await?;
    ok(json!({"items": rows.iter().map(link_out).collect::<Vec<_>>()}))
}

/// The key and the manifest id are in this answer and nowhere else, ever.
async fn create(State(rt): State<Runtime>, caller: Caller, Path(p): Path<String>, Body(b): Body<CreateIn>) -> R {
    let lang = b.lang.clone().unwrap_or_else(|| caller.0.lang.clone());
    let days = b.days.unwrap_or(DEFAULT_DAYS);
    let mut check = Check::body();
    check.literal("lang", Some(&lang), &LANGS);
    check.range("days", days, json!(days), 1, MAX_DAYS);
    check.len("label", b.label.as_deref(), 1, LABEL_MAX);
    // the passcode's own length rule; the answer never echoes the passcode back
    if let Some(n) = b.passcode.as_deref().map(|pc| pc.chars().count())
        && !(PASSCODE_MIN..=PASSCODE_MAX).contains(&n)
    {
        let rule = format!("The passcode has {PASSCODE_MIN} to {PASSCODE_MAX} characters.");
        check.push("string_length", "passcode", &rule, Value::Null);
    }
    for c in b.categories.iter().flatten() {
        check.literal("categories", Some(c), &SHAREABLE);
    }
    check.done()?;
    let share_url = rt
        .settings
        .share_url
        .clone()
        .ok_or_else(|| ApiError::new(StatusCode::CONFLICT, "sharing_off", msg("off")))?;
    let id = share::random_segment();
    let url = share::manifest_url(&share_url, &id);
    if url.chars().count() > URL_MAX {
        return Err(ApiError::new(StatusCode::CONFLICT, "share_url_too_long", msg("long")).into());
    }
    let mut tx = rt.pool.begin().await?;
    let perm = sharer(&mut tx, &caller, &p).await?;
    sessions::require_step_up(&mut tx, &caller.0, sessions::STEP_UP_SECONDS).await?;
    let user = caller.0.user_id.ok_or_else(ApiError::unauthenticated)?;
    let open = open_categories(&perm);
    let mut scopes: Scopes = match &b.categories {
        Some(c) => c.iter().cloned().collect(),
        None => open.clone(),
    };
    // a summary is what a link carries: read:summary comes with every link
    scopes.insert("read:summary".into());
    if !scopes.is_subset(&open) {
        return Err(ownhealth_core::Error::access_denied("forbidden", msg("category")).into());
    }
    let sealed = share::seal(&mut tx, perm.profile.id, &scopes, &lang, today_utc()).await?;
    let passcode_hash = match b.passcode.as_deref() {
        Some(pc) => Some(hash_password_async(pc).await?),
        None => None,
    };
    let expires_at = Utc::now() + Duration::days(days);
    let flag = if passcode_hash.is_some() { "P" } else { "" };
    let categories: Vec<String> = scopes.iter().cloned().collect();
    let link_id: Uuid = sqlx::query_scalar(
        "insert into shl (profile_id, created_by, manifest_sha256, label, flag, passcode_hash, attempts_left, \
         categories, lang, expires_at) values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10) returning id",
    )
    .bind(perm.profile.id)
    .bind(user)
    .bind(share::sha256(&id))
    .bind(b.label.as_deref())
    .bind(flag)
    .bind(passcode_hash.as_deref())
    .bind(passcode_hash.as_ref().map(|_| ATTEMPTS))
    .bind(&categories)
    .bind(&lang)
    .bind(expires_at)
    .fetch_one(&mut *tx)
    .await?;
    sqlx::query("insert into shl_file (shl_id, content_type, jwe) values ($1, $2, $3)")
        .bind(link_id)
        .bind(CONTENT_TYPE)
        .bind(&sealed.jwe)
        .execute(&mut *tx)
        .await?;
    // the audit row names every fact that can leave through the link, as the IPS export's does
    let mut e = AuditEvent::new("user", "ui", "shl.create");
    e.actor_id = caller.0.user_id;
    e.tool = Some("shl".into());
    e.profile_id = Some(perm.profile.id);
    e.resource_type = Some("shl".into());
    e.resource_ids = Some(vec![link_id.to_string()]);
    e.src_ids = Some(sealed.ips.srcs.clone());
    e.rows_out = Some(i32::try_from(sealed.ips.srcs.len()).unwrap_or(i32::MAX));
    e.withheld = Some(json!({"by_scope": sealed.ips.withheld}));
    e.args_summary = Some(format!(
        "categories={};days={days};flag={flag};lang={lang}",
        categories.join(",")
    ));
    write_audit(&mut tx, &e).await?;
    let row: Listed = sqlx::query_as(sqlx::AssertSqlSafe(format!("select {COLS} from shl s where s.id = $1")))
        .bind(link_id)
        .fetch_one(&mut *tx)
        .await?;
    tx.commit().await?;
    let link = share::shlink(
        &url,
        &sealed.key,
        expires_at.timestamp(),
        flag == "P",
        b.label.as_deref(),
    );
    Ok(answer(
        StatusCode::CREATED,
        json!({"link": link_out(&row), "shlink": link}),
        &[],
    ))
}

/// One link of the profile, locked, while it is not revoked.
async fn one(conn: &mut PgConnection, perm: &Permission, id: &str) -> R<Uuid> {
    let id = Uuid::parse_str(id).map_err(|_| ApiError::not_found())?;
    let found: Option<Uuid> =
        sqlx::query_scalar("select id from shl where id = $1 and profile_id = $2 and revoked_at is null for update")
            .bind(id)
            .bind(perm.profile.id)
            .fetch_optional(conn)
            .await?;
    found.ok_or_else(|| gone().into())
}

async fn access(State(rt): State<Runtime>, caller: Caller, Path((p, id)): Path<(String, String)>) -> R {
    let mut conn = rt.pool.acquire().await?;
    let perm = sharer(&mut conn, &caller, &p).await?;
    let id = Uuid::parse_str(&id).map_err(|_| ApiError::not_found())?;
    type Row = (DateTime<Utc>, String, String, Option<String>, Option<String>);
    let rows: Vec<Row> = sqlx::query_as(
        "select a.at, a.kind, a.recipient, host(a.ip), a.user_agent from shl_access a join shl s on s.id = a.shl_id \
         where s.id = $1 and s.profile_id = $2 order by a.at desc, a.id desc",
    )
    .bind(id)
    .bind(perm.profile.id)
    .fetch_all(&mut *conn)
    .await?;
    let items: Vec<Value> = rows
        .iter()
        .map(|r| json!({"at": r.0, "kind": r.1, "recipient": r.2, "ip": r.3, "user_agent": r.4}))
        .collect();
    ok(json!({"items": items}))
}

/// Revoking only takes access away, so it needs no step-up. security-privacy.md section 11:
/// revocation deletes the object.
async fn revoke(State(rt): State<Runtime>, caller: Caller, Path((p, id)): Path<(String, String)>) -> R {
    let mut tx = rt.pool.begin().await?;
    let perm = sharer(&mut tx, &caller, &p).await?;
    let id = one(&mut tx, &perm, &id).await?;
    sqlx::query("update shl set revoked_at = now() where id = $1")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("delete from shl_file where shl_id = $1")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    audit(&mut tx, &caller.0, &perm, "shl.revoke", id, String::new()).await?;
    tx.commit().await?;
    ok(json!({"id": id, "revoked": true}))
}

pub fn router() -> Router<Runtime> {
    Router::new()
        .route("/shl/{id}", post(manifest).options(preflight))
        .route("/shl/{id}/file/{ticket}", get(file).head(no_head))
        .route("/profiles/{p}/shl", get(list).post(create))
        .route("/profiles/{p}/shl/{id}", delete(revoke))
        .route("/profiles/{p}/shl/{id}/access", get(access))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_recipient_is_trimmed_text_without_control_characters_and_never_empty() {
        assert_eq!(recipient(&json!("  Dr. Ortega\u{7}\n ")).as_deref(), Some("Dr. Ortega"));
        assert_eq!(recipient(&json!(" \n ")), None);
        assert_eq!(recipient(&json!(7)), None);
        assert_eq!(recipient(&json!("x".repeat(500))).map(|r| r.len()), Some(RECIPIENT_MAX));
    }

    #[test]
    fn hex_is_lowercase_two_digits_per_byte() {
        assert_eq!(hex(&[0, 15, 255]), "000fff");
    }
}
