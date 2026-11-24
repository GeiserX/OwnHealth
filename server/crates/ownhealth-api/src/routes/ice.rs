//! The profile owner's emergency card. The downloads carry the QR codes. The sharing list keeps
//! only the card's status and public access log. The owner can leave any record out, so a long
//! record still fits one wallet-size QR; sensitive records stay out unless picked.

use std::collections::BTreeSet;

use axum::Router;
use axum::extract::{Path, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::routing::get;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use chrono::{Days, NaiveTime};
use ownhealth_auth::sessions;
use ownhealth_core::access::{Permission, today_utc};
use ownhealth_core::audit::{AuditEvent, write_audit};
use ownhealth_core::langs::{LANGS, lang_or_default};
use ownhealth_core::{Error, Runtime};
use ownhealth_ops::{ice, shl};
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::PgConnection;
use uuid::Uuid;

use super::auth::{Body, Check, R, answer};
use super::shl::{DEFAULT_DAYS, MAX_DAYS, open_categories};
use crate::error::ApiError;
use crate::extract::Caller;

const MAX_CONTACTS: usize = 3;
const OWNER: &str = "Only the profile owner can make an emergency card.";
const CLOSED: &str = "The emergency card needs access to the profile summary and records.";
const CONTACT: &str = "Each emergency contact needs a name and a phone number.";
const PICKS: &str = "One or more selected records are no longer available for the emergency card.";
const SHARING_OFF: &str = "Online sharing is off on this instance.";
const LONG_URL: &str = "The public share address is too long for the online QR code.";
const TOO_MANY_CONTACTS: &str = "An emergency card accepts at most three contacts.";

pub fn phrases() -> Value {
    json!([
        ice::phrases(),
        OWNER,
        CLOSED,
        CONTACT,
        PICKS,
        SHARING_OFF,
        LONG_URL,
        TOO_MANY_CONTACTS
    ])
}

async fn owner(conn: &mut PgConnection, caller: &Caller, slug: &str) -> R<Permission> {
    let perm = caller.require(conn, slug, None, None).await?;
    if perm.role != "owner" || perm.break_glass || !perm.has_ordinary_grant {
        return Err(Error::access_denied("ice_owner", OWNER).into());
    }
    let open = open_categories(&perm);
    if !open.contains("read:summary") || !open.contains("read:records") {
        return Err(Error::access_denied("ice_closed", CLOSED).into());
    }
    Ok(perm)
}

fn event(caller: &Caller, perm: &Permission, action: &str) -> AuditEvent {
    let mut e = AuditEvent::new("user", "ui", action);
    e.actor_id = caller.0.user_id;
    e.profile_id = Some(perm.profile.id);
    e.resource_type = Some("ice".into());
    e
}

fn no_store(status: StatusCode, body: Value) -> axum::response::Response {
    let mut response = answer(status, body, &[]);
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

async fn options(State(rt): State<Runtime>, caller: Caller, Path(slug): Path<String>) -> R {
    let mut tx = rt.pool.begin().await?;
    let perm = owner(&mut tx, &caller, &slug).await?;
    let items = ice::items(&mut tx, perm.profile.id, perm.may_share_sensitive("read:records")).await?;
    let mut audit = event(&caller, &perm, "ice.options");
    audit.src_ids = Some(items.iter().map(|item| item.src_id.clone()).collect());
    write_audit(&mut tx, &audit).await?;
    tx.commit().await?;
    let items: Vec<_> = items
        .into_iter()
        .map(|item| {
            json!({"id": item.id, "kind": ice::KINDS[item.section as usize], "name": item.name, "sensitive": item.sensitive})
        })
        .collect();
    Ok(no_store(
        StatusCode::OK,
        json!({"items": items, "online": rt.settings.share_url.is_some()}),
    ))
}

#[derive(Deserialize)]
struct Contact {
    name: String,
    phone: String,
}

impl Contact {
    fn line(&self) -> R<String> {
        let name = ice::words(&self.name);
        let phone = self.phone.trim();
        if name.is_empty()
            || name.chars().count() > 50
            || self.name.chars().any(char::is_control)
            || phone.len() > 32
            || phone.bytes().filter(u8::is_ascii_digit).count() < 3
            || !phone.bytes().all(|b| b.is_ascii_digit() || b" +().-".contains(&b))
        {
            return Err(ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, "ice_contact", CONTACT).into());
        }
        Ok(format!("{name} {phone}"))
    }
}

#[derive(Deserialize)]
struct CardIn {
    lang: Option<String>,
    days: Option<i64>,
    #[serde(default)]
    online: bool,
    #[serde(default)]
    contacts: Vec<Contact>,
    #[serde(default)]
    sensitive_ids: BTreeSet<Uuid>,
    /// Records to keep off the card. An id that is no longer on the record is ignored.
    #[serde(default)]
    left_out: BTreeSet<Uuid>,
}

async fn issue(State(rt): State<Runtime>, caller: Caller, Path(slug): Path<String>, Body(input): Body<CardIn>) -> R {
    let lang = input.lang.unwrap_or_else(|| lang_or_default(&caller.0.lang).to_owned());
    let days = input.days.unwrap_or(DEFAULT_DAYS);
    Check::body()
        .literal("lang", Some(&lang), &LANGS)
        .range("days", days, json!(days), 1, MAX_DAYS)
        .done()?;
    if input.contacts.len() > MAX_CONTACTS {
        return Err(ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, "ice_contact", TOO_MANY_CONTACTS).into());
    }
    let contacts = input.contacts.iter().map(Contact::line).collect::<R<Vec<_>>>()?;
    let mut tx = rt.pool.begin().await?;
    let user = caller.0.user_id.ok_or_else(ApiError::unauthenticated)?;
    let first = owner(&mut tx, &caller, &slug).await?;
    // Serialize reissues for this profile. Keep its locks, owner's grant, and login fixed until
    // the new card commits. NO KEY UPDATE permits foreign-key checks while a reader logs access.
    let locked: Option<i32> = sqlx::query_scalar(
        "select 1 from profile p join profile_grant g on g.profile_id = p.id join app_user u on u.id = g.user_id
         where p.id = $1 and g.user_id = $2 and u.disabled_at is null for no key update of p for share of g, u",
    )
    .bind(first.profile.id)
    .bind(user)
    .fetch_optional(&mut *tx)
    .await?;
    if locked.is_none() {
        return Err(Error::access_denied("ice_owner", OWNER).into());
    }
    let perm = owner(&mut tx, &caller, &slug).await?;
    sessions::require_step_up(&mut tx, &caller.0, sessions::STEP_UP_SECONDS).await?;

    let expires = (today_utc() + Days::new(days as u64))
        .and_time(NaiveTime::MIN)
        .and_utc();
    let profile = perm.profile.id;
    let may_pick = perm.may_share_sensitive("read:records") && !input.sensitive_ids.is_empty();
    let items = ice::items(&mut tx, profile, may_pick).await?;
    let available: BTreeSet<Uuid> = items.iter().filter(|item| item.sensitive).map(|item| item.id).collect();
    if !input.sensitive_ids.is_subset(&available) {
        return Err(ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, "ice_picks", PICKS).into());
    }
    let shown: Vec<_> = items
        .iter()
        .filter(|item| !input.left_out.contains(&item.id))
        .filter(|item| !item.sensitive || input.sensitive_ids.contains(&item.id))
        .collect();
    let plain = ice::text(&lang, &shown, &contacts);

    let id = Uuid::new_v4();
    let segment = shl::random_segment();
    let online = if input.online {
        let share_url = rt
            .settings
            .share_url
            .as_deref()
            .ok_or_else(|| ApiError::new(StatusCode::CONFLICT, "ice_sharing_off", SHARING_OFF))?;
        let url = format!("{}/ice", shl::manifest_url(share_url, &segment));
        if url.len() > shl::URL_MAX {
            return Err(ApiError::new(StatusCode::CONFLICT, "ice_share_url", LONG_URL).into());
        }
        let dek = ice::dek(rt.blobs.keyring(), profile, true).await?;
        Some(ice::online(&dek, profile, id, &url, expires)?)
    } else {
        None
    };
    let (files, online) = tokio::task::spawn_blocking({
        let lang = lang.clone();
        move || {
            let files = ice::render(
                &lang,
                expires.date_naive(),
                &plain,
                online.as_ref().map(|o| o.qr.as_str()),
            )?;
            Ok::<_, Error>((files, online))
        }
    })
    .await
    .map_err(|e| Error::Other(e.into()))?
    .map_err(|e| match e {
        Error::Invalid(_) => ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, "ice_full", ice::TOO_FULL),
        e => e.into(),
    })?;
    // Reissue only after both downloads rendered. The previous card and key are retired in the
    // same transaction as the replacement.
    sqlx::query(
        "with ended as (
            update shl set revoked_at = clock_timestamp(), revoked_reason = 'replaced'
            where profile_id = $1 and flag in ('I', 'LU') and revoked_at is null and expires_at > now()
            returning id
         ) delete from shl_file f using ended e where f.shl_id = e.id",
    )
    .bind(profile)
    .execute(&mut *tx)
    .await?;

    let categories: Vec<String> = if online.is_some() {
        open_categories(&perm).into_iter().collect()
    } else {
        vec!["read:records".into(), "read:summary".into()]
    };
    sqlx::query(
        "insert into shl (id, profile_id, created_by, manifest_sha256, flag, categories, lang, expires_at, created_at)
         values ($1, $2, $3, $4, $5, $6, $7, $8, clock_timestamp())",
    )
    .bind(id)
    .bind(profile)
    .bind(user)
    .bind(shl::sha256(&segment))
    .bind(if online.is_some() { "LU" } else { "I" })
    .bind(categories)
    .bind(lang)
    .bind(expires)
    .execute(&mut *tx)
    .await?;
    if let Some(online) = online {
        sqlx::query("insert into shl_file (shl_id, content_type, ice_key) values ($1, $2, $3)")
            .bind(id)
            .bind(shl::CONTENT_TYPE)
            .bind(&online.sealed_key)
            .execute(&mut *tx)
            .await?;
    }
    let mut audit = event(&caller, &perm, "ice.issue");
    audit.via = "export".into();
    audit.resource_ids = Some(vec![id.to_string()]);
    audit.src_ids = Some(shown.iter().map(|item| item.src_id.clone()).collect());
    write_audit(&mut tx, &audit).await?;
    tx.commit().await?;
    Ok(no_store(
        StatusCode::CREATED,
        json!({
            "id": id, "expires_at": expires, "pdf": STANDARD.encode(files.pdf), "png": STANDARD.encode(files.png)
        }),
    ))
}

pub fn router() -> Router<Runtime> {
    Router::new().route("/profiles/{p}/ice", get(options).post(issue))
}
