//! get_genome_findings and check_drug carry the Genome screen's readings (v0.3) to an agent, from the
//! real services: `stale` with the OwnGenome steps to re-run, the PRS code, and the chip PPV note with
//! its citation on a planted rare BRCA2 P/LP.

mod common;

use common::*;
use ownhealth_genome::synthetic;
use serde_json::{Value, json};

#[tokio::test]
async fn an_agent_reads_stale_the_prs_code_and_the_chip_ppv_note() -> R {
    let w = world().await?;
    {
        let mut c = w.conn().await?;
        let mut alex = synthetic::alex_facts();
        alex["provenance"]["db_versions"]["cpic"] = json!("2025-01");
        for (slug, facts) in [("alex", alex), ("sam", synthetic::chip_with_rare_brca2())] {
            synthetic::seed_genome(
                &mut c,
                &w.rt.blobs,
                w.family.household_id,
                w.pid(slug),
                &facts,
                &[],
                "genome-facts.json",
                None,
            )
            .await?;
        }
    }
    w.set_switch("sam", true).await?;
    let c = w.connect_real(&w.tok().await?).await?;

    let env = c
        .call_tool("get_genome_findings", json!({"profile": "alex", "min_evidence": "E3"}))
        .await?
        .ok();
    let find = |env: &Value, gene: &str| -> Value {
        env["data"]["findings"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|f| f["gene"] == gene)
            .cloned()
            .unwrap_or_default()
    };
    let tpmt = find(&env, "TPMT");
    let steps = &tpmt["stale"]["rerun_steps"];
    assert_eq!(
        (&steps[0]["step"], &steps[1]["step"], &steps[2]["step"]),
        (&json!(36), &json!(7), &json!(27)),
        "{tpmt}"
    );
    // each step names its doc, so an agent can say which OwnGenome step it is
    assert!(
        steps[0]["cite"]
            .as_str()
            .is_some_and(|c| c.starts_with("https://github.com/GeiserX/OwnGenome/")),
        "{tpmt}"
    );
    // the findings page by size, so the score is read in its own category
    let env = c
        .call_tool(
            "get_genome_findings",
            json!({"profile": "alex", "category": "prs", "min_evidence": "E3"}),
        )
        .await?
        .ok();
    let prs = env["data"]["findings"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|f| f["category"] == "prs")
        .cloned()
        .unwrap_or_default();
    assert_eq!(
        prs["prs"],
        json!({"status": "interpretable", "reason_code": null}),
        "{prs}"
    );

    let env = c
        .call_tool("check_drug", json!({"profile": "alex", "drug": "azathioprine"}))
        .await?
        .ok();
    let row = env["data"]["pgx"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|p| p["gene"] == "TPMT")
        .cloned()
        .unwrap_or_default();
    assert_eq!(row["stale"], tpmt["stale"], "{env}");

    // Sam reads his own chip
    let sam = w
        .token(Tok {
            user: "sam",
            profiles: vec!["sam"],
            ..Tok::default()
        })
        .await?;
    let c = w.connect_real(&sam).await?;
    let env = c
        .call_tool("get_genome_findings", json!({"profile": "sam", "detail": "full"}))
        .await?
        .ok();
    let brca2 = find(&env, "BRCA2");
    assert_eq!(brca2["confidence"], "unconfirmed", "{env}");
    assert_eq!(
        brca2["chip_note"],
        json!({"code": "chip_rare_unconfirmed", "ppv_pct": 16, "cite": "https://pubmed.ncbi.nlm.nih.gov/33589468/"})
    );
    Ok(())
}
