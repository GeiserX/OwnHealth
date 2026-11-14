//! The family patterns in GET /profiles/{p}/pedigree: the same relatives the drawing shows feed them,
//! so a relative masked for the caller (linked to a profile they cannot read) or a sensitive one
//! without read:sensitive never shows through a pattern.

use axum::body::Body;
use http::{Request, StatusCode, header};
use ownhealth_auth::sessions::{COOKIE, create_session};
use ownhealth_core::Runtime;
use ownhealth_core::access::{scope_set, set_grant};
use ownhealth_core::demo::{Family, create_rivera_family};
use ownhealth_core::family::{self, FamilyCondition, MemberIn};
use ownhealth_core::provenance::{SrcIn, SrcRef};
use ownhealth_core::testing::TestDb;
use serde_json::{Value, json};
use tower::ServiceExt;

type R = anyhow::Result<()>;

struct App {
    _db: TestDb,
    _dir: tempfile::TempDir,
    rt: Runtime,
    family: Family,
}

async fn app() -> anyhow::Result<App> {
    let db = TestDb::new().await?;
    let dir = tempfile::tempdir()?;
    let rt = Runtime::with_pool(db.settings(dir.path())?, db.pool.clone())?;
    let mut c = db.pool.acquire().await?;
    let family = create_rivera_family(&mut c, &format!("Rivera {}", ownhealth_core::ids::token_hex(3))).await?;
    Ok(App {
        _db: db,
        _dir: dir,
        rt,
        family,
    })
}

async fn pedigree(a: &App, who: &str) -> anyhow::Result<(StatusCode, Value)> {
    let mut c = a.rt.pool.acquire().await?;
    let secret = create_session(&mut c, a.family.user(who), None, None, "passkey").await?;
    let req = Request::builder()
        .uri("/profiles/alex/pedigree")
        .header(header::COOKIE, format!("{COOKIE}={secret}"))
        .body(Body::empty())?;
    let res = ownhealth_api::router().with_state(a.rt.clone()).oneshot(req).await?;
    let status = res.status();
    let bytes = axum::body::to_bytes(res.into_body(), 1 << 24).await?;
    Ok((status, serde_json::from_slice(&bytes).unwrap_or(Value::Null)))
}

/// A relative of Alex's with a heart attack at `onset`, linked to a profile or with a sensitivity tag.
async fn relative(a: &App, code: &str, onset: i32, link: Option<&str>, sensitivity: &[&str]) -> anyhow::Result<String> {
    let alex = a.family.profile("alex");
    let mut c = a.rt.pool.acquire().await?;
    let mut m = MemberIn::new(alex, code);
    m.conditions = vec![FamilyCondition {
        kind: "mi".into(),
        text: None,
        code_system: None,
        code: None,
        onset_age: Some(onset),
        onset_approx: false,
    }];
    m.sensitivity = sensitivity.iter().map(|s| s.to_string()).collect();
    let src = SrcRef::New(SrcIn {
        profile_id: Some(alex),
        ..SrcIn::manual(None)
    });
    let done = family::add(&mut c, &m, &src, "person", None, link.map(|l| a.family.profile(l))).await?;
    Ok(done.id.to_string())
}

fn rules(ped: &Value) -> Vec<String> {
    ped["patterns"]["items"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|i| i["rule"].as_str().unwrap_or_default().to_string())
        .collect()
}

#[tokio::test]
async fn the_pedigree_lists_the_patterns_of_the_relatives_the_caller_can_read() -> R {
    let a = app().await?;
    let mother = relative(&a, "MTH", 60, Some("carmen"), &[]).await?;
    // Alex is Carmen's caregiver: his mother's heart attack at 60 is premature for a woman
    let (s, ped) = pedigree(&a, "alex").await?;
    assert_eq!(s, StatusCode::OK, "{ped}");
    let p = &ped["patterns"];
    assert_eq!(p["schema"], "ownhealth.family/v1");
    assert_eq!(rules(&ped), ["fam.ascvd"]);
    let item = &p["items"][0];
    assert_eq!(item["members"], json!([mother]));
    assert_eq!(item["rel"], json!(["MTH"]));
    assert!(item["cite"].as_str().unwrap_or_default().starts_with("https://"));
    assert!(item["title"]["es"].as_str().is_some_and(|t| !t.is_empty()), "{item}");
    assert!(item["why"]["en"].as_str().is_some_and(|t| !t.is_empty()), "{item}");
    // Sam reads Alex's records but not Carmen's: the masked row feeds no pattern
    let mut c = a.rt.pool.acquire().await?;
    let cats = scope_set(["read:summary", "read:records"]);
    set_grant(
        &mut c,
        a.family.user("sam"),
        a.family.profile("alex"),
        "caregiver",
        Some(&cats),
        None,
        None,
    )
    .await?;
    drop(c);
    let (s, ped) = pedigree(&a, "sam").await?;
    assert_eq!(s, StatusCode::OK, "{ped}");
    assert_eq!(ped["patterns"]["items"], json!([]), "{ped}");
    assert!(!ped["patterns"].to_string().contains("\"mi\""), "{ped}");
    Ok(())
}

#[tokio::test]
async fn a_sensitive_relative_feeds_no_pattern_without_read_sensitive() -> R {
    let a = app().await?;
    relative(&a, "BRO", 50, None, &["mental_health"]).await?;
    let (_, ped) = pedigree(&a, "alex").await?;
    assert_eq!(rules(&ped), ["fam.ascvd"]);
    let mut c = a.rt.pool.acquire().await?;
    let cats = scope_set(["read:summary", "read:records"]);
    set_grant(
        &mut c,
        a.family.user("sam"),
        a.family.profile("alex"),
        "caregiver",
        Some(&cats),
        None,
        None,
    )
    .await?;
    drop(c);
    let (_, ped) = pedigree(&a, "sam").await?;
    assert_eq!(ped["patterns"]["items"], json!([]), "{ped}");
    Ok(())
}

/// SNOMED 32895009 is "Hereditary disease": an imported relative coded with it keeps the received
/// condition as `other` and never fires the gene-variant pattern; 106221001 "Genetic finding" is the
/// code the hereditary_variant kind carries.
#[test]
fn a_hereditary_disease_code_never_imports_as_a_known_gene_variant() {
    use ownhealth_ingest_structured::fhir::mapping::{Mapped, family_member_history};
    let kind = |code: &str| {
        let r = json!({
            "resourceType": "FamilyMemberHistory",
            "status": "completed",
            "relationship": {"coding": [{"system": family::ROLE_CODE, "code": "MTH"}]},
            "condition": [{"code": {
                "coding": [{"system": "http://snomed.info/sct", "code": code}],
                "text": "haemochromatosis"
            }}]
        });
        match family_member_history(&r, uuid::Uuid::nil()) {
            Mapped::Family { member, .. } => member.conditions[0].kind.clone(),
            _ => String::new(),
        }
    };
    assert_eq!(kind("32895009"), "other");
    assert_eq!(kind("106221001"), "hereditary_variant");
}
