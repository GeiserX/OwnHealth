//! v1.0 the connector routes (connectors.md 6.2) as a person uses them, against a fake vendor on
//! loopback: connect, see it, reconnect, disconnect; every callback that must not count (another
//! browser, another session, another person, a replay, an expired state, a refused consent); launches
//! for another household; and the revocation before a profile delete. No token ever comes back in an
//! answer, a redirect, the audit log or a log line. Synthetic data only (the Rivera family, LabNorte).

#[path = "auth_support.rs"]
mod support;

#[path = "../../ownhealth-core/tests/connector_fake/mod.rs"]
mod connector_fake;

use std::sync::{Arc, Mutex};

use axum::Extension;
use axum::http::StatusCode;
use connector_fake::{CLIENT_ID, CLIENT_SECRET, Fake, REDIRECT, param};
use ownhealth_api::routes::connectors::{BINDER_COOKIE, EgressOverride};
use ownhealth_core::Runtime;
use ownhealth_core::demo::create_rivera_family;
use ownhealth_core::net::Egress;
use ownhealth_core::secrets::{SecretBox, SecretString, store_secret};
use serde_json::{Value, json};
use support::{Client, Env, Resp};
use uuid::Uuid;

type R = anyhow::Result<()>;

/// 32 bytes of 0x09, base64.
const KEY: &str = "CQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQk=";

struct T {
    env: Env,
    fake: Fake,
    client: Uuid,
}

async fn env() -> anyhow::Result<Env> {
    let mut env = Env::new(true).await?;
    env.settings.secrets_key = Some(KEY.into());
    env.rt = Runtime::with_pool(env.settings.clone(), env.db.pool.clone())?;
    Ok(env)
}

async fn household(env: &Env, profile: Uuid) -> anyhow::Result<Uuid> {
    Ok(sqlx::query_scalar("select household_id from profile where id = $1")
        .bind(profile)
        .fetch_one(env.pool())
        .await?)
}

/// A confidential client of `household` at `fake`, as the wizard (slice 19) will leave it.
async fn client_at(env: &Env, household: Uuid, vendor: &str, fake: &Fake) -> anyhow::Result<Uuid> {
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
    Ok(sqlx::query_scalar(
        "insert into connector_client (household_id, vendor, issuer, client_id, client_secret_id, redirect_uri) \
         values ($1, $2, $3, $4, $5, $6) returning id",
    )
    .bind(household)
    .bind(vendor)
    .bind(fake.issuer())
    .bind(CLIENT_ID)
    .bind(secret)
    .bind(REDIRECT)
    .fetch_one(&mut *c)
    .await?)
}

async fn setup() -> anyhow::Result<T> {
    let env = env().await?;
    let fake = Fake::start().await;
    let h = household(&env, env.profile("alex")).await?;
    let client = client_at(&env, h, "labnorte-ehr", &fake).await?;
    Ok(T { env, fake, client })
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

impl T {
    async fn browser(&self, slug: &str) -> anyhow::Result<Client> {
        Ok(widen(self.env.as_user(self.env.user(slug)).await?))
    }

    async fn launch(&self, c: &mut Client, profile: Uuid) -> anyhow::Result<Resp> {
        c.post(
            "/api/connectors/launch/smart",
            &json!({"profile_id": profile, "client_id": self.client}),
        )
        .await
    }

    /// The vendor's half: the person consents, and the browser comes back with a code.
    async fn callback(&self, c: &mut Client, authorize_url: &str) -> anyhow::Result<Resp> {
        let code = self.fake.consent(authorize_url);
        let state = param(authorize_url, "state").unwrap_or_default();
        c.get(&format!("/api/connectors/callback?state={state}&code={code}"))
            .await
    }

    async fn connect(&self, c: &mut Client, profile: Uuid) -> anyhow::Result<Uuid> {
        let r = self.launch(c, profile).await?;
        assert_eq!(r.status, StatusCode::OK, "{}", r.json);
        let url = r.json["authorize_url"].as_str().unwrap_or_default().to_string();
        let back = self.callback(c, &url).await?;
        assert_eq!(back.status, StatusCode::FOUND, "{}", back.json);
        let loc = back.location();
        let id = loc.strip_prefix("/connections?connected=").unwrap_or_default();
        id.parse().map_err(|_| anyhow::anyhow!("not connected: {loc}"))
    }

    async fn count(&self, sql: &str) -> anyhow::Result<i64> {
        self.env.scalar_i64(sql).await
    }

    /// Every token the fake handed out, and the client secret.
    fn secrets(&self) -> Vec<String> {
        let mut out = self.fake.book().handed_out.clone();
        out.push(CLIENT_SECRET.into());
        out
    }

    fn assert_no_secret(&self, what: &str, text: &str) {
        for s in self.secrets() {
            assert!(!text.contains(&s), "{what} carries {s}: {text}");
        }
    }
}

fn error_of(r: &Resp) -> String {
    r.location()
        .strip_prefix("/connections?connector_error=")
        .unwrap_or("(no error)")
        .to_string()
}

#[tokio::test]
async fn a_person_connects_and_sees_the_connection_and_no_answer_carries_a_token() -> R {
    let t = setup().await?;
    let mut c = t.browser("alex").await?;
    let r = t.launch(&mut c, t.env.profile("alex")).await?;
    assert_eq!(r.status, StatusCode::OK, "{}", r.json);
    let cookie = r
        .set_cookies()
        .into_iter()
        .find(|s| s.starts_with(&format!("{BINDER_COOKIE}=")))
        .unwrap_or_default();
    assert!(
        cookie.contains("HttpOnly") && cookie.contains("Path=/api/connectors") && cookie.contains("Secure"),
        "{cookie}"
    );
    let url = r.json["authorize_url"].as_str().unwrap_or_default().to_string();
    assert!(url.starts_with(&format!("{}/authorize?", t.fake.base)), "{url}");
    let back = t.callback(&mut c, &url).await?;
    assert_eq!(back.status, StatusCode::FOUND);
    let id: Uuid = back
        .location()
        .strip_prefix("/connections?connected=")
        .unwrap_or_default()
        .parse()?;
    // the binder goes once it is used
    assert!(
        back.set_cookies()
            .iter()
            .any(|s| s.starts_with(BINDER_COOKIE) && s.contains("Max-Age=0")),
        "{:?}",
        back.set_cookies()
    );
    // the Connections screen reads the health list
    let health = c.get("/api/inboxes/health").await?;
    let row = health
        .json
        .as_array()
        .and_then(|a| a.iter().find(|s| s["id"] == id.to_string().as_str()).cloned())
        .unwrap_or(Value::Null);
    assert_eq!(
        (row["kind"].as_str(), row["enabled"].as_bool()),
        (Some("smart"), Some(true)),
        "{}",
        health.json
    );
    let row: (Uuid, Option<Uuid>, bool) =
        sqlx::query_as("select profile_id, secret_id, reconsent_needed_at is null from source where id = $1")
            .bind(id)
            .fetch_one(t.env.pool())
            .await?;
    assert_eq!((row.0, row.1.is_some(), row.2), (t.env.profile("alex"), true, true));
    // no token anywhere a person or an agent reads
    for (what, text) in [
        ("launch", r.json.to_string()),
        ("callback", format!("{:?} {}", back.headers, back.json)),
        ("health", health.json.to_string()),
        ("inboxes", c.get("/api/inboxes").await?.json.to_string()),
        ("audit", c.get("/api/audit?limit=200").await?.json.to_string()),
    ] {
        t.assert_no_secret(what, &text);
    }
    let audited = t
        .count("select count(*) from audit_event where action = 'connector.connect' and tool = 'connectors'")
        .await?;
    assert_eq!(audited, 1);
    Ok(())
}

#[tokio::test]
async fn a_callback_counts_only_in_the_browser_and_the_session_that_started_it() -> R {
    let t = setup().await?;
    let alex = t.env.profile("alex");
    let mut a = t.browser("alex").await?;

    // the same person in another browser: no binder cookie
    let r = t.launch(&mut a, alex).await?;
    let url = r.json["authorize_url"].as_str().unwrap_or_default().to_string();
    let mut other_browser = t.browser("alex").await?;
    let back = t.callback(&mut other_browser, &url).await?;
    assert_eq!(error_of(&back), "wrong_browser");
    // and the state is used up: the right browser cannot finish it now
    assert_eq!(error_of(&t.callback(&mut a, &url).await?), "state_invalid");

    // another person in the browser that started it (the binder came along)
    let r = t.launch(&mut a, alex).await?;
    let url = r.json["authorize_url"].as_str().unwrap_or_default().to_string();
    let mut sam = t.browser("sam").await?;
    let binder = a.cookies.get(BINDER_COOKIE).cloned().unwrap_or_default();
    sam.cookies.insert(BINDER_COOKIE.into(), binder.clone());
    assert_eq!(error_of(&t.callback(&mut sam, &url).await?), "wrong_user");

    // the same person, another session, with the binder
    let r = t.launch(&mut a, alex).await?;
    let url = r.json["authorize_url"].as_str().unwrap_or_default().to_string();
    let mut later = t.browser("alex").await?;
    let binder = a.cookies.get(BINDER_COOKIE).cloned().unwrap_or_default();
    later.cookies.insert(BINDER_COOKIE.into(), binder);
    assert_eq!(error_of(&t.callback(&mut later, &url).await?), "wrong_user");

    // logged out
    let r = t.launch(&mut a, alex).await?;
    let url = r.json["authorize_url"].as_str().unwrap_or_default().to_string();
    let mut anon = widen(t.env.anon());
    let binder = a.cookies.get(BINDER_COOKIE).cloned().unwrap_or_default();
    anon.cookies.insert(BINDER_COOKIE.into(), binder);
    assert_eq!(error_of(&t.callback(&mut anon, &url).await?), "wrong_user");

    // the person refused at the vendor: the state goes too
    let r = t.launch(&mut a, alex).await?;
    let url = r.json["authorize_url"].as_str().unwrap_or_default().to_string();
    let state = param(&url, "state").unwrap_or_default();
    let refused = a
        .get(&format!("/api/connectors/callback?state={state}&error=access_denied"))
        .await?;
    assert_eq!(error_of(&refused), "denied");
    assert_eq!(error_of(&t.callback(&mut a, &url).await?), "state_invalid");

    // expired
    let r = t.launch(&mut a, alex).await?;
    let url = r.json["authorize_url"].as_str().unwrap_or_default().to_string();
    sqlx::query("update auth_challenge set expires_at = now() - interval '1 second' where purpose = 'connector' and used_at is null")
        .execute(t.env.pool())
        .await?;
    assert_eq!(error_of(&t.callback(&mut a, &url).await?), "state_invalid");

    // no state, a made-up state
    assert_eq!(
        error_of(&a.get("/api/connectors/callback?code=x").await?),
        "state_invalid"
    );
    assert_eq!(
        error_of(
            &a.get("/api/connectors/callback?state=made-up-state-of-some-length&code=x")
                .await?
        ),
        "state_invalid"
    );
    assert_eq!(t.count("select count(*) from source").await?, 0);
    assert_eq!(
        t.fake.book().auth_seen.len(),
        0,
        "a refused callback reached the vendor"
    );

    // the reverse: the right browser and session connect, once; the replay does not
    let r = t.launch(&mut a, alex).await?;
    let url = r.json["authorize_url"].as_str().unwrap_or_default().to_string();
    let code = t.fake.consent(&url);
    let state = param(&url, "state").unwrap_or_default();
    let ok = a
        .get(&format!("/api/connectors/callback?state={state}&code={code}"))
        .await?;
    assert!(
        ok.location().starts_with("/connections?connected="),
        "{}",
        ok.location()
    );
    assert!(!a.cookies.contains_key(BINDER_COOKIE), "the binder outlived its launch");
    let replay = a
        .get(&format!("/api/connectors/callback?state={state}&code={code}"))
        .await?;
    assert_eq!(error_of(&replay), "state_invalid");
    assert_eq!(t.count("select count(*) from source").await?, 1);
    Ok(())
}

#[tokio::test]
async fn a_launch_for_another_household_or_a_profile_the_caller_does_not_manage_is_refused() -> R {
    let t = setup().await?;
    let ortega = {
        let mut c = t.env.pool().acquire().await?;
        create_rivera_family(&mut c, "Ortega").await?
    };
    let their_household = ortega.household_id;
    let their_fake = Fake::start().await;
    let their_client = client_at(&t.env, their_household, "labnorte-ehr", &their_fake).await?;
    let mut a = t.browser("alex").await?;
    // their profile, with our client
    let r = t.launch(&mut a, ortega.profile("lucia")).await?;
    assert!(matches!(r.status.as_u16(), 403 | 404), "{} {}", r.status, r.json);
    // our profile, with their client
    let r = a
        .post(
            "/api/connectors/launch/smart",
            &json!({"profile_id": t.env.profile("alex"), "client_id": their_client}),
        )
        .await?;
    assert_eq!(r.status, StatusCode::NOT_FOUND, "{}", r.json);
    // a profile Alex only cares for, and Sam on Alex's profile (a viewer)
    let r = t.launch(&mut a, t.env.profile("carmen")).await?;
    assert_eq!(r.status, StatusCode::FORBIDDEN, "{}", r.json);
    let mut sam = t.browser("sam").await?;
    let r = t.launch(&mut sam, t.env.profile("alex")).await?;
    assert_eq!(r.status, StatusCode::FORBIDDEN, "{}", r.json);
    // a kind no slice serves yet, and a kind that is not a connection
    for kind in ["bluebutton", "dexcom", "email", "nope"] {
        let r = a
            .post(
                &format!("/api/connectors/launch/{kind}"),
                &json!({"profile_id": t.env.profile("alex"), "client_id": t.client}),
            )
            .await?;
        assert_eq!(r.status, StatusCode::NOT_FOUND, "{kind}: {}", r.json);
    }
    // a client registered for one server cannot be pointed at another
    let r = a
        .post(
            "/api/connectors/launch/smart",
            &json!({"profile_id": t.env.profile("alex"), "client_id": t.client, "issuer": "https://fhir.otro.example/r4"}),
        )
        .await?;
    assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY, "{}", r.json);
    // the reverse: Alex launches for Lucía, whom Alex guards
    let r = t.launch(&mut a, t.env.profile("lucia")).await?;
    assert_eq!(r.status, StatusCode::OK, "{}", r.json);
    assert_eq!(
        t.count("select count(*) from auth_challenge where purpose = 'connector'")
            .await?,
        1
    );
    // and the other household cannot finish or touch our connections
    let id = t.connect(&mut a, t.env.profile("alex")).await?;
    let mut theirs = widen(t.env.as_user(ortega.user("alex")).await?);
    for r in [
        theirs.post0(&format!("/api/connectors/{id}/reconnect")).await?,
        theirs.delete(&format!("/api/connectors/{id}")).await?,
    ] {
        assert_eq!(r.status, StatusCode::NOT_FOUND, "{}", r.json);
    }
    assert!(t.fake.book().revoked.is_empty());
    Ok(())
}

#[tokio::test]
async fn reconnect_clears_the_reconsent_flag_and_disconnect_revokes_and_removes_the_tokens() -> R {
    let t = setup().await?;
    let mut a = t.browser("alex").await?;
    let id = t.connect(&mut a, t.env.profile("alex")).await?;
    sqlx::query("update source set reconsent_needed_at = now() where id = $1")
        .bind(id)
        .execute(t.env.pool())
        .await?;
    let r = a.post0(&format!("/api/connectors/{id}/reconnect")).await?;
    assert_eq!(r.status, StatusCode::OK, "{}", r.json);
    let url = r.json["authorize_url"].as_str().unwrap_or_default().to_string();
    let back = t.callback(&mut a, &url).await?;
    assert_eq!(back.location(), format!("/connections?connected={id}"));
    let cleared: bool = sqlx::query_scalar("select reconsent_needed_at is null from source where id = $1")
        .bind(id)
        .fetch_one(t.env.pool())
        .await?;
    assert!(cleared, "reconnect left the reconsent flag");
    assert_eq!(t.count("select count(*) from source").await?, 1);
    assert_eq!(
        t.count("select count(*) from secret where purpose = 'connector_refresh_token'")
            .await?,
        1
    );
    // the token the reconnect stored: the newest refresh token the vendor handed out
    let held: String = {
        let book = t.fake.book();
        let newest = book.handed_out.iter().rev().find(|x| x.starts_with("fake-refresh"));
        newest.cloned().unwrap_or_default()
    };

    let r = a.delete(&format!("/api/connectors/{id}")).await?;
    assert_eq!(r.status, StatusCode::OK, "{}", r.json);
    assert_eq!(r.json["revocation"], "revoked");
    t.assert_no_secret("disconnect", &r.json.to_string());
    assert!(
        t.fake.book().revoked.contains(&held),
        "the vendor was not asked to revoke"
    );
    assert_eq!(
        t.count("select count(*) from secret where purpose = 'connector_refresh_token'")
            .await?,
        0
    );
    let row: (bool, Option<Uuid>, Option<Value>) =
        sqlx::query_as("select enabled, secret_id, cursor from source where id = $1")
            .bind(id)
            .fetch_one(t.env.pool())
            .await?;
    assert_eq!(row, (false, None, None));
    let audited: String = sqlx::query_scalar(
        "select args_summary from audit_event where action = 'connector.disconnect' order by id desc limit 1",
    )
    .fetch_one(t.env.pool())
    .await?;
    assert_eq!(audited, "revocation=revoked");
    // a failed revocation is said, and the tokens still go
    let id2 = t.connect(&mut a, t.env.profile("alex")).await?;
    t.fake.set(|b| b.fail_revoke = true);
    let r = a.delete(&format!("/api/connectors/{id2}")).await?;
    assert_eq!(
        (r.status, r.json["revocation"].as_str()),
        (StatusCode::OK, Some("failed")),
        "{}",
        r.json
    );
    assert_eq!(
        t.count("select count(*) from secret where purpose = 'connector_refresh_token'")
            .await?,
        0
    );
    Ok(())
}

#[tokio::test]
async fn a_profile_delete_revokes_at_each_vendor_first_and_shows_a_failed_revocation() -> R {
    let t = setup().await?;
    let down = Fake::start().await;
    down.set(|b| b.fail_revoke = true);
    let h = household(&t.env, t.env.profile("lucia")).await?;
    let down_client = client_at(&t.env, h, "labsur-ehr", &down).await?;
    let mut a = t.browser("alex").await?;
    let up_id = t.connect(&mut a, t.env.profile("lucia")).await?;
    let held_up = t.fake.book().live.iter().next().cloned().unwrap_or_default();
    // the second connection's vendor is down
    let r = a
        .post(
            "/api/connectors/launch/smart",
            &json!({"profile_id": t.env.profile("lucia"), "client_id": down_client, "label": "Hospital LabSur"}),
        )
        .await?;
    let url = r.json["authorize_url"].as_str().unwrap_or_default().to_string();
    let code = down.consent(&url);
    let state = param(&url, "state").unwrap_or_default();
    let back = a
        .get(&format!("/api/connectors/callback?state={state}&code={code}"))
        .await?;
    let down_id: Uuid = back
        .location()
        .strip_prefix("/connections?connected=")
        .unwrap_or_default()
        .parse()?;
    assert_eq!(
        t.count("select count(*) from secret where purpose = 'connector_refresh_token'")
            .await?,
        2
    );

    let r = a.delete("/api/profiles/lucia?confirm=lucia").await?;
    assert_eq!(r.status, StatusCode::OK, "{}", r.json);
    let outcomes: Vec<(String, String)> = r.json["connectors"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|x| {
            (
                x["source_id"].as_str().unwrap_or_default().to_string(),
                x["outcome"].as_str().unwrap_or_default().to_string(),
            )
        })
        .collect();
    assert_eq!(
        outcomes,
        vec![
            (up_id.to_string(), "revoked".to_string()),
            (down_id.to_string(), "failed".to_string())
        ],
        "{}",
        r.json
    );
    assert!(t.fake.book().revoked.contains(&held_up));
    for x in r.json["connectors"].as_array().into_iter().flatten() {
        assert_eq!(x["label"].as_str().map(str::is_empty), Some(false), "{x}");
    }
    t.assert_no_secret("delete", &r.json.to_string());
    // the tokens go either way
    assert_eq!(
        t.count("select count(*) from secret where purpose = 'connector_refresh_token'")
            .await?,
        0
    );
    assert_eq!(t.count("select count(*) from source where kind = 'smart'").await?, 0);
    let audited: Vec<(String, String)> = sqlx::query_as(
        "select args_summary, result_kind from audit_event where action = 'connector.revoke' order by id",
    )
    .fetch_all(t.env.pool())
    .await?;
    assert_eq!(
        audited,
        vec![
            ("revocation=revoked".to_string(), "ok".to_string()),
            ("revocation=failed".to_string(), "error".to_string())
        ]
    );
    Ok(())
}

/// A writer that keeps every log line of the test.
#[derive(Clone, Default)]
struct Captured(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Captured {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[tokio::test]
async fn no_log_line_carries_a_token_on_the_happy_or_the_failing_paths() -> R {
    let logs = Captured::default();
    let writer = logs.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::TRACE)
        .with_writer(move || writer.clone())
        .with_ansi(false)
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);
    let t = setup().await?;
    let mut a = t.browser("alex").await?;
    let id = t.connect(&mut a, t.env.profile("alex")).await?;
    // a code the vendor refuses
    let r = t.launch(&mut a, t.env.profile("alex")).await?;
    let url = r.json["authorize_url"].as_str().unwrap_or_default().to_string();
    let state = param(&url, "state").unwrap_or_default();
    let bad = a
        .get(&format!(
            "/api/connectors/callback?state={state}&code=fake-code-unknown"
        ))
        .await?;
    assert_eq!(error_of(&bad), "exchange_failed");
    // a revocation the vendor refuses
    t.fake.set(|b| b.fail_revoke = true);
    let r = a.delete(&format!("/api/connectors/{id}")).await?;
    assert_eq!(r.json["revocation"], "failed");
    let text = String::from_utf8_lossy(&logs.0.lock().unwrap_or_else(|e| e.into_inner())).to_string();
    assert!(text.contains("exchange failed"), "the capture saw nothing: {text}");
    assert!(text.contains("revocation"), "the capture missed the revocation: {text}");
    t.assert_no_secret("the log", &text);
    Ok(())
}
