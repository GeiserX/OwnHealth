//! The connection profiles of `data/connectors/connectors.yaml` drive the token code: each vendor
//! connects and refreshes against its recorded fixtures (`fixtures/connectors/<vendor>/`, served by
//! the fake vendor), and the stored tokens and dates follow its profile. Oracle keeps one refresh
//! token, healow's rotate, the VA's end 42 days after their last use. Synthetic data only (the Rivera
//! family).

mod common;
mod connector_fake;

use std::sync::Arc;

use chrono::{DateTime, Duration, TimeZone, Utc};
use common::{R, family};
use connector_fake::{CLIENT_ID, CLIENT_SECRET, Fake, REDIRECT, param};
use ownhealth_core::connector_oauth::{self as oauth, ConnectorClient, Endpoints, Launch, OAuthError};
use ownhealth_core::connector_profiles::{self as profiles, ClientType, Vendor};
use ownhealth_core::demo::Family;
use ownhealth_core::keys::{Kek, Keyring};
use ownhealth_core::net::Egress;
use ownhealth_core::secrets::{SecretBox, SecretString, read_secret, store_secret};
use ownhealth_core::testing::{TestDb, random_secrets_key};
use serde_json::Value;
use uuid::Uuid;

const BINDER: &str = "binder-of-alex-browser-0001";
/// The SMART vendors with recorded fixtures.
const RECORDED: [&str; 7] = ["epic", "oracle", "athena", "healow", "nextgen", "meditech", "va"];

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

struct V {
    key: &'static str,
    profile: &'static Vendor,
    db: TestDb,
    sb: SecretBox,
    fam: Family,
    fake: Fake,
    client: ConnectorClient,
    endpoints: Endpoints,
}

/// The household's client of `key` at the vendor's fake, as the wizard saves it: a confidential
/// client with its secret for its one server, a public client for every server.
async fn vendor(key: &'static str) -> anyhow::Result<V> {
    let profile = profiles::vendor(key).ok_or_else(|| anyhow::anyhow!("{key} has no profile"))?;
    let db = TestDb::new().await?;
    let mut c = db.pool.acquire().await?;
    let fam = family(&mut c).await?;
    let kek = Kek::from_base64(&random_secrets_key())?;
    let sb = SecretBox::new(&[5u8; 32])?.with_keys(Some(Arc::new(Keyring::new(kek, db.url.clone()))));
    let fake = Fake::recorded(key).await;
    let (secret, issuer) = match profile.client_type {
        ClientType::Confidential => (
            Some(
                store_secret(
                    &mut c,
                    &sb,
                    fam.household_id,
                    "connector_client_secret",
                    Some(&profile.name),
                    &SecretString::new(CLIENT_SECRET),
                )
                .await?,
            ),
            Some(fake.issuer()),
        ),
        ClientType::Public => (None, None),
    };
    let id: Uuid = sqlx::query_scalar(
        "insert into connector_client (household_id, vendor, issuer, client_id, client_secret_id, redirect_uri) \
         values ($1, $2, $3, $4, $5, $6) returning id",
    )
    .bind(fam.household_id)
    .bind(key)
    .bind(issuer)
    .bind(CLIENT_ID)
    .bind(secret)
    .bind(REDIRECT)
    .fetch_one(&mut *c)
    .await?;
    let client = oauth::load_client(&mut c, Some(&sb), fam.household_id, id)
        .await?
        .expect("the client");
    let spec = oauth::kind_spec(&profile.kind).expect("a served kind");
    let endpoints = oauth::resolve(&spec, &fake.issuer(), &egress()).await?;
    drop(c);
    Ok(V {
        key,
        profile,
        db,
        sb,
        fam,
        fake,
        client,
        endpoints,
    })
}

/// source dates: access_expires_at, grant_expires_at
type Dates = (Option<DateTime<Utc>>, Option<DateTime<Utc>>);

impl V {
    async fn connect(&self, now: DateTime<Utc>) -> anyhow::Result<Uuid> {
        let mut c = self.db.pool.acquire().await?;
        let url = oauth::begin(
            &mut c,
            &Launch {
                kind: &self.profile.kind,
                client: &self.client,
                endpoints: &self.endpoints,
                profile_id: self.fam.profile("alex"),
                label: &self.profile.name,
                source_id: None,
                user_id: Some(self.fam.user("alex")),
                session_id: None,
                binder: BINDER,
            },
        )
        .await?
        .url;
        let code = self.fake.consent(&url);
        let p = oauth::take_state(&mut c, &param(&url, "state").unwrap_or_default(), BINDER).await?;
        let t = oauth::exchange(&p, &self.client, &code, &egress()).await?;
        Ok(oauth::save_connection(&mut c, &self.sb, &p, &self.client, &t, now).await?)
    }

    async fn refresh(&self, id: Uuid, now: DateTime<Utc>) -> Result<SecretString, OAuthError> {
        oauth::access_token(&self.db.pool, &self.sb, id, &egress(), now).await
    }

    async fn dates(&self, id: Uuid) -> anyhow::Result<Dates> {
        Ok(
            sqlx::query_as("select access_expires_at, grant_expires_at from source where id = $1")
                .bind(id)
                .fetch_one(&self.db.pool)
                .await?,
        )
    }

    /// The refresh token held for the source, opened.
    async fn held(&self, id: Uuid) -> anyhow::Result<Option<String>> {
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

    /// The `expires_in` of the vendor's recorded answer.
    fn expires_in(&self, which: &str) -> Option<i64> {
        let reply: Option<Value> = match which {
            "code" => self.fake_behaviour(|b| b.code_reply.clone()),
            _ => self.fake_behaviour(|b| b.refresh_reply.clone()),
        };
        reply.and_then(|r| r["expires_in"].as_i64())
    }

    fn fake_behaviour<T>(&self, f: impl FnOnce(&connector_fake::Behaviour) -> T) -> T {
        let mut out = None;
        self.fake.set(|b| out = Some(f(b)));
        out.expect("read the fake")
    }

    fn grant_after(&self, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
        self.profile
            .refresh
            .unused_expiry_days
            .map(|d| now + Duration::days(i64::from(d)))
    }
}

#[tokio::test]
async fn every_recorded_vendor_connects_and_refreshes_the_way_its_profile_says() -> R {
    for key in RECORDED {
        let v = vendor(key).await?;
        // the launch asks for exactly the scopes the wizard told the person to tick at the vendor
        let asked: Vec<&str> = v.endpoints.scopes.split_whitespace().collect();
        assert_eq!(asked, v.profile.scopes, "{key}: the scopes of the launch");
        // what the vendor's own document says comes through: a revocation endpoint only where it names one
        let named = v.fake_behaviour(|b| b.discovery.clone()).unwrap_or_default();
        assert_eq!(
            v.endpoints.revocation.is_some(),
            named.get("revocation_endpoint").is_some(),
            "{key}"
        );

        let id = v.connect(t0()).await?;
        let first = v.held(id).await?.expect("a refresh token");
        let (access, grant) = v.dates(id).await?;
        let lasts = v.expires_in("code").expect("the recorded answer names expires_in");
        assert_eq!(access, Some(t0() + Duration::seconds(lasts)), "{key}: access token");
        assert_eq!(
            grant,
            v.grant_after(t0()),
            "{key}: the sign-in ends as the profile says"
        );

        // two days later the access token is long gone: a refresh
        let t1 = t0() + Duration::days(2);
        v.refresh(id, t1).await?;
        let second = v.held(id).await?.expect("still a refresh token");
        let rotates = v.fake_behaviour(|b| b.rotate);
        if rotates {
            assert_ne!(second, first, "{key}: a rotating vendor's new token is kept");
            assert!(!v.fake.book().live.contains(&first), "{key}: and the old one is dead");
        } else {
            assert_eq!(second, first, "{key}: the same refresh token stays");
            assert!(v.fake.book().live.contains(&first), "{key}");
        }
        let (_, grant) = v.dates(id).await?;
        assert_eq!(
            grant,
            v.grant_after(t1),
            "{key}: each refresh starts the unused count again"
        );

        // and the next day's refresh works with whatever is held
        v.refresh(id, t1 + Duration::days(1)).await?;
        assert_eq!(v.dates(id).await?.1, v.grant_after(t1 + Duration::days(1)), "{key}");
        assert_eq!(v.fake.book().refresh_calls, 2, "{key}");
    }
    Ok(())
}

#[tokio::test]
async fn a_rotating_vendor_that_sends_no_new_token_retired_the_old_one_and_it_is_never_sent_again() -> R {
    let v = vendor("healow").await?;
    assert_eq!(v.key, "healow");
    let id = v.connect(t0()).await?;
    let old = v.held(id).await?.expect("a refresh token");
    let secret: Option<Uuid> = sqlx::query_scalar("select secret_id from source where id = $1")
        .bind(id)
        .fetch_one(&v.db.pool)
        .await?;
    // the vendor rotates (it retires the token it is sent) but this answer carries no new one
    v.fake.set(|b| {
        if let Some(r) = b.refresh_reply.as_mut().and_then(Value::as_object_mut) {
            r.shift_remove("refresh_token");
        }
    });
    let t1 = t0() + Duration::days(2);
    v.refresh(id, t1).await?;
    assert!(!v.fake.book().live.contains(&old));
    assert_eq!(v.held(id).await?, None, "the retired token is not kept");
    let left: i64 = sqlx::query_scalar("select count(*) from secret where id = $1")
        .bind(secret)
        .fetch_one(&v.db.pool)
        .await?;
    assert_eq!(left, 0, "its sealed copy is deleted");
    // the sign-in now ends with the access token, and the screen can say so
    let (access, grant) = v.dates(id).await?;
    assert_eq!(access, Some(t1 + Duration::seconds(3600)));
    assert_eq!(grant, access);
    // once the access token ends, the connection asks for consent without sending the dead token
    let calls = v.fake.book().refresh_calls;
    let r = v.refresh(id, t1 + Duration::hours(2)).await;
    assert!(matches!(r, Err(OAuthError::ReconsentNeeded)), "{r:?}");
    assert_eq!(v.fake.book().refresh_calls, calls);
    Ok(())
}

#[tokio::test]
async fn an_answer_without_expires_in_lasts_as_long_as_the_profile_says() -> R {
    // Oracle documents 570 seconds; the answer leaves it out
    let oracle = vendor("oracle").await?;
    oracle.fake.set(|b| {
        for r in [b.code_reply.as_mut(), b.refresh_reply.as_mut()].into_iter().flatten() {
            if let Some(m) = r.as_object_mut() {
                m.shift_remove("expires_in");
            }
        }
    });
    let id = oracle.connect(t0()).await?;
    assert_eq!(oracle.dates(id).await?.0, Some(t0() + Duration::seconds(570)));
    let t1 = t0() + Duration::hours(1);
    oracle.refresh(id, t1).await?;
    assert_eq!(oracle.dates(id).await?.0, Some(t1 + Duration::seconds(570)));

    // a vendor that documents nothing: five minutes, as before
    let nextgen = vendor("nextgen").await?;
    assert_eq!(nextgen.profile.access_token_seconds, None);
    nextgen.fake.set(|b| {
        if let Some(m) = b.code_reply.as_mut().and_then(Value::as_object_mut) {
            m.shift_remove("expires_in");
        }
    });
    let id = nextgen.connect(t0()).await?;
    assert_eq!(nextgen.dates(id).await?.0, Some(t0() + Duration::seconds(300)));
    Ok(())
}

#[tokio::test]
async fn the_vendors_own_word_on_the_grant_beats_the_profile() -> R {
    let va = vendor("va").await?;
    va.fake.set(|b| b.refresh_expires_in = Some(10 * 86_400));
    let id = va.connect(t0()).await?;
    assert_eq!(va.dates(id).await?.1, Some(t0() + Duration::days(10)));
    let t1 = t0() + Duration::days(2);
    va.refresh(id, t1).await?;
    assert_eq!(va.dates(id).await?.1, Some(t1 + Duration::days(10)));
    Ok(())
}

#[tokio::test]
async fn a_vendor_the_file_does_not_name_keeps_the_answers_own_dates() -> R {
    // the plain path every test client of connector_oauth.rs uses ('labnorte-ehr')
    assert!(profiles::vendor("labnorte-ehr").is_none());
    let va = vendor("va").await?;
    sqlx::query("update connector_client set vendor = 'labnorte-ehr' where id = $1")
        .bind(va.client.id)
        .execute(&va.db.pool)
        .await?;
    let client = {
        let mut c = va.db.pool.acquire().await?;
        oauth::load_client(&mut c, Some(&va.sb), va.fam.household_id, va.client.id)
            .await?
            .expect("the client")
    };
    let renamed = V { client, ..va };
    let id = renamed.connect(t0()).await?;
    assert_eq!(renamed.dates(id).await?.1, None);
    Ok(())
}

#[tokio::test]
async fn the_answers_own_expires_in_beats_the_documented_lifetime() -> R {
    // Oracle documents 570 seconds; this answer says 300, and the vendor's word on this token wins
    let oracle = vendor("oracle").await?;
    oracle.fake.set(|b| {
        for r in [b.code_reply.as_mut(), b.refresh_reply.as_mut()].into_iter().flatten() {
            if let Some(m) = r.as_object_mut() {
                m.insert("expires_in".into(), Value::from(300));
            }
        }
    });
    let id = oracle.connect(t0()).await?;
    assert_eq!(oracle.dates(id).await?.0, Some(t0() + Duration::seconds(300)));
    let t1 = t0() + Duration::hours(1);
    oracle.refresh(id, t1).await?;
    assert_eq!(oracle.dates(id).await?.0, Some(t1 + Duration::seconds(300)));
    Ok(())
}

#[tokio::test]
async fn a_sign_in_without_a_refresh_token_gets_no_unused_expiry() -> R {
    // the VA's 42 days are the life of an unused refresh token: an answer with none has nothing to end
    let va = vendor("va").await?;
    assert_eq!(va.profile.refresh.unused_expiry_days, Some(42));
    va.fake.set(|b| {
        if let Some(m) = b.code_reply.as_mut().and_then(Value::as_object_mut) {
            m.shift_remove("refresh_token");
        }
    });
    let id = va.connect(t0()).await?;
    assert_eq!(va.held(id).await?, None);
    assert_eq!(va.dates(id).await?.1, None, "no refresh token, no grant date");
    Ok(())
}
