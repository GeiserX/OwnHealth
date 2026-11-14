//! REST: the Genome screen's PGx gene grid (v0.3). Every CPIC gene answers with its call or why there
//! is none, and names the current medicines from the records that CPIC ties to it; a reader without
//! read:records gets `meds: null`, never an empty list that reads as "no medicine".

use ownhealth_api::routes::genome;
use ownhealth_core::Runtime;
use ownhealth_core::access::{Principal, PrincipalKind, scope_set, set_genome_switches};
use ownhealth_core::demo::create_rivera_family;
use ownhealth_core::provenance::{SrcIn, SrcRef};
use ownhealth_core::records::{RecordIn, RecordOptions, add_record};
use ownhealth_core::testing::TestDb;
use ownhealth_genome::synthetic;
use serde_json::{Value, json};

type R = anyhow::Result<()>;

fn row<'a>(body: &'a Value, gene: &str) -> &'a Value {
    body["pgx_grid"]
        .as_array()
        .and_then(|g| g.iter().find(|r| r["gene"] == gene))
        .unwrap_or_else(|| panic!("no {gene} row in {}", body["pgx_grid"]))
}

#[tokio::test]
async fn the_grid_names_each_gene_and_the_current_medicines_it_touches() -> R {
    let db = TestDb::new().await?;
    let dir = tempfile::tempdir()?;
    let rt = Runtime::with_pool(db.settings(dir.path())?, db.pool.clone())?;
    let mut c = db.pool.acquire().await?;
    let fam = create_rivera_family(&mut c, "Rivera").await?;
    let alex = fam.profile("alex");
    synthetic::seed_genome(
        &mut c,
        &rt.blobs,
        fam.household_id,
        alex,
        &synthetic::alex_facts(),
        &[],
        "genome-facts.json",
        None,
    )
    .await?;
    let mut src = SrcIn::new("manual");
    src.profile_id = Some(alex);
    src.actor_kind = "system".into();
    for (name, started) in [("Omeprazole 20 mg", "2026-04-01"), ("Paracetamol 1 g", "2026-05-01")] {
        let med = RecordIn::from_json(
            "medication",
            json!({"profile_id": alex, "name": name, "started": started}),
        )?;
        add_record(&mut c, &med, &SrcRef::New(src.clone()), RecordOptions::new("person")).await?;
    }
    drop(c);

    let person = Principal::session(fam.user("alex"), fam.household_id);
    let body = genome::overview(&rt, &person, "alex", None)
        .await
        .map_err(|e| anyhow::anyhow!("{e:?}"))?;
    let cyp = row(&body, "CYP2C19");
    assert_eq!(
        (&cyp["status"], &cyp["diplotype"], &cyp["actionable"]),
        (&json!("found"), &json!("*1/*2"), &json!(true))
    );
    assert_eq!(cyp["meds"][0]["name"], "Omeprazole 20 mg", "{cyp}");
    assert!(
        cyp["meds"][0]["src"].as_str().is_some_and(|s| s.starts_with("src_")),
        "{cyp}"
    );
    // paracetamol has no CPIC gene; SLCO1B1 guides no current medicine here
    assert_eq!(row(&body, "SLCO1B1")["meds"], json!([]));
    assert!(!body["pgx_grid"].to_string().contains("Paracetamol"));
    // a CPIC gene with no finding and an assessed coverage entry
    assert_eq!(row(&body, "CYP3A5")["status"], "no_finding");
    // the demo was called with PharmCAT 3.1.0: an older minor than the shipped release
    let steps: Vec<&Value> = cyp["stale"]["rerun_steps"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|s| &s["step"])
        .collect();
    assert_eq!(steps, [&json!(36), &json!(7), &json!(27)], "{cyp}");
    // the tile counts the same actionable calls the grid frames
    let framed = body["pgx_grid"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|r| r["actionable"] == true && !r["gene"].as_str().unwrap_or_default().starts_with("HLA-"))
        .count();
    let tile = body["tiles"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|t| t["key"] == "pgx")
        .cloned()
        .unwrap_or_default();
    assert_eq!((tile["actionable"].as_u64(), framed), (Some(3), 3), "{tile}");
    // the audit names every src the overview returned, the medicines of the grid too
    let src_ids: Option<Vec<String>> = sqlx::query_scalar(
        "select src_ids from audit_event where profile_id = $1 and tool = 'GET /api/profiles/{p}/genome' order by id desc limit 1",
    )
    .bind(alex)
    .fetch_one(&db.pool)
    .await?;
    let med_src = cyp["meds"][0]["src"].as_str().unwrap_or("none");
    assert!(
        src_ids.is_some_and(|s| s.iter().any(|x| x == med_src)),
        "{med_src} not audited"
    );

    // an agent with read:genome alone sees the calls, not the medicines
    {
        let mut c = db.pool.acquire().await?;
        set_genome_switches(&mut c, alex, None, Some(true)).await?;
    }
    let mut agent = Principal::session(fam.user("alex"), fam.household_id);
    agent.kind = PrincipalKind::Token;
    agent.scopes = scope_set(["read:genome"]);
    agent.token_label = Some("test agent".into());
    let body = genome::overview(&rt, &agent, "alex", None)
        .await
        .map_err(|e| anyhow::anyhow!("{e:?}"))?;
    let cyp = row(&body, "CYP2C19");
    assert_eq!((&cyp["status"], &cyp["meds"]), (&json!("found"), &Value::Null));
    assert!(!body.to_string().contains("Omeprazole"));
    Ok(())
}

/// The grid answers each gene whatever evidence level the table below it shows: a PGx call under the
/// reader's min_evidence is still a call, never "assessed, no call in the file".
#[tokio::test]
async fn the_grid_keeps_a_call_the_table_filters_out_by_evidence() -> R {
    let db = TestDb::new().await?;
    let dir = tempfile::tempdir()?;
    let rt = Runtime::with_pool(db.settings(dir.path())?, db.pool.clone())?;
    let mut c = db.pool.acquire().await?;
    let fam = create_rivera_family(&mut c, "Rivera").await?;
    let alex = fam.profile("alex");
    let mut facts = synthetic::alex_facts();
    for f in facts["facts"].as_array_mut().into_iter().flatten() {
        if f["gene"] == "CYP2D6" {
            f["evidence"]["level"] = json!("E2");
        }
    }
    synthetic::seed_genome(
        &mut c,
        &rt.blobs,
        fam.household_id,
        alex,
        &facts,
        &[],
        "genome-facts.json",
        None,
    )
    .await?;
    drop(c);
    let person = Principal::session(fam.user("alex"), fam.household_id);
    let body = genome::overview(&rt, &person, "alex", Some("E1"))
        .await
        .map_err(|e| anyhow::anyhow!("{e:?}"))?;
    assert!(
        !body["findings"].to_string().contains("CYP2D6"),
        "the table filters it out"
    );
    let cyp = row(&body, "CYP2D6");
    assert_eq!(
        (&cyp["status"], &cyp["diplotype"]),
        (&json!("found"), &json!("*1/*41")),
        "{cyp}"
    );
    // the audit names the src of the call the grid shows
    let src_ids: Option<Vec<String>> = sqlx::query_scalar(
        "select src_ids from audit_event where profile_id = $1 and tool = 'GET /api/profiles/{p}/genome' order by id desc limit 1",
    )
    .bind(alex)
    .fetch_one(&db.pool)
    .await?;
    let src = cyp["src"].as_str().unwrap_or("none");
    assert!(src_ids.is_some_and(|s| s.iter().any(|x| x == src)), "{src} not audited");
    Ok(())
}
