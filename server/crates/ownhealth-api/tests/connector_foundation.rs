//! v1.0 the connector foundation (migration 1200): connector sources are never made through
//! POST /api/inboxes, and a connector client (one per vendor per household) keeps its secret only
//! while it exists and cannot go while a source still uses it.

#[path = "auth_support.rs"]
mod support;

use axum::http::{Method, StatusCode};
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
        let message = r.json["detail"]["message"].as_str().unwrap_or_default();
        assert!(!message.is_empty(), "{kind}: no message in {}", r.json);
        assert!(!message.contains(kind), "{kind}: the raw kind is in {message:?}");
        assert!(
            !message.contains("its own connection"),
            "{kind}: a route that does not exist: {message:?}"
        );
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
    let row: (bool, bool, bool) = sqlx::query_as(
        "insert into source (household_id, profile_id, kind, label, connector_client_id, reconsent_needed_at, \
         grant_expires_at, access_expires_at) values ($1, $2, 'smart', 'Hospital LabNorte', $3, now(), \
         now() + interval '90 days', now() + interval '1 hour') returning reconsent_needed_at is not null, \
         grant_expires_at > access_expires_at, access_expires_at > now()",
    )
    .bind(household)
    .bind(env.profile("alex"))
    .bind(client)
    .fetch_one(env.pool())
    .await?;
    assert_eq!(row, (true, true, true));
    Ok(())
}

/// A connector source as the connector launch (slice 13) will leave it: a client and a reconnect state.
async fn connector_source(env: &Env) -> anyhow::Result<Uuid> {
    let (household, client, _) = client_with_secret(env).await?;
    Ok(sqlx::query_scalar(
        "insert into source (household_id, profile_id, kind, label, connector_client_id, reconsent_needed_at) \
         values ($1, $2, 'smart', 'Hospital LabNorte', $3, now()) returning id",
    )
    .bind(household)
    .bind(env.profile("alex"))
    .bind(client)
    .fetch_one(env.pool())
    .await?)
}

#[tokio::test]
async fn a_connector_source_stays_out_of_every_inbox_route() -> R {
    let env = env().await?;
    let id = connector_source(&env).await?;
    let mut c = env.as_user(env.user("alex")).await?;

    let list = c.get("/api/inboxes").await?;
    assert_eq!(list.status, StatusCode::OK, "{}", list.json);
    let listed = list
        .json
        .as_array()
        .is_some_and(|a| a.iter().any(|s| s["id"] == id.to_string().as_str()));
    assert!(!listed, "the inbox list shows a connector source: {}", list.json);

    let path = format!("/api/inboxes/{id}");
    let answers = [
        ("get", c.get(&path).await?),
        (
            "patch",
            c.send(
                Method::PATCH,
                &path,
                Some(&json!({"label": "Otro", "profile_id": env.profile("lucia")})),
                &[],
            )
            .await?,
        ),
        ("runs", c.get(&format!("{path}/runs")).await?),
        ("token", c.post0(&format!("{path}/token")).await?),
        ("sync", c.post0(&format!("{path}/sync")).await?),
        ("delete", c.delete(&path).await?),
    ];
    for (route, r) in answers {
        assert_eq!(r.status, StatusCode::NOT_FOUND, "{route}: {}", r.json);
    }
    let row: (String, Option<Uuid>, Option<String>) =
        sqlx::query_as("select label, profile_id, token_prefix from source where id = $1")
            .bind(id)
            .fetch_one(env.pool())
            .await?;
    assert_eq!(row, ("Hospital LabNorte".into(), Some(env.profile("alex")), None));

    // the Connections screen reads the health routes, so they keep the source
    let health = c.get("/api/inboxes/health").await?;
    let in_health = health
        .json
        .as_array()
        .is_some_and(|a| a.iter().any(|s| s["id"] == id.to_string().as_str()));
    assert!(in_health, "the health list lost the connector source: {}", health.json);
    let one = c.get(&format!("{path}/health")).await?;
    assert_eq!(one.status, StatusCode::OK, "{}", one.json);
    Ok(())
}

#[tokio::test]
async fn changing_a_client_secret_deletes_the_old_one() -> R {
    let env = env().await?;
    let (household, client, old) = client_with_secret(&env).await?;
    let sb = SecretBox::from_settings(&env.settings)?;
    let mut conn = env.pool().acquire().await?;
    let new = store_secret(
        &mut conn,
        &sb,
        household,
        "connector_client_secret",
        Some("Blue Button"),
        &SecretString::new("fake-client-secret-two"),
    )
    .await?;
    sqlx::query("update connector_client set client_secret_id = $2 where id = $1")
        .bind(client)
        .bind(new)
        .execute(&mut *conn)
        .await?;
    let left: Vec<Uuid> = sqlx::query_scalar("select id from secret order by created_at")
        .fetch_all(&mut *conn)
        .await?;
    assert_eq!(left, vec![new], "the replaced client secret outlived the change");
    // the reverse: an update that keeps the secret keeps it
    sqlx::query("update connector_client set client_secret_id = $2, redirect_uri = 'https://ownhealth.rivera.example/cb' where id = $1")
        .bind(client)
        .bind(new)
        .execute(&mut *conn)
        .await?;
    assert_eq!(env.scalar_i64("select count(*) from secret").await?, 1);
    let _ = old;
    Ok(())
}

#[tokio::test]
async fn one_client_per_vendor_and_issuer() -> R {
    let env = env().await?;
    let household: Uuid = sqlx::query_scalar("select household_id from profile where id = $1")
        .bind(env.profile("alex"))
        .fetch_one(env.pool())
        .await?;
    let insert = |vendor: &'static str, issuer: Option<&'static str>| {
        sqlx::query(
            "insert into connector_client (household_id, vendor, issuer, client_id, redirect_uri) \
             values ($1, $2, $3, 'fake-client-id', 'https://ownhealth.rivera.example/cb')",
        )
        .bind(household)
        .bind(vendor)
        .bind(issuer)
    };
    insert("meditech", Some("https://fhir.hospital-norte.example"))
        .execute(env.pool())
        .await?;
    insert("meditech", Some("https://fhir.hospital-sur.example"))
        .execute(env.pool())
        .await?;
    insert("epic", None).execute(env.pool()).await?;
    for (vendor, issuer) in [
        ("epic", None),
        ("meditech", Some("https://fhir.hospital-norte.example")),
    ] {
        let refused = insert(vendor, issuer)
            .execute(env.pool())
            .await
            .expect_err("a second client for the same vendor and issuer");
        let code = refused.as_database_error().and_then(|e| e.code()).unwrap_or_default();
        assert_eq!(code, "23505", "{vendor}: {refused}");
    }
    Ok(())
}

#[tokio::test]
async fn finding_the_sources_of_a_client_uses_an_index() -> R {
    let env = env().await?;
    let n = env
        .scalar_i64(
            "select count(*) from pg_index i join pg_attribute a on a.attrelid = i.indrelid and a.attnum = i.indkey[0] \
             where i.indrelid = 'source'::regclass and a.attname = 'connector_client_id'",
        )
        .await?;
    assert_eq!(n, 1, "no index leads with source.connector_client_id");
    Ok(())
}
