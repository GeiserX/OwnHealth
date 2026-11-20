//! v1.0 the connector foundation (migration 1200): connector sources are never made through
//! POST /api/inboxes, and a connector client (one per vendor per household) keeps its secret only
//! while it exists and cannot go while a source still uses it.

#[path = "auth_support.rs"]
mod support;

use axum::http::StatusCode;
use ownhealth_core::Runtime;
use ownhealth_core::secrets::{SecretBox, SecretString, store_secret};
use ownhealth_core::sources::CONNECTOR_KINDS;
use serde_json::json;
use support::Env;
use uuid::Uuid;

type R = anyhow::Result<()>;

/// 32 bytes of 0x09, base64.
const KEY: &str = "CQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQk=";

async fn env() -> anyhow::Result<Env> {
    let mut env = Env::new(true).await?;
    env.settings.secrets_key = Some(KEY.into());
    env.rt = Runtime::with_pool(env.settings.clone(), env.db.pool.clone())?;
    Ok(env)
}

#[tokio::test]
async fn the_inbox_route_refuses_every_connector_kind() -> R {
    let env = env().await?;
    let mut c = env.as_user(env.user("alex")).await?;
    for kind in ["smart", "bluebutton", "brands", "openwearables", "dexcom"] {
        assert!(CONNECTOR_KINDS.contains(&kind), "{kind} is not a connector kind");
        let body = json!({"kind": kind, "label": "Hospital LabNorte", "profile_id": env.profile("alex")});
        let r = c.post("/api/inboxes", &body).await?;
        assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY, "{kind}: {}", r.json);
        assert_eq!(r.code(), "invalid", "{kind}: {}", r.json);
    }
    assert_eq!(env.scalar_i64("select count(*) from source").await?, 0);
    // the reverse: a kind the route owns is still made there
    let r = c
        .post(
            "/api/inboxes",
            &json!({"kind": "share", "label": "Compartir del móvil"}),
        )
        .await?;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.json);
    Ok(())
}

async fn client_with_secret(env: &Env) -> anyhow::Result<(Uuid, Uuid, Uuid)> {
    let household: Uuid = sqlx::query_scalar("select household_id from profile where id = $1")
        .bind(env.profile("alex"))
        .fetch_one(env.pool())
        .await?;
    let sb = SecretBox::from_settings(&env.settings)?;
    let mut conn = env.pool().acquire().await?;
    let secret = store_secret(
        &mut conn,
        &sb,
        household,
        "connector_client_secret",
        Some("Blue Button"),
        &SecretString::new("fake-client-secret-not-real"),
    )
    .await?;
    let client: Uuid = sqlx::query_scalar(
        "insert into connector_client (household_id, vendor, issuer, client_id, client_secret_id, redirect_uri) \
         values ($1, 'bluebutton', 'https://sandbox.bluebutton.example', 'fake-client-id', $2, \
         'https://ownhealth.rivera.example/api/connectors/callback') returning id",
    )
    .bind(household)
    .bind(secret)
    .fetch_one(&mut *conn)
    .await?;
    Ok((household, client, secret))
}

#[tokio::test]
async fn a_connector_client_in_use_cannot_be_deleted() -> R {
    let env = env().await?;
    let (household, client, secret) = client_with_secret(&env).await?;
    let source: Uuid = sqlx::query_scalar(
        "insert into source (household_id, profile_id, kind, label, connector_client_id) \
         values ($1, $2, 'bluebutton', 'Medicare de Alex', $3) returning id",
    )
    .bind(household)
    .bind(env.profile("alex"))
    .bind(client)
    .fetch_one(env.pool())
    .await?;
    let refused = sqlx::query("delete from connector_client where id = $1")
        .bind(client)
        .execute(env.pool())
        .await
        .expect_err("a client in use was deleted");
    let code = refused.as_database_error().and_then(|e| e.code()).unwrap_or_default();
    assert_eq!(code, "23503", "{refused}");
    // its secret cannot go from under it either
    let refused = sqlx::query("delete from secret where id = $1")
        .bind(secret)
        .execute(env.pool())
        .await
        .expect_err("the client's secret was deleted");
    let code = refused.as_database_error().and_then(|e| e.code()).unwrap_or_default();
    assert_eq!(code, "23503", "{refused}");
    // the reverse: once the source is gone, the client goes and takes its secret
    sqlx::query("delete from source where id = $1")
        .bind(source)
        .execute(env.pool())
        .await?;
    sqlx::query("delete from connector_client where id = $1")
        .bind(client)
        .execute(env.pool())
        .await?;
    assert_eq!(env.scalar_i64("select count(*) from secret").await?, 0);
    Ok(())
}

#[tokio::test]
async fn deleting_an_unused_connector_client_deletes_its_secret() -> R {
    let env = env().await?;
    let (_, client, secret) = client_with_secret(&env).await?;
    // a public client keeps no secret, and deleting it touches no other secret
    let household: Uuid = sqlx::query_scalar("select household_id from connector_client where id = $1")
        .bind(client)
        .fetch_one(env.pool())
        .await?;
    let public: Uuid = sqlx::query_scalar(
        "insert into connector_client (household_id, vendor, issuer, client_id, redirect_uri) \
         values ($1, 'epic', null, 'fake-public-id', 'https://ownhealth.rivera.example/api/connectors/callback') \
         returning id",
    )
    .bind(household)
    .fetch_one(env.pool())
    .await?;
    sqlx::query("delete from connector_client where id = $1")
        .bind(public)
        .execute(env.pool())
        .await?;
    assert_eq!(env.scalar_i64("select count(*) from secret").await?, 1);

    sqlx::query("delete from connector_client where id = $1")
        .bind(client)
        .execute(env.pool())
        .await?;
    let left: i64 = sqlx::query_scalar("select count(*) from secret where id = $1")
        .bind(secret)
        .fetch_one(env.pool())
        .await?;
    assert_eq!(left, 0, "the client secret outlived its client");
    Ok(())
}

#[tokio::test]
async fn a_source_shows_its_reconsent_and_expiry_state() -> R {
    let env = env().await?;
    let (household, client, _) = client_with_secret(&env).await?;
    let row: (
        Option<chrono::DateTime<chrono::Utc>>,
        Option<chrono::DateTime<chrono::Utc>>,
        Option<chrono::DateTime<chrono::Utc>>,
    ) = sqlx::query_as(
        "insert into source (household_id, profile_id, kind, label, connector_client_id, reconsent_needed_at, \
             grant_expires_at, access_expires_at) values ($1, $2, 'smart', 'Hospital LabNorte', $3, now(), \
             now() + interval '90 days', now() + interval '1 hour') \
             returning reconsent_needed_at, grant_expires_at, access_expires_at",
    )
    .bind(household)
    .bind(env.profile("alex"))
    .bind(client)
    .fetch_one(env.pool())
    .await?;
    assert!(row.0.is_some() && row.1.is_some() && row.2.is_some(), "{row:?}");
    Ok(())
}
