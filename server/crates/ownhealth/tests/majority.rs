//! One answer to "is this person a minor?" everywhere: the profile list (`/api/me`), the genome page,
//! the agent's gate over MCP and the rules engine all read the household setting, else
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

/// (profile list, genome page, MCP gate, rules engine): is Lucía a minor?
async fn answers(app: &axum::Router, db: &TestDb, lucia: Uuid, token: &str) -> anyhow::Result<[bool; 4]> {
    let me = send(app, Request::get("/api/me").body(Body::empty())?).await?;
    let listed = me["profiles"]
        .as_array()
        .and_then(|ps| ps.iter().find(|p| p["slug"] == "lucia"))
        .map(|p| p["is_minor"] == true)
        .ok_or_else(|| anyhow::anyhow!("lucia missing from /api/me"))?;
    let page = send(app, Request::get("/api/profiles/lucia/genome").body(Body::empty())?).await?;
    let call = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
        "params": {"name": "get_genome_findings", "arguments": {"profile": "lucia"}}});
    let mcp = send(
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
    let withheld = &mcp["result"]["structuredContent"]["withheld"]["minor_rule"];
    anyhow::ensure!(withheld.is_i64(), "no minor_rule count in {mcp}");
    let mut c = db.pool.acquire().await?;
    let pack = ownhealth_rules::load_pack()?;
    let ev = ownhealth_rules::engine::evaluate(&mut c, lucia, Local::now().date_naive(), &pack, None, None).await?;
    Ok([listed, page["is_minor"] == true, withheld != 0, ev.minor])
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
    assert_eq!(answers(&app, &db, lucia, &token).await?, [false; 4]);

    // the household's own 18 wins over the environment everywhere
    sqlx::query("insert into household_setting (household_id, key, value) values ($1, 'age_of_majority', '18')")
        .bind(fam.household_id)
        .execute(&mut *c)
        .await?;
    assert_eq!(answers(&app, &db, lucia, &token).await?, [true; 4]);
    Ok(())
}
