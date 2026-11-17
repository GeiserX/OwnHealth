//! Cross-profile hints with per-relative consent (`/profiles/{p}/family-shares`, migration 1111) and
//! their place in `GET /profiles/{p}/pedigree`, over HTTP with real session cookies. The agent side
//! (tokens, the status family section) is in ownhealth-mcp `tests/family_hints.rs`.
//!
//! The Rivera family, synthetic: Alex is Carmen's guardian here, so he manages her consent. Alex's
//! history links his mother to Carmen and his daughter to Lucía; Carmen's links her son to Alex;
//! Lucía's links her father to Alex. Carmen, Alex and Lucía each have an LDL-C of 190 mg/dL or more,
//! and Alex carries a BRCA2 P/LP (an ACMG SF gene, so sensitive).

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use ownhealth_auth::sessions;
use ownhealth_core::access::{scope_set, set_genome_switches, set_grant};
use ownhealth_core::demo::{Family, create_rivera_family};
use ownhealth_core::family::{self, MemberIn};
use ownhealth_core::observations::{CommitOptions, ObservationIn, commit_observation};
use ownhealth_core::provenance::{SrcIn, SrcRef, ensure_src};
use ownhealth_core::testing::TestDb;
use serde_json::{Value, json};
use sqlx::PgConnection;
use tower::ServiceExt;
use uuid::Uuid;

type R = anyhow::Result<()>;

struct Env {
    db: TestDb,
    _dir: tempfile::TempDir,
    app: Router,
    fam: Family,
    /// Carmen's LDL-C src id.
    carmen_ldl: String,
}

fn manual(profile_id: Uuid) -> SrcIn {
    SrcIn {
        profile_id: Some(profile_id),
        ..SrcIn::manual(None)
    }
}

/// An LDL-C in mg/dL (its canonical unit); returns its src id.
async fn ldl(c: &mut PgConnection, profile_id: Uuid, value: &str) -> anyhow::Result<String> {
    let o = ObservationIn::from_json(json!({
        "profile_id": profile_id, "category": "laboratory", "printed_name": "Colesterol LDL",
        "printed_value": value, "printed_unit": "mg/dL", "value_num": value, "loinc": "13457-7",
        "alias_group": "ldl_c", "unit_ucum": "mg/dL", "value_canonical": value, "unit_canonical": "mg/dL",
        "derivation_version": "test", "effective_at": "2026-06-10T08:00:00+00:00",
    }))?;
    Ok(
        commit_observation(c, &o, &SrcRef::New(manual(profile_id)), CommitOptions::new("person"))
            .await?
            .src_id,
    )
}

/// A genome dataset with one P/LP finding in an ACMG SF gene.
async fn sf_finding(c: &mut PgConnection, profile_id: Uuid, gene: &str, confidence: &str) -> R {
    let mut s = SrcIn::new("genome_facts");
    s.profile_id = Some(profile_id);
    s.import_ref = Some(format!("facts-{gene}-{confidence}"));
    let sid = ensure_src(&mut *c, &SrcRef::New(s), Some(profile_id)).await?;
    let sha: Vec<u8> = Uuid::new_v4().as_bytes().repeat(2);
    let dataset: Uuid = sqlx::query_scalar(
        "insert into genome_dataset (profile_id, sample_id, assay, facts_schema, facts_sha256, facts_blob, \
         owngenome_version, db_versions, src_id) values ($1, 'S1', 'wgs', 'ownhealth.genome-facts/v1', $2, $2, \
         '1.4.2', '{}', $3) returning id",
    )
    .bind(profile_id)
    .bind(sha)
    .bind(&sid)
    .fetch_one(&mut *c)
    .await?;
    sqlx::query(
        "insert into genome_finding (dataset_id, profile_id, fact_id, category, gene, evidence, confidence, \
         call_state, sensitive, adult_onset, carrier, status, src_id, fact) values \
         ($1, $2, $3, 'acmg_sf', $4, 'E1', $5, 'alt', true, true, false, 'current', $6, '{}')",
    )
    .bind(dataset)
    .bind(profile_id)
    .bind(format!("f_{}", &Uuid::new_v4().simple().to_string()[..16]))
    .bind(gene)
    .bind(confidence)
    .bind(&sid)
    .execute(&mut *c)
    .await?;
    Ok(())
}

async fn relative(c: &mut PgConnection, of: Uuid, code: &str, link: Uuid) -> R {
    let m = MemberIn::new(of, code);
    family::add(&mut *c, &m, &SrcRef::New(manual(of)), "person", None, Some(link)).await?;
    Ok(())
}

async fn env() -> anyhow::Result<Env> {
    let db = TestDb::new().await?;
    let dir = tempfile::tempdir()?;
    let mut settings = db.settings(dir.path())?;
    settings.dev_auth = false;
    let rt = ownhealth_core::Runtime::with_pool(settings, db.pool.clone())?;
    let mut c = db.pool.acquire().await?;
    let fam = create_rivera_family(&mut c, "Rivera").await?;
    let p = |s: &str| fam.profile(s);
    // an elderly parent with no login: her son is her guardian, so he manages her consent
    set_grant(&mut c, fam.user("alex"), p("carmen"), "guardian", None, None, None).await?;
    relative(&mut c, p("alex"), "MTH", p("carmen")).await?;
    relative(&mut c, p("alex"), "DAU", p("lucia")).await?;
    relative(&mut c, p("carmen"), "SON", p("alex")).await?;
    relative(&mut c, p("lucia"), "FTH", p("alex")).await?;
    let carmen_ldl = ldl(&mut c, p("carmen"), "247").await?;
    ldl(&mut c, p("alex"), "195").await?;
    ldl(&mut c, p("lucia"), "200").await?;
    sf_finding(&mut c, p("alex"), "BRCA2", "high").await?;
    drop(c);
    let app = Router::new().nest("/api", ownhealth_api::router()).with_state(rt);
    Ok(Env {
        db,
        _dir: dir,
        app,
        fam,
        carmen_ldl,
    })
}

impl Env {
    async fn send(&self, req: Request<Body>) -> anyhow::Result<(StatusCode, Value)> {
        let res = self.app.clone().oneshot(req).await?;
        let status = res.status();
        let body = to_bytes(res.into_body(), 1 << 24).await?;
        Ok((status, serde_json::from_slice(&body).unwrap_or(Value::Null)))
    }

    /// A browser session for one person; `fresh` is a step-up within the last minutes.
    async fn cookie(&self, who: &str, fresh: bool) -> anyhow::Result<String> {
        let mut c = self.db.pool.acquire().await?;
        let secret =
            sessions::create_session_with(&mut c, self.fam.user(who), None, None, "passkey", Some(fresh)).await?;
        Ok(format!("{}={secret}", sessions::COOKIE))
    }

    async fn call(
        &self,
        who: &str,
        fresh: bool,
        method: &str,
        uri: &str,
        body: Option<Value>,
    ) -> anyhow::Result<(StatusCode, Value)> {
        let req = Request::builder()
            .method(method)
            .uri(uri)
            .header("cookie", self.cookie(who, fresh).await?)
            .header("content-type", "application/json")
            .body(body.map_or_else(Body::empty, |b| Body::from(b.to_string())))?;
        self.send(req).await
    }

    async fn get(&self, who: &str, uri: &str) -> anyhow::Result<(StatusCode, Value)> {
        self.call(who, false, "GET", uri, None).await
    }

    /// `source` shares `categories` with `target`, as `who` with a fresh step-up.
    async fn share(
        &self,
        who: &str,
        source: &str,
        target: &str,
        categories: &[&str],
    ) -> anyhow::Result<(StatusCode, Value)> {
        let uri = format!("/api/profiles/{source}/family-shares");
        self.call(
            who,
            true,
            "PUT",
            &uri,
            Some(json!({"target": target, "categories": categories})),
        )
        .await
    }

    async fn revoke(&self, who: &str, source: &str, target: &str) -> anyhow::Result<(StatusCode, Value)> {
        let uri = format!("/api/profiles/{source}/family-shares?target={target}");
        self.call(who, false, "DELETE", &uri, None).await
    }

    async fn pedigree(&self, who: &str, slug: &str) -> anyhow::Result<Value> {
        let (s, v) = self.get(who, &format!("/api/profiles/{slug}/pedigree")).await?;
        assert_eq!(s, StatusCode::OK, "{v}");
        Ok(v["patterns"].clone())
    }

    async fn shares(&self) -> anyhow::Result<i64> {
        Ok(sqlx::query_scalar("select count(*) from family_share")
            .fetch_one(&self.db.pool)
            .await?)
    }

    /// A share written straight to the table, as a stale or hand-made row would be.
    async fn raw_share(&self, source: &str, target: &str, categories: &[&str]) -> R {
        sqlx::query("insert into family_share (source_profile_id, target_profile_id, categories) values ($1, $2, $3)")
            .bind(self.fam.profile(source))
            .bind(self.fam.profile(target))
            .bind(categories.iter().map(|c| c.to_string()).collect::<Vec<_>>())
            .execute(&self.db.pool)
            .await?;
        Ok(())
    }
}

fn hints(patterns: &Value) -> Vec<String> {
    patterns["hints"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|h| {
            format!(
                "{} {}",
                h["rule"].as_str().unwrap_or_default(),
                h["rel"].as_str().unwrap_or_default()
            )
        })
        .collect()
}

#[tokio::test]
async fn a_lab_hint_fires_on_consent_and_a_revoke_removes_it_at_once() -> R {
    let e = env().await?;
    let before = e.pedigree("alex", "alex").await?;
    assert!(before.get("hints").is_none(), "no share, no hint: {before}");
    let (s, v) = e.share("alex", "carmen", "alex", &["labs"]).await?;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["categories"], json!(["labs"]));
    let after = e.pedigree("alex", "alex").await?;
    assert_eq!(hints(&after), ["lab.ldl_fh MTH"], "{after}");
    let h = &after["hints"][0];
    assert_eq!(h["analyte"], "ldl_c");
    assert!(h["cite"].as_str().is_some_and(|c| c.starts_with("https://")), "{h}");
    assert!(
        h["title"]["es"].as_str().is_some_and(|t| t.contains("190 mg/dL")),
        "{h}"
    );
    assert_ne!(after["family_hash"], before["family_hash"]);
    let (s, _) = e.revoke("alex", "carmen", "alex").await?;
    assert_eq!(s, StatusCode::OK);
    let gone = e.pedigree("alex", "alex").await?;
    assert!(gone.get("hints").is_none(), "{gone}");
    assert_eq!(gone["family_hash"], before["family_hash"]);
    Ok(())
}

#[tokio::test]
async fn the_consent_list_shows_each_profile_and_its_degree() -> R {
    let e = env().await?;
    e.share("alex", "carmen", "alex", &["labs"]).await?;
    let (s, v) = e.get("alex", "/api/profiles/carmen/family-shares").await?;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["categories"], json!(["labs", "cascade"]));
    let item = |slug: &str| {
        v["items"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|i| i["profile"] == slug)
            .cloned()
            .unwrap_or_default()
    };
    assert_eq!(item("alex")["categories"], json!(["labs"]));
    assert_eq!(item("alex")["degree"], 1, "Alex's history links his mother to Carmen");
    assert_eq!(item("sam")["degree"], Value::Null, "Sam's history does not list Carmen");
    assert_eq!(item("lucia")["minor"], true);
    assert_eq!(v["items"].as_array().map(Vec::len), Some(3));
    Ok(())
}

#[tokio::test]
async fn only_the_sources_manager_writes_and_a_grant_needs_step_up() -> R {
    let e = env().await?;
    // widening needs a fresh step-up; narrowing and revoking do not
    let uri = "/api/profiles/carmen/family-shares";
    let body = json!({"target": "alex", "categories": ["labs", "cascade"]});
    let (s, v) = e.call("alex", false, "PUT", uri, Some(body.clone())).await?;
    assert_eq!(
        (s, v["detail"]["code"].clone()),
        (StatusCode::FORBIDDEN, json!("step_up_required")),
        "{v}"
    );
    assert_eq!(e.shares().await?, 0);
    let (s, _) = e.call("alex", true, "PUT", uri, Some(body)).await?;
    assert_eq!(s, StatusCode::OK);
    let narrow = json!({"target": "alex", "categories": ["labs"]});
    let (s, v) = e.call("alex", false, "PUT", uri, Some(narrow)).await?;
    assert_eq!((s, v["categories"].clone()), (StatusCode::OK, json!(["labs"])), "{v}");
    // Sam is a viewer of Alex's profile: she manages nothing of his
    let (s, v) = e.share("sam", "alex", "carmen", &["labs"]).await?;
    assert_eq!(s, StatusCode::FORBIDDEN, "{v}");
    for (body, why) in [
        (
            json!({"target": "alex", "categories": ["vitals"]}),
            "an unknown category",
        ),
        (json!({"target": "carmen", "categories": ["labs"]}), "itself"),
        (json!({"target": "nobody", "categories": ["labs"]}), "no such profile"),
        (json!({"target": "alex"}), "no categories"),
    ] {
        let (s, v) = e.call("alex", true, "PUT", uri, Some(body)).await?;
        assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{why}: {v}");
    }
    // every change is audited with ids only
    let n: i64 = sqlx::query_scalar(
        "select count(*) from audit_event where tool = 'family_share' and action = 'consent_change' and profile_id = $1",
    )
    .bind(e.fam.profile("carmen"))
    .fetch_one(&e.db.pool)
    .await?;
    assert_eq!(n, 2);
    Ok(())
}

#[tokio::test]
async fn a_minor_is_never_a_source_and_never_the_target_of_a_genome_hint() -> R {
    let e = env().await?;
    // Lucía (8) with LDL-C 200: her guardian cannot share it, and a row written by hand feeds nothing
    let (s, v) = e.share("alex", "lucia", "alex", &["labs"]).await?;
    assert_eq!(
        (s, v["detail"]["code"].clone()),
        (StatusCode::CONFLICT, json!("minor")),
        "{v}"
    );
    e.raw_share("lucia", "alex", &["labs", "cascade"]).await?;
    assert!(e.pedigree("alex", "alex").await?.get("hints").is_none());
    // Alex's BRCA2 never lists Lucía: the share is refused, and a row written by hand gives nothing
    let (s, v) = e.share("alex", "alex", "lucia", &["cascade"]).await?;
    assert_eq!(
        (s, v["detail"]["code"].clone()),
        (StatusCode::CONFLICT, json!("minor_target")),
        "{v}"
    );
    e.raw_share("alex", "lucia", &["labs", "cascade"]).await?;
    let lucia = e.pedigree("alex", "lucia").await?;
    assert_eq!(hints(&lucia), ["lab.ldl_fh FTH"], "{lucia}");
    assert!(!lucia.to_string().contains("cascade\""), "{lucia}");
    // the FH hint about Lucía is for her guardians only: Sam as a caregiver reads her labs, sees none
    assert_eq!(hints(&e.pedigree("sam", "lucia").await?), ["lab.ldl_fh FTH"]);
    let mut c = e.db.pool.acquire().await?;
    set_grant(
        &mut c,
        e.fam.user("sam"),
        e.fam.profile("lucia"),
        "caregiver",
        None,
        None,
        None,
    )
    .await?;
    drop(c);
    let as_caregiver = e.pedigree("sam", "lucia").await?;
    assert!(as_caregiver.get("hints").is_none(), "{as_caregiver}");
    Ok(())
}

#[tokio::test]
async fn a_gene_shows_only_through_the_targets_open_gate() -> R {
    let e = env().await?;
    let (s, v) = e.share("alex", "alex", "carmen", &["cascade"]).await?;
    assert_eq!(s, StatusCode::OK, "{v}");
    // Alex reads Carmen's genome, but her sensitive switch is off: the hint names no gene
    let closed = e.pedigree("alex", "carmen").await?;
    assert_eq!(hints(&closed), ["genome.cascade SON"], "{closed}");
    assert_eq!(closed["hints"][0]["withheld"], true);
    assert_eq!(closed["hints"][0]["title"]["en"], "A relative shared a genetic finding");
    assert!(!closed.to_string().contains("BRCA2"), "{closed}");
    // the control: her switch on, the gene is named
    let mut c = e.db.pool.acquire().await?;
    set_genome_switches(&mut c, e.fam.profile("carmen"), Some(true), Some(false)).await?;
    drop(c);
    let open = e.pedigree("alex", "carmen").await?;
    assert_eq!(open["hints"][0]["gene"], "BRCA2", "{open}");
    Ok(())
}

#[tokio::test]
async fn an_unconfirmed_call_feeds_no_cascade_hint() -> R {
    let e = env().await?;
    let mut c = e.db.pool.acquire().await?;
    sf_finding(&mut c, e.fam.profile("carmen"), "MLH1", "unconfirmed").await?;
    drop(c);
    e.share("alex", "carmen", "alex", &["cascade"]).await?;
    assert!(e.pedigree("alex", "alex").await?.get("hints").is_none());
    Ok(())
}

#[tokio::test]
async fn a_hint_names_no_source_and_opens_no_source_record() -> R {
    let e = env().await?;
    e.share("alex", "carmen", "alex", &["labs"]).await?;
    // Sam reads Alex's records and labs, never Carmen's
    let mut c = e.db.pool.acquire().await?;
    let cats = scope_set(["read:summary", "read:records", "read:labs"]);
    set_grant(
        &mut c,
        e.fam.user("sam"),
        e.fam.profile("alex"),
        "caregiver",
        Some(&cats),
        None,
        None,
    )
    .await?;
    drop(c);
    let seen = e.pedigree("sam", "alex").await?;
    assert_eq!(hints(&seen), ["lab.ldl_fh MTH"], "{seen}");
    let text = seen["hints"].to_string();
    for leak in [
        "Carmen",
        "carmen",
        e.carmen_ldl.as_str(),
        "247",
        &e.fam.profile("carmen").to_string(),
    ] {
        assert!(!text.contains(leak), "{leak} in {text}");
    }
    let (s, _) = e.get("sam", &format!("/api/sources/{}", e.carmen_ldl)).await?;
    assert_eq!(s, StatusCode::NOT_FOUND, "the hint opens no door to Carmen's record");
    // the control: Alex, who reads Carmen's record, resolves the same src
    let (s, _) = e.get("alex", &format!("/api/sources/{}", e.carmen_ldl)).await?;
    assert_eq!(s, StatusCode::OK);
    Ok(())
}

#[tokio::test]
async fn deleting_either_profile_removes_the_share() -> R {
    let e = env().await?;
    e.share("alex", "carmen", "alex", &["labs"]).await?;
    e.share("alex", "alex", "sam", &["labs"]).await?;
    assert_eq!(e.shares().await?, 2);
    let mut c = e.db.pool.acquire().await?;
    ownhealth_auth::household::delete_profile(&mut c, e.fam.profile("carmen")).await?;
    assert_eq!(e.shares().await?, 1, "the source went");
    ownhealth_auth::household::delete_profile(&mut c, e.fam.profile("sam")).await?;
    assert_eq!(e.shares().await?, 0, "the target went");
    Ok(())
}

#[tokio::test]
async fn a_claim_at_majority_clears_the_shares_her_guardians_made() -> R {
    let e = env().await?;
    let mut c = e.db.pool.acquire().await?;
    let hh = e.fam.household_id;
    // Marta turned 18 last year and has not claimed her profile; her guardian shared her labs
    let marta = ownhealth_core::access::create_profile(
        &mut c,
        &ownhealth_core::access::ProfileIn {
            household_id: hh,
            slug: "marta".into(),
            display_name: "Marta Rivera".into(),
            relationship: Some("child".into()),
            birth_date: chrono::NaiveDate::from_ymd_opt(2007, 1, 2),
            sex_at_birth: Some("female".into()),
            ..Default::default()
        },
    )
    .await?;
    set_grant(&mut c, e.fam.user("alex"), marta, "guardian", None, None, None).await?;
    drop(c);
    let (s, v) = e.share("alex", "marta", "alex", &["labs"]).await?;
    assert_eq!(s, StatusCode::OK, "{v}");
    e.share("alex", "alex", "marta", &["labs"]).await?;
    assert_eq!(e.shares().await?, 2);
    let mut c = e.db.pool.acquire().await?;
    let user = ownhealth_core::access::create_user(&mut c, hh, "Marta Rivera", "es", None).await?;
    ownhealth_auth::household::claim_profile(&mut c, marta, user, None, ownhealth_core::access::today()).await?;
    let left: Vec<Uuid> = sqlx::query_scalar("select source_profile_id from family_share")
        .fetch_all(&mut *c)
        .await?;
    assert_eq!(left, [e.fam.profile("alex")], "hers went, her father's to her stayed");
    Ok(())
}
