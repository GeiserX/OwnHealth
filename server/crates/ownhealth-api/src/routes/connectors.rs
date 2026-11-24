//! v1.0 connections that sign in at a vendor (connectors.md 6.2): the one launch, callback,
//! reconnect and disconnect route set for every OAuth kind. A kind's slice adds its arm to
//! `connector_oauth::kind_spec`; none adds a route.
//!
//! | Route | What |
//! |---|---|
//! | `POST /connectors/launch/{kind}` | `{profile_id, client_id, issuer?, label?}`: `{authorize_url}` and the binder cookie |
//! | `GET /connectors/callback?state=&code=` | where the vendor sends the browser back: 302 to `/connections?connected=<id>` or `/connections?connector_error=<code>`; a connect or reconnect of a polled kind queues its first pull |
//! | `POST /connectors/{id}/reconnect` | `{authorize_url}` for a new grant on the same source; clears the reconsent flag on return |
//! | `DELETE /connectors/{id}` | revoke at the vendor when it offers it, then delete the tokens and the cursor: `{id, revocation, revoke_page}`; `revoke_page` is the vendor's own page when the revocation failed or is not offered, and always for Blue Button |
//!
//! An owner or guardian of the household who manages the profile. The callback counts only in the
//! browser and the session that started the launch, for a profile that person still manages. No
//! answer, redirect or audit row carries a token.
//!
//! Each launch sets its own binder cookie, named after its state, so two launches in one browser (two
//! tabs, two family members) do not undo each other.
//!
//! Tests reach a fake vendor on loopback with the [`EgressOverride`] request extension. On a debug
//! build that runs with dev auth (`OWNHEALTH_DEV_AUTH=1`), `OWNHEALTH_DEV_CONNECTOR_LOOPBACK=1` does
//! the same, so a person can try the flow against a local fake; a release build ignores it, as it
//! does without dev auth.

use std::time::Duration;

use axum::extract::{Path, Query, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Extension, Router};
use chrono::Utc;
use ownhealth_auth::household::{can_manage, require_manage};
use ownhealth_auth::{principal, sessions, token_urlsafe};
use ownhealth_core::access::{Principal, accessible_profiles};
use ownhealth_core::audit::{AuditEvent, write_audit};
use ownhealth_core::connector_oauth::{self as oauth, OAuthError, Revocation};
use ownhealth_core::net::Egress;
use ownhealth_core::secrets::SecretBox;
use ownhealth_core::sources;
use ownhealth_core::{Error, Runtime};
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::PgConnection;
use uuid::Uuid;

use super::auth::{Body, Fail, Meta, Params, R, answer, ok, set_cookie};
use crate::error::ApiError;
use crate::{Caller, SessionCaller};

/// The browser binder of a launch: the callback counts only where its cookie comes back. Each launch
/// has its own, named this prefix and a part of its state's hash ([`binder_cookie_name`]).
pub const BINDER_COOKIE: &str = "oh_connector";

/// The binder cookie of the launch with this `state`.
pub fn binder_cookie_name(state: &str) -> String {
    let hash = oauth::binder_hash(state);
    format!("{BINDER_COOKIE}_{}", &hash[..16])
}
/// Only the callback reads a binder.
const BINDER_PATH: &str = "/api/connectors/callback";
/// How long a profile delete waits for each vendor's revocation.
const DELETE_REVOKE_TIMEOUT: Duration = Duration::from_secs(5);

pub fn router() -> Router<Runtime> {
    Router::new()
        .route("/connectors/launch/{kind}", post(launch))
        .route("/connectors/callback", get(callback))
        .route("/connectors/{id}/reconnect", post(reconnect))
        .route("/connectors/{id}", delete(disconnect))
}

/// Tests point the connector egress at a fake vendor on loopback with this request extension.
#[derive(Debug, Clone)]
pub struct EgressOverride(pub Egress);

/// How connector requests go out: https to public addresses only, unless a test or a dev instance
/// widened it (module docs).
pub fn egress(rt: &Runtime, o: &Option<Extension<EgressOverride>>) -> Egress {
    let switch = std::env::var(oauth::DEV_LOOPBACK_SWITCH).ok();
    egress_for(rt.settings.dev_auth, switch.as_deref(), o)
}

/// [`egress`] with the dev switch's value given, so a test needs no process environment. The rule
/// is `connector_oauth::configured_egress`, which the pollers use too.
fn egress_for(dev_auth: bool, dev_loopback_switch: Option<&str>, o: &Option<Extension<EgressOverride>>) -> Egress {
    if let Some(Extension(EgressOverride(e))) = o {
        return e.clone();
    }
    oauth::configured_egress(dev_auth, dev_loopback_switch)
}

fn fail(e: OAuthError) -> Fail {
    match e {
        OAuthError::Core(e) => e.into(),
        OAuthError::Refused { code, message } => ApiError::new(StatusCode::CONFLICT, code, message).into(),
        OAuthError::ReconsentNeeded => ApiError::new(
            StatusCode::CONFLICT,
            "reconsent_needed",
            "The vendor no longer accepts this connection. Connect it again.",
        )
        .into(),
        other => ApiError::new(
            StatusCode::BAD_GATEWAY,
            "vendor_unreachable",
            format!("The vendor's server could not be used: {other}."),
        )
        .into(),
    }
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

/// The caller may bring data into this profile.
async fn manages_profile(conn: &mut PgConnection, p: &Principal, profile_id: Uuid) -> R<()> {
    let perm = Caller(p.clone())
        .require(conn, &profile_id.to_string(), None, None)
        .await?;
    require_manage(perm)?;
    Ok(())
}

/// A connection of the caller's household, on a profile the caller manages.
struct Connection {
    id: Uuid,
    kind: String,
    label: String,
    profile_id: Uuid,
    client_id: Uuid,
    config: Value,
}

/// kind, label, profile_id, connector_client_id, config
type ConnectionRow = (String, String, Option<Uuid>, Option<Uuid>, Value);

async fn managed(conn: &mut PgConnection, p: &Principal, id: &str) -> R<(Uuid, Connection)> {
    let unknown = || Error::NotFound("Unknown connection.".into());
    let id: Uuid = id.parse().map_err(|_| unknown())?;
    let household = manager(conn, p).await?;
    let kinds: Vec<&str> = oauth::OAUTH_KINDS.to_vec();
    let row: Option<ConnectionRow> = sqlx::query_as(
        "select kind, label, profile_id, connector_client_id, config from source \
         where id = $1 and household_id = $2 and kind = any($3)",
    )
    .bind(id)
    .bind(household)
    .bind(&kinds)
    .fetch_optional(&mut *conn)
    .await?;
    let Some((kind, label, Some(profile_id), Some(client_id), config)) = row else {
        return Err(unknown().into());
    };
    manages_profile(conn, p, profile_id).await?;
    Ok((
        household,
        Connection {
            id,
            kind,
            label,
            profile_id,
            client_id,
            config,
        },
    ))
}

async fn audit(
    conn: &mut PgConnection,
    p: &Principal,
    action: &str,
    profile_id: Uuid,
    source_id: Uuid,
    summary: Option<String>,
    failed: bool,
) -> R<()> {
    write_audit(
        conn,
        &AuditEvent {
            actor_id: p.user_id,
            tool: Some("connectors".into()),
            profile_id: Some(profile_id),
            resource_type: Some("source".into()),
            resource_ids: Some(vec![source_id.to_string()]),
            args_summary: summary,
            result_kind: Some(if failed { "error" } else { "ok" }.into()),
            ..AuditEvent::new("user", "ui", action)
        },
    )
    .await?;
    Ok(())
}

fn binder_cookie(rt: &Runtime, state: &str, binder: &str) -> String {
    set_cookie(
        &binder_cookie_name(state),
        binder,
        oauth::STATE_TTL_SECONDS,
        BINDER_PATH,
        rt.settings.base_url.starts_with("https://"),
    )
}

/// Send the browser to the vendor's sign-in: one launch, its binder in this browser.
async fn start(rt: &Runtime, p: &Principal, l: Started<'_>, egress: &Egress) -> R {
    let endpoints = oauth::resolve(&l.spec, l.issuer, egress).await.map_err(fail)?;
    let binder = token_urlsafe(32);
    let mut tx = rt.pool.begin().await?;
    let begun = oauth::begin(
        &mut tx,
        &oauth::Launch {
            kind: l.kind,
            client: l.client,
            endpoints: &endpoints,
            profile_id: l.profile_id,
            label: l.label,
            source_id: l.source_id,
            user_id: p.user_id,
            session_id: sessions::session_id(p),
            binder: &binder,
        },
    )
    .await
    .map_err(fail)?;
    tx.commit().await?;
    Ok(answer(
        StatusCode::OK,
        json!({"authorize_url": begun.url}),
        &[binder_cookie(rt, &begun.state, &binder)],
    ))
}

struct Started<'a> {
    kind: &'a str,
    spec: oauth::KindSpec,
    client: &'a oauth::ConnectorClient,
    issuer: &'a str,
    profile_id: Uuid,
    label: &'a str,
    source_id: Option<Uuid>,
}

#[derive(Debug, Deserialize)]
struct LaunchIn {
    profile_id: Uuid,
    client_id: Uuid,
    #[serde(default)]
    issuer: Option<String>,
    #[serde(default)]
    label: Option<String>,
}

fn host_of(url: &str) -> String {
    oauth::authority(url)
        .and_then(|a| a.rsplit_once(':').map(|(h, _)| h.to_string()))
        .unwrap_or_default()
}

async fn launch(
    State(rt): State<Runtime>,
    SessionCaller(p): SessionCaller,
    Path(kind): Path<String>,
    o: Option<Extension<EgressOverride>>,
    Body(b): Body<LaunchIn>,
) -> R {
    let spec =
        oauth::kind_spec(&kind).ok_or_else(|| Error::NotFound("This kind of connection is not available.".into()))?;
    // the refresh token is sealed when the browser comes back: refuse now, not after the consent
    SecretBox::from_settings(&rt.settings)?;
    let client = {
        let mut tx = rt.pool.begin().await?;
        let household = manager(&mut tx, &p).await?;
        manages_profile(&mut tx, &p, b.profile_id).await?;
        let client = oauth::load_client(&mut tx, None, household, b.client_id)
            .await
            .map_err(fail)?
            .filter(|c| oauth::client_fits(&kind, &c.vendor))
            .ok_or_else(|| Error::NotFound("Unknown client.".into()))?;
        tx.commit().await?;
        client
    };
    let given = b.issuer.as_deref().map(str::trim).filter(|s| !s.is_empty());
    let issuer = match (client.issuer.as_deref(), given, spec.default_issuer) {
        (Some(fixed), Some(given), _) if fixed.trim_end_matches('/') != given.trim_end_matches('/') => {
            return Err(Error::Invalid("This client is registered for another server.".into()).into());
        }
        (Some(fixed), _, _) => fixed.to_string(),
        // the secret goes to the token endpoint of the server named here: only the registered one
        (None, Some(_), _) if client.confidential => {
            return Err(Error::Invalid(
                "This client has a secret, so it serves only the server it was registered for. Register it with that server's address."
                    .into(),
            )
            .into());
        }
        (None, Some(given), _) => given.to_string(),
        (None, None, Some(d)) => d.to_string(),
        (None, None, None) => return Err(Error::Invalid("Name the server to connect to.".into()).into()),
    };
    let label = b
        .label
        .as_deref()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map_or_else(|| host_of(&issuer), String::from);
    if label.is_empty() || label.chars().count() > 120 {
        return Err(Error::Invalid("The name of the connection must have 1 to 120 characters.".into()).into());
    }
    start(
        &rt,
        &p,
        Started {
            kind: &kind,
            spec,
            client: &client,
            issuer: &issuer,
            profile_id: b.profile_id,
            label: &label,
            source_id: None,
        },
        &egress(&rt, &o),
    )
    .await
}

async fn reconnect(
    State(rt): State<Runtime>,
    SessionCaller(p): SessionCaller,
    Path(id): Path<String>,
    o: Option<Extension<EgressOverride>>,
) -> R {
    SecretBox::from_settings(&rt.settings)?;
    let (c, client) = {
        let mut tx = rt.pool.begin().await?;
        let (household, c) = managed(&mut tx, &p, &id).await?;
        let client = oauth::load_client(&mut tx, None, household, c.client_id)
            .await
            .map_err(fail)?
            .ok_or_else(|| Error::NotFound("The vendor client of this connection is gone.".into()))?;
        tx.commit().await?;
        (c, client)
    };
    let spec =
        oauth::kind_spec(&c.kind).ok_or_else(|| Error::NotFound("This kind of connection is not available.".into()))?;
    let issuer = c.config["endpoints"]["issuer"]
        .as_str()
        .map(String::from)
        .or_else(|| client.issuer.clone())
        .or_else(|| spec.default_issuer.map(String::from))
        .ok_or_else(|| Error::Invalid("This connection names no server.".into()))?;
    start(
        &rt,
        &p,
        Started {
            kind: &c.kind,
            spec,
            client: &client,
            issuer: &issuer,
            profile_id: c.profile_id,
            label: &c.label,
            source_id: Some(c.id),
        },
        &egress(&rt, &o),
    )
    .await
}

/// A redirect that also drops the binder of the launch with `state`, and no other launch's.
fn redirect(state: &str, location: &str) -> Response {
    let mut res = StatusCode::FOUND.into_response();
    if let Ok(v) = HeaderValue::from_str(location) {
        res.headers_mut().insert(header::LOCATION, v);
    }
    if !state.is_empty() {
        let gone = format!(
            "{}=\"\"; Max-Age=0; Path={BINDER_PATH}; HttpOnly; SameSite=lax",
            binder_cookie_name(state)
        );
        if let Ok(v) = HeaderValue::from_str(&gone) {
            res.headers_mut().append(header::SET_COOKIE, v);
        }
    }
    res
}

/// Back to the Connections screen. `code` is one of our own fixed words, never the vendor's text.
fn back(state: &str, code: &str) -> Response {
    redirect(state, &format!("/connections?connector_error={code}"))
}

async fn callback(State(rt): State<Runtime>, meta: Meta, Query(q): Params, o: Option<Extension<EgressOverride>>) -> R {
    let state = q.get("state").cloned().unwrap_or_default();
    if state.is_empty() {
        return Ok(back(&state, "state_invalid"));
    }
    // the state is used up whatever comes next, so it commits on its own
    let pending = {
        let mut tx = rt.pool.begin().await?;
        let binder = meta.cookie(&binder_cookie_name(&state)).unwrap_or_default();
        let taken = oauth::take_state(&mut tx, &state, &binder).await;
        tx.commit().await?;
        match taken {
            Ok(p) => p,
            Err(OAuthError::Refused { code, .. }) => return Ok(back(&state, code)),
            Err(e) => return Err(fail(e)),
        }
    };
    let code = q.get("code").cloned().unwrap_or_default();
    if q.get("error").is_some_and(|e| !e.is_empty()) || code.is_empty() {
        return Ok(back(&state, "denied"));
    }
    // the person who started it, in the session that started it, who still manages the profile
    let mut conn = rt.pool.acquire().await?;
    let session = meta.cookie(sessions::COOKIE);
    let p = principal::resolve(&mut conn, &rt.settings, None, session.as_deref()).await?;
    let Some(p) = p.filter(|p| p.user_id == pending.user_id && sessions::session_id(p) == pending.session_id) else {
        return Ok(back(&state, "wrong_user"));
    };
    let Ok(household) = manager(&mut conn, &p).await else {
        return Ok(back(&state, "forbidden"));
    };
    if manages_profile(&mut conn, &p, pending.profile_id).await.is_err() {
        return Ok(back(&state, "forbidden"));
    }
    let sb = SecretBox::from_settings(&rt.settings)?.with_keys(rt.blobs.keyring());
    let Some(client) = oauth::load_client(&mut conn, Some(&sb), household, pending.client)
        .await
        .map_err(fail)?
    else {
        return Ok(back(&state, "client_gone"));
    };
    drop(conn);
    let tokens = match oauth::exchange(&pending, &client, &code, &egress(&rt, &o)).await {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!(error = %e, profile_id = %pending.profile_id, "a connector code exchange failed");
            return Ok(back(&state, exchange_refusal(&e)));
        }
    };
    let now = Utc::now();
    let mut tx = rt.pool.begin().await?;
    let id = match oauth::save_connection(&mut tx, &sb, &pending, &client, &tokens, now).await {
        Ok(id) => id,
        Err(OAuthError::Refused { code, .. }) => return Ok(back(&state, code)),
        Err(e) => return Err(fail(e)),
    };
    let action = if pending.source_id.is_some() {
        "connector.reconnect"
    } else {
        "connector.connect"
    };
    audit(&mut tx, &p, action, pending.profile_id, id, None, false).await?;
    // The first pull shows what the new grant brought. On reconnect it follows any active pull.
    if sources::poller_for(&pending.kind).is_some() {
        sources::enqueue_grant_poll(&mut tx, id).await?;
    }
    tx.commit().await?;
    Ok(redirect(&state, &format!("/connections?connected={id}")))
}

/// The code a failed exchange sends back to the Connections screen: a refusal that has its own words
/// there keeps its code; anything else reads as the generic exchange failure, which names the
/// registration at the vendor.
fn exchange_refusal(e: &OAuthError) -> &'static str {
    match e {
        OAuthError::Refused { code, .. } if matches!(*code, "no_patient" | "client_secret_required") => code,
        _ => "exchange_failed",
    }
}

fn word(r: Revocation) -> &'static str {
    match r {
        Revocation::Revoked => "revoked",
        Revocation::NotOffered => "not_offered",
        Revocation::Failed => "failed",
        Revocation::NoToken => "no_token",
    }
}

fn revoke_page(kind: &str, r: Revocation) -> Option<&'static str> {
    oauth::revoke_page(kind, r)
}

async fn disconnect(
    State(rt): State<Runtime>,
    SessionCaller(p): SessionCaller,
    Path(id): Path<String>,
    o: Option<Extension<EgressOverride>>,
) -> R {
    let c = {
        let mut tx = rt.pool.begin().await?;
        let (_, c) = managed(&mut tx, &p, &id).await?;
        tx.commit().await?;
        c
    };
    let sb = SecretBox::from_settings(&rt.settings)?.with_keys(rt.blobs.keyring());
    let outcome = oauth::disconnect(&rt.pool, &sb, c.id, &egress(&rt, &o))
        .await
        .map_err(fail)?;
    let mut conn = rt.pool.acquire().await?;
    audit(
        &mut conn,
        &p,
        "connector.disconnect",
        c.profile_id,
        c.id,
        Some(format!("revocation={}", word(outcome))),
        outcome == Revocation::Failed,
    )
    .await?;
    ok(json!({"id": c.id, "revocation": outcome, "revoke_page": revoke_page(&c.kind, outcome)}))
}

/// Before a profile delete: revoke each of the profile's connections at its vendor, best effort and
/// at most [`DELETE_REVOKE_TIMEOUT`] each, and audit every outcome. The delete removes the tokens
/// whatever happens here, so nothing here stops it; a failure is only logged. Empty when the profile
/// has no connection that holds a token.
pub async fn revoke_before_delete(rt: &Runtime, p: &Principal, profile_id: Uuid, egress: &Egress) -> Vec<Value> {
    let quick = Egress {
        timeout: egress.timeout.min(DELETE_REVOKE_TIMEOUT),
        ..egress.clone()
    };
    let sb = SecretBox::from_settings(&rt.settings)
        .ok()
        .map(|b| b.with_keys(rt.blobs.keyring()));
    let done = match oauth::revoke_profile(&rt.pool, sb.as_ref(), profile_id, &quick, DELETE_REVOKE_TIMEOUT).await {
        Ok(d) => d,
        Err(e) => {
            tracing::warn!(error = %e, %profile_id, "could not revoke the connections before a profile delete");
            return Vec::new();
        }
    };
    for r in &done {
        let said = async {
            let mut conn = rt.pool.acquire().await?;
            audit(
                &mut conn,
                p,
                "connector.revoke",
                profile_id,
                r.source_id,
                Some(format!("revocation={}", word(r.outcome))),
                r.outcome == Revocation::Failed,
            )
            .await
        }
        .await;
        if let Err(e) = said {
            tracing::warn!(error = ?e, source_id = %r.source_id, "could not audit a revocation before a profile delete");
        }
    }
    done.into_iter().map(|r| json!(r)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_medicare_disconnect_always_links_the_medicare_page() {
        // CMS's revoke_token ends the tokens; the app stays listed at Medicare until the person removes it
        for r in [
            Revocation::Revoked,
            Revocation::Failed,
            Revocation::NotOffered,
            Revocation::NoToken,
        ] {
            assert_eq!(
                revoke_page("bluebutton", r),
                Some("https://www.medicare.gov/account/"),
                "{r:?}"
            );
        }
        assert_eq!(revoke_page("smart", Revocation::Revoked), None);
        assert_eq!(revoke_page("smart", Revocation::Failed), None);
    }

    #[test]
    fn an_exchange_refusal_with_its_own_words_keeps_its_code() {
        let refused = |code| OAuthError::Refused { code, message: "" };
        assert_eq!(exchange_refusal(&refused("no_patient")), "no_patient");
        assert_eq!(
            exchange_refusal(&refused("client_secret_required")),
            "client_secret_required"
        );
        // a client registered for another server reads as the generic words that name the registration
        assert_eq!(exchange_refusal(&refused("bad_issuer")), "exchange_failed");
        let vendor = OAuthError::Vendor {
            status: 400,
            code: "invalid_grant".into(),
        };
        assert_eq!(exchange_refusal(&vendor), "exchange_failed");
    }

    #[test]
    fn the_dev_loopback_switch_is_ignored_without_dev_auth() {
        let on = Some("1");
        assert_eq!(egress_for(false, on, &None), Egress::connector());
        assert_eq!(egress_for(true, Some("0"), &None), Egress::connector());
        assert_eq!(egress_for(true, None, &None), Egress::connector());
        // the reverse, in a debug build with dev auth: loopback over plain http
        let dev = egress_for(true, on, &None);
        assert!(dev.allow_private && !dev.https_only, "{dev:?}");
    }
}
