//! SMART Health Links shared from the instance (migration 1270, routes/shl.rs): a link opens a scoped
//! IPS for an outside client, holds its passcode limit under parallel attempts, stops at revoke and
//! expiry, logs every answer, and stores neither the key nor the plaintext. Only the owner or a
//! guardian shares, in a browser session. Each test runs on its own throwaway database with the
//! synthetic Rivera family.

#[path = "auth_support.rs"]
mod support;

use std::collections::BTreeSet;

use axum::http::{Method, StatusCode};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ownhealth_api::routes::shl::{ATTEMPTS, RATE_PER_HOUR};
use ownhealth_core::access::{scope_set, set_locks, today_utc};
use ownhealth_core::demo::{DEMO_HOUSEHOLD, seed_demo};
use ownhealth_core::jose;
use ownhealth_ops::shl::CONTENT_TYPE;
use serde_json::{Value, json};
use support::{Client, Env, Resp};
use uuid::Uuid;

type R = anyhow::Result<()>;

const SHARE: &str = "https://share.rivera.test";
/// Obviously fake test passcodes.
const PIN: &str = "test-pin-1234";
const WRONG: &str = "test-pin-0000";

async fn env() -> anyhow::Result<Env> {
    let mut env = Env::new(true).await?;
    env.settings.share_url = Some(SHARE.into());
    Ok(env)
}

/// The link's payload ("Construct a SMART Health Link Payload").
fn payload(shlink: &str) -> anyhow::Result<Value> {
    let b64 = shlink
        .strip_prefix("shlink:/")
        .ok_or_else(|| anyhow::anyhow!("no shlink:/ prefix: {shlink}"))?;
    Ok(serde_json::from_slice(&URL_SAFE_NO_PAD.decode(b64)?)?)
}

/// The request path of a URL on the share host.
fn path_of(url: &str) -> String {
    url.strip_prefix(SHARE).unwrap_or(url).to_string()
}

fn key_of(p: &Value) -> anyhow::Result<[u8; jose::KEY_LEN]> {
    let raw = URL_SAFE_NO_PAD.decode(p["key"].as_str().unwrap_or_default())?;
    raw.try_into().map_err(|_| anyhow::anyhow!("the key is not 32 bytes"))
}

/// A new link of profile `p`, made by a client with a fresh step-up: (the answer, the payload).
async fn share(c: &mut Client, p: &str, body: Value) -> anyhow::Result<(Value, Value)> {
    let r = c.post(&format!("/api/profiles/{p}/shl"), &body).await?;
    assert_eq!(r.status, StatusCode::CREATED, "{:?}", r.json);
    let pl = payload(r.json["shlink"].as_str().unwrap_or_default())?;
    Ok((r.json, pl))
}

/// The manifest request of an outside client ("SMART Health Link Manifest Request").
async fn manifest(env: &Env, pl: &Value, passcode: Option<&str>) -> anyhow::Result<Resp> {
    let mut body = json!({"recipient": "Dr. Carmen Ortega, LabNorte"});
    if let Some(pc) = passcode {
        body["passcode"] = json!(pc);
    }
    let path = path_of(pl["url"].as_str().unwrap_or_default());
    env.anon().send(Method::POST, &path, Some(&body), &[]).await
}

/// The manifest's one file location.
fn location(r: &Resp) -> String {
    path_of(r.json["files"][0]["location"].as_str().unwrap_or_default())
}

/// The plaintext an outside client gets: manifest, then file, then the JWE opened with the key.
async fn open(env: &Env, pl: &Value, passcode: Option<&str>) -> anyhow::Result<Vec<u8>> {
    let m = manifest(env, pl, passcode).await?;
    assert_eq!(m.status, StatusCode::OK, "{:?}", m.json);
    let f = env.anon().get(&location(&m)).await?;
    assert_eq!(f.status, StatusCode::OK, "{:?}", f.json);
    assert_eq!(f.header("content-type"), "application/jose");
    let jwe = String::from_utf8(f.bytes)?;
    let d = jose::decrypt(&key_of(pl)?, &jwe, 1 << 24).map_err(|e| anyhow::anyhow!("{e}"))?;
    assert_eq!(d.header["cty"], CONTENT_TYPE);
    Ok(d.plaintext)
}

/// The IPS the owner would export today with these categories.
async fn ips(env: &Env, profile: Uuid, scopes: &[&str]) -> anyhow::Result<Vec<u8>> {
    let mut tx = env.pool().begin().await?;
    let out = ownhealth_ops::ips::build(&mut tx, profile, &scope_set(scopes), "en", today_utc()).await?;
    tx.commit().await?;
    Ok(out.to_bytes()?)
}

/// "SMART Health Link Manifest Request": the lifetime count of wrong passcodes holds even when the
/// guesses arrive in parallel. 20 wrong passcodes at once get exactly ATTEMPTS 401 answers, and
/// then the right passcode finds the link disabled.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn parallel_wrong_passcodes_never_exceed_the_attempt_limit() -> R {
    let env = env().await?;
    let mut alex = env.as_user(env.user("alex")).await?;
    let (_, pl) = share(&mut alex, "alex", json!({"passcode": PIN, "lang": "en"})).await?;
    assert_eq!(pl["flag"], "P");
    let path = path_of(pl["url"].as_str().unwrap_or_default());
    let mut tasks = Vec::new();
    for i in 0..20 {
        let mut c = env.anon();
        let path = path.clone();
        tasks.push(tokio::spawn(async move {
            let body = json!({"recipient": format!("guesser {i}"), "passcode": format!("{WRONG}-{i}")});
            c.send(Method::POST, &path, Some(&body), &[]).await
        }));
    }
    let mut remaining = BTreeSet::new();
    let (mut denied, mut gone) = (0, 0);
    for t in tasks {
        let r = t.await??;
        match r.status {
            StatusCode::UNAUTHORIZED => {
                denied += 1;
                remaining.insert(r.json["remainingAttempts"].as_i64().unwrap_or(-1));
            }
            StatusCode::NOT_FOUND => gone += 1,
            s => anyhow::bail!("{s}: {:?}", r.json),
        }
    }
    assert_eq!((denied, gone), (ATTEMPTS, 20 - ATTEMPTS));
    assert_eq!(remaining, (0..i64::from(ATTEMPTS)).collect::<BTreeSet<_>>());
    assert_eq!(manifest(&env, &pl, Some(PIN)).await?.status, StatusCode::NOT_FOUND);
    let logged = env
        .scalar_i64("select count(*) from shl_access where kind = 'passcode_wrong'")
        .await?;
    assert_eq!(logged, i64::from(ATTEMPTS));
    Ok(())
}

#[tokio::test]
async fn a_wrong_passcode_costs_one_attempt_and_a_missing_one_costs_none() -> R {
    let env = env().await?;
    let mut alex = env.as_user(env.user("alex")).await?;
    let (_, pl) = share(&mut alex, "alex", json!({"passcode": PIN})).await?;
    let r = manifest(&env, &pl, None).await?;
    assert_eq!(
        (r.status, r.json.clone()),
        (StatusCode::UNAUTHORIZED, json!({"remainingAttempts": ATTEMPTS}))
    );
    let r = manifest(&env, &pl, Some(WRONG)).await?;
    assert_eq!(r.json, json!({"remainingAttempts": ATTEMPTS - 1}));
    assert_eq!(r.header("access-control-allow-origin"), "*");
    // the right one opens it, and costs nothing either
    assert_eq!(manifest(&env, &pl, Some(PIN)).await?.status, StatusCode::OK);
    let r = manifest(&env, &pl, Some(WRONG)).await?;
    assert_eq!(r.json, json!({"remainingAttempts": ATTEMPTS - 2}));
    Ok(())
}

/// What a recipient decrypts is exactly the IPS of the link's categories: the creator's categories
/// minus the profile's locks (the owner's own too), never read:sensitive or a genome scope.
#[tokio::test]
async fn the_file_is_the_ips_of_the_link_s_categories_minus_the_profile_s_locks() -> R {
    ownhealth_ops::register();
    ownhealth_ingest_structured::register();
    let env = env().await?;
    assert!(seed_demo(env.pool(), &env.settings, &env.rt.blobs, false).await?);
    let (owner, profile): (Uuid, Uuid) = sqlx::query_as(
        "select g.user_id, p.id from profile_grant g join profile p on p.id = g.profile_id \
         join household h on h.id = p.household_id where h.name = $1 and p.slug = 'alex' and g.role = 'owner'",
    )
    .bind(DEMO_HOUSEHOLD)
    .fetch_one(env.pool())
    .await?;
    let mut alex = env.as_user(owner).await?;
    let pid = profile.to_string();

    // everything the IPS reads, by default
    let (made, pl) = share(&mut alex, &pid, json!({"lang": "en", "label": "Analítica LabNorte"})).await?;
    let all = ["read:labs", "read:records", "read:summary"];
    assert_eq!(made["link"]["categories"], json!(all));
    let plain = open(&env, &pl, None).await?;
    assert_eq!(plain, ips(&env, profile, &all).await?);
    let bundle: Value = serde_json::from_slice(&plain)?;
    assert_eq!(bundle["resourceType"], "Bundle");
    assert!(bundle.to_string().contains("30954-2"), "the Results section is there");

    // records only: the labs stay home
    let (_, pl) = share(&mut alex, &pid, json!({"lang": "en", "categories": ["read:records"]})).await?;
    let only = ["read:records", "read:summary"];
    assert_eq!(open(&env, &pl, None).await?, ips(&env, profile, &only).await?);
    assert_ne!(
        ips(&env, profile, &only).await?,
        ips(&env, profile, &all).await?,
        "a control that can fail"
    );

    // a lock binds the owner's own links: labs locked, the default leaves them out, asking is refused
    let mut c = env.pool().acquire().await?;
    set_locks(&mut c, profile, &scope_set(["read:labs"])).await?;
    let (made, pl) = share(&mut alex, &pid, json!({"lang": "en"})).await?;
    assert_eq!(made["link"]["categories"], json!(only));
    assert_eq!(open(&env, &pl, None).await?, ips(&env, profile, &only).await?);
    let r = alex
        .post(
            &format!("/api/profiles/{pid}/shl"),
            &json!({"categories": ["read:labs"]}),
        )
        .await?;
    assert_eq!(
        (r.status, r.code()),
        (StatusCode::FORBIDDEN, "forbidden"),
        "{:?}",
        r.json
    );

    // sensitive and genome categories are never a link's
    for c in [
        "read:sensitive",
        "read:genome",
        "read:genome_raw",
        "read:documents_text",
    ] {
        let r = alex
            .post(&format!("/api/profiles/{pid}/shl"), &json!({"categories": [c]}))
            .await?;
        assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY, "{c}: {:?}", r.json);
    }
    let cats: Vec<Vec<String>> = sqlx::query_scalar("select categories from shl")
        .fetch_all(env.pool())
        .await?;
    assert!(cats.iter().flatten().all(|c| all.contains(&c.as_str())), "{cats:?}");
    Ok(())
}

/// The manifest and file shapes of the spec, as an outside client reads them.
#[tokio::test]
async fn the_link_manifest_and_file_follow_the_shl_spec() -> R {
    let env = env().await?;
    let mut alex = env.as_user(env.user("alex")).await?;
    let (made, pl) = share(&mut alex, "alex", json!({"label": "Para la consulta", "days": 3})).await?;
    // payload: url of at most 128 characters on the share host, a 43-character key, exp, label
    let url = pl["url"].as_str().unwrap_or_default();
    assert!(
        url.starts_with(&format!("{SHARE}/api/shl/")) && url.len() <= 128,
        "{url}"
    );
    assert_eq!(pl["key"].as_str().map(str::len), Some(43));
    assert!(pl.get("flag").is_none(), "no passcode, no P");
    assert_eq!(pl["label"], "Para la consulta");
    let exp = pl["exp"].as_i64().unwrap_or_default();
    let in3 = (chrono::Utc::now() + chrono::Duration::days(3)).timestamp();
    assert!((in3 - exp).abs() < 60, "{exp} {in3}");
    assert_eq!(made["link"]["passcode"], false);
    // preflight for a viewer on another origin
    let r = env
        .anon()
        .send(
            Method::OPTIONS,
            &path_of(url),
            None,
            &[
                ("origin", "https://viewer.example"),
                ("access-control-request-method", "POST"),
            ],
        )
        .await?;
    assert_eq!(r.status, StatusCode::NO_CONTENT);
    assert_eq!(r.header("access-control-allow-origin"), "*");
    assert!(r.header("access-control-allow-methods").contains("POST"));
    // manifest
    let m = manifest(&env, &pl, None).await?;
    assert_eq!(m.status, StatusCode::OK);
    assert_eq!(m.header("content-type"), "application/json");
    assert_eq!(m.header("access-control-allow-origin"), "*");
    assert_eq!(m.json["status"], "finalized");
    assert_eq!(m.json["files"].as_array().map(Vec::len), Some(1));
    assert_eq!(m.json["files"][0]["contentType"], CONTENT_TYPE);
    assert!(
        m.json["files"][0]["location"]
            .as_str()
            .unwrap_or_default()
            .starts_with(SHARE)
    );
    // a recipient is required (1..1)
    let r = env
        .anon()
        .send(Method::POST, &path_of(url), Some(&json!({"recipient": "  "})), &[])
        .await?;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    // no U flag: a GET of the manifest URL is not a way in, and a HEAD of the file writes nothing
    assert_eq!(
        env.anon().get(&path_of(url)).await?.status,
        StatusCode::METHOD_NOT_ALLOWED
    );
    let before = env.scalar_i64("select count(*) from shl_access").await?;
    let r = env.anon().send(Method::HEAD, &location(&m), None, &[]).await?;
    assert_eq!(r.status, StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(env.scalar_i64("select count(*) from shl_access").await?, before);
    // ".files.location links": the lifetime SHALL NOT exceed one hour (the spec's number, not ours)
    let secs = env
        .scalar_i64("select max(extract(epoch from ticket_expires_at - at))::bigint from shl_access")
        .await?;
    assert!(secs <= 3600 && secs > 3540, "{secs}");
    let f = env.anon().get(&location(&m)).await?;
    assert_eq!(f.status, StatusCode::OK);
    assert_eq!(f.header("access-control-allow-origin"), "*");
    Ok(())
}

#[tokio::test]
async fn revoke_and_expiry_answer_404_and_revoke_deletes_the_file() -> R {
    let env = env().await?;
    let mut alex = env.as_user(env.user("alex")).await?;
    let (made, pl) = share(&mut alex, "alex", json!({})).await?;
    let m = manifest(&env, &pl, None).await?;
    let loc = location(&m);
    assert_eq!(env.anon().get(&loc).await?.status, StatusCode::OK);
    let id = made["link"]["id"].as_str().unwrap_or_default().to_string();
    let r = alex.delete(&format!("/api/profiles/alex/shl/{id}")).await?;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
    assert_eq!(manifest(&env, &pl, None).await?.status, StatusCode::NOT_FOUND);
    assert_eq!(env.anon().get(&loc).await?.status, StatusCode::NOT_FOUND);
    assert_eq!(env.scalar_i64("select count(*) from shl_file").await?, 0);
    let r = alex.delete(&format!("/api/profiles/alex/shl/{id}")).await?;
    assert_eq!(r.status, StatusCode::NOT_FOUND, "a link is revoked once");
    let listed = alex.get("/api/profiles/alex/shl").await?;
    assert_eq!(listed.json["items"][0]["active"], false);

    // expiry: the link, and a location it gave out before
    let (made, pl) = share(&mut alex, "alex", json!({})).await?;
    let m = manifest(&env, &pl, None).await?;
    sqlx::query("update shl set expires_at = now() - interval '1 second' where id = $1::uuid")
        .bind(made["link"]["id"].as_str().unwrap_or_default())
        .execute(env.pool())
        .await?;
    assert_eq!(manifest(&env, &pl, None).await?.status, StatusCode::NOT_FOUND);
    assert_eq!(env.anon().get(&location(&m)).await?.status, StatusCode::NOT_FOUND);

    // a location past its hour, on a live link
    let (_, pl) = share(&mut alex, "alex", json!({})).await?;
    let m = manifest(&env, &pl, None).await?;
    sqlx::query(
        "update shl_access set ticket_expires_at = now() - interval '1 second' where ticket_sha256 is not null",
    )
    .execute(env.pool())
    .await?;
    assert_eq!(env.anon().get(&location(&m)).await?.status, StatusCode::NOT_FOUND);
    assert_eq!(
        manifest(&env, &pl, None).await?.status,
        StatusCode::OK,
        "a new manifest gives a new one"
    );
    Ok(())
}

/// The key lives only in the link: no table holds it, the manifest id, a location ticket or the
/// passcode, and the one file the server keeps is ciphertext that another key does not open.
#[tokio::test]
async fn no_plaintext_and_no_key_is_stored_on_the_server() -> R {
    let env = env().await?;
    let mut alex = env.as_user(env.user("alex")).await?;
    let (_, pl) = share(&mut alex, "alex", json!({"passcode": PIN})).await?;
    let m = manifest(&env, &pl, Some(PIN)).await?;
    let loc = location(&m);
    let key = key_of(&pl)?;
    let id = pl["url"]
        .as_str()
        .unwrap_or_default()
        .rsplit('/')
        .next()
        .unwrap_or_default()
        .to_string();
    let ticket = loc.rsplit('/').next().unwrap_or_default().to_string();
    let key_hex: String = key.iter().map(|b| format!("{b:02x}")).collect();
    let tables: Vec<String> = sqlx::query_scalar(
        "select table_name::text from information_schema.tables where table_schema = current_schema() \
         and table_type = 'BASE TABLE' order by 1",
    )
    .fetch_all(env.pool())
    .await?;
    for t in &tables {
        let text: String = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "select coalesce(string_agg(t::text, E'\\n'), '') from \"{t}\" t"
        )))
        .fetch_one(env.pool())
        .await?;
        for (what, needle) in [
            ("key", pl["key"].as_str().unwrap_or("?")),
            ("key hex", &key_hex),
            ("manifest id", &id),
            ("ticket", &ticket),
            ("passcode", PIN),
        ] {
            assert!(!text.contains(needle), "the {what} is in table {t}");
        }
    }
    // the stored file: ciphertext that opens with the link's key only
    let jwe: String = sqlx::query_scalar("select jwe from shl_file")
        .fetch_one(env.pool())
        .await?;
    let opened = jose::decrypt(&key, &jwe, 1 << 24).map_err(|e| anyhow::anyhow!("{e}"))?;
    let other = [7u8; jose::KEY_LEN];
    assert!(jose::decrypt(&other, &jwe, 1 << 24).is_err());
    let header: Value = serde_json::from_slice(&URL_SAFE_NO_PAD.decode(jwe.split('.').next().unwrap_or_default())?)?;
    assert_eq!(
        (header["alg"].as_str(), header["enc"].as_str()),
        (Some("dir"), Some("A256GCM"))
    );
    assert_eq!(jwe.split('.').nth(1), Some(""), "dir: no encrypted key part");
    let plain = String::from_utf8(opened.plaintext)?;
    assert!(plain.contains("\"resourceType\": \"Bundle\""));
    assert!(!jwe.contains("Bundle"));
    Ok(())
}

/// Sharing is a person's act in the browser: an access token gets 403 on every share route.
#[tokio::test]
async fn a_bearer_token_gets_403_on_every_share_route() -> R {
    let env = env().await?;
    let mut c = env.pool().acquire().await?;
    let t = ownhealth_auth::tokens::create_token(
        &mut c,
        &ownhealth_auth::tokens::TokenIn {
            user_id: env.user("alex"),
            label: "agent",
            profile_ids: vec![env.profile("alex")],
            scopes: ownhealth_core::access::all_scopes().into_iter().collect(),
            default_profile: None,
            days: 30,
            identity_mode: "full",
            lang: "en",
            units: "si",
        },
    )
    .await?;
    let bearer = format!("Bearer {}", t.plaintext);
    let id = Uuid::new_v4();
    for (m, p, body) in [
        (Method::GET, "/api/profiles/alex/shl".to_string(), None),
        (Method::POST, "/api/profiles/alex/shl".to_string(), Some(json!({}))),
        (Method::GET, format!("/api/profiles/alex/shl/{id}/access"), None),
        (Method::DELETE, format!("/api/profiles/alex/shl/{id}"), None),
    ] {
        let r = env
            .anon()
            .send(m.clone(), &p, body.as_ref(), &[("authorization", bearer.as_str())])
            .await?;
        assert_eq!(
            (r.status, r.code()),
            (StatusCode::FORBIDDEN, "forbidden"),
            "{m} {p}: {:?}",
            r.json
        );
    }
    assert_eq!(env.scalar_i64("select count(*) from shl").await?, 0);
    Ok(())
}

/// Alex is Sam's viewer and Carmen's caregiver; Sam is Lucía's guardian, here through break-glass.
/// Each is refused; Alex as owner and as Lucía's guardian shares.
#[tokio::test]
async fn a_viewer_a_caregiver_and_a_break_glass_contact_are_each_refused() -> R {
    let env = env().await?;
    sqlx::query(
        "update profile_grant set break_glass = true, expires_at = now() + interval '14 days' \
         where user_id = $1 and profile_id = $2",
    )
    .bind(env.user("sam"))
    .bind(env.profile("lucia"))
    .execute(env.pool())
    .await?;
    for (who, p) in [("alex", "sam"), ("alex", "carmen"), ("sam", "lucia")] {
        let mut c = env.as_user(env.user(who)).await?;
        for (m, body) in [(Method::POST, Some(json!({}))), (Method::GET, None)] {
            let r = c
                .send(m.clone(), &format!("/api/profiles/{p}/shl"), body.as_ref(), &[])
                .await?;
            assert_eq!(
                (r.status, r.code()),
                (StatusCode::FORBIDDEN, "forbidden"),
                "{m} {p}: {:?}",
                r.json
            );
        }
    }
    assert_eq!(env.scalar_i64("select count(*) from shl").await?, 0);
    let mut alex = env.as_user(env.user("alex")).await?;
    share(&mut alex, "alex", json!({})).await?;
    share(&mut alex, "lucia", json!({})).await?;
    // a link stops when its creator may no longer share: Alex's guardianship of Lucía ends
    let (_, pl) = share(&mut alex, "lucia", json!({})).await?;
    assert_eq!(manifest(&env, &pl, None).await?.status, StatusCode::OK);
    sqlx::query("update profile_grant set role = 'viewer' where user_id = $1 and profile_id = $2")
        .bind(env.user("alex"))
        .bind(env.profile("lucia"))
        .execute(env.pool())
        .await?;
    assert_eq!(manifest(&env, &pl, None).await?.status, StatusCode::NOT_FOUND);
    Ok(())
}

#[tokio::test]
async fn creating_a_link_needs_a_fresh_step_up_and_a_share_address() -> R {
    let mut env = env().await?;
    let mut stale = env.stale(env.user("alex")).await?;
    let r = stale.post("/api/profiles/alex/shl", &json!({})).await?;
    assert_eq!((r.status, r.code()), (StatusCode::FORBIDDEN, "step_up_required"));
    for bad in [
        json!({"days": 0}),
        json!({"days": 366}),
        json!({"passcode": "123"}),
        json!({"lang": "fr"}),
        json!({"label": "x".repeat(81)}),
    ] {
        let mut alex = env.as_user(env.user("alex")).await?;
        let r = alex.post("/api/profiles/alex/shl", &bad).await?;
        assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY, "{bad}");
        assert!(
            !r.json.to_string().contains("123"),
            "a passcode is never echoed: {}",
            r.json
        );
    }
    env.settings.share_url = None;
    let mut alex = env.as_user(env.user("alex")).await?;
    let r = alex.post("/api/profiles/alex/shl", &json!({})).await?;
    assert_eq!((r.status, r.code()), (StatusCode::CONFLICT, "sharing_off"));
    assert_eq!(env.scalar_i64("select count(*) from shl").await?, 0);
    Ok(())
}

/// The access log the owner reads, and the audit rows: one per answer, actor share. The recipient's
/// words stay in the access log; the audit log keeps only their hash.
#[tokio::test]
async fn each_answer_writes_one_access_row_and_one_audit_row() -> R {
    let env = env().await?;
    let mut alex = env.as_user(env.user("alex")).await?;
    let (made, pl) = share(&mut alex, "alex", json!({"passcode": PIN})).await?;
    let id = made["link"]["id"].as_str().unwrap_or_default().to_string();
    manifest(&env, &pl, Some(WRONG)).await?;
    let m = manifest(&env, &pl, Some(PIN)).await?;
    env.anon().get(&location(&m)).await?;
    let r = alex.get(&format!("/api/profiles/alex/shl/{id}/access")).await?;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
    let kinds: Vec<&str> = r.json["items"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|i| i["kind"].as_str())
        .collect();
    assert_eq!(kinds, ["file", "manifest", "passcode_wrong"]);
    assert_eq!(r.json["items"][0]["recipient"], "Dr. Carmen Ortega, LabNorte");
    let rows: Vec<(String, String, String, Option<String>)> = sqlx::query_as(
        "select actor_kind, via, action, args_summary from audit_event where tool = 'shl' and actor_kind = 'share' order by id",
    )
    .fetch_all(env.pool())
    .await?;
    let actions: Vec<&str> = rows.iter().map(|r| r.2.as_str()).collect();
    assert_eq!(actions, ["shl.passcode_wrong", "shl.manifest", "shl.file"]);
    for (kind, via, _, args) in &rows {
        assert_eq!((kind.as_str(), via.as_str()), ("share", "share"));
        assert!(!args.as_deref().unwrap_or_default().contains("Ortega"), "{args:?}");
    }
    let listed = alex.get("/api/profiles/alex/shl").await?;
    assert_eq!(listed.json["items"][0]["opened"], 1);
    assert_eq!(listed.json["items"][0]["attempts_left"], ATTEMPTS - 1);
    // the creation is audited with the facts that can leave
    let created = env
        .scalar_i64("select count(*) from audit_event where action = 'shl.create' and actor_kind = 'user'")
        .await?;
    assert_eq!(created, 1);
    Ok(())
}

/// "Polling Manifest for Changes": too many requests get 429 with Retry-After.
#[tokio::test]
async fn a_link_answers_a_limited_number_of_times_an_hour() -> R {
    let env = env().await?;
    let mut alex = env.as_user(env.user("alex")).await?;
    let (_, pl) = share(&mut alex, "alex", json!({})).await?;
    sqlx::query(
        "insert into shl_access (shl_id, kind, recipient) select id, 'manifest', 'synthetic' from shl, \
         generate_series(1, $1)",
    )
    .bind(RATE_PER_HOUR)
    .execute(env.pool())
    .await?;
    let r = manifest(&env, &pl, None).await?;
    assert_eq!(r.status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(r.header("retry-after"), "600");
    assert_eq!(r.header("access-control-allow-origin"), "*");
    Ok(())
}
