//! SMART Health Links shared from the instance (migration 1270, routes/shl.rs): a link opens a scoped
//! IPS for an outside client, holds its passcode limit under parallel attempts, stops at revoke and
//! expiry, logs every answer, and stores neither the key nor the plaintext. Only the owner or a
//! guardian shares, in a browser session. Each test runs on its own throwaway database with the
//! synthetic Rivera family.

#[path = "auth_support.rs"]
mod support;

use std::collections::BTreeSet;

use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{Method, Request, StatusCode};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ownhealth_api::routes::shl::{ATTEMPTS, RATE_PER_HOUR};
use ownhealth_core::access::{scope_set, set_locks, today_utc};
use ownhealth_core::demo::{DEMO_HOUSEHOLD, seed_demo};
use ownhealth_core::jose;
use ownhealth_ops::shl::CONTENT_TYPE;
use serde_json::{Value, json};
use support::{Client, Env, Resp};
use tower::ServiceExt;
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

/// The IPS a link of these categories carries today.
async fn ips(env: &Env, profile: Uuid, scopes: &[&str]) -> anyhow::Result<Vec<u8>> {
    let mut tx = env.pool().begin().await?;
    let out = ownhealth_ops::ips::build_link(&mut tx, profile, &scope_set(scopes), "en", today_utc()).await?;
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
        (StatusCode::FORBIDDEN, "category_closed"),
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
    // a second revoke (a double click) answers the same and changes nothing
    let audited = env
        .scalar_i64("select count(*) from audit_event where action = 'shl.revoke'")
        .await?;
    let r = alex.delete(&format!("/api/profiles/alex/shl/{id}")).await?;
    assert_eq!(
        (r.status, r.json["revoked"].clone()),
        (StatusCode::OK, json!(true)),
        "{:?}",
        r.json
    );
    assert_eq!(
        env.scalar_i64("select count(*) from audit_event where action = 'shl.revoke'")
            .await?,
        audited
    );
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
    let hex = |b: &[u8]| -> String { b.iter().map(|b| format!("{b:02x}")).collect() };
    let key_hex = hex(&key);
    // a bytea column prints as hex: the id or the ticket stored as raw bytes would show only so
    let (id_hex, ticket_hex) = (hex(id.as_bytes()), hex(ticket.as_bytes()));
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
            ("manifest id hex", &id_hex),
            ("ticket", &ticket),
            ("ticket hex", &ticket_hex),
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
                (StatusCode::FORBIDDEN, "not_sharer"),
                "{m} {p}: {:?}",
                r.json
            );
            assert!(!r.json.to_string().contains("read:"), "no scope name: {:?}", r.json);
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
    assert_eq!(r.header("access-control-allow-origin"), "*");
    // the hour filled just now: room again in about an hour
    let wait: i64 = r.header("retry-after").parse()?;
    assert!((3590..=3600).contains(&wait), "{wait}");
    // the same answers 601 s old: room again 2999 s from now, not after a fixed 600 s
    sqlx::query("update shl_access set at = at - interval '601 seconds'")
        .execute(env.pool())
        .await?;
    let r = manifest(&env, &pl, None).await?;
    assert_eq!(r.status, StatusCode::TOO_MANY_REQUESTS);
    let wait: i64 = r.header("retry-after").parse()?;
    assert!((2990..=2999).contains(&wait), "{wait}");
    // one answer older than the hour makes room
    sqlx::query("update shl_access set at = now() - interval '61 minutes' where id = (select min(id) from shl_access)")
        .execute(env.pool())
        .await?;
    assert_eq!(manifest(&env, &pl, None).await?.status, StatusCode::OK);
    Ok(())
}

/// A manifest request from `peer`, with an X-Forwarded-For header when given: the TCP peer the
/// server sees is the proxy in front of it, or the client itself.
async fn manifest_from(env: &Env, pl: &Value, peer: &str, xff: Option<&str>) -> anyhow::Result<StatusCode> {
    let body = json!({"recipient": "Dr. Carmen Ortega, LabNorte"});
    let mut req = Request::builder()
        .method(Method::POST)
        .uri(path_of(pl["url"].as_str().unwrap_or_default()))
        .header("host", "testserver")
        .header("content-type", "application/json");
    if let Some(x) = xff {
        req = req.header("x-forwarded-for", x);
    }
    let mut req = req.body(Body::from(serde_json::to_vec(&body)?))?;
    req.extensions_mut()
        .insert(ConnectInfo(peer.parse::<std::net::SocketAddr>()?));
    Ok(env.anon().app.oneshot(req).await?.status())
}

/// The access log names the client, never the proxy hop: X-Forwarded-For counts only when the
/// direct peer is a trusted proxy (OWNHEALTH_TRUSTED_PROXIES), and what the client wrote into the
/// header itself stays left of the address the proxy saw. Documentation addresses (RFC 5737).
#[tokio::test]
async fn the_access_log_keeps_the_client_address_and_trusts_only_a_trusted_proxy() -> R {
    let mut env = env().await?;
    env.settings.trusted_proxies = vec!["172.16.0.0/12".parse()?];
    let mut alex = env.as_user(env.user("alex")).await?;
    let (made, pl) = share(&mut alex, "alex", json!({})).await?;
    let id = made["link"]["id"].as_str().unwrap_or_default().to_string();
    // Caddy (172.18.0.3) forwards for the tunnel hop (172.18.0.1), which forwards for the client
    let chain = "198.51.100.66, 203.0.113.9, 172.18.0.1";
    assert_eq!(
        manifest_from(&env, &pl, "172.18.0.3:40000", Some(chain)).await?,
        StatusCode::OK
    );
    // a peer that is no proxy: its own address, whatever header it sends
    assert_eq!(
        manifest_from(&env, &pl, "198.51.100.20:40000", Some("203.0.113.77")).await?,
        StatusCode::OK
    );
    let r = alex.get(&format!("/api/profiles/alex/shl/{id}/access")).await?;
    let ips: Vec<&str> = r.json["items"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|i| i["ip"].as_str())
        .collect();
    assert_eq!(ips, ["198.51.100.20", "203.0.113.9"], "{:?}", r.json);
    let audited: Vec<Option<String>> =
        sqlx::query_scalar("select host(ip) from audit_event where actor_kind = 'share' order by id")
            .fetch_all(env.pool())
            .await?;
    assert_eq!(audited, [Some("203.0.113.9".into()), Some("198.51.100.20".into())]);
    // no trusted proxy configured: the peer, header or not
    env.settings.trusted_proxies.clear();
    assert_eq!(
        manifest_from(&env, &pl, "172.18.0.3:40000", Some(chain)).await?,
        StatusCode::OK
    );
    let last: Option<String> = sqlx::query_scalar("select host(ip) from shl_access order by id desc limit 1")
        .fetch_one(env.pool())
        .await?;
    assert_eq!(last.as_deref(), Some("172.18.0.3"));
    Ok(())
}

/// A lock set, or the creator's grant narrowed, after the link was made stops that link at once:
/// the manifest and a file location handed out before both answer 404 (the rule "a lock means
/// never out of the house" holds for links already out, not only new ones).
#[tokio::test]
async fn a_lock_or_a_narrower_grant_stops_a_link_already_out() -> R {
    let env = env().await?;
    let mut alex = env.as_user(env.user("alex")).await?;
    // the owner's own lock
    let (made, pl) = share(&mut alex, "alex", json!({"categories": ["read:labs"]})).await?;
    assert_eq!(made["link"]["categories"], json!(["read:labs", "read:summary"]));
    let m = manifest(&env, &pl, None).await?;
    assert_eq!(m.status, StatusCode::OK);
    let mut c = env.pool().acquire().await?;
    set_locks(&mut c, env.profile("alex"), &scope_set(["read:labs"])).await?;
    assert_eq!(manifest(&env, &pl, None).await?.status, StatusCode::NOT_FOUND);
    assert_eq!(env.anon().get(&location(&m)).await?.status, StatusCode::NOT_FOUND);
    // a link without the locked category keeps working: the lock stops only what it closes
    set_locks(&mut c, env.profile("alex"), &scope_set(Vec::<&str>::new())).await?;
    let (_, records) = share(&mut alex, "alex", json!({"categories": ["read:records"]})).await?;
    set_locks(&mut c, env.profile("alex"), &scope_set(["read:labs"])).await?;
    assert_eq!(manifest(&env, &records, None).await?.status, StatusCode::OK);

    // a guardian whose categories on Lucía shrink
    let (_, pl) = share(&mut alex, "lucia", json!({"categories": ["read:labs"]})).await?;
    assert_eq!(manifest(&env, &pl, None).await?.status, StatusCode::OK);
    sqlx::query(
        "update profile_grant set categories = array['read:summary', 'read:records'] \
         where user_id = $1 and profile_id = $2",
    )
    .bind(env.user("alex"))
    .bind(env.profile("lucia"))
    .execute(env.pool())
    .await?;
    assert_eq!(manifest(&env, &pl, None).await?.status, StatusCode::NOT_FOUND);
    Ok(())
}

/// The creator's login disabled, the creator's grant expired, or the grant turned into break-glass:
/// the link stops.
#[tokio::test]
async fn a_disabled_creator_or_an_expired_grant_stops_the_link() -> R {
    let env = env().await?;
    // Sam is Lucía's guardian; the same grant becomes break-glass
    let mut sam = env.as_user(env.user("sam")).await?;
    let (_, by_sam) = share(&mut sam, "lucia", json!({})).await?;
    assert_eq!(manifest(&env, &by_sam, None).await?.status, StatusCode::OK);
    sqlx::query(
        "update profile_grant set break_glass = true, expires_at = now() + interval '14 days' \
         where user_id = $1 and profile_id = $2",
    )
    .bind(env.user("sam"))
    .bind(env.profile("lucia"))
    .execute(env.pool())
    .await?;
    assert_eq!(manifest(&env, &by_sam, None).await?.status, StatusCode::NOT_FOUND);
    let mut alex = env.as_user(env.user("alex")).await?;
    let (_, pl) = share(&mut alex, "lucia", json!({})).await?;
    let (_, own) = share(&mut alex, "alex", json!({})).await?;
    assert_eq!(manifest(&env, &pl, None).await?.status, StatusCode::OK);
    sqlx::query(
        "update profile_grant set expires_at = now() - interval '1 second' where user_id = $1 and profile_id = $2",
    )
    .bind(env.user("alex"))
    .bind(env.profile("lucia"))
    .execute(env.pool())
    .await?;
    assert_eq!(manifest(&env, &pl, None).await?.status, StatusCode::NOT_FOUND);
    assert_eq!(manifest(&env, &own, None).await?.status, StatusCode::OK);
    sqlx::query("update app_user set disabled_at = now() where id = $1")
        .bind(env.user("alex"))
        .execute(env.pool())
        .await?;
    assert_eq!(manifest(&env, &own, None).await?.status, StatusCode::NOT_FOUND);
    Ok(())
}

/// A location ticket opens the file of its own link only: a ticket from link A on link B's path is
/// 404, so a ticket never skips B's passcode.
#[tokio::test]
async fn a_ticket_opens_only_its_own_link() -> R {
    let env = env().await?;
    let mut alex = env.as_user(env.user("alex")).await?;
    let (_, a) = share(&mut alex, "alex", json!({})).await?;
    let (_, b) = share(&mut alex, "alex", json!({"passcode": PIN})).await?;
    let ticket_a = location(&manifest(&env, &a, None).await?)
        .rsplit('/')
        .next()
        .unwrap_or_default()
        .to_string();
    let b_path = path_of(b["url"].as_str().unwrap_or_default());
    let r = env.anon().get(&format!("{b_path}/file/{ticket_a}")).await?;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
    // the control: the same ticket on its own link opens
    let a_path = path_of(a["url"].as_str().unwrap_or_default());
    assert_eq!(
        env.anon().get(&format!("{a_path}/file/{ticket_a}")).await?.status,
        StatusCode::OK
    );
    // every link has its own random key, and never a constant one
    assert_ne!(key_of(&a)?, key_of(&b)?);
    assert_ne!(key_of(&a)?, [0u8; jose::KEY_LEN]);
    // a location never outlives its link: one 10 minutes from its end gets a 10-minute location
    sqlx::query("update shl set expires_at = now() + interval '10 minutes'")
        .execute(env.pool())
        .await?;
    assert_eq!(manifest(&env, &a, None).await?.status, StatusCode::OK);
    let ok = env
        .scalar_i64(
            "select count(*) from shl_access a join shl s on s.id = a.shl_id \
             where a.id = (select max(id) from shl_access where ticket_sha256 is not null) \
             and a.ticket_expires_at <= s.expires_at",
        )
        .await?;
    assert_eq!(ok, 1);
    Ok(())
}

/// Sharing turned off (OWNHEALTH_SHARE_URL unset) stops what was handed out before: a file location
/// is 404 too, and a manifest request is 404 before its body is read.
#[tokio::test]
async fn turning_sharing_off_stops_locations_already_handed_out() -> R {
    let mut env = env().await?;
    let mut alex = env.as_user(env.user("alex")).await?;
    let (_, pl) = share(&mut alex, "alex", json!({})).await?;
    let m = manifest(&env, &pl, None).await?;
    assert_eq!(env.anon().get(&location(&m)).await?.status, StatusCode::OK);
    env.settings.share_url = None;
    assert_eq!(env.anon().get(&location(&m)).await?.status, StatusCode::NOT_FOUND);
    let path = path_of(pl["url"].as_str().unwrap_or_default());
    let r = env.anon().send(Method::POST, &path, Some(&json!({})), &[]).await?;
    assert_eq!((r.status, r.code()), (StatusCode::NOT_FOUND, "unknown"), "{:?}", r.json);
    // with sharing on, an unknown link is 404 whatever the body
    env.settings.share_url = Some(SHARE.into());
    let r = env
        .anon()
        .send(
            Method::POST,
            &format!("/api/shl/{}", "A".repeat(43)),
            Some(&json!({})),
            &[],
        )
        .await?;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
    Ok(())
}

/// A request with no passcode on a link that needs one costs no attempt, but the owner sees it in
/// the access log and it counts toward the hourly limit.
#[tokio::test]
async fn a_request_without_the_passcode_is_logged_and_limited() -> R {
    let env = env().await?;
    let mut alex = env.as_user(env.user("alex")).await?;
    let (made, pl) = share(&mut alex, "alex", json!({"passcode": PIN})).await?;
    let id = made["link"]["id"].as_str().unwrap_or_default().to_string();
    for _ in 0..RATE_PER_HOUR {
        let r = manifest(&env, &pl, None).await?;
        assert_eq!(r.json, json!({"remainingAttempts": ATTEMPTS}));
    }
    let r = manifest(&env, &pl, None).await?;
    assert_eq!(r.status, StatusCode::TOO_MANY_REQUESTS);
    let r = alex.get(&format!("/api/profiles/alex/shl/{id}/access")).await?;
    let kinds: BTreeSet<&str> = r.json["items"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|i| i["kind"].as_str())
        .collect();
    assert_eq!(kinds, BTreeSet::from(["passcode_missing"]));
    assert_eq!(r.json["items"].as_array().map(Vec::len), Some(RATE_PER_HOUR as usize));
    let denied = env
        .scalar_i64("select count(*) from audit_event where action = 'shl.passcode_missing' and result_kind = 'denied'")
        .await?;
    assert_eq!(denied, RATE_PER_HOUR);
    Ok(())
}

/// A link out of passcode attempts is dead: a location it gave out before is 404, and its file is
/// deleted at once. The file of an expired link goes with the scheduler's next pass.
#[tokio::test]
async fn a_locked_out_or_expired_link_loses_its_file() -> R {
    let env = env().await?;
    let mut alex = env.as_user(env.user("alex")).await?;
    let (made, pl) = share(&mut alex, "alex", json!({"passcode": PIN})).await?;
    let locked = made["link"]["id"].as_str().unwrap_or_default().to_string();
    let m = manifest(&env, &pl, Some(PIN)).await?;
    for _ in 0..ATTEMPTS {
        manifest(&env, &pl, Some(WRONG)).await?;
    }
    assert_eq!(env.anon().get(&location(&m)).await?.status, StatusCode::NOT_FOUND);
    let files = |id: String| {
        let pool = env.pool().clone();
        async move {
            sqlx::query_scalar::<_, i64>("select count(*) from shl_file where shl_id = $1::uuid")
                .bind(id)
                .fetch_one(&pool)
                .await
        }
    };
    assert_eq!(files(locked.clone()).await?, 0, "locked out: the file is gone");

    let (made, _) = share(&mut alex, "alex", json!({})).await?;
    let expired = made["link"]["id"].as_str().unwrap_or_default().to_string();
    let (made, _) = share(&mut alex, "alex", json!({})).await?;
    let live = made["link"]["id"].as_str().unwrap_or_default().to_string();
    sqlx::query("update shl set expires_at = now() - interval '1 second' where id = $1::uuid")
        .bind(&expired)
        .execute(env.pool())
        .await?;
    let mut c = env.pool().acquire().await?;
    ownhealth_core::sources::sweep_link_files(&mut c).await?;
    assert_eq!(files(expired).await?, 0, "expired: the sweep deleted it");
    assert_eq!(files(live).await?, 1, "a live link keeps its file");
    Ok(())
}

/// The recipient has no account: a section the person did not share says it is not in this link,
/// never "this account cannot read them", and every link's Composition carries the same one
/// sentence about what a link holds, in the link's language.
#[tokio::test]
async fn the_recipient_reads_not_shared_and_the_same_notice_on_every_link() -> R {
    let env = env().await?;
    let mut alex = env.as_user(env.user("alex")).await?;
    for (lang, not_in, notice, account) in [
        (
            "en",
            "Not included in this link.",
            "This summary holds only what the person chose to share, and never a sensitive category.",
            "this account",
        ),
        (
            "es",
            "No se incluye en este enlace.",
            "Este resumen contiene solo lo que la persona eligió compartir, y nunca una categoría sensible.",
            "esta cuenta",
        ),
    ] {
        for cats in [json!(["read:labs"]), json!(["read:labs", "read:records"])] {
            let (_, pl) = share(&mut alex, "alex", json!({"lang": lang, "categories": cats})).await?;
            let bundle: Value = serde_json::from_slice(&open(&env, &pl, None).await?)?;
            let comp = &bundle["entry"][0]["resource"];
            let narratives: Vec<String> = comp["section"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|s| s["text"]["div"].as_str().unwrap_or_default().to_string())
                .collect();
            assert!(
                narratives.iter().all(|d| !d.contains(account)),
                "{lang} {cats}: {narratives:?}"
            );
            let withheld = narratives.iter().filter(|d| d.contains(not_in)).count();
            assert_eq!(withheld > 0, cats.as_array().map(Vec::len) == Some(1), "{lang} {cats}");
            let shown = comp["text"]["div"].as_str().unwrap_or_default();
            assert!(shown.contains(notice), "{lang} {cats}: {shown}");
        }
    }
    Ok(())
}

/// The person creating a link sees how many sensitive items it leaves out; the recipient's file
/// never names them and reads the same either way.
#[tokio::test]
async fn the_creator_sees_how_many_sensitive_items_stay_home() -> R {
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
    let count = |made: &Value| made["left_out"]["sensitive"].as_i64().unwrap_or(-1);
    let labs = json!({"lang": "en", "categories": ["read:labs"]});
    let (made, pl) = share(&mut alex, &pid, json!({"lang": "en"})).await?;
    let all0 = count(&made);
    let labs0 = count(&share(&mut alex, &pid, labs.clone()).await?.0);
    assert!(all0 >= 0 && labs0 >= 0, "{made:?}");
    let before: Value = serde_json::from_slice(&open(&env, &pl, None).await?)?;
    // two new synthetic conditions, copies of a current one: a plain twin and one tagged mental health
    for (text, tag) in [("SYNTH-PLAIN-50", "{}"), ("SYNTH-SENSITIVE-50", "{mental_health}")] {
        sqlx::query(
            "insert into condition (profile_id, text, clinical_status, verification_status, src_id, accepted_by, \
             status, sensitivity, fhir) select profile_id, $2, 'active', 'confirmed', src_id, accepted_by, 'current', \
             $3::text[], jsonb_set(fhir, '{code}', jsonb_build_object('text', $2::text)) from condition \
             where profile_id = $1 and status = 'current' order by id limit 1",
        )
        .bind(profile)
        .bind(text)
        .bind(tag)
        .execute(env.pool())
        .await?;
    }
    let (made, pl) = share(&mut alex, &pid, json!({"lang": "en"})).await?;
    assert_eq!(count(&made), all0 + 1, "{made:?}");
    let after = String::from_utf8(open(&env, &pl, None).await?)?;
    assert!(
        after.contains("SYNTH-PLAIN-50"),
        "a control that can fail: the plain twin is in the link"
    );
    assert!(
        !after.contains("SYNTH-SENSITIVE-50"),
        "the sensitive one left the house"
    );
    let after: Value = serde_json::from_str(&after)?;
    assert_eq!(
        after["entry"][0]["resource"]["text"], before["entry"][0]["resource"]["text"],
        "the notice is the same with or without anything left out"
    );
    // labs only: the condition is in no section the link carries
    assert_eq!(count(&share(&mut alex, &pid, labs).await?.0), labs0);
    Ok(())
}

/// The refusals a person can meet each have their own code, and none shows an internal scope name.
#[tokio::test]
async fn a_closed_category_and_a_locked_summary_have_their_own_code() -> R {
    let env = env().await?;
    let mut alex = env.as_user(env.user("alex")).await?;
    let mut c = env.pool().acquire().await?;
    set_locks(&mut c, env.profile("alex"), &scope_set(["read:labs"])).await?;
    let r = alex
        .post("/api/profiles/alex/shl", &json!({"categories": ["read:labs"]}))
        .await?;
    assert_eq!(
        (r.status, r.code()),
        (StatusCode::FORBIDDEN, "category_closed"),
        "{:?}",
        r.json
    );
    set_locks(&mut c, env.profile("alex"), &scope_set(["read:summary"])).await?;
    let r = alex
        .post("/api/profiles/alex/shl", &json!({"categories": ["read:labs"]}))
        .await?;
    assert_eq!(
        (r.status, r.code()),
        (StatusCode::FORBIDDEN, "category_closed"),
        "{:?}",
        r.json
    );
    assert!(!r.json.to_string().contains("read:"), "no scope name: {:?}", r.json);
    assert!(r.json.to_string().contains("summary is locked"), "{:?}", r.json);
    // the owner still lists and revokes with the summary locked
    assert_eq!(alex.get("/api/profiles/alex/shl").await?.status, StatusCode::OK);
    Ok(())
}

/// Reading the link list and a link's access log (recipients, addresses) writes one audit row each,
/// as every read of the profile does.
#[tokio::test]
async fn reading_the_links_and_their_access_log_is_audited() -> R {
    let env = env().await?;
    let mut alex = env.as_user(env.user("alex")).await?;
    let (made, _) = share(&mut alex, "alex", json!({})).await?;
    let id = made["link"]["id"].as_str().unwrap_or_default().to_string();
    assert_eq!(alex.get("/api/profiles/alex/shl").await?.status, StatusCode::OK);
    let r = alex.get(&format!("/api/profiles/alex/shl/{id}/access")).await?;
    assert_eq!(r.status, StatusCode::OK);
    let rows: Vec<(String, String, Option<Vec<String>>)> = sqlx::query_as(
        "select actor_kind, action, resource_ids from audit_event where action in ('shl.list', 'shl.access') order by id",
    )
    .fetch_all(env.pool())
    .await?;
    assert_eq!(
        rows,
        [
            ("user".to_string(), "shl.list".to_string(), None),
            ("user".to_string(), "shl.access".to_string(), Some(vec![id]))
        ]
    );
    Ok(())
}
