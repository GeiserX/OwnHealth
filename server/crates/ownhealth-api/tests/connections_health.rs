//! v1.0 the Connections screen reads the health list (`GET /api/inboxes/health`): each connection
//! says when the vendor wants consent again, when the person's grant and the current access token
//! end, how many sync-gap alerts are open, which server it talks to, and whether the caller may
//! reconnect it. Against the fake vendor on loopback; synthetic data only (the Rivera family).

#[path = "auth_support.rs"]
mod support;

#[path = "../../ownhealth-core/tests/connector_fake/mod.rs"]
mod connector_fake;

use axum::Extension;
use axum::http::StatusCode;
use chrono::{DateTime, Duration, Utc};
use connector_fake::{CLIENT_ID, CLIENT_SECRET, Fake, REDIRECT, param};
use ownhealth_api::routes::connectors::EgressOverride;
use ownhealth_core::Runtime;
use ownhealth_core::net::Egress;
use ownhealth_core::secrets::{SecretBox, SecretString, store_secret};
use serde_json::{Value, json};
use support::{Client, Env};
use uuid::Uuid;

type R = anyhow::Result<()>;

/// 32 bytes of 0x09, base64.
const KEY: &str = "CQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQk=";

struct T {
    env: Env,
    fake: Fake,
    client: Uuid,
}

async fn setup() -> anyhow::Result<T> {
    let mut env = Env::new(true).await?;
    env.settings.secrets_key = Some(KEY.into());
    env.rt = Runtime::with_pool(env.settings.clone(), env.db.pool.clone())?;
    let fake = Fake::start().await;
    let household: Uuid = sqlx::query_scalar("select household_id from profile where id = $1")
        .bind(env.profile("alex"))
        .fetch_one(env.pool())
        .await?;
    let sb = SecretBox::from_settings(&env.settings)?;
    let mut c = env.pool().acquire().await?;
    let secret = store_secret(
        &mut c,
        &sb,
        household,
        "connector_client_secret",
        Some("Hospital LabNorte"),
        &SecretString::new(CLIENT_SECRET),
    )
    .await?;
    let client = sqlx::query_scalar(
        "insert into connector_client (household_id, vendor, issuer, client_id, client_secret_id, redirect_uri) \
         values ($1, 'labnorte-ehr', $2, $3, $4, $5) returning id",
    )
    .bind(household)
    .bind(fake.issuer())
    .bind(CLIENT_ID)
    .bind(secret)
    .bind(REDIRECT)
    .fetch_one(&mut *c)
    .await?;
    drop(c);
    Ok(T { env, fake, client })
}

impl T {
    /// A browser of `slug` whose connector requests may reach the fake on loopback.
    async fn browser(&self, slug: &str) -> anyhow::Result<Client> {
        let mut c = self.env.as_user(self.env.user(slug)).await?;
        c.app = c.app.layer(Extension(EgressOverride(Egress {
            allow_private: true,
            https_only: false,
            ..Egress::connector()
        })));
        Ok(c)
    }

    /// The vendor's half of an authorize URL: consent, then the browser comes back to the callback.
    async fn come_back(&self, c: &mut Client, authorize_url: &str) -> anyhow::Result<String> {
        let code = self.fake.consent(authorize_url);
        let state = param(authorize_url, "state").unwrap_or_default();
        let back = c
            .get(&format!("/api/connectors/callback?state={state}&code={code}"))
            .await?;
        assert_eq!(back.status, StatusCode::FOUND, "{}", back.json);
        Ok(back.location())
    }

    async fn connect(&self, c: &mut Client, profile: Uuid) -> anyhow::Result<Uuid> {
        let r = c
            .post(
                "/api/connectors/launch/smart",
                &json!({"profile_id": profile, "client_id": self.client, "label": "Hospital LabNorte"}),
            )
            .await?;
        assert_eq!(r.status, StatusCode::OK, "{}", r.json);
        let url = r.json["authorize_url"].as_str().unwrap_or_default().to_string();
        let loc = self.come_back(c, &url).await?;
        let id = loc.strip_prefix("/connections?connected=").unwrap_or_default();
        id.parse().map_err(|_| anyhow::anyhow!("not connected: {loc}"))
    }
}

/// The fields a health row carries only for a person who manages the source.
const CONNECTION_FIELDS: [&str; 7] = [
    "reconsent_needed_at",
    "grant_expires_at",
    "access_expires_at",
    "open_gaps",
    "server",
    "created_at",
    "poll_seconds",
];

/// The health row of `id` as `c` reads it, or null when it is not listed.
async fn row(c: &mut Client, id: Uuid) -> anyhow::Result<Value> {
    let r = c.get("/api/inboxes/health").await?;
    assert_eq!(r.status, StatusCode::OK, "{}", r.json);
    Ok(r.json
        .as_array()
        .and_then(|a| a.iter().find(|s| s["id"] == id.to_string().as_str()).cloned())
        .unwrap_or(Value::Null))
}

fn at(v: &Value) -> Option<DateTime<Utc>> {
    v.as_str()
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|d| d.with_timezone(&Utc))
}

#[tokio::test]
async fn a_connection_that_needs_consent_again_says_so_and_a_reconnect_clears_it() -> R {
    let t = setup().await?;
    let mut a = t.browser("alex").await?;
    let id = t.connect(&mut a, t.env.profile("alex")).await?;
    let fresh = row(&mut a, id).await?;
    assert_eq!(fresh["reconsent_needed_at"], Value::Null, "{fresh}");
    assert_eq!(fresh["can_manage"], true, "{fresh}");
    assert_eq!(
        fresh["server"].as_str(),
        Some(t.fake.issuer().trim_end_matches('/')),
        "{fresh}"
    );
    // the fake's access tokens last an hour
    let access = at(&fresh["access_expires_at"]).ok_or_else(|| anyhow::anyhow!("no access end: {fresh}"))?;
    assert!(
        access > Utc::now() + Duration::minutes(50) && access < Utc::now() + Duration::minutes(70),
        "{fresh}"
    );

    // the vendor refused the token (what a refresh does on invalid_grant)
    sqlx::query("update source set reconsent_needed_at = now() where id = $1")
        .bind(id)
        .execute(t.env.pool())
        .await?;
    let flagged = row(&mut a, id).await?;
    assert!(at(&flagged["reconsent_needed_at"]).is_some(), "{flagged}");

    // the reverse: the Reconnect button's route, then the vendor, then the screen again
    let r = a.post0(&format!("/api/connectors/{id}/reconnect")).await?;
    assert_eq!(r.status, StatusCode::OK, "{}", r.json);
    let url = r.json["authorize_url"].as_str().unwrap_or_default().to_string();
    assert_eq!(t.come_back(&mut a, &url).await?, format!("/connections?connected={id}"));
    let cleared = row(&mut a, id).await?;
    assert_eq!(cleared["reconsent_needed_at"], Value::Null, "{cleared}");
    assert_eq!(cleared["enabled"], true, "{cleared}");
    Ok(())
}

#[tokio::test]
async fn a_grant_that_ends_soon_shows_its_end_before_it_ends() -> R {
    let t = setup().await?;
    t.fake.set(|b| b.refresh_expires_in = Some(3 * 86_400));
    let mut a = t.browser("alex").await?;
    let id = t.connect(&mut a, t.env.profile("alex")).await?;
    let r = row(&mut a, id).await?;
    let grant = at(&r["grant_expires_at"]).ok_or_else(|| anyhow::anyhow!("no grant end: {r}"))?;
    let three_days = Utc::now() + Duration::days(3);
    assert!((grant - three_days).num_minutes().abs() < 10, "{r}");
    assert_eq!(r["reconsent_needed_at"], Value::Null, "{r}");
    Ok(())
}

#[tokio::test]
async fn only_a_person_who_manages_the_profile_is_told_they_may_reconnect() -> R {
    let t = setup().await?;
    let mut a = t.browser("alex").await?;
    let id = t.connect(&mut a, t.env.profile("alex")).await?;
    // Sam views Alex's profile: he sees the connection, but may not reconnect it
    let mut s = t.browser("sam").await?;
    let seen = row(&mut s, id).await?;
    assert_eq!(seen["kind"], "smart", "{seen}");
    assert_eq!(seen["can_manage"], false, "{seen}");
    // the connection's dates, server and alerts are for the people who manage it
    for k in CONNECTION_FIELDS {
        assert!(seen.get(k).is_none(), "a viewer is sent {k}: {seen}");
    }
    let r = s.post0(&format!("/api/connectors/{id}/reconnect")).await?;
    assert!(
        matches!(r.status, StatusCode::FORBIDDEN | StatusCode::NOT_FOUND),
        "{} {}",
        r.status,
        r.json
    );
    // a household-wide inbox: Alex owns a profile, so he manages it; it names no server
    let folder: Uuid = sqlx::query_scalar(
        "insert into source (household_id, kind, label, config) select household_id, 'folder', 'Escáner', '{}' \
         from profile where id = $1 returning id",
    )
    .bind(t.env.profile("alex"))
    .fetch_one(t.env.pool())
    .await?;
    let f = row(&mut a, folder).await?;
    assert_eq!(
        (f["can_manage"].as_bool(), f["server"].is_null()),
        (Some(true), true),
        "{f}"
    );
    Ok(())
}

#[tokio::test]
async fn open_sync_gap_alerts_of_a_connection_are_counted_and_closed_ones_are_not() -> R {
    let t = setup().await?;
    let mut a = t.browser("alex").await?;
    let id = t.connect(&mut a, t.env.profile("alex")).await?;
    assert_eq!(row(&mut a, id).await?["open_gaps"], 0);
    for (status, source) in [("open", id), ("open", id), ("dismissed", id), ("open", Uuid::new_v4())] {
        sqlx::query(
            "insert into review_item (household_id, profile_id, kind, ref_table, ref_id, payload, status) \
             select household_id, id, 'source_gap', 'source', $2, '{}', $3 from profile where id = $1",
        )
        .bind(t.env.profile("alex"))
        .bind(source)
        .bind(status)
        .execute(t.env.pool())
        .await?;
    }
    // another kind of item on the same source, and a gap filed on Carmen's profile that names Alex's
    // connection, are not this connection's alerts either
    for (profile, kind) in [("alex", "job_failed"), ("carmen", "source_gap")] {
        sqlx::query(
            "insert into review_item (household_id, profile_id, kind, ref_table, ref_id, payload) \
             select household_id, id, $3, 'source', $2, '{}' from profile where id = $1",
        )
        .bind(t.env.profile(profile))
        .bind(id)
        .bind(kind)
        .execute(t.env.pool())
        .await?;
    }
    // two open alerts of this connection: the dismissed one and another source's alert do not count
    let r = row(&mut a, id).await?;
    assert_eq!(r["open_gaps"], 2, "{r}");
    // Sam views Alex's profile but is sent no count of its review items
    let mut s = t.browser("sam").await?;
    let seen = row(&mut s, id).await?;
    assert!(seen.get("open_gaps").is_none(), "{seen}");
    Ok(())
}

#[tokio::test]
async fn a_connection_made_a_moment_ago_says_when_it_was_made_and_how_often_it_syncs() -> R {
    let t = setup().await?;
    let mut a = t.browser("alex").await?;
    let id = t.connect(&mut a, t.env.profile("alex")).await?;
    let r = row(&mut a, id).await?;
    // no run yet: the screen reads No data until one poll interval has passed since created_at
    assert_eq!(
        (r["last_run_at"].clone(), r["stale"].clone()),
        (Value::Null, Value::Bool(true)),
        "{r}"
    );
    let made = at(&r["created_at"]).ok_or_else(|| anyhow::anyhow!("no created_at: {r}"))?;
    assert!((Utc::now() - made).num_minutes().abs() < 5, "{r}");
    assert!(r["poll_seconds"].as_i64().is_some_and(|s| s > 0), "{r}");
    Ok(())
}

#[tokio::test]
async fn a_guardian_stops_managing_a_connection_once_its_person_has_a_login() -> R {
    let t = setup().await?;
    let mut a = t.browser("alex").await?;
    // Alex guards Lucia, who has no login: he manages her connection
    let id = t.connect(&mut a, t.env.profile("lucia")).await?;
    assert_eq!(row(&mut a, id).await?["can_manage"], true);
    // Lucia gets her own login: she manages her profile now, and Alex still sees the row, read-only
    let household = t.env.fam().household_id;
    let mut c = t.env.pool().acquire().await?;
    let lucia = ownhealth_core::access::create_user(&mut c, household, "Lucía Rivera", "es", None).await?;
    sqlx::query("update profile set user_id = $1 where id = $2")
        .bind(lucia)
        .bind(t.env.profile("lucia"))
        .execute(&mut *c)
        .await?;
    drop(c);
    let seen = row(&mut a, id).await?;
    assert_eq!(seen["can_manage"], false, "{seen}");
    for k in CONNECTION_FIELDS {
        assert!(seen.get(k).is_none(), "{k}: {seen}");
    }
    let r = a.post0(&format!("/api/connectors/{id}/reconnect")).await?;
    assert!(
        matches!(r.status, StatusCode::FORBIDDEN | StatusCode::NOT_FOUND),
        "{} {}",
        r.status,
        r.json
    );
    Ok(())
}
