//! The audit page's two views (routes/audit.rs): the state of the hash chain and of the anchor the
//! last backup kept outside the database (`GET /api/audit/chain`), and who saw one profile's data
//! (`GET /api/audit/who?profile=`), with only the rows the person may see.

#[path = "auth_support.rs"]
mod support;

use axum::http::StatusCode;
use ownhealth_core::audit::{AuditEvent, write_audit};
use serde_json::{Value, json};
use support::Env;

type R = anyhow::Result<()>;

/// `n` reads of `profile` by the user `actor`, or by the server's own jobs for "system".
async fn read(env: &Env, actor: &str, profile: &str, n: usize) -> R {
    let mut c = env.pool().acquire().await?;
    for _ in 0..n {
        let e = if actor == "system" {
            AuditEvent::new("system", "job", "read")
        } else {
            AuditEvent {
                actor_id: Some(env.user(actor)),
                ..AuditEvent::new("user", "ui", "read")
            }
        };
        let e = AuditEvent {
            tool: Some("GET /x".into()),
            profile_id: Some(env.profile(profile)),
            ..e
        };
        write_audit(&mut c, &e).await?;
    }
    Ok(())
}

/// As the database owner: change one event and leave its link alone.
async fn edit_newest(env: &Env) -> anyhow::Result<i64> {
    sqlx::raw_sql(
        "alter table audit_event disable trigger user; \
         update audit_event set tool = 'edited' where id = (select event_id from audit_chain order by seq desc limit 1); \
         alter table audit_event enable trigger user",
    )
    .execute(env.pool())
    .await?;
    Ok(sqlx::query_scalar("select max(seq) from audit_chain")
        .fetch_one(env.pool())
        .await?)
}

/// The anchor file a backup writes (ownhealth-ops backup.rs), for the newest link.
async fn anchor_head(env: &Env, tamper: bool) -> anyhow::Result<i64> {
    let (seq, hash): (i64, Vec<u8>) = sqlx::query_as("select seq, hash from audit_chain order by seq desc limit 1")
        .fetch_one(env.pool())
        .await?;
    let hash: String = if tamper {
        "0".repeat(64)
    } else {
        hash.iter().map(|b| format!("{b:02x}")).collect()
    };
    let dir = env.settings.anchor_path();
    std::fs::create_dir_all(&dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt; // a backup refuses a folder others may write
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
    }
    let body =
        json!({"schema": "ownhealth.audit-anchor/v1", "seq": seq, "hash": hash, "at": "2026-11-22T08:00:00+00:00"});
    std::fs::write(dir.join("audit-head.json"), serde_json::to_vec(&body)?)?;
    Ok(seq)
}

#[tokio::test]
async fn the_chain_state_turns_broken_when_a_row_is_edited() -> R {
    let env = Env::new(true).await?;
    read(&env, "sam", "alex", 2).await?;
    let mut alex = env.as_user(env.user("alex")).await?;
    let r = alex.get("/api/audit/chain").await?;
    assert_eq!(r.status, StatusCode::OK, "{}", r.json);
    let head: i64 = sqlx::query_scalar("select max(seq) from audit_chain")
        .fetch_one(env.pool())
        .await?;
    assert_eq!(r.json["ok"], true, "{}", r.json);
    assert_eq!(r.json["verified"], head);
    assert_eq!(r.json["broken"], Value::Null);
    assert_eq!(r.json["anchor"]["state"], "none");

    let seq = edit_newest(&env).await?;
    // The page reuses a verdict for up to a minute while the newest entry and the anchor stay the
    // same; a new entry moves the newest one, so the page checks again at once.
    read(&env, "sam", "alex", 1).await?;
    let r = alex.get("/api/audit/chain").await?;
    assert_eq!(r.json["ok"], false, "{}", r.json);
    assert_eq!(r.json["broken"], json!({"seq": seq, "kind": "hash"}));
    assert_eq!(r.json["verified"], seq - 1);
    Ok(())
}

#[tokio::test]
async fn the_chain_state_shows_the_anchor_of_the_last_backup() -> R {
    let env = Env::new(true).await?;
    read(&env, "sam", "alex", 1).await?;
    let seq = anchor_head(&env, false).await?;
    let mut alex = env.as_user(env.user("alex")).await?;
    let r = alex.get("/api/audit/chain").await?.json;
    assert_eq!(r["ok"], true, "{r}");
    assert_eq!(
        r["anchor"],
        json!({"state": "ok", "seq": seq, "at": "2026-11-22T08:00:00+00:00"})
    );
    // the hash the backup kept is no longer the one the database holds at that entry
    anchor_head(&env, true).await?;
    let r = alex.get("/api/audit/chain").await?.json;
    assert_eq!(r["ok"], false, "{r}");
    assert_eq!(r["anchor"]["state"], "mismatch");
    assert_eq!(r["broken"], Value::Null);
    std::fs::write(env.settings.anchor_path().join("audit-head.json"), b"{")?;
    let r = alex.get("/api/audit/chain").await?.json;
    assert_eq!(
        (r["ok"].clone(), r["anchor"]["state"].clone()),
        (json!(false), json!("unreadable"))
    );
    // the page never shows the hashes or where the file is
    let text = r.to_string();
    assert!(!text.contains("hash") && !text.contains("audit-head"), "{text}");
    Ok(())
}

#[tokio::test]
async fn the_chain_state_needs_a_signed_in_person() -> R {
    let env = Env::new(true).await?;
    assert_eq!(
        env.anon().get("/api/audit/chain").await?.status,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        env.anon().get("/api/audit/who?profile=alex").await?.status,
        StatusCode::UNAUTHORIZED
    );
    Ok(())
}

fn who(v: &Value) -> Vec<(String, i64)> {
    v["items"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|i| {
            (
                i["actor"].as_str().unwrap_or_default().to_string(),
                i["events"].as_i64().unwrap_or(0),
            )
        })
        .collect()
}

/// Alex owns his profile, so he sees everyone who read it (Sam, the server's jobs, himself); Sam only
/// views Alex, so on Alex's profile he sees his own reads and nobody else's; and nobody sees anything
/// of a profile they hold no grant on. Each row counts one actor's entries on that profile only.
#[tokio::test]
async fn who_saw_a_profile_shows_only_what_the_person_may_see() -> R {
    let env = Env::new(true).await?;
    read(&env, "sam", "alex", 2).await?;
    read(&env, "alex", "alex", 1).await?;
    read(&env, "alex", "carmen", 4).await?; // another profile: never counted on alex's view
    read(&env, "system", "alex", 3).await?;

    let mut alex = env.as_user(env.user("alex")).await?;
    let r = alex.get("/api/audit/who?profile=alex").await?;
    assert_eq!(r.status, StatusCode::OK, "{}", r.json);
    assert_eq!(
        (r.json["profile"].clone(), r.json["scope"].clone()),
        (json!("alex"), json!("everyone"))
    );
    let mut rows = who(&r.json);
    rows.sort();
    assert_eq!(
        rows,
        [
            ("Alex Rivera".into(), 1),
            ("OwnHealth".into(), 3),
            ("Sam Rivera".into(), 2)
        ],
        "{}",
        r.json
    );
    let item = &r.json["items"][0];
    for key in ["actor", "actor_kind", "via", "events", "first_ts", "last_ts"] {
        assert!(item.get(key).is_some(), "{key} missing: {item}");
    }

    let mut sam = env.as_user(env.user("sam")).await?;
    let r = sam.get("/api/audit/who?profile=alex").await?;
    assert_eq!(r.status, StatusCode::OK, "{}", r.json);
    assert_eq!(
        who(&r.json),
        [("Sam Rivera".to_string(), 2)],
        "a viewer sees only his own reads"
    );
    assert_eq!(r.json["scope"], "own");
    assert_eq!(
        sam.get("/api/audit/who?profile=carmen").await?.status,
        StatusCode::NOT_FOUND
    );
    let r = sam.get("/api/audit/who").await?;
    assert_eq!(
        (r.status, r.code()),
        (StatusCode::UNPROCESSABLE_ENTITY, "bad_input"),
        "{}",
        r.json
    );
    Ok(())
}
