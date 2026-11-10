//! v0.3 Insight family history: the relatives' table, its link to a relative's own profile, the REST
//! routes and the pedigree, each test on its own throwaway database as a browser session.

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
use uuid::Uuid;

type R = anyhow::Result<()>;

struct App {
    db: TestDb,
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
        db,
        _dir: dir,
        rt,
        family,
    })
}

async fn send(a: &App, who: &str, method: &str, uri: &str, body: Option<Value>) -> anyhow::Result<(StatusCode, Value)> {
    let mut c = a.rt.pool.acquire().await?;
    let secret = create_session(&mut c, a.family.user(who), None, None, "passkey").await?;
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .header(header::COOKIE, format!("{COOKIE}={secret}"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(body.map_or_else(Body::empty, |b| Body::from(b.to_string())))?;
    let res = ownhealth_api::router().with_state(a.rt.clone()).oneshot(req).await?;
    let status = res.status();
    let bytes = axum::body::to_bytes(res.into_body(), 1 << 24).await?;
    Ok((status, serde_json::from_slice(&bytes).unwrap_or(Value::Null)))
}

fn manual(profile: Uuid) -> SrcRef {
    SrcRef::New(SrcIn {
        profile_id: Some(profile),
        ..SrcIn::manual(None)
    })
}

fn mi_at(age: i32) -> FamilyCondition {
    FamilyCondition {
        kind: "mi".into(),
        text: None,
        code_system: None,
        code: None,
        onset_age: Some(age),
        onset_approx: false,
    }
}

/// Alex's mother, linked to Carmen's profile.
async fn mother(a: &App) -> anyhow::Result<Uuid> {
    let alex = a.family.profile("alex");
    let mut c = a.db.pool.acquire().await?;
    let mut m = MemberIn::new(alex, "MTH");
    m.born_year = Some(1955);
    m.conditions = vec![FamilyCondition {
        kind: "t2d".into(),
        ..mi_at(69)
    }];
    Ok(family::add(
        &mut c,
        &m,
        &manual(alex),
        "person",
        None,
        Some(a.family.profile("carmen")),
    )
    .await?
    .id)
}

async fn count(a: &App, sql: &str, id: Uuid) -> anyhow::Result<i64> {
    Ok(sqlx::query_scalar(sqlx::AssertSqlSafe(sql))
        .bind(id)
        .fetch_one(&a.db.pool)
        .await?)
}

#[tokio::test]
async fn deleting_a_linked_relative_keeps_the_member_row_without_its_link() -> R {
    let a = app().await?;
    let member = mother(&a).await?;
    let mut tx = a.db.pool.begin().await?;
    // a SET NULL column on the immutable row would raise 23514 here (the 1051 trap)
    ownhealth_auth::household::delete_profile(&mut tx, a.family.profile("carmen")).await?;
    tx.commit().await?;
    let mut c = a.db.pool.acquire().await?;
    let m = family::get(&mut c, member).await?.expect("the member row stays");
    assert_eq!((m.status.as_str(), m.linked_profile_id), ("current", None));
    assert_eq!(m.conditions.0[0].kind, "t2d");
    Ok(())
}

#[tokio::test]
async fn deleting_a_profile_removes_its_relatives_and_their_links() -> R {
    let a = app().await?;
    let member = mother(&a).await?;
    let alex = a.family.profile("alex");
    // a second version, so the delete also meets a supersedes chain
    let mut c = a.db.pool.acquire().await?;
    let mut m = MemberIn::new(alex, "MTH");
    m.born_year = Some(1954);
    let v2 = family::supersede(
        &mut c,
        member,
        &m,
        &manual(alex),
        "person",
        "year",
        family::LinkChange::Keep,
    )
    .await?;
    assert_eq!(
        family::linked_profile(&mut c, v2.id).await?,
        Some(a.family.profile("carmen"))
    );
    drop(c);
    let mut tx = a.db.pool.begin().await?;
    let gone = ownhealth_auth::household::delete_profile(&mut tx, alex).await?;
    tx.commit().await?;
    assert!(
        gone.rows.iter().any(|(t, n)| t == "family_member" && *n == 2),
        "{:?}",
        gone.rows
    );
    assert_eq!(
        count(&a, "select count(*) from family_member where profile_id = $1", alex).await?,
        0
    );
    let links = count(
        &a,
        "select count(*) from family_member_link where linked_profile_id = $1",
        a.family.profile("carmen"),
    )
    .await?;
    assert_eq!(links, 0);
    Ok(())
}

#[tokio::test]
async fn add_list_correct_and_retract_a_relative() -> R {
    let a = app().await?;
    let body = json!({
        "relationship": "FTH", "deceased": true, "death_age": 64,
        "conditions": [{"kind": "mi", "onset_age": 52}],
    });
    let (s, father) = send(&a, "alex", "POST", "/profiles/alex/family", Some(body)).await?;
    assert_eq!(s, StatusCode::CREATED, "{father}");
    assert_eq!(father["sex_at_birth"], "male");
    assert_eq!(father["conditions"][0]["code"], "22298006");
    assert!(father["src"].as_str().is_some_and(|s| s.starts_with("src_")));
    let fid = father["id"].as_str().unwrap_or_default().to_string();

    let (s, mom) = send(
        &a,
        "alex",
        "POST",
        "/profiles/alex/family",
        Some(json!({"relationship": "MTH", "linked_profile": "carmen"})),
    )
    .await?;
    assert_eq!(s, StatusCode::CREATED, "{mom}");
    assert_eq!(
        (&mom["name"], &mom["linked_profile"]["slug"]),
        (&json!("Carmen Ortega"), &json!("carmen"))
    );
    // the same profile cannot stand for two relatives, nor for the profile itself
    let (s, _) = send(
        &a,
        "alex",
        "POST",
        "/profiles/alex/family",
        Some(json!({"relationship": "SIS", "linked_profile": "carmen"})),
    )
    .await?;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
    let (s, _) = send(
        &a,
        "alex",
        "POST",
        "/profiles/alex/family",
        Some(json!({"relationship": "BRO", "linked_profile": "alex"})),
    )
    .await?;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
    let (s, e) = send(
        &a,
        "alex",
        "POST",
        "/profiles/alex/family",
        Some(json!({"relationship": "XYZ"})),
    )
    .await?;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{e}");
    let (s, e) = send(
        &a,
        "alex",
        "POST",
        "/profiles/alex/family",
        Some(json!({"relationship": "SIS", "conditions": [{"kind": "other"}]})),
    )
    .await?;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{e}");

    // a new version keeps the link when the body does not name it
    let mid = mom["id"].as_str().unwrap_or_default().to_string();
    let (s, mom2) = send(
        &a,
        "alex",
        "PUT",
        &format!("/profiles/alex/family/{mid}"),
        Some(json!({"relationship": "MTH", "conditions": [{"kind": "t2d", "onset_age": 69}]})),
    )
    .await?;
    assert_eq!(s, StatusCode::OK, "{mom2}");
    assert_eq!(
        (&mom2["supersedes"], &mom2["linked_profile"]["slug"]),
        (&json!(mid), &json!("carmen"))
    );
    // and the old version is no longer current
    let (s, _) = send(
        &a,
        "alex",
        "PUT",
        &format!("/profiles/alex/family/{mid}"),
        Some(json!({"relationship": "MTH"})),
    )
    .await?;
    assert_eq!(s, StatusCode::NOT_FOUND);

    let (s, out) = send(
        &a,
        "alex",
        "DELETE",
        &format!("/profiles/alex/family/{fid}?reason=entered%20twice"),
        None,
    )
    .await?;
    assert_eq!((s, &out["status"]), (StatusCode::OK, &json!("retracted")));
    let (_, list) = send(&a, "alex", "GET", "/profiles/alex/family", None).await?;
    let rels: Vec<&Value> = list["items"]
        .as_array()
        .map(|v| v.iter().map(|i| &i["relationship"]).collect())
        .unwrap_or_default();
    assert_eq!(rels, vec![&json!("MTH")]);
    assert!(
        list["catalog"]["relationships"]
            .as_array()
            .is_some_and(|r| r.len() > 20)
    );
    // nothing of this enters the state body
    let (_, status) = send(&a, "alex", "GET", "/profiles/alex/status", None).await?;
    assert!(!status.to_string().contains("FamilyMemberHistory") && !status.to_string().contains("\"MTH\""));
    Ok(())
}

#[tokio::test]
async fn a_caller_who_cannot_read_the_linked_profile_sees_no_name_and_no_conditions() -> R {
    let a = app().await?;
    let member = mother(&a).await?;
    // Sam reads Alex's records, but has no grant on Carmen's profile
    let mut c = a.db.pool.acquire().await?;
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
    let (s, ped) = send(&a, "sam", "GET", "/profiles/alex/pedigree", None).await?;
    assert_eq!(s, StatusCode::OK, "{ped}");
    let node = &ped["nodes"][0];
    assert_eq!(
        (&node["relationship"], &node["masked"], &node["name"]),
        (&json!("MTH"), &json!(true), &Value::Null)
    );
    assert_eq!(node["conditions"], json!([]));
    assert!(!ped.to_string().contains("Carmen") && !ped.to_string().contains("t2d"));
    let (s, list) = send(&a, "sam", "GET", "/profiles/alex/family", None).await?;
    assert_eq!(s, StatusCode::OK);
    assert!(!list["items"].to_string().contains("Carmen") && list["items"][0]["born_year"].is_null());
    // and cannot change it
    let (s, _) = send(
        &a,
        "sam",
        "PUT",
        &format!("/profiles/alex/family/{member}"),
        Some(json!({"relationship": "MTH"})),
    )
    .await?;
    assert_eq!(s, StatusCode::FORBIDDEN);
    // Alex, Carmen's caregiver, sees her
    let (_, ped) = send(&a, "alex", "GET", "/profiles/alex/pedigree", None).await?;
    assert_eq!(
        (&ped["nodes"][0]["name"], &ped["nodes"][0]["masked"]),
        (&json!("Carmen Ortega"), &json!(false))
    );
    assert_eq!(ped["nodes"][0]["conditions"][0]["kind"], "t2d");
    assert_eq!(ped["profile"]["name"], "Alex Rivera");
    // a viewer (read:summary only) cannot read the pedigree at all
    let mut c = a.db.pool.acquire().await?;
    set_grant(
        &mut c,
        a.family.user("sam"),
        a.family.profile("alex"),
        "viewer",
        None,
        None,
        None,
    )
    .await?;
    drop(c);
    let (s, _) = send(&a, "sam", "GET", "/profiles/alex/pedigree", None).await?;
    assert_eq!(s, StatusCode::FORBIDDEN);
    Ok(())
}

#[tokio::test]
async fn a_writer_who_cannot_read_the_linked_profile_can_still_unlink_or_remove_the_relative() -> R {
    let a = app().await?;
    let member = mother(&a).await?;
    let mut c = a.db.pool.acquire().await?;
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
    // unlinking is a new version with only what the writer sends: nothing of the old one shows
    let (s, out) = send(
        &a,
        "sam",
        "PUT",
        &format!("/profiles/alex/family/{member}"),
        Some(json!({"relationship": "MTH", "linked_profile": null})),
    )
    .await?;
    assert_eq!(s, StatusCode::OK, "{out}");
    assert_eq!(
        (&out["linked"], &out["masked"], &out["born_year"], &out["conditions"]),
        (&json!(false), &json!(false), &Value::Null, &json!([]))
    );
    // a masked row can be removed outright
    let other = mother(&a).await?;
    let (s, out) = send(&a, "sam", "DELETE", &format!("/profiles/alex/family/{other}"), None).await?;
    assert_eq!(s, StatusCode::OK, "{out}");
    assert_eq!(out["status"], "retracted");
    Ok(())
}

#[tokio::test]
async fn a_sensitive_relative_is_hidden_without_read_sensitive() -> R {
    let a = app().await?;
    let (s, row) = send(
        &a,
        "alex",
        "POST",
        "/profiles/alex/family",
        Some(json!({"relationship": "BRO", "sensitivity": ["mental_health"], "conditions": [{"kind": "other", "text": "a synthetic note"}]})),
    )
    .await?;
    assert_eq!(s, StatusCode::CREATED, "{row}");
    let (_, own) = send(&a, "alex", "GET", "/profiles/alex/pedigree", None).await?;
    assert_eq!(own["nodes"].as_array().map(Vec::len), Some(1));
    let mut c = a.db.pool.acquire().await?;
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
    let (_, ped) = send(&a, "sam", "GET", "/profiles/alex/pedigree", None).await?;
    assert_eq!(ped["nodes"], json!([]));
    let (_, list) = send(&a, "sam", "GET", "/profiles/alex/family", None).await?;
    assert_eq!(list["items"], json!([]));
    let id = row["id"].as_str().unwrap_or_default();
    let (s, _) = send(&a, "sam", "DELETE", &format!("/profiles/alex/family/{id}"), None).await?;
    assert_eq!(s, StatusCode::NOT_FOUND);
    Ok(())
}

#[tokio::test]
async fn the_family_names_pass_the_wording_lint() -> R {
    let cat = family::catalog()?;
    let v = serde_json::to_value(cat)?;
    assert_eq!(ownhealth_rules::wording::lint(&v, &[]), Vec::<(String, String)>::new());
    // the lint itself fires on advice (the control)
    assert!(!ownhealth_rules::wording::lint(&json!({"es": "Debe hacerse una prueba"}), &[]).is_empty());
    Ok(())
}
