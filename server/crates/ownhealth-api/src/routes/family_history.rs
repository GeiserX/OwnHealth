//! v0.3 Insight: family history and the pedigree (`core::family`, migration 1110).
//!
//! - `GET /profiles/{p}/family`: the current relatives and the catalog (relationship codes, condition
//!   kinds) the add/edit form offers.
//! - `POST /profiles/{p}/family`, `PUT /profiles/{p}/family/{id}` (a new version),
//!   `DELETE /profiles/{p}/family/{id}?reason=` (retract).
//! - `GET /profiles/{p}/pedigree`: the drawing data for this profile's own rows, placed by
//!   `relationships.yaml`.
//!
//! Reading needs `read:records`; a row with a sensitivity tag is left out without `read:sensitive`.
//! A relative linked to their own profile shows that profile's name only when the caller can read
//! it (`read:records` there); otherwise the row comes back masked: no name, no years, no conditions.
//! Writing needs the role action `write`. A masked row can only be removed, or unlinked by a new
//! version that holds what the caller sends. Nothing here enters the hashed state body.

use axum::extract::rejection::JsonRejection;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::get;
use axum::{Json, Router};
use chrono::Datelike;
use ownhealth_core::Runtime;
use ownhealth_core::access::{Permission, Principal, accessible_profiles, effective_permission};
use ownhealth_core::audit::{AuditEvent, write_audit};
use ownhealth_core::family::{self, Catalog, LinkChange, Member, MemberIn};
use ownhealth_core::provenance::{SrcIn, SrcRef};
use serde_json::{Map, Value, json};
use sqlx::PgConnection;
use uuid::Uuid;

use crate::error::ApiError;
use crate::extract::Caller;

pub fn router() -> Router<Runtime> {
    Router::new()
        .route("/profiles/{p}/family", get(list).post(add))
        .route("/profiles/{p}/family/{id}", axum::routing::put(update).delete(retract))
        .route("/profiles/{p}/pedigree", get(pedigree))
}

type Answer = Result<(StatusCode, Json<Value>), ApiError>;

fn bad_input(message: impl Into<String>) -> ApiError {
    ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, "bad_input", message)
}

fn unknown() -> ApiError {
    ApiError::new(StatusCode::NOT_FOUND, "unknown", "No such relative.")
}

fn body_object(body: Result<Json<Value>, JsonRejection>) -> Result<Map<String, Value>, ApiError> {
    match body {
        Ok(Json(Value::Object(m))) => Ok(m),
        Ok(_) => Err(bad_input("The body must be a JSON object.")),
        Err(e) => Err(bad_input(format!("JSON decode error: {}", e.body_text()))),
    }
}

async fn audit(
    conn: &mut PgConnection,
    principal: &Principal,
    perm: &Permission,
    action: &str,
    ids: &[String],
) -> Result<(), ApiError> {
    let via = if principal.kind == ownhealth_core::access::PrincipalKind::Token {
        "api"
    } else {
        "ui"
    };
    let mut ev = AuditEvent::new(principal.actor_kind(), via, action);
    ev.actor_id = principal.user_id;
    ev.token_id = principal.token_id;
    ev.tool = Some("family".into());
    ev.profile_id = Some(perm.profile.id);
    ev.resource_type = Some(family::TABLE.into());
    ev.resource_ids = Some(ids.to_vec());
    ev.rows_out = Some(i32::try_from(ids.len()).unwrap_or(i32::MAX));
    write_audit(conn, &ev).await?;
    Ok(())
}

/// The linked profile as the caller may see it: `Some((slug, name))` when readable.
async fn readable(
    conn: &mut PgConnection,
    principal: &Principal,
    profile_id: Uuid,
) -> Result<Option<(String, String)>, ApiError> {
    Ok(match effective_permission(conn, principal, profile_id).await {
        Ok(perm) if perm.has("read:records") => Some((perm.profile.slug, perm.profile.display_name)),
        Ok(_) | Err(ownhealth_core::Error::AccessDenied { .. }) => None,
        Err(e) => return Err(e.into()),
    })
}

/// One relative as the caller may see it.
struct Shown {
    member: Member,
    /// `Some` when linked and readable; a linked row that is not readable is masked.
    linked: Option<(String, String)>,
}

impl Shown {
    fn masked(&self) -> bool {
        self.member.linked_profile_id.is_some() && self.linked.is_none()
    }

    fn name(&self) -> Value {
        if self.masked() {
            return Value::Null;
        }
        match (&self.member.display_name, &self.linked) {
            (Some(n), _) => json!(n),
            (None, Some((_, n))) => json!(n),
            _ => Value::Null,
        }
    }

    fn out(&self) -> Value {
        let m = &self.member;
        let masked = self.masked();
        let hide = |v: Value| if masked { Value::Null } else { v };
        json!({
            "id": m.id,
            "relationship": m.relationship,
            "display_name": hide(json!(m.display_name)),
            "name": self.name(),
            "sex_at_birth": m.sex_at_birth,
            "born_year": hide(json!(m.born_year)),
            "deceased": m.deceased,
            "death_age": hide(json!(m.death_age)),
            "conditions": if masked { json!([]) } else { json!(m.conditions.0) },
            "sensitivity": m.sensitivity,
            "linked": m.linked_profile_id.is_some(),
            "linked_profile": self.linked.as_ref().map(|(slug, name)| json!({"slug": slug, "display_name": name})),
            "masked": masked,
            "src": m.src_id,
            "supersedes": m.supersedes,
            "recorded_at": m.recorded_at,
        })
    }
}

async fn shown(conn: &mut PgConnection, principal: &Principal, perm: &Permission) -> Result<Vec<Shown>, ApiError> {
    let rows = family::list(&mut *conn, perm.profile.id, perm.has("read:sensitive")).await?;
    let mut out = Vec::with_capacity(rows.len());
    for member in rows {
        let linked = match member.linked_profile_id {
            Some(id) => readable(&mut *conn, principal, id).await?,
            None => None,
        };
        out.push(Shown { member, linked });
    }
    Ok(out)
}

fn catalog_out(cat: &Catalog) -> Value {
    json!({
        "version": cat.version,
        "relationships": cat.relationships.iter().map(|r| json!({
            "code": r.code, "en": r.en, "es": r.es, "generation": r.generation, "side": r.side, "sex": r.sex,
        })).collect::<Vec<_>>(),
        "conditions": cat.conditions.iter().map(|c| json!({
            "kind": c.kind, "en": c.en, "es": c.es, "short_en": c.short_en, "short_es": c.short_es, "group": c.group,
        })).collect::<Vec<_>>(),
    })
}

/// `linked_profile`: a slug or id of a profile the caller can read, in the same household.
async fn link_target(conn: &mut PgConnection, principal: &Principal, v: &Value) -> Result<Option<Uuid>, ApiError> {
    let refused = || bad_input("linked_profile: not a profile of this household you can read.");
    let s = match v {
        Value::Null => return Ok(None),
        Value::String(s) if !s.trim().is_empty() => s.trim(),
        _ => return Err(refused()),
    };
    let id = match Uuid::parse_str(s) {
        Ok(id) => id,
        Err(_) => accessible_profiles(&mut *conn, principal)
            .await?
            .into_iter()
            .find(|p| p.profile.slug == s)
            .map(|p| p.profile.id)
            .ok_or_else(refused)?,
    };
    readable(conn, principal, id).await?.ok_or_else(refused)?;
    Ok(Some(id))
}

fn member_in(profile_id: Uuid, body: &Map<String, Value>) -> Result<MemberIn, ApiError> {
    let mut data: Map<String, Value> = body
        .iter()
        .filter(|(k, _)| !matches!(k.as_str(), "linked_profile" | "reason" | "profile_id" | "fhir"))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    data.insert("profile_id".into(), json!(profile_id));
    let m: MemberIn = serde_json::from_value(Value::Object(data)).map_err(|e| bad_input(e.to_string()))?;
    m.normalized().map_err(|e| bad_input(e.to_string()))
}

/// A profile appears at most once among one profile's current relatives.
async fn linked_once(
    conn: &mut PgConnection,
    profile_id: Uuid,
    link: Uuid,
    except: Option<Uuid>,
) -> Result<(), ApiError> {
    let taken: Option<i32> = sqlx::query_scalar(
        "select 1 from family_member m join family_member_link l on l.family_member_id = m.id \
         where m.profile_id = $1 and m.status = 'current' and l.linked_profile_id = $2 \
         and m.id is distinct from $3 limit 1",
    )
    .bind(profile_id)
    .bind(link)
    .bind(except)
    .fetch_optional(conn)
    .await?;
    if taken.is_some() {
        return Err(bad_input(
            "linked_profile: this profile is already linked to another relative.",
        ));
    }
    Ok(())
}

fn src(principal: &Principal, profile_id: Uuid) -> SrcRef {
    SrcRef::New(SrcIn {
        profile_id: Some(profile_id),
        label: Some("manual".into()), // a code: the UI says "entered by hand" in its language
        ..SrcIn::manual(principal.user_id)
    })
}

/// A current relative of this profile that the caller may change. A row linked to a profile the
/// caller cannot read is refused unless `masked_ok`: removing it, or unlinking it with a new version
/// that holds only what the caller sends, shows nothing of the linked profile.
async fn editable(
    conn: &mut PgConnection,
    principal: &Principal,
    perm: &Permission,
    id: &str,
    masked_ok: bool,
) -> Result<Member, ApiError> {
    let id = Uuid::parse_str(id).map_err(|_| unknown())?;
    let m = family::get(&mut *conn, id)
        .await?
        .filter(|m| m.profile_id == perm.profile.id && m.status == "current");
    let m = m.ok_or_else(unknown)?;
    if !m.sensitivity.is_empty() && !perm.has("read:sensitive") {
        return Err(unknown());
    }
    if let Some(l) = m.linked_profile_id
        && !masked_ok
        && readable(&mut *conn, principal, l).await?.is_none()
    {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "forbidden",
            "This relative is linked to a profile you cannot read.",
        ));
    }
    Ok(m)
}

async fn one(conn: &mut PgConnection, principal: &Principal, id: Uuid) -> Result<Value, ApiError> {
    let member = family::get(&mut *conn, id).await?.ok_or_else(unknown)?;
    let linked = match member.linked_profile_id {
        Some(l) => readable(&mut *conn, principal, l).await?,
        None => None,
    };
    Ok(Shown { member, linked }.out())
}

// ------------------------------------------------------------------ handlers ----

async fn list(State(rt): State<Runtime>, caller: Caller, Path(p): Path<String>) -> Answer {
    let mut tx = rt.pool.begin().await?;
    let perm = caller.require(&mut tx, &p, Some("read:records"), None).await?;
    let rows = shown(&mut tx, &caller.0, &perm).await?;
    let ids: Vec<String> = rows.iter().map(|r| r.member.id.to_string()).collect();
    audit(&mut tx, &caller.0, &perm, "read", &ids).await?;
    tx.commit().await?;
    let items: Vec<Value> = rows.iter().map(Shown::out).collect();
    Ok((
        StatusCode::OK,
        Json(json!({"items": items, "catalog": catalog_out(family::catalog()?)})),
    ))
}

async fn add(
    State(rt): State<Runtime>,
    caller: Caller,
    Path(p): Path<String>,
    body: Result<Json<Value>, JsonRejection>,
) -> Answer {
    let body = body_object(body)?;
    let mut tx = rt.pool.begin().await?;
    let perm = caller.require(&mut tx, &p, None, Some("write")).await?;
    let member = member_in(perm.profile.id, &body)?;
    let link = match body.get("linked_profile") {
        Some(v) => link_target(&mut tx, &caller.0, v).await?,
        None => None,
    };
    if let Some(l) = link {
        linked_once(&mut tx, perm.profile.id, l, None).await?;
    }
    let done = family::add(&mut tx, &member, &src(&caller.0, perm.profile.id), "person", None, link).await?;
    audit(&mut tx, &caller.0, &perm, "write", &[done.id.to_string()]).await?;
    let out = one(&mut tx, &caller.0, done.id).await?;
    tx.commit().await?;
    Ok((StatusCode::CREATED, Json(out)))
}

/// Body: the full new version. `linked_profile` absent keeps the link, null removes it.
async fn update(
    State(rt): State<Runtime>,
    caller: Caller,
    Path((p, id)): Path<(String, String)>,
    body: Result<Json<Value>, JsonRejection>,
) -> Answer {
    let body = body_object(body)?;
    let mut tx = rt.pool.begin().await?;
    let perm = caller.require(&mut tx, &p, None, Some("write")).await?;
    let unlink = body.get("linked_profile") == Some(&Value::Null);
    let old = editable(&mut tx, &caller.0, &perm, &id, unlink).await?;
    let member = member_in(perm.profile.id, &body)?;
    let link = match body.get("linked_profile") {
        None => LinkChange::Keep,
        Some(v) => match link_target(&mut tx, &caller.0, v).await? {
            Some(l) => {
                linked_once(&mut tx, perm.profile.id, l, Some(old.id)).await?;
                LinkChange::To(l)
            }
            None => LinkChange::Remove,
        },
    };
    let reason = body
        .get("reason")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|r| !r.is_empty())
        .unwrap_or("corrected")
        .to_string();
    let done = family::supersede(
        &mut tx,
        old.id,
        &member,
        &src(&caller.0, perm.profile.id),
        "person",
        &reason,
        link,
    )
    .await?;
    audit(
        &mut tx,
        &caller.0,
        &perm,
        "correct",
        &[old.id.to_string(), done.id.to_string()],
    )
    .await?;
    let out = one(&mut tx, &caller.0, done.id).await?;
    tx.commit().await?;
    Ok((StatusCode::OK, Json(out)))
}

async fn retract(
    State(rt): State<Runtime>,
    caller: Caller,
    Path((p, id)): Path<(String, String)>,
    Query(q): Query<Vec<(String, String)>>,
) -> Answer {
    let reason = q
        .iter()
        .rev()
        .find(|(k, _)| k == "reason")
        .map(|(_, v)| v.trim().to_string())
        .filter(|r| !r.is_empty())
        .unwrap_or_else(|| "removed".into());
    if reason.chars().count() > 500 {
        return Err(bad_input("reason: at most 500 characters"));
    }
    let mut tx = rt.pool.begin().await?;
    let perm = caller.require(&mut tx, &p, None, Some("write")).await?;
    let old = editable(&mut tx, &caller.0, &perm, &id, true).await?;
    family::retract(&mut tx, old.id, &reason).await?;
    audit(&mut tx, &caller.0, &perm, "retract", &[old.id.to_string()]).await?;
    tx.commit().await?;
    Ok((StatusCode::OK, Json(json!({"id": old.id, "status": "retracted"}))))
}

/// The drawing: the proband and this profile's relatives, each placed by its relationship row.
async fn pedigree(State(rt): State<Runtime>, caller: Caller, Path(p): Path<String>) -> Answer {
    let mut tx = rt.pool.begin().await?;
    let perm = caller.require(&mut tx, &p, Some("read:records"), None).await?;
    let rows = shown(&mut tx, &caller.0, &perm).await?;
    let ids: Vec<String> = rows.iter().map(|r| r.member.id.to_string()).collect();
    audit(&mut tx, &caller.0, &perm, "read", &ids).await?;
    tx.commit().await?;
    let cat = family::catalog()?;
    let today = ownhealth_core::access::today_utc();
    let nodes: Vec<Value> = rows
        .iter()
        .filter_map(|r| {
            let rel = cat.relationship(&r.member.relationship)?;
            let masked = r.masked();
            let m = &r.member;
            let age = (!masked && !m.deceased)
                .then(|| m.born_year.map(|y| today.year() - y))
                .flatten();
            let conditions: Vec<Value> = if masked {
                Vec::new()
            } else {
                m.conditions
                    .0
                    .iter()
                    .map(|c| {
                        let k = cat.condition(&c.kind);
                        json!({
                            "kind": c.kind,
                            "text": c.text,
                            "en": k.map(|k| k.en.as_str()), "es": k.map(|k| k.es.as_str()),
                            "short_en": k.map(|k| k.short_en.as_str()), "short_es": k.map(|k| k.short_es.as_str()),
                            "group": k.map(|k| k.group.as_str()),
                            "onset_age": c.onset_age, "onset_approx": c.onset_approx,
                        })
                    })
                    .collect()
            };
            Some(json!({
                "id": m.id,
                "relationship": rel.code,
                "label": {"en": rel.en, "es": rel.es},
                "generation": rel.generation,
                "side": rel.side,
                "degree": rel.degree,
                "slot": rel.slot,
                "union": rel.union,
                "child_of": rel.child_of,
                "sex_at_birth": m.sex_at_birth,
                "name": r.name(),
                "age": age,
                "deceased": m.deceased,
                "death_age": if masked { None } else { m.death_age },
                "conditions": conditions,
                "linked": m.linked_profile_id.is_some(),
                "masked": masked,
                "src": m.src_id,
            }))
        })
        .collect();
    let prof = &perm.profile;
    Ok((
        StatusCode::OK,
        Json(json!({
            "profile": {
                "slug": prof.slug,
                "name": prof.display_name,
                "sex_at_birth": prof.sex_at_birth,
                "age": prof.age_on(today),
                "deceased": prof.deceased,
                "slot": 25,
                "union": "self",
                "child_of": "parents",
            },
            "as_of": today.to_string(),
            "rules": cat.version,
            "nodes": nodes,
        })),
    ))
}
