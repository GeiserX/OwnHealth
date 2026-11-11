//! REST: `GET /profiles/{p}/genome/events`, the ClinVar diff's history, gated per reader. All data
//! is synthetic.

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use ownhealth_api::routes::genome::events::events;
use ownhealth_auth::sessions;
use ownhealth_core::Runtime;
use ownhealth_core::access::{Principal, PrincipalKind, scope_set};
use ownhealth_core::demo::{Family, create_rivera_family};
use ownhealth_core::testing::TestDb;
use ownhealth_genome::{clinvar, synthetic};
use serde_json::Value;
use tower::ServiceExt;

type R = anyhow::Result<()>;

struct Env {
    db: TestDb,
    dir: tempfile::TempDir,
    rt: Runtime,
    app: Router,
    fam: Family,
    /// Alex's genome-facts document.
    doc: uuid::Uuid,
}

const HEAD: &str = "##fileformat=VCFv4.1\n#CHROM\tPOS\tID\tREF\tALT\tQUAL\tFILTER\tINFO\n";

/// Two releases: APOE (sensitive) and CFTR change between them; Alex carries both sites.
fn releases(dir: &std::path::Path) -> anyhow::Result<(std::path::PathBuf, std::path::PathBuf)> {
    let row = |pos: &str, sig: &str, rev: &str| -> String {
        let (chrom, p, vid, r, a, gene) = match pos {
            "apoe" => ("19", 44908684, 17864, "T", "C", "APOE"),
            _ => ("7", 117559590, 7105, "ATCT", "A", "CFTR"),
        };
        format!("{chrom}\t{p}\t{vid}\t{r}\t{a}\t.\t.\tCLNREVSTAT={rev};CLNSIG={sig};GENEINFO={gene}:1\n")
    };
    let one = dir.join("one.vcf");
    let two = dir.join("two.vcf");
    std::fs::write(
        &one,
        format!(
            "##fileDate=20261005\n{HEAD}{}{}",
            row("cftr", "Pathogenic", "reviewed_by_expert_panel"),
            row("apoe", "risk_factor", "no_assertion_criteria_provided")
        ),
    )?;
    std::fs::write(
        &two,
        format!(
            "##fileDate=20261012\n{HEAD}{}{}",
            row("cftr", "Uncertain_significance", "criteria_provided,_single_submitter"),
            row("apoe", "Pathogenic", "criteria_provided,_single_submitter")
        ),
    )?;
    Ok((one, two))
}

async fn env() -> anyhow::Result<Env> {
    let db = TestDb::new().await?;
    let dir = tempfile::tempdir()?;
    let mut settings = db.settings(dir.path())?;
    settings.dev_auth = false;
    let rt = Runtime::with_pool(settings, db.pool.clone())?;
    let mut c = db.pool.acquire().await?;
    let fam = create_rivera_family(&mut c, "Rivera").await?;
    let raw = synthetic::alex_raw_files()?;
    let (facts, att) = synthetic::with_files(&synthetic::alex_facts(), &rt.blobs, &raw)?;
    let (doc, _) = synthetic::seed_genome(
        &mut c,
        &rt.blobs,
        fam.household_id,
        fam.profile("alex"),
        &facts,
        &att,
        "g.json",
        None,
    )
    .await?;
    // Carmen, whom Alex cares for without read:genome, carries the same CFTR site.
    let mut carmen = synthetic::alex_facts();
    carmen["subject"]["sample_id"] = serde_json::json!("CO-01");
    synthetic::seed_genome(
        &mut c,
        &rt.blobs,
        fam.household_id,
        fam.profile("carmen"),
        &carmen,
        &[],
        "c.json",
        None,
    )
    .await?;
    drop(c);
    let (one, two) = releases(dir.path())?;
    for r in [one, two] {
        clinvar::diff_release(&db.pool, &rt.blobs, &r, 18).await?;
    }
    let app = Router::new()
        .nest("/api", ownhealth_api::router())
        .with_state(rt.clone());
    Ok(Env {
        db,
        dir,
        rt,
        app,
        fam,
        doc,
    })
}

impl Env {
    async fn get(&self, slug: &str, uri: &str) -> anyhow::Result<(StatusCode, Value)> {
        self.send(slug, Request::get(uri), Body::empty()).await
    }

    async fn post(&self, slug: &str, uri: &str, body: Value) -> anyhow::Result<(StatusCode, Value)> {
        let req = Request::post(uri).header("content-type", "application/json");
        self.send(slug, req, Body::from(body.to_string())).await
    }

    async fn send(
        &self,
        slug: &str,
        req: axum::http::request::Builder,
        body: Body,
    ) -> anyhow::Result<(StatusCode, Value)> {
        let mut c = self.db.pool.acquire().await?;
        let secret =
            sessions::create_session_with(&mut c, self.fam.user(slug), None, None, "passkey", Some(false)).await?;
        let req = req
            .header("cookie", format!("{}={secret}", sessions::COOKIE))
            .body(body)?;
        let res = self.app.clone().oneshot(req).await?;
        let status = res.status();
        let body = to_bytes(res.into_body(), 1 << 24).await?;
        Ok((status, serde_json::from_slice(&body).unwrap_or(Value::Null)))
    }

    fn token(&self, scopes: &[&str]) -> Principal {
        let mut p = Principal::session(self.fam.user("alex"), self.fam.household_id);
        p.kind = PrincipalKind::Token;
        p.scopes = scope_set(scopes);
        p.token_label = Some("test agent".into());
        p
    }
}

fn genes(body: &Value) -> Vec<String> {
    body["events"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|e| e["gene"].as_str().unwrap_or("-").to_string())
        .collect()
}

#[tokio::test]
async fn the_reader_gate_decides_whether_an_event_is_named() -> R {
    let e = env().await?;
    assert!(e.dir.path().exists());
    // The sensitive switch is off: APOE reads as a withheld result, CFTR is named.
    let (status, body) = e.get("alex", "/api/profiles/alex/genome/events").await?;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(genes(&body), vec!["CFTR", "-"], "{body}");
    assert_eq!(body["withheld"]["sensitive"], 1);
    let hidden = &body["events"][1];
    assert_eq!(hidden["withheld"], "sensitive");
    assert_eq!(hidden["text"]["en"], "A withheld result changed in ClinVar.");
    assert_eq!(
        hidden["text"]["es"],
        "Ha cambiado en ClinVar un resultado que está oculto."
    );
    for leak in ["APOE", "17864", "upgraded", "Pathogenic"] {
        assert!(!hidden.to_string().contains(leak), "{leak} in {hidden}");
    }
    // Nothing but the release window, the chip flag and the generic text: not even whether it alerted,
    // which would narrow the change to an upgrade, a shown downgrade or an SF gene.
    for key in ["kind", "gene", "variation_id", "old", "new", "alerted"] {
        assert!(hidden.get(key).is_none(), "{key} in {hidden}");
    }
    let cftr = &body["events"][0];
    assert_eq!(cftr["kind"], "downgraded_from_plp");
    assert_eq!(cftr["new"]["class"], "vus");
    assert_eq!(cftr["release_from"], "2026-10-05");
    assert_eq!(cftr["alerted"], true);

    // Switch on: the owner sees the APOE event named.
    sqlx::query("update profile set genome_sensitive_ui = true where id = $1")
        .bind(e.fam.profile("alex"))
        .execute(&e.db.pool)
        .await?;
    let (_, body) = e.get("alex", "/api/profiles/alex/genome/events").await?;
    assert_eq!(genes(&body), vec!["CFTR", "APOE"], "{body}");
    assert!(
        body["events"][1]["text"]["es"]
            .as_str()
            .is_some_and(|s| s.contains("APOE") && s.contains("Antes: otro tipo de clasificación")),
        "{body}"
    );

    // An agent needs the agents switch and read:sensitive too; without them APOE stays withheld.
    let agent = events(&e.rt, &e.token(&["read:genome"]), "alex")
        .await
        .map_err(|e| anyhow::anyhow!("{e:?}"))?;
    assert_eq!(genes(&agent), vec!["CFTR", "-"]);
    // Without read:genome nothing is answered.
    let refused = events(&e.rt, &e.token(&["read:labs"]), "alex").await;
    assert!(refused.is_err());

    // Every read wrote one audit row with ids only.
    let audits: Vec<(Option<String>, Option<Value>)> = sqlx::query_as(
        "select tool, withheld from audit_event where profile_id = $1 and resource_type = 'genome_event' order by id",
    )
    .bind(e.fam.profile("alex"))
    .fetch_all(&e.db.pool)
    .await?;
    assert_eq!(audits.len(), 3);
    assert_eq!(audits[0].0.as_deref(), Some("GET /api/profiles/{p}/genome/events"));
    Ok(())
}

#[tokio::test]
async fn a_minor_s_events_are_withheld_by_the_minor_rule() -> R {
    let e = env().await?;
    let mut c = e.db.pool.acquire().await?;
    synthetic::seed_genome(
        &mut c,
        &e.rt.blobs,
        e.fam.household_id,
        e.fam.profile("lucia"),
        &synthetic::lucia_facts(),
        &[],
        "l.json",
        None,
    )
    .await?;
    drop(c);
    let (_, two) = releases(e.dir.path())?;
    let three = e.dir.path().join("three.vcf");
    std::fs::write(
        &three,
        std::fs::read_to_string(two)?.replace("20261012", "20261019").replace(
            "CLNREVSTAT=criteria_provided,_single_submitter;CLNSIG=Uncertain_significance",
            "CLNREVSTAT=criteria_provided,_single_submitter;CLNSIG=Benign",
        ),
    )?;
    clinvar::diff_release(&e.db.pool, &e.rt.blobs, &three, 18).await?;
    // Lucía's first run against her facts: CFTR P -> benign, APOE risk factor -> P.
    let (status, body) = e.get("alex", "/api/profiles/lucia/genome/events").await?;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["withheld"]["minor_rule"], 2, "{body}");
    assert_eq!(genes(&body), vec!["-", "-"]);
    assert!(!body.to_string().contains("CFTR") && !body.to_string().contains("APOE"));
    Ok(())
}

#[tokio::test]
async fn a_caregiver_without_read_genome_sees_no_gene_in_the_review_queue() -> R {
    let e = env().await?;
    let (status, body) = e.get("alex", "/api/profiles/carmen/genome/events").await?;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    let (status, body) = e
        .get(
            "alex",
            "/api/review?status=open&profile=carmen&kind=genome_reinterpretation",
        )
        .await?;
    assert_eq!(status, StatusCode::OK, "{body}");
    let items = body["items"].as_array().cloned().unwrap_or_default();
    assert!(!items.is_empty(), "{body}");
    // Nor which way the class moved: that alone would say Carmen carries a pathogenic variant.
    for leak in ["CFTR", "7105", "APOE", "17864"]
        .into_iter()
        .chain(clinvar::EVENT_KINDS)
    {
        assert!(!body.to_string().contains(leak), "{leak} in {body}");
    }
    // The single-item view says no more than the list.
    for item in &items {
        let id = item["id"].as_str().unwrap_or_default();
        let (status, one) = e.get("alex", &format!("/api/review/{id}")).await?;
        assert_eq!(status, StatusCode::OK, "{one}");
        for leak in ["CFTR", "7105", "APOE", "17864"]
            .into_iter()
            .chain(clinvar::EVENT_KINDS)
        {
            assert!(!one.to_string().contains(leak), "{leak} in {one}");
        }
    }
    Ok(())
}

#[tokio::test]
async fn retracting_the_genome_hides_its_events_and_dismisses_its_alerts() -> R {
    let e = env().await?;
    let open = "/api/review?status=open&profile=alex&kind=genome_reinterpretation";
    let (_, before) = e.get("alex", open).await?;
    assert!(
        !before["items"].as_array().cloned().unwrap_or_default().is_empty(),
        "{before}"
    );
    let (status, body) = e
        .post(
            "alex",
            &format!("/api/documents/{}/retract", e.doc),
            serde_json::json!({"reason": "another person's file"}),
        )
        .await?;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = e.get("alex", "/api/profiles/alex/genome/events").await?;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["events"], serde_json::json!([]), "{body}");
    let (_, after) = e.get("alex", open).await?;
    assert_eq!(after["items"], serde_json::json!([]), "{after}");
    Ok(())
}
