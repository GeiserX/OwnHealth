//! The US endpoint directory over REST (routes/brands.rs): a GET never makes the directory, the first
//! POST (the wizard opening) makes it once and queues its first pull, and a search reads only the
//! caller's household. The entries are written straight into the table: no vendor is contacted.

#[path = "auth_support.rs"]
mod support;

use axum::http::StatusCode;
use ownhealth_core::demo::create_rivera_family;
use ownhealth_ingest_structured::brands::{self, Brand};
use serde_json::Value;
use support::Env;
use uuid::Uuid;

type R = anyhow::Result<()>;

fn names(v: &Value) -> Vec<String> {
    v["results"]
        .as_array()
        .map(|a| a.iter().filter_map(|h| h["name"].as_str().map(String::from)).collect())
        .unwrap_or_default()
}

#[tokio::test]
async fn opening_the_wizard_makes_the_directory_once_and_a_search_stays_in_the_household() -> R {
    let env = Env::new(true).await?;
    let mut alex = env.as_user(env.user("alex")).await?;
    // before anyone opens the wizard: no directory, and a search makes none
    let r = alex.get("/api/brands?q=labnorte").await?;
    assert_eq!(r.status, StatusCode::OK, "{}", r.json);
    assert!(r.json["directory"].is_null(), "{}", r.json);
    assert_eq!(r.json["results"], serde_json::json!([]));
    assert_eq!(env.scalar_i64("select count(*) from source").await?, 0);
    assert_eq!(env.scalar_i64("select count(*) from job").await?, 0);
    // every search is a read of the household's directory: one audit row each, query text left out
    let searches = "select count(*) from audit_event where action = 'search' and tool = 'connections'";
    assert_eq!(env.scalar_i64(searches).await?, 1);
    // the wizard opens: one source, one pull queued, audited once
    let r = alex.post0("/api/brands").await?;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.json);
    assert_eq!(r.json["created"], true);
    let id = r.json["directory"]["source_id"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    assert!(!id.is_empty(), "{}", r.json);
    assert_eq!(r.json["directory"]["ready"], false);
    let mut sam = env.as_user(env.user("sam")).await?;
    let r = sam.post0("/api/brands").await?;
    assert_eq!(r.status, StatusCode::OK, "{}", r.json);
    assert_eq!(r.json["created"], false);
    assert_eq!(r.json["directory"]["source_id"], id.as_str());
    assert_eq!(
        env.scalar_i64("select count(*) from source where kind = 'brands'")
            .await?,
        1
    );
    assert_eq!(
        env.scalar_i64("select count(*) from job where kind = 'source.poll'")
            .await?,
        1
    );
    assert_eq!(
        env.scalar_i64("select count(*) from audit_event where action = 'source.create'")
            .await?,
        1
    );
    // a pulled copy, written as the poller writes it
    let source: Uuid = id.parse()?;
    let mut tx = env.pool().begin().await?;
    let entries = [
        ("Hospital LabNorte", "https://fhir.labnorte.example/r4/", Some("TX")),
        ("LabNorte Centro", "https://fhir.labnorte.example/r4/", Some("TX")),
        ("Clínica Ortega", "https://fhir.ortega.example/r4/", None),
    ]
    .map(|(n, e, s)| Brand {
        name: n.into(),
        endpoint: e.into(),
        state: s.map(String::from),
    });
    brands::replace(&mut tx, source, "epic", &entries).await?;
    tx.commit().await?;
    let r = alex.get("/api/brands?q=LABNORTE").await?;
    assert_eq!(r.status, StatusCode::OK, "{}", r.json);
    assert_eq!(env.scalar_i64(searches).await?, 2);
    assert_eq!(
        env.scalar_i64(&format!(
            "select count(*) from audit_event where action = 'search' and '{id}' = any(resource_ids)"
        ))
        .await?,
        1
    );
    assert_eq!(names(&r.json), vec!["LabNorte Centro", "Hospital LabNorte"]);
    assert_eq!(r.json["results"][0]["endpoint"], "https://fhir.labnorte.example/r4/");
    assert_eq!(r.json["results"][0]["state"], "TX");
    assert_eq!(r.json["results"][0]["vendor"], "epic");
    assert_eq!(r.json["directory"]["ready"], true);
    assert_eq!(r.json["directory"]["stale"], false);
    assert_eq!(r.json["directory"]["feeds"][0]["entries"], 3);
    let r = alex.get("/api/brands?q=labnorte&limit=1").await?;
    assert_eq!(names(&r.json), vec!["LabNorte Centro"]);
    let r = alex.get("/api/brands?q=labnorte&limit=0").await?;
    assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY, "{}", r.json);
    let r = alex.get("/api/brands?q=l").await?;
    assert_eq!(names(&r.json), Vec::<String>::new());
    // another household sees no directory, and its own wizard makes its own
    let mut conn = env.pool().acquire().await?;
    let other = create_rivera_family(&mut conn, &format!("Rivera {}", ownhealth_core::ids::token_hex(3))).await?;
    let mut alex2 = env.as_user(other.user("alex")).await?;
    let r = alex2.get("/api/brands?q=labnorte").await?;
    assert!(r.json["directory"].is_null(), "{}", r.json);
    let r = alex2.post0("/api/brands").await?;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.json);
    let r = alex2.get("/api/brands?q=labnorte").await?;
    assert_eq!(names(&r.json), Vec::<String>::new());
    // nobody signed in gets nothing
    let mut anon = env.anon();
    assert_eq!(
        anon.get("/api/brands?q=labnorte").await?.status,
        StatusCode::UNAUTHORIZED
    );
    assert!(anon.post0("/api/brands").await?.status.is_client_error());
    assert_eq!(
        env.scalar_i64("select count(*) from source where kind = 'brands'")
            .await?,
        2
    );
    Ok(())
}

#[tokio::test]
async fn a_person_who_manages_no_profile_gets_no_directory_and_makes_none() -> R {
    let env = Env::new(true).await?;
    // Alex keeps every grant, but none of them lets him manage a profile: owner and guardian become
    // viewer (caregiver of Carmen and viewer of Sam stay as they are)
    let changed =
        sqlx::query("update profile_grant set role = 'viewer' where user_id = $1 and role in ('owner', 'guardian')")
            .bind(env.user("alex"))
            .execute(env.pool())
            .await?
            .rows_affected();
    assert_eq!(changed, 2);
    let mut alex = env.as_user(env.user("alex")).await?;
    let get = alex.get("/api/brands?q=va").await?;
    let post = alex.post0("/api/brands").await?;
    for (what, r) in [("GET", get), ("POST", post)] {
        assert_eq!(r.status, StatusCode::FORBIDDEN, "{what}: {}", r.json);
        assert!(
            r.json.get("directory").is_none() && r.json.get("results").is_none(),
            "{what}: {}",
            r.json
        );
    }
    assert_eq!(env.scalar_i64("select count(*) from source").await?, 0);
    assert_eq!(env.scalar_i64("select count(*) from job").await?, 0);
    // the control: Sam still owns his profile, so the same calls work for him
    let mut sam = env.as_user(env.user("sam")).await?;
    assert_eq!(sam.post0("/api/brands").await?.status, StatusCode::CREATED);
    assert_eq!(sam.get("/api/brands?q=va").await?.status, StatusCode::OK);
    Ok(())
}
