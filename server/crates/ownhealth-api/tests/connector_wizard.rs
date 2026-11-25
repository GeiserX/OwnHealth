//! v1.0 the add wizard's routes (slice 19): the vendor profiles with this instance's redirect URI, and
//! the household's own vendor client, saved with its secret sealed and then used by the launch route.
//! No answer, audit row or database column carries the secret in clear. Synthetic data only (the Rivera
//! family, LabNorte).

#[path = "auth_support.rs"]
mod support;

#[path = "../../ownhealth-core/tests/connector_fake/mod.rs"]
mod connector_fake;

use std::time::Duration;

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
        [
            "epic",
            "oracle",
            "athena",
            "healow",
            "nextgen",
            "meditech",
            "va",
            "bluebutton"
        ]
    );
    for v in &vendors {
        assert_eq!(v["verified_live"], false, "{v}");
        assert!(v["scopes"].as_array().is_some_and(|s| !s.is_empty()), "{v}");
        assert!(
            v["registration"].as_str().is_some_and(|u| u.starts_with("https://")),
            "{v}"
        );
    }
    let va = vendors.iter().find(|v| v["key"] == "va").cloned().unwrap_or_default();
    assert_eq!(va["refresh"]["unused_expiry_days"], 42);
    assert_eq!(va["client_type"], "confidential");
    let oracle = vendors
        .iter()
        .find(|v| v["key"] == "oracle")
        .cloned()
        .unwrap_or_default();
    assert_eq!(
        (oracle["client_type"].as_str(), oracle["refresh"]["rotation"].as_str()),
        (Some("public"), Some("none"))
    );

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
async fn each_successful_wizard_read_commits_one_audit_row() -> R {
    let env = env().await?;
    let mut c = alex(&env).await?;
    for path in ["/api/connectors/profiles", "/api/connectors/clients"] {
        for expected in 1..=2 {
            let r = c.get(path).await?;
            assert_eq!(r.status, StatusCode::OK, "{}", r.json);
            let audited: i64 = sqlx::query_scalar(
                "select count(*) from audit_event \
                 where action = 'read' and actor_id = $1 and tool = $2",
            )
            .bind(env.user("alex"))
            .bind(format!("GET {path}"))
            .fetch_one(env.pool())
            .await?;
            assert_eq!(audited, expected, "{path}");
        }
    }
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
        (
            Some("healow"),
            Some(fake.issuer().as_str()),
            Some(CLIENT_ID),
            Some(true),
            Some(REDIRECT)
        ),
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
    assert!(
        !sealed
            .windows(CLIENT_SECRET.len())
            .any(|w| w == CLIENT_SECRET.as_bytes())
    );
    let listed = c.get("/api/connectors/clients").await?;
    assert_eq!(listed.status, StatusCode::OK);
    assert_eq!(
        listed.json["clients"].as_array().map(Vec::len),
        Some(1),
        "{}",
        listed.json
    );
    no_secret("list", &listed.json.to_string(), &[CLIENT_SECRET]);
    assert_eq!(
        count(
            &env,
            "select count(*) from audit_event where action = 'connector.client' and args_summary = 'vendor=healow replaced=false'"
        )
        .await?,
        1
    );
    no_secret(
        "audit",
        &c.get("/api/audit?limit=200").await?.json.to_string(),
        &[CLIENT_SECRET],
    );

    // the launch takes it, and the fake vendor accepts the secret at its token endpoint
    let launch = c
        .post(
            "/api/connectors/launch/smart",
            &json!({"profile_id": env.profile("alex"), "client_id": id, "issuer": fake.issuer(), "label": "Hospital LabNorte"}),
        )
        .await?;
    assert_eq!(launch.status, StatusCode::OK, "{}", launch.json);
    let url = launch.json["authorize_url"].as_str().unwrap_or_default().to_string();
    assert_eq!(
        param(&url, "redirect_uri").map(|r| r.contains("ownhealth.rivera.example")),
        Some(true),
        "{url}"
    );
    let code = fake.consent(&url);
    let state = param(&url, "state").unwrap_or_default();
    let back = c
        .get(&format!("/api/connectors/callback?state={state}&code={code}"))
        .await?;
    let connected = back
        .location()
        .strip_prefix("/connections?connected=")
        .map(String::from);
    assert!(connected.is_some(), "{}", back.location());
    assert_eq!(
        fake.book().auth_seen,
        ["basic"],
        "a confidential client sends its secret"
    );
    assert_eq!(
        count(
            &env,
            &format!("select count(*) from source where connector_client_id = '{id}' and kind = 'smart'")
        )
        .await?,
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
    assert_eq!(
        (again.json["replaced"].as_bool(), again.json["client"]["id"].as_str()),
        (Some(true), Some(id.to_string().as_str()))
    );
    assert_eq!(again.json["client"]["client_id"], "fake-client-id-2");
    for (summary, want) in [("vendor=healow replaced=false", 1), ("vendor=healow replaced=true", 1)] {
        assert_eq!(
            count(
                &env,
                &format!(
                    "select count(*) from audit_event where action = 'connector.client' and args_summary = '{summary}'"
                )
            )
            .await?,
            want,
            "{summary}"
        );
    }
    assert_eq!(
        count(&env, &format!("select count(*) from secret where id = '{old}'")).await?,
        0
    );
    assert_eq!(
        count(
            &env,
            "select count(*) from secret where purpose = 'connector_client_secret'"
        )
        .await?,
        1
    );
    Ok(())
}

/// `n` random hex characters.
fn noise(n: usize) -> String {
    let mut s = String::new();
    while s.len() < n {
        s.push_str(&Uuid::new_v4().simple().to_string());
    }
    s.truncate(n);
    s
}

/// The code of a refused save.
fn refusal(r: &support::Resp) -> String {
    format!(
        "{} {}",
        r.status.as_u16(),
        r.json["detail"]["code"].as_str().unwrap_or("-")
    )
}

#[tokio::test]
async fn each_vendor_takes_the_credential_and_the_server_its_profile_names() -> R {
    let env = env().await?;
    let mut c = alex(&env).await?;
    let secret = "s3cret-pasted-by-mistake-not-real";
    // random, so Postgres cannot compress it under the index row limit (a run of one letter fits)
    let long_issuer = format!("https://fhir.labnorte.example/{}", noise(6000));
    let cases: Vec<(Value, &str)> = vec![
        // Oracle's apps are public: a pasted secret is refused, never stored
        (
            json!({"vendor": "oracle", "client_id": CLIENT_ID, "client_secret": secret}),
            "422 client_secret_unexpected",
        ),
        // healow's are confidential, each for one server
        (
            json!({"vendor": "healow", "client_id": CLIENT_ID}),
            "422 client_secret_missing",
        ),
        (
            json!({"vendor": "healow", "client_id": CLIENT_ID, "client_secret": secret}),
            "422 issuer_required",
        ),
        (
            json!({"vendor": "healow", "client_id": CLIENT_ID, "client_secret": secret, "issuer": "http://fhir.labnorte.example/r4"}),
            "422 bad_issuer",
        ),
        (
            json!({"vendor": "healow", "client_id": CLIENT_ID, "client_secret": secret, "issuer": "javascript:alert(1)"}),
            "422 bad_issuer",
        ),
        (
            json!({"vendor": "labnorte-ehr", "client_id": CLIENT_ID}),
            "422 unknown_vendor",
        ),
        (json!({"vendor": "oracle", "client_id": "  "}), "422 bad_client_id"),
        (
            json!({"vendor": "oracle", "client_id": "two\nlines"}),
            "422 bad_client_id",
        ),
        (
            json!({"vendor": "bluebutton", "client_id": CLIENT_ID, "client_secret": secret, "issuer": "https://bluebutton.labnorte.example/v3/fhir"}),
            "422 bad_issuer",
        ),
        // the server address, the client id and the secret each have a cap
        (
            json!({"vendor": "healow", "client_id": CLIENT_ID, "client_secret": secret, "issuer": long_issuer}),
            "422 bad_issuer",
        ),
        (
            json!({"vendor": "oracle", "client_id": CLIENT_ID, "issuer": long_issuer}),
            "422 bad_issuer",
        ),
        (
            json!({"vendor": "oracle", "client_id": "c".repeat(201)}),
            "422 bad_client_id",
        ),
        (
            json!({"vendor": "healow", "client_id": CLIENT_ID, "client_secret": "s".repeat(1001), "issuer": "https://fhir.labnorte.example/r4"}),
            "422 bad_client_secret",
        ),
        // a body of the wrong shape answers 422 without repeating the secret it carried
        (
            json!({"vendor": "healow", "client_id": 7, "client_secret": secret}),
            "422 -",
        ),
    ];
    // every case runs, and the failure names each one that answered otherwise
    let mut wrong = Vec::new();
    for (body, want) in cases {
        let r = c.post("/api/connectors/clients", &body).await?;
        if refusal(&r) != want {
            let shown: String = body.to_string().chars().take(160).collect();
            wrong.push(format!("{shown}: want {want}, got {} {}", refusal(&r), r.json));
        }
        no_secret("a refusal", &r.json.to_string(), &[secret]);
    }
    assert!(wrong.is_empty(), "{wrong:#?}");
    assert_eq!(
        count(&env, "select count(*) from secret").await?,
        0,
        "no refusal kept a secret"
    );
    assert_eq!(count(&env, "select count(*) from connector_client").await?, 0);

    // the caps themselves are allowed
    let r = c
        .post(
            "/api/connectors/clients",
            &json!({"vendor": "healow", "client_id": "c".repeat(200), "client_secret": "s".repeat(1000),
                    "issuer": format!("https://fhir.labnorte.example/{}", noise(2048 - 30))}),
        )
        .await?;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.json);
    sqlx::query("delete from connector_client").execute(env.pool()).await?;

    // Oracle with no server: one client id for every Oracle server
    let r = c
        .post(
            "/api/connectors/clients",
            &json!({"vendor": "oracle", "client_id": CLIENT_ID}),
        )
        .await?;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.json);
    assert_eq!(
        (
            r.json["client"]["issuer"].clone(),
            r.json["client"]["confidential"].clone()
        ),
        (Value::Null, json!(false))
    );
    // Blue Button: production unless the sandbox is named
    for (issuer, want) in [
        (Value::Null, "https://api.bluebutton.cms.gov/v3/fhir"),
        (
            json!("https://sandbox.bluebutton.cms.gov/v3/fhir/"),
            "https://sandbox.bluebutton.cms.gov/v3/fhir",
        ),
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

/// Save the household's healow client at the fake vendor, as `c`.
async fn save_healow(c: &mut Client, fake: &Fake, client_id: &str, secret: &str) -> anyhow::Result<support::Resp> {
    c.post(
        "/api/connectors/clients",
        &json!({"vendor": "healow", "client_id": client_id, "client_secret": secret, "issuer": fake.issuer()}),
    )
    .await
}

/// Connect `profile`'s record at the fake vendor with the saved client `id`, as `c`.
async fn connect(env: &Env, c: &mut Client, fake: &Fake, id: &str, profile: &str) -> R {
    let launch = c
        .post(
            "/api/connectors/launch/smart",
            &json!({"profile_id": env.profile(profile), "client_id": id, "issuer": fake.issuer(), "label": format!("LabNorte {profile}")}),
        )
        .await?;
    assert_eq!(launch.status, StatusCode::OK, "{}", launch.json);
    let url = launch.json["authorize_url"].as_str().unwrap_or_default().to_string();
    let code = fake.consent(&url);
    let state = param(&url, "state").unwrap_or_default();
    let back = c
        .get(&format!("/api/connectors/callback?state={state}&code={code}"))
        .await?;
    assert!(
        back.location().starts_with("/connections?connected="),
        "{}",
        back.location()
    );
    Ok(())
}

#[tokio::test]
async fn another_household_lists_none_of_these_clients_and_saves_its_own() -> R {
    let env = env().await?;
    let fake = Fake::recorded("healow").await;
    let mut c = alex(&env).await?;
    let first = save_healow(&mut c, &fake, CLIENT_ID, CLIENT_SECRET).await?;
    assert_eq!(first.status, StatusCode::CREATED, "{}", first.json);

    let mut conn = env.pool().acquire().await?;
    let other =
        ownhealth_core::demo::create_rivera_family(&mut conn, &format!("Rivera {}", ownhealth_core::ids::token_hex(3)))
            .await?;
    drop(conn);
    let mut alex2 = widen(env.as_user(other.user("alex")).await?);
    let listed = alex2.get("/api/connectors/clients").await?;
    assert_eq!(listed.status, StatusCode::OK, "{}", listed.json);
    assert_eq!(
        listed.json["clients"],
        json!([]),
        "another household's clients stay hidden"
    );

    // the same vendor and server in the other household is its own client, not a replace
    let theirs = save_healow(&mut alex2, &fake, "rivera-2-app", "rivera-2-secret-not-real").await?;
    assert_eq!(theirs.status, StatusCode::CREATED, "{}", theirs.json);
    assert_eq!(theirs.json["replaced"], false);
    assert_ne!(theirs.json["client"]["id"], first.json["client"]["id"]);
    let mine = c.get("/api/connectors/clients").await?;
    assert_eq!(mine.json["clients"].as_array().map(Vec::len), Some(1), "{}", mine.json);
    assert_eq!(mine.json["clients"][0]["client_id"], CLIENT_ID);
    assert_eq!(count(&env, "select count(*) from connector_client").await?, 2);
    Ok(())
}

#[tokio::test]
async fn a_client_that_connections_of_profiles_the_caller_does_not_manage_use_is_not_theirs_to_replace() -> R {
    let env = env().await?;
    let fake = Fake::recorded("healow").await;
    let mut alex = alex(&env).await?;
    // Sam owns Sam, is a guardian of Lucía, and only views Alex
    let mut sam = widen(env.as_user(env.user("sam")).await?);
    let saved = save_healow(&mut alex, &fake, CLIENT_ID, CLIENT_SECRET).await?;
    assert_eq!(saved.status, StatusCode::CREATED, "{}", saved.json);
    let id = saved.json["client"]["id"].as_str().unwrap_or_default().to_string();

    // while only Lucía's record uses it, Sam manages every connection on it and may replace it
    connect(&env, &mut alex, &fake, &id, "lucia").await?;
    let r = save_healow(&mut sam, &fake, CLIENT_ID, CLIENT_SECRET).await?;
    assert_eq!(r.status, StatusCode::OK, "{}", r.json);
    assert_eq!(r.json["replaced"], true);

    // once Alex's own record uses it too, Sam's replace is refused and the client stays as it was
    connect(&env, &mut alex, &fake, &id, "alex").await?;
    let before: (String, Option<Uuid>) =
        sqlx::query_as("select client_id, client_secret_id from connector_client where id = $1::uuid")
            .bind(&id)
            .fetch_one(env.pool())
            .await?;
    let r = save_healow(&mut sam, &fake, "sam-own-healow-app", "sam-own-secret-not-real").await?;
    assert_eq!(refusal(&r), "403 client_in_use", "{}", r.json);
    let after: (String, Option<Uuid>) =
        sqlx::query_as("select client_id, client_secret_id from connector_client where id = $1::uuid")
            .bind(&id)
            .fetch_one(env.pool())
            .await?;
    assert_eq!(after, before);
    assert_eq!(
        count(
            &env,
            "select count(*) from secret where purpose = 'connector_client_secret'"
        )
        .await?,
        1,
        "the refused secret is not kept"
    );
    // Alex manages both records, so he still may
    let r = save_healow(&mut alex, &fake, "fake-client-id-2", "fake-client-secret-2-not-real").await?;
    assert_eq!(r.status, StatusCode::OK, "{}", r.json);
    Ok(())
}

// Return the backend blocked by this test's save or barrier, so the test can order the commits
// without relying on a delay being long enough for a request to have reached the database.
async fn blocked_by(env: &Env, blocker: i32) -> anyhow::Result<i32> {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Some(pid) = sqlx::query_scalar(
                "select pid from pg_stat_activity \
                 where datname = current_database() and $1 = any(pg_blocking_pids(pid))",
            )
            .bind(blocker)
            .fetch_optional(env.pool())
            .await?
            {
                return Ok(pid);
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await?
}

#[tokio::test]
async fn a_concurrent_first_save_cannot_replace_a_client_as_its_first_connection_commits() -> R {
    let env = env().await?;
    let mut alex = alex(&env).await?;
    let mut sam = widen(env.as_user(env.user("sam")).await?);
    // Pause Alex's first save with the client and a source for Alex already inserted. When
    // released, they become visible together, as if the source committed before Sam's upsert.
    sqlx::raw_sql(
        "create function pause_first_client() returns trigger language plpgsql as $$
         begin
             insert into source (household_id, profile_id, kind, label, connector_client_id)
                 select new.household_id, id, 'smart', 'LabNorte', new.id from profile
                 where household_id = new.household_id and slug = 'alex';
             perform pg_advisory_xact_lock(19, 2);
             return null;
         end;
         $$;
         create trigger pause_first_client after insert on connector_client for each row
         when (new.client_id = 'alex-oracle-app') execute function pause_first_client();",
    )
    .execute(env.pool())
    .await?;
    let mut barrier = env.pool().begin().await?;
    let barrier_pid: i32 = sqlx::query_scalar("select pg_backend_pid()")
        .fetch_one(&mut *barrier)
        .await?;
    sqlx::query("select pg_advisory_xact_lock(19, 2)")
        .execute(&mut *barrier)
        .await?;

    let alex_body = json!({"vendor": "oracle", "client_id": "alex-oracle-app"});
    let (first, second) = tokio::try_join!(alex.post("/api/connectors/clients", &alex_body), async {
        let first_pid = blocked_by(&env, barrier_pid).await?;
        let sam_body = json!({"vendor": "oracle", "client_id": "sam-oracle-app"});
        let (second, ()) = tokio::try_join!(sam.post("/api/connectors/clients", &sam_body), async {
            blocked_by(&env, first_pid).await?;
            barrier.commit().await?;
            anyhow::Ok(())
        })?;
        Ok(second)
    })?;
    assert_eq!(first.status, StatusCode::CREATED, "{}", first.json);
    assert_eq!(refusal(&second), "403 client_in_use", "{}", second.json);
    let saved: String = sqlx::query_scalar(
        "select c.client_id from connector_client c join source s on s.connector_client_id = c.id \
         where s.profile_id = $1",
    )
    .bind(env.profile("alex"))
    .fetch_one(env.pool())
    .await?;
    assert_eq!(saved, "alex-oracle-app");
    Ok(())
}

#[tokio::test]
async fn a_first_launch_at_an_address_with_no_smart_configuration_says_so() -> R {
    let env = env().await?;
    let fake = Fake::recorded("oracle").await;
    let mut c = alex(&env).await?;
    let saved = c
        .post(
            "/api/connectors/clients",
            &json!({"vendor": "oracle", "client_id": CLIENT_ID}),
        )
        .await?;
    assert_eq!(saved.status, StatusCode::CREATED, "{}", saved.json);
    let id = saved.json["client"]["id"].clone();
    // a path with no SMART document, and a name that does not resolve
    for issuer in [
        format!("{}/es", fake.issuer()),
        "https://fhir.nowhere.invalid/r4".to_string(),
    ] {
        let r = c
            .post(
                "/api/connectors/launch/smart",
                &json!({"profile_id": env.profile("alex"), "client_id": id, "issuer": issuer}),
            )
            .await?;
        assert_eq!(refusal(&r), "502 not_a_smart_server", "{issuer}: {}", r.json);
    }
    // the right address still launches
    let r = c
        .post(
            "/api/connectors/launch/smart",
            &json!({"profile_id": env.profile("alex"), "client_id": id, "issuer": fake.issuer()}),
        )
        .await?;
    assert_eq!(r.status, StatusCode::OK, "{}", r.json);
    Ok(())
}
