//! REST: the couple carrier view (`GET /api/genome/carrier-couple`), genomics.md section 10.
//!
//! Alex is a WGS genome with CFTR, GJB2 and BRCA2 carrier facts. Sam is a consumer chip with GJB2
//! (unconfirmed, as the importer demands of a chip) and an X-linked DMD fact, and no CFTR coverage.
//! People call over HTTP with real session cookies.

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use ownhealth_auth::sessions;
use ownhealth_core::access::{scope_set, set_grant};
use ownhealth_core::demo::{Family, create_rivera_family};
use ownhealth_core::testing::TestDb;
use ownhealth_genome::{fact_id, synthetic};
use serde_json::{Value, json};
use tower::ServiceExt;
use uuid::Uuid;

type R = anyhow::Result<()>;

struct Env {
    db: TestDb,
    _dir: tempfile::TempDir,
    app: Router,
    fam: Family,
}

fn carrier(
    gene: &str,
    chrom: &str,
    pos: i64,
    inheritance: &str,
    condition: &str,
    confidence: &str,
    sensitivity: &str,
) -> Value {
    let vrs = synthetic::vrs_id(chrom, pos, "G", "A");
    json!({
        "id": fact_id("carrier", gene, &vrs),
        "category": "carrier",
        "gene": gene,
        "variants": [{"chrom": chrom, "pos": pos, "ref": "G", "alt": "A", "vrs_id": vrs,
                      "genotype": {"call_state": "alt", "gt": "0/1", "zygosity": "het"}}],
        "condition": {"name": condition, "inheritance": inheritance},
        "summary": format!("Heterozygous carrier of a {gene} variant (synthetic)."),
        "evidence": {"level": "E1", "native": "ClinVar Pathogenic, 3 stars"},
        "confidence": {"level": confidence, "basis": "synthetic"},
        "sensitivity": {"level": sensitivity},
    })
}

fn push_facts(doc: &mut Value, facts: Vec<Value>) {
    if let Some(a) = doc["facts"].as_array_mut() {
        a.extend(facts);
    }
}

/// Alex's WGS facts plus GJB2 and BRCA2 carrier facts.
fn alex() -> Value {
    let mut doc = synthetic::alex_facts();
    push_facts(
        &mut doc,
        vec![
            carrier("GJB2", "chr13", 20189546, "AR", "Hearing loss", "high", "normal"),
            carrier("BRCA2", "chr13", 32340300, "AR", "Fanconi anemia", "high", "sensitive"),
        ],
    );
    if let Some(c) = doc["coverage"].as_array_mut() {
        c.push(json!({"scope": "gene", "id": "GJB2", "panel": "carrier", "state": "assessed"}));
    }
    doc
}

/// Sam's chip: GJB2 and DMD calls, no CFTR coverage.
fn sam_chip() -> Value {
    let mut doc = synthetic::sam_facts();
    doc["assay"] = json!({"type": "chip", "platform": "consumer array", "build": "GRCh38"});
    doc["facts"] = json!([]);
    doc["coverage"] = json!([
        {"scope": "gene", "id": "GJB2", "panel": "carrier", "state": "partial", "reason": "not_on_array"},
        {"scope": "gene", "id": "CFTR", "panel": "carrier", "state": "not_assessed", "reason": "not_on_array"},
    ]);
    push_facts(
        &mut doc,
        vec![
            carrier("GJB2", "chr13", 20189546, "AR", "Hearing loss", "unconfirmed", "normal"),
            carrier(
                "DMD",
                "chrX",
                31200000,
                "XL",
                "Duchenne muscular dystrophy",
                "unconfirmed",
                "normal",
            ),
        ],
    );
    doc
}

async fn env() -> anyhow::Result<Env> {
    env_with(alex(), sam_chip()).await
}

async fn env_with(alex: Value, sam: Value) -> anyhow::Result<Env> {
    let db = TestDb::new().await?;
    let dir = tempfile::tempdir()?;
    let mut settings = db.settings(dir.path())?;
    settings.dev_auth = false;
    let rt = ownhealth_core::Runtime::with_pool(settings, db.pool.clone())?;
    let mut c = db.pool.acquire().await?;
    let fam = create_rivera_family(&mut c, "Rivera").await?;
    for (slug, facts) in [("alex", alex), ("sam", sam), ("lucia", synthetic::lucia_facts())] {
        synthetic::seed_genome(
            &mut c,
            &rt.blobs,
            fam.household_id,
            fam.profile(slug),
            &facts,
            &[],
            "genome-facts.json",
            None,
        )
        .await?;
    }
    drop(c);
    let app = Router::new().nest("/api", ownhealth_api::router()).with_state(rt);
    Ok(Env {
        db,
        _dir: dir,
        app,
        fam,
    })
}

impl Env {
    async fn get(&self, slug: &str, uri: &str) -> anyhow::Result<(StatusCode, Value)> {
        let mut c = self.db.pool.acquire().await?;
        let secret =
            sessions::create_session_with(&mut c, self.fam.user(slug), None, None, "passkey", Some(true)).await?;
        let req = Request::get(uri)
            .header("cookie", format!("{}={secret}", sessions::COOKIE))
            .body(Body::empty())?;
        let res = self.app.clone().oneshot(req).await?;
        let status = res.status();
        let body = to_bytes(res.into_body(), 1 << 24).await?;
        Ok((status, serde_json::from_slice(&body).unwrap_or(Value::Null)))
    }

    /// Sam shares her genome with Alex (a viewer grant widened by read:genome).
    async fn share_sam_genome(&self) -> R {
        let mut c = self.db.pool.acquire().await?;
        let cats = scope_set(["read:summary", "read:genome"]);
        set_grant(
            &mut c,
            self.fam.user("alex"),
            self.fam.profile("sam"),
            "viewer",
            Some(&cats),
            None,
            None,
        )
        .await?;
        Ok(())
    }

    /// Opens sensitive results on both sides for Alex: both profiles' switches, and read:sensitive on Sam.
    async fn open_sensitive(&self) -> R {
        let mut c = self.db.pool.acquire().await?;
        let cats = scope_set(["read:summary", "read:genome", "read:sensitive"]);
        set_grant(
            &mut c,
            self.fam.user("alex"),
            self.fam.profile("sam"),
            "viewer",
            Some(&cats),
            None,
            None,
        )
        .await?;
        sqlx::query("update profile set genome_sensitive_ui = true where id = any($1)")
            .bind(vec![self.fam.profile("alex"), self.fam.profile("sam")])
            .execute(&mut *c)
            .await?;
        Ok(())
    }

    async fn audit_count(&self, slug: &str) -> anyhow::Result<i64> {
        Ok(sqlx::query_scalar(
            "select count(*) from audit_event where profile_id = $1 and tool = 'GET /api/genome/carrier-couple'",
        )
        .bind(self.fam.profile(slug))
        .fetch_one(&self.db.pool)
        .await?)
    }
}

fn row<'a>(body: &'a Value, gene: &str) -> &'a Value {
    body["rows"]
        .as_array()
        .and_then(|rows| rows.iter().find(|r| r["gene"] == gene))
        .unwrap_or(&Value::Null)
}

const URI: &str = "/api/genome/carrier-couple?a=alex&b=sam";

#[tokio::test]
async fn without_read_genome_on_the_partner_the_answer_is_403_with_no_data() -> R {
    let env = env().await?;
    let (s, body) = env.get("alex", URI).await?;
    assert_eq!(s, StatusCode::FORBIDDEN);
    assert_eq!(body["detail"]["code"], "insufficient_scope");
    assert!(body.get("rows").is_none());
    let text = body.to_string();
    for gene in ["GJB2", "CFTR", "DMD", "BRCA2"] {
        assert!(!text.contains(gene), "{gene} in {text}");
    }
    assert_eq!(env.audit_count("alex").await? + env.audit_count("sam").await?, 0);
    Ok(())
}

#[tokio::test]
async fn a_chip_side_gives_the_unconfirmed_row_with_no_figure() -> R {
    let env = env().await?;
    env.share_sam_genome().await?;
    let (s, body) = env.get("alex", URI).await?;
    assert_eq!(s, StatusCode::OK, "{body}");
    let gjb2 = row(&body, "GJB2");
    assert_eq!(gjb2["kind"], "ar_both_unconfirmed");
    assert_eq!(gjb2["risk"], Value::Null);
    assert_eq!(gjb2["a"]["confirmed"], true);
    assert_eq!(gjb2["b"]["confirmed"], false);
    assert!(
        gjb2["b"]["label"]
            .as_str()
            .unwrap_or_default()
            .contains("a clinical carrier test comes first")
    );
    assert!(!gjb2["text"].as_str().unwrap_or_default().contains("1 in 4"));
    assert!(
        gjb2["counsel"]
            .as_str()
            .unwrap_or_default()
            .contains("genetic counselling")
    );
    assert_eq!(gjb2["a"]["src"].as_array().map(Vec::len), Some(1));
    Ok(())
}

#[tokio::test]
async fn a_reviewed_partner_gives_the_one_in_four_row() -> R {
    let env = env().await?;
    env.share_sam_genome().await?;
    // A review confirms Sam's GJB2 call: a new version of the finding, as the lifecycle allows.
    let mut c = env.db.pool.acquire().await?;
    let old: Uuid = sqlx::query_scalar("select id from genome_finding where profile_id = $1 and gene = 'GJB2'")
        .bind(env.fam.profile("sam"))
        .fetch_one(&mut *c)
        .await?;
    sqlx::query("update genome_finding set status = 'superseded', superseded_at = now() where id = $1")
        .bind(old)
        .execute(&mut *c)
        .await?;
    sqlx::query(
        "insert into genome_finding (dataset_id, profile_id, fact_id, category, gene, rsid, hgvs, vrs_id, zygosity, \
         evidence, confidence, call_state, verification_status, sensitive, adult_onset, carrier, src_id, supersedes, fact) \
         select dataset_id, profile_id, fact_id, category, gene, rsid, hgvs, vrs_id, zygosity, evidence, confidence, \
         call_state, 'confirmed', sensitive, adult_onset, carrier, src_id, id, fact from genome_finding where id = $1",
    )
    .bind(old)
    .execute(&mut *c)
    .await?;
    drop(c);
    let (s, body) = env.get("alex", URI).await?;
    assert_eq!(s, StatusCode::OK, "{body}");
    let gjb2 = row(&body, "GJB2");
    assert_eq!(gjb2["kind"], "ar_both");
    assert_eq!(gjb2["risk"], "1/4");
    assert!(gjb2["text"].as_str().unwrap_or_default().contains("1 in 4"));
    assert!(gjb2["cite"].as_str().unwrap_or_default().starts_with("https://"));
    assert_eq!(body["rows"][0]["gene"], "GJB2", "the shared gene comes first");
    Ok(())
}

#[tokio::test]
async fn not_assessed_x_linked_and_withheld_rows() -> R {
    let env = env().await?;
    env.share_sam_genome().await?;
    let (s, body) = env.get("alex", &format!("{URI}&lang=es")).await?;
    assert_eq!(s, StatusCode::OK, "{body}");
    let cftr = row(&body, "CFTR");
    assert_eq!(cftr["kind"], "ar_one_not_assessed");
    assert_eq!(cftr["b"]["state"], "not_assessed");
    assert!(
        cftr["text"]
            .as_str()
            .unwrap_or_default()
            .contains("no evaluado no es lo mismo que no portador")
    );
    // Alex's whole genome holds no DMD coverage row: nothing found, coverage not stated.
    let dmd = row(&body, "DMD");
    assert_eq!(dmd["a"]["state"], "not_stated", "{dmd}");
    assert_eq!((&dmd["kind"], &dmd["inheritance"]), (&json!("xl_one"), &json!("XL")));
    assert_eq!(dmd["risk"], Value::Null);
    // BRCA2 is a sensitive gene and Alex's sensitive switch is off: no gene name anywhere.
    assert!(!body.to_string().contains("BRCA2"), "{body}");
    assert_eq!(body["withheld"]["sensitive"], 1);
    let withheld: Vec<&Value> = body["rows"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|r| r["kind"] == "withheld")
        .collect();
    assert_eq!(withheld.len(), 1);
    assert_eq!(withheld[0]["gene"], Value::Null);
    assert_eq!(body["lang"], "es");
    // One audit row per profile, identifiers only.
    assert_eq!((env.audit_count("alex").await?, env.audit_count("sam").await?), (1, 1));
    let ids: Option<Vec<String>> = sqlx::query_scalar(
        "select resource_ids from audit_event where profile_id = $1 and tool = 'GET /api/genome/carrier-couple'",
    )
    .bind(env.fam.profile("alex"))
    .fetch_one(&env.db.pool)
    .await?;
    let ids = ids.unwrap_or_default();
    assert_eq!(ids.len(), 2, "CFTR and GJB2, never the withheld BRCA2: {ids:?}");
    assert!(ids.iter().all(|i| i.starts_with("f_")));
    Ok(())
}

#[tokio::test]
async fn a_minor_or_the_same_profile_is_refused() -> R {
    let env = env().await?;
    for uri in [
        "/api/genome/carrier-couple?a=alex&b=lucia",
        "/api/genome/carrier-couple?a=lucia&b=alex",
    ] {
        let (s, body) = env.get("alex", uri).await?;
        assert_eq!(
            (s, &body["detail"]["code"]),
            (StatusCode::BAD_REQUEST, &json!("couple_not_eligible")),
            "{uri}"
        );
        assert!(body.get("rows").is_none());
    }
    let (s, body) = env.get("alex", "/api/genome/carrier-couple?a=alex&b=me").await?;
    assert_eq!(
        (s, &body["detail"]["code"]),
        (StatusCode::BAD_REQUEST, &json!("bad_input"))
    );
    let (s, _) = env.get("alex", "/api/genome/carrier-couple?a=alex").await?;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert_eq!(env.audit_count("alex").await? + env.audit_count("lucia").await?, 0);
    Ok(())
}

#[tokio::test]
async fn a_deceased_partner_is_refused() -> R {
    let env = env().await?;
    env.share_sam_genome().await?;
    sqlx::query("update profile set deceased = true where id = $1")
        .bind(env.fam.profile("sam"))
        .execute(&env.db.pool)
        .await?;
    let (s, body) = env.get("alex", URI).await?;
    assert_eq!(
        (s, &body["detail"]["code"]),
        (StatusCode::BAD_REQUEST, &json!("couple_not_eligible"))
    );
    Ok(())
}

/// The wording file loads through the cited loader: every row cited over https, every template in es
/// and en, none that reads as advice.
#[test]
fn the_wording_file_is_a_cited_file() -> R {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../data/genome/couple.json");
    let f = ownhealth_rules::cited::load(&path)?;
    assert_eq!(f.version, ownhealth_genome::couple::rules_version());
    assert_eq!(f.rows.len(), 3);
    Ok(())
}

/// Sam's sequencing data with one GJB2 carrier call that the pipeline itself marks unconfirmed.
fn sam_wes_unconfirmed() -> Value {
    let mut doc = sam_chip();
    doc["assay"] = json!({"type": "wes", "platform": "exome 100x", "build": "GRCh38"});
    doc["facts"] = json!([carrier(
        "GJB2",
        "chr13",
        20189546,
        "AR",
        "Hearing loss",
        "unconfirmed",
        "normal"
    )]);
    doc
}

#[tokio::test]
async fn an_unconfirmed_sequencing_call_gives_no_figure() -> R {
    let env = env_with(alex(), sam_wes_unconfirmed()).await?;
    env.share_sam_genome().await?;
    let (s, body) = env.get("alex", URI).await?;
    assert_eq!(s, StatusCode::OK, "{body}");
    let gjb2 = row(&body, "GJB2");
    assert_eq!(gjb2["b"]["assays"], json!(["wes"]), "{gjb2}");
    assert_eq!(gjb2["b"]["confirmed"], false);
    assert_eq!(gjb2["kind"], "ar_both_unconfirmed");
    assert_eq!(gjb2["risk"], Value::Null);
    Ok(())
}

/// Alex holds an ACMG SF BRCA2 finding (a dominant cancer gene, not a carrier fact); Sam carries a BRCA2
/// variant for the recessive Fanconi anemia. Alex's side must point at that finding, never "not detected".
#[tokio::test]
async fn another_finding_in_the_gene_is_never_read_as_not_detected() -> R {
    let mut a = synthetic::alex_facts();
    let vrs = synthetic::vrs_id("chr13", 32338000, "C", "T");
    push_facts(
        &mut a,
        vec![json!({
            "id": fact_id("monogenic_pathogenic", "BRCA2", &vrs),
            "category": "monogenic_pathogenic",
            "gene": "BRCA2",
            "variants": [{"chrom": "chr13", "pos": 32338000, "ref": "C", "alt": "T", "vrs_id": vrs,
                          "genotype": {"call_state": "alt", "gt": "0/1", "zygosity": "het"}}],
            "condition": {"name": "Hereditary breast and ovarian cancer", "inheritance": "AD"},
            "summary": "Heterozygous BRCA2 variant (synthetic).",
            "evidence": {"level": "E1", "native": "ClinVar Pathogenic, 3 stars"},
            "confidence": {"level": "high", "basis": "synthetic"},
            "sensitivity": {"level": "sensitive"},
        })],
    );
    let mut sam = sam_chip();
    sam["facts"] = json!([carrier(
        "BRCA2",
        "chr13",
        32340300,
        "AR",
        "Fanconi anemia",
        "unconfirmed",
        "sensitive"
    )]);
    let env = env_with(a, sam).await?;
    env.open_sensitive().await?;
    let (s, body) = env.get("alex", URI).await?;
    assert_eq!(s, StatusCode::OK, "{body}");
    let brca2 = row(&body, "BRCA2");
    assert_eq!(brca2["kind"], "ar_one_other_finding", "{body}");
    assert_eq!(brca2["a"]["state"], "other_finding");
    assert_eq!(brca2["a"]["src"].as_array().map(Vec::len), Some(1));
    assert_eq!(brca2["risk"], Value::Null);
    let all = body["rows"].as_array().cloned().unwrap_or_default();
    assert!(
        all.iter()
            .all(|r| r["a"]["state"] != "not_detected" && r["b"]["state"] != "not_detected"),
        "{body}"
    );
    Ok(())
}
