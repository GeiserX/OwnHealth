//! v1.0 the add wizard's routes (slice 19): the vendor profiles with this instance's redirect URI, and
//! the household's own vendor client, saved with its secret sealed and then used by the launch route.
//! No answer, audit row or database column carries the secret in clear. Synthetic data only (the Rivera
//! family, LabNorte).

#[path = "auth_support.rs"]
mod support;

#[path = "../../ownhealth-core/tests/connector_fake/mod.rs"]
mod connector_fake;

use axum::Extension;
use axum::http::StatusCode;
use connector_fake::{CLIENT_ID, CLIENT_SECRET, Fake, REDIRECT, param};
use ownhealth_api::routes::connectors::EgressOverride;
use ownhealth_core::Runtime;
use ownhealth_core::net::Egress;
use serde_json::{Value, json};
use support::{Client, Env};
use uuid::Uuid;

type R = anyhow::Result<()>;

/// 32 bytes of 0x09, base64.
const KEY: &str = "CQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQk=";
/// The instance the fake vendor's registration names (its REDIRECT).
const INSTANCE: &str = "https://ownhealth.rivera.example";

async fn env() -> anyhow::Result<Env> {
    let mut env = Env::new(true).await?;
    env.settings.secrets_key = Some(KEY.into());
    env.settings.base_url = INSTANCE.into();
    env.rt = Runtime::with_pool(env.settings.clone(), env.db.pool.clone())?;
    Ok(env)
}

/// The fake is plain http on loopback: the routes may reach it only through this test extension.
fn widen(mut c: Client) -> Client {
    c.app = c.app.layer(Extension(EgressOverride(Egress {
        allow_private: true,
        https_only: false,
        ..Egress::connector()
    })));
    c
}

async fn alex(env: &Env) -> anyhow::Result<Client> {
    Ok(widen(env.as_user(env.user("alex")).await?))
}

async fn count(env: &Env, sql: &str) -> anyhow::Result<i64> {
    env.scalar_i64(sql).await
}

fn no_secret(what: &str, text: &str, secrets: &[&str]) {
    for s in secrets {
        assert!(!text.contains(s), "{what} carries the client secret: {text}");
    }
}

#[tokio::test]
async fn the_wizard_reads_every_vendor_profile_with_this_instances_redirect_uri() -> R {
    let env = env().await?;
    let mut c = alex(&env).await?;
    let r = c.get("/api/connectors/profiles").await?;
    assert_eq!(r.status, StatusCode::OK, "{}", r.json);
    assert_eq!(r.json["redirect_uri"], REDIRECT);
    let vendors = r.json["vendors"].as_array().cloned().unwrap_or_default();
    let keys: Vec<&str> = vendors.iter().filter_map(|v| v["key"].as_str()).collect();
    assert_eq!(
        keys,
        ["epic", "oracle", "athena", "healow", "nextgen", "meditech", "va", "bluebutton"]
    );
    for v in &vendors {
        assert_eq!(v["verified_live"], false, "{v}");
        assert!(v["scopes"].as_array().is_some_and(|s| !s.is_empty()), "{v}");
        assert!(v["registration"].as_str().is_some_and(|u| u.starts_with("https://")), "{v}");
    }
    let va = vendors.iter().find(|v| v["key"] == "va").cloned().unwrap_or_default();
    assert_eq!(va["refresh"]["unused_expiry_days"], 42);
    assert_eq!(va["client_type"], "confidential");
    let oracle = vendors.iter().find(|v| v["key"] == "oracle").cloned().unwrap_or_default();
    assert_eq!((oracle["client_type"].as_str(), oracle["refresh"]["rotation"].as_str()), (Some("public"), Some("none")));

    // a person who manages no profile reads no profile, lists no client and saves none
    sqlx::query("update profile_grant set role = 'viewer' where user_id = $1 and role in ('owner', 'guardian')")
        .bind(env.user("alex"))
        .execute(env.pool())
        .await?;
    let mut viewer = alex(&env).await?;
    let saved = viewer
        .post(
            "/api/connectors/clients",
            &json!({"vendor": "oracle", "client_id": CLIENT_ID}),
        )
        .await?;
    for r in [
        viewer.get("/api/connectors/profiles").await?,
        viewer.get("/api/connectors/clients").await?,
        saved,
    ] {
        assert_eq!(r.status, StatusCode::FORBIDDEN, "{}", r.json);
    }
    assert_eq!(count(&env, "select count(*) from connector_client").await?, 0);
    Ok(())
}

#[tokio::test]
async fn a_confidential_client_is_saved_sealed_and_the_launch_connects_with_it() -> R {
    let env = env().await?;
    let fake = Fake::recorded("healow").await;
    let mut c = alex(&env).await?;
    let r = c
        .post(
            "/api/connectors/clients",
            &json!({"vendor": "healow", "client_id": format!(" {CLIENT_ID} "), "client_secret": CLIENT_SECRET,
                    "issuer": format!("{}/", fake.issuer())}),
        )
        .await?;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.json);
    assert_eq!(r.json["replaced"], false);
    let client = &r.json["client"];
    assert_eq!(
        (
            client["vendor"].as_str(),
            client["issuer"].as_str(),
            client["client_id"].as_str(),
            client["confidential"].as_bool(),
            client["redirect_uri"].as_str()
        ),
        (Some("healow"), Some(fake.issuer().as_str()), Some(CLIENT_ID), Some(true), Some(REDIRECT)),
        "{client}"
    );
    let id: Uuid = client["id"].as_str().unwrap_or_default().parse()?;
    no_secret("save", &r.json.to_string(), &[CLIENT_SECRET]);

    // sealed in the secret table, never in clear
    let (purpose, sealed): (String, Vec<u8>) = sqlx::query_as(
        "select s.purpose, s.ciphertext from connector_client c join secret s on s.id = c.client_secret_id where c.id = $1",
    )
    .bind(id)
    .fetch_one(env.pool())
    .await?;
    assert_eq!(purpose, "connector_client_secret");
    assert!(!sealed.windows(CLIENT_SECRET.len()).any(|w| w == CLIENT_SECRET.as_bytes()));
    let listed = c.get("/api/connectors/clients").await?;
    assert_eq!(listed.status, StatusCode::OK);
    assert_eq!(listed.json["clients"].as_array().map(Vec::len), Some(1), "{}", listed.json);
    no_secret("list", &listed.json.to_string(), &[CLIENT_SECRET]);
    assert_eq!(
        count(&env, "select count(*) from audit_event where action = 'connector.client'").await?,
        1
    );
    no_secret("audit", &c.get("/api/audit?limit=200").await?.json.to_string(), &[CLIENT_SECRET]);

    // the launch takes it, and the fake vendor accepts the secret at its token endpoint
    let launch = c
        .post(
            "/api/connectors/launch/smart",
            &json!({"profile_id": env.profile("alex"), "client_id": id, "issuer": fake.issuer(), "label": "Hospital LabNorte"}),
        )
        .await?;
    assert_eq!(launch.status, StatusCode::OK, "{}", launch.json);
    let url = launch.json["authorize_url"].as_str().unwrap_or_default().to_string();
    assert_eq!(param(&url, "redirect_uri").map(|r| r.contains("ownhealth.rivera.example")), Some(true), "{url}");
    let code = fake.consent(&url);
    let state = param(&url, "state").unwrap_or_default();
    let back = c.get(&format!("/api/connectors/callback?state={state}&code={code}")).await?;
    let connected = back.location().strip_prefix("/connections?connected=").map(String::from);
    assert!(connected.is_some(), "{}", back.location());
    assert_eq!(fake.book().auth_seen, ["basic"], "a confidential client sends its secret");
    assert_eq!(
        count(&env, &format!("select count(*) from source where connector_client_id = '{id}' and kind = 'smart'")).await?,
        1
    );

    // registering again replaces the client and its secret, and the old sealed secret goes
    let old: Uuid = sqlx::query_scalar("select client_secret_id from connector_client where id = $1")
        .bind(id)
        .fetch_one(env.pool())
        .await?;
    let again = c
        .post(
            "/api/connectors/clients",
            &json!({"vendor": "healow", "client_id": "fake-client-id-2", "client_secret": "fake-client-secret-2-not-real",
                    "issuer": fake.issuer()}),
        )
        .await?;
    assert_eq!(again.status, StatusCode::OK, "{}", again.json);
    assert_eq!((again.json["replaced"].as_bool(), again.json["client"]["id"].as_str()), (Some(true), Some(id.to_string().as_str())));
    assert_eq!(again.json["client"]["client_id"], "fake-client-id-2");
    assert_eq!(count(&env, &format!("select count(*) from secret where id = '{old}'")).await?, 0);
    assert_eq!(
        count(&env, "select count(*) from secret where purpose = 'connector_client_secret'").await?,
        1
    );
    Ok(())
}

/// The code of a refused save.
fn refusal(r: &support::Resp) -> String {
    format!("{} {}", r.status.as_u16(), r.json["detail"]["code"].as_str().unwrap_or("-"))
}

#[tokio::test]
async fn each_vendor_takes_the_credential_and_the_server_its_profile_names() -> R {
    let env = env().await?;
    let mut c = alex(&env).await?;
    let secret = "s3cret-pasted-by-mistake-not-real";
    let cases: [(Value, &str); 10] = [
        // Oracle's apps are public: a pasted secret is refused, never stored
        (json!({"vendor": "oracle", "client_id": CLIENT_ID, "client_secret": secret}), "422 client_secret_unexpected"),
        // healow's are confidential, each for one server
        (json!({"vendor": "healow", "client_id": CLIENT_ID}), "422 client_secret_missing"),
        (json!({"vendor": "healow", "client_id": CLIENT_ID, "client_secret": secret}), "422 issuer_required"),
        (
            json!({"vendor": "healow", "client_id": CLIENT_ID, "client_secret": secret, "issuer": "http://fhir.labnorte.example/r4"}),
            "422 bad_issuer",
        ),
        (
            json!({"vendor": "healow", "client_id": CLIENT_ID, "client_secret": secret, "issuer": "javascript:alert(1)"}),
            "422 bad_issuer",
        ),
        (json!({"vendor": "labnorte-ehr", "client_id": CLIENT_ID}), "422 unknown_vendor"),
        (json!({"vendor": "oracle", "client_id": "  "}), "422 bad_client_id"),
        (json!({"vendor": "oracle", "client_id": "two\nlines"}), "422 bad_client_id"),
        (
            json!({"vendor": "bluebutton", "client_id": CLIENT_ID, "client_secret": secret, "issuer": "https://bluebutton.labnorte.example/v3/fhir"}),
            "422 bad_issuer",
        ),
        // a body of the wrong shape answers 422 without repeating the secret it carried
        (json!({"vendor": "healow", "client_id": 7, "client_secret": secret}), "422 -"),
    ];
    for (body, want) in cases {
        let r = c.post("/api/connectors/clients", &body).await?;
        assert_eq!(refusal(&r), want, "{body}: {}", r.json);
        no_secret("a refusal", &r.json.to_string(), &[secret]);
    }
    assert_eq!(count(&env, "select count(*) from secret").await?, 0, "no refusal kept a secret");
    assert_eq!(count(&env, "select count(*) from connector_client").await?, 0);

    // Oracle with no server: one client id for every Oracle server
    let r = c
        .post("/api/connectors/clients", &json!({"vendor": "oracle", "client_id": CLIENT_ID}))
        .await?;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.json);
    assert_eq!((r.json["client"]["issuer"].clone(), r.json["client"]["confidential"].clone()), (Value::Null, json!(false)));
    // Blue Button: production unless the sandbox is named
    for (issuer, want) in [
        (Value::Null, "https://api.bluebutton.cms.gov/v3/fhir"),
        (json!("https://sandbox.bluebutton.cms.gov/v3/fhir/"), "https://sandbox.bluebutton.cms.gov/v3/fhir"),
    ] {
        let r = c
            .post(
                "/api/connectors/clients",
                &json!({"vendor": "bluebutton", "client_id": "e2e-medicare", "client_secret": secret, "issuer": issuer}),
            )
            .await?;
        assert_eq!(r.status, StatusCode::CREATED, "{}", r.json);
        assert_eq!(r.json["client"]["issuer"], want);
    }
    Ok(())
}
