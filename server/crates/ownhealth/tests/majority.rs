//! One answer to "is this person a minor?" everywhere: the profile list (`/api/me`), the genome page,
//! the agent's gate over MCP, the status page and the agent's health status all read the household setting, else
//! `OWNHEALTH_AGE_OF_MAJORITY`, else 18.
//!
//! Its own test binary: the environment default lives in the process, and this test sets it to 16.

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use ownhealth::app::build_app;
use ownhealth_core::Runtime;
use ownhealth_core::provenance::{SrcIn, SrcRef, ensure_src};
use ownhealth_core::testing::TestDb;
use serde_json::{Value, json};
use sqlx::types::Uuid;
use sqlx::types::chrono::Local;
use tower::ServiceExt;

type R = anyhow::Result<()>;

async fn send(app: &axum::Router, req: Request<Body>) -> anyhow::Result<Value> {
    let res = app.clone().oneshot(req).await?;
    let status = res.status();
    let body = to_bytes(res.into_body(), 1 << 20).await?;
    anyhow::ensure!(status == StatusCode::OK, "{status}: {}", String::from_utf8_lossy(&body));
    Ok(serde_json::from_slice(&body)?)
}

/// A carrier finding: shown to an adult's agent, withheld from a minor's by the minor rule.
async fn carrier_finding(c: &mut sqlx::PgConnection, profile_id: Uuid) -> R {
    let mut s = SrcIn::new("genome_facts");
    s.profile_id = Some(profile_id);
    s.import_ref = Some("facts".into());
    let sid = ensure_src(&mut *c, &SrcRef::New(s), Some(profile_id)).await?;
    let dataset: Uuid = sqlx::query_scalar(
        "insert into genome_dataset (profile_id, sample_id, assay, facts_schema, facts_sha256, facts_blob, \
         owngenome_version, db_versions, src_id) values ($1, 'S1', 'wgs', 'ownhealth.genome-facts/v1', \
         sha256('majority'::bytea), sha256('majority'::bytea), '1.4.2', '{}', $2) returning id",
    )
    .bind(profile_id)
    .bind(sid)
    .fetch_one(&mut *c)
    .await?;
    let mut s = SrcIn::new("genome_facts");
    s.profile_id = Some(profile_id);
    s.import_ref = Some("facts#f_00000000000000ca".into());
    let sid = ensure_src(&mut *c, &SrcRef::New(s), Some(profile_id)).await?;
    sqlx::query(
        "insert into genome_finding (dataset_id, profile_id, fact_id, category, gene, evidence, confidence, \
         call_state, sensitive, adult_onset, carrier, status, src_id, fact) values \
         ($1, $2, 'f_00000000000000ca', 'carrier', 'CFTR', 'E1', 'high', 'alt', false, false, true, 'current', $3, '{}')",
    )
    .bind(dataset)
    .bind(profile_id)
    .bind(sid)
    .execute(&mut *c)
    .await?;
    Ok(())
}

/// One MCP tool call as the agent with `token`; returns its `structuredContent`.
async fn tool(app: &axum::Router, token: &str, name: &str) -> anyhow::Result<Value> {
    let call = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
        "params": {"name": name, "arguments": {"profile": "lucia"}}});
    let out = send(
        app,
        Request::post("/mcp")
            .header("host", "127.0.0.1")
            .header("content-type", "application/json")
            .header("accept", "application/json, text/event-stream")
            .header("mcp-protocol-version", "2025-11-25")
            .header("authorization", format!("Bearer {token}"))
            .body(Body::from(call.to_string()))?,
    )
    .await?;
    Ok(out["result"]["structuredContent"].clone())
}

/// Whether a carrier finding was withheld by the minor rule, from a `withheld.minor_rule` count.
fn minor_rule(v: &Value) -> anyhow::Result<bool> {
    let n = &v["withheld"]["minor_rule"];
    anyhow::ensure!(n.is_i64(), "no minor_rule count in {v}");
    Ok(n != 0)
}

/// (profile list, genome page, MCP gate, status page): is Lucía a minor? Each is asked the way the app
/// asks it, with no day passed, so each uses its own default day. The agent's health status must give
/// the status page's state hash: its `withheld` does not carry the minor count, but the hash does.
async fn answers(app: &axum::Router, token: &str) -> anyhow::Result<[bool; 4]> {
    let me = send(app, Request::get("/api/me").body(Body::empty())?).await?;
    let listed = me["profiles"]
        .as_array()
        .and_then(|ps| ps.iter().find(|p| p["slug"] == "lucia"))
        .map(|p| p["is_minor"] == true)
        .ok_or_else(|| anyhow::anyhow!("lucia missing from /api/me"))?;
    let page = send(app, Request::get("/api/profiles/lucia/genome").body(Body::empty())?).await?;
    let gate = minor_rule(&tool(app, token, "get_genome_findings").await?)?;
    let status = send(app, Request::get("/api/profiles/lucia/status").body(Body::empty())?).await?;
    let health = tool(app, token, "get_health_status").await?;
    let hash = &status["state_hash"];
    anyhow::ensure!(hash.is_string(), "no state hash in {status}");
    anyhow::ensure!(
        health["data"]["state_hash"] == *hash,
        "the agent's health status ({}) is not the status page's ({})",
        health["as_of"],
        status["as_of"]
    );
    Ok([listed, page["is_minor"] == true, gate, minor_rule(&status)?])
}

#[tokio::test]
async fn every_surface_agrees_on_who_is_a_minor() -> R {
    let db = TestDb::new().await?;
    let dir = tempfile::tempdir()?;
    let mut settings = db.settings(dir.path())?;
    settings.age_of_majority = 16;
    let app = build_app(Runtime::with_pool(settings, db.pool.clone())?);
    ownhealth::register_lanes();
    let mut c = db.pool.acquire().await?;
    let fam = ownhealth_core::demo::create_rivera_family(&mut c, "Rivera").await?;
    let lucia = fam.profile("lucia");
    // seventeen, a month past her birthday: an adult at 16, a minor at 18
    sqlx::query("update profile set birth_date = current_date - interval '17 years 30 days' where id = $1")
        .bind(lucia)
        .execute(&mut *c)
        .await?;
    carrier_finding(&mut c, lucia).await?;
    let token = ownhealth_auth::tokens::create_token(
        &mut c,
        &ownhealth_auth::tokens::TokenIn {
            user_id: fam.user("alex"),
            label: "t",
            profile_ids: vec![lucia],
            scopes: vec!["read:summary".into(), "read:genome".into()],
            default_profile: None,
            days: 30,
            identity_mode: "full",
            lang: "en",
            units: "si",
        },
    )
    .await?
    .plaintext;

    // no household setting: the environment's 16 decides everywhere
    let none: i64 = sqlx::query_scalar("select count(*) from household_setting where household_id = $1")
        .bind(fam.household_id)
        .fetch_one(&mut *c)
        .await?;
    assert_eq!(none, 0);
    assert_eq!(answers(&app, &token).await?, [false; 4]);

    // the household's own 18 wins over the environment everywhere
    sqlx::query("insert into household_setting (household_id, key, value) values ($1, 'age_of_majority', '18')")
        .bind(fam.household_id)
        .execute(&mut *c)
        .await?;
    assert_eq!(answers(&app, &token).await?, [true; 4]);
    Ok(())
}

/// Lucía's state hash in the manifest `ownhealth backup` writes under `OWNHEALTH_AGE_OF_MAJORITY=years`.
fn backup_hash(db: &TestDb, dir: &std::path::Path, years: &str, lucia: Uuid) -> anyhow::Result<String> {
    let archive = dir.join(format!("backup-{years}"));
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_ownhealth"))
        .args(["backup", &archive.display().to_string()])
        .env("OWNHEALTH_DATABASE_URL", &db.url)
        .env("OWNHEALTH_DATA_DIR", dir.join("live"))
        .env("OWNHEALTH_AGE_OF_MAJORITY", years)
        .output()?;
    anyhow::ensure!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let m: Value = serde_json::from_slice(&std::fs::read(archive.join("manifest.json"))?)?;
    let hash = m["state_hash"][lucia.to_string()].as_str();
    hash.map(str::to_string)
        .ok_or_else(|| anyhow::anyhow!("no state hash for lucia in {m}"))
}

#[tokio::test]
async fn the_backup_command_uses_the_same_age_of_majority_as_the_server() -> R {
    let db = TestDb::new().await?;
    let dir = tempfile::tempdir()?;
    let mut c = db.pool.acquire().await?;
    let fam = ownhealth_core::demo::create_rivera_family(&mut c, "Rivera").await?;
    let lucia = fam.profile("lucia");
    sqlx::query("update profile set birth_date = current_date - interval '17 years 30 days' where id = $1")
        .bind(lucia)
        .execute(&mut *c)
        .await?;
    carrier_finding(&mut c, lucia).await?;
    let at16 = backup_hash(&db, dir.path(), "16", lucia)?;
    // the env decides the minor rule in the manifest too, so a restored instance agrees with the app
    assert_ne!(at16, backup_hash(&db, dir.path(), "18", lucia)?);
    ownhealth_core::access::set_env_majority(16);
    let today = sqlx::types::chrono::Utc::now().date_naive();
    let state = ownhealth_rules::build_state(&mut c, lucia, today, None).await?;
    assert_eq!(state["state_hash"].as_str(), Some(at16.as_str()));
    Ok(())
}

/// On her 18th birthday, with the server's local day on another day than UTC's: the profile list, the
/// genome page, the agent's gate, the status page and the agent's health status all count the UTC day,
/// the one records are stored on. The case runs in a child process whose `TZ` makes the split, because
/// the local zone is read from the environment.
#[tokio::test]
async fn on_the_birthday_every_surface_counts_the_same_day() -> R {
    use sqlx::types::chrono::Utc;
    const CHILD: &str = "OWNHEALTH_TEST_SPLIT_DAY";
    if std::env::var_os(CHILD).is_none() {
        // a zone whose day differs from UTC's right now: 12 hours behind before noon, 14 ahead after
        let tz = if *Utc::now().format("%H").to_string() < *"12" {
            "Etc/GMT+12"
        } else {
            "Etc/GMT-14"
        };
        let out = std::process::Command::new(std::env::current_exe()?)
            .args([
                "--exact",
                "on_the_birthday_every_surface_counts_the_same_day",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .env("TZ", tz)
            // the agent's daily budgets reset on this zone's midnight; its default day must not follow
            .env("OWNHEALTH_TIMEZONE", tz)
            .output()?;
        anyhow::ensure!(
            out.status.success() && String::from_utf8_lossy(&out.stdout).contains("1 passed"),
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        return Ok(());
    }
    let (local, utc) = (Local::now().date_naive(), Utc::now().date_naive());
    anyhow::ensure!(local != utc, "TZ did not split the day: {local}");
    let db = TestDb::new().await?;
    let dir = tempfile::tempdir()?;
    let mut settings = db.settings(dir.path())?;
    settings.age_of_majority = 18;
    let app = build_app(Runtime::with_pool(settings, db.pool.clone())?);
    ownhealth::register_lanes();
    let mut c = db.pool.acquire().await?;
    let fam = ownhealth_core::demo::create_rivera_family(&mut c, "Rivera").await?;
    let lucia = fam.profile("lucia");
    // 18 on the later of the two days: an adult if UTC's day is that one, a minor if the local day is
    let later = local.max(utc);
    sqlx::query("update profile set birth_date = ($1::date - interval '18 years')::date where id = $2")
        .bind(later)
        .bind(lucia)
        .execute(&mut *c)
        .await?;
    carrier_finding(&mut c, lucia).await?;
    let token = ownhealth_auth::tokens::create_token(
        &mut c,
        &ownhealth_auth::tokens::TokenIn {
            user_id: fam.user("alex"),
            label: "t",
            profile_ids: vec![lucia],
            scopes: vec!["read:summary".into(), "read:genome".into()],
            default_profile: None,
            days: 30,
            identity_mode: "full",
            lang: "en",
            units: "si",
        },
    )
    .await?
    .plaintext;
    let minor = utc < local;
    assert_eq!(answers(&app, &token).await?, [minor; 4]);
    Ok(())
}
