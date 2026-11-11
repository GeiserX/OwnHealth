//! The medication lane over REST (migration 1100): schedule, prescriber and codes are accepted,
//! checked and returned; a correction keeps them; deleting the profile removes every version.

use axum::body::Body;
use http::{Request, StatusCode, header};
use ownhealth_auth::household::delete_profile;
use ownhealth_auth::sessions::{COOKIE, create_session};
use ownhealth_core::Runtime;
use ownhealth_core::demo::{Family, create_rivera_family};
use ownhealth_core::testing::TestDb;
use serde_json::{Value, json};
use tower::ServiceExt;

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

async fn post(a: &App, uri: &str, body: Value) -> anyhow::Result<(StatusCode, Value)> {
    let mut c = a.rt.pool.acquire().await?;
    let secret = create_session(&mut c, a.family.user("alex"), None, None, "passkey").await?;
    let req = Request::post(uri)
        .header(header::COOKIE, format!("{COOKIE}={secret}"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))?;
    let res = ownhealth_api::router().with_state(a.rt.clone()).oneshot(req).await?;
    let status = res.status();
    let bytes = axum::body::to_bytes(res.into_body(), 1 << 24).await?;
    Ok((status, serde_json::from_slice(&bytes).unwrap_or(Value::Null)))
}

#[tokio::test]
async fn a_medication_keeps_its_schedule_prescriber_and_codes() -> R {
    let a = app().await?;
    let codes = json!([{"system": "http://www.whocc.no/atc", "code": "A10BA02", "display": "metformin"}]);
    let (s, made) = post(
        &a,
        "/profiles/carmen/records/medication",
        json!({"name": "Metformin", "strength": "850 mg", "dose_text": "1 tablet", "started": "2024-02-01",
               "schedule_text": "twice daily", "prescriber": "Dr. Synthetic", "codes": codes}),
    )
    .await?;
    assert_eq!(s, StatusCode::CREATED, "{made}");
    let d = &made["detail"];
    assert_eq!(
        (&d["schedule_text"], &d["prescriber"], &d["codes"]),
        (&json!("twice daily"), &json!("Dr. Synthetic"), &codes)
    );
    // a supplement is a medication record of kind supplement
    let (s, supp) = post(
        &a,
        "/profiles/carmen/records/medication",
        json!({"kind": "supplement", "name": "Vitamin D3", "strength": "1000 IU", "started": "2026-01-01"}),
    )
    .await?;
    assert_eq!(
        (s, &supp["detail"]["kind"]),
        (StatusCode::CREATED, &json!("supplement"))
    );
    assert_eq!(supp["detail"]["codes"], Value::Null);

    // refused, field by field
    for (bad, loc) in [
        (json!({"name": "X", "codes": "A10BA02"}), json!(["codes"])),
        (
            json!({"name": "X", "codes": [{"system": "http://www.whocc.no/atc"}]}),
            json!(["codes", 0, "code"]),
        ),
        (json!({"name": "X", "schedule_text": 2}), json!(["schedule_text"])),
    ] {
        let (s, r) = post(&a, "/profiles/carmen/records/medication", bad).await?;
        assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{r}");
        assert_eq!(r["detail"]["errors"][0]["loc"], loc, "{r}");
    }

    // the screen corrects with the row's own detail plus the change: the codes stay
    let id = made["id"].as_str().unwrap_or_default();
    let mut body = d.clone();
    if let Some(o) = body.as_object_mut() {
        o.insert("schedule_text".into(), json!("with breakfast and dinner"));
        o.insert("reason".into(), json!("schedule clarified"));
    }
    let (s, fixed) = post(&a, &format!("/profiles/carmen/records/medication/{id}/correct"), body).await?;
    assert_eq!(s, StatusCode::CREATED, "{fixed}");
    assert_eq!(
        (
            &fixed["detail"]["schedule_text"],
            &fixed["detail"]["codes"],
            &fixed["supersedes"]
        ),
        (&json!("with breakfast and dinner"), &codes, &json!(id))
    );

    // deleting the profile removes every version, the new columns with them
    let pid = a.family.profile("carmen");
    let mut c = a.db.pool.acquire().await?;
    let before: i64 =
        sqlx::query_scalar("select count(*) from medication_statement where profile_id = $1 and codes is not null")
            .bind(pid)
            .fetch_one(&mut *c)
            .await?;
    assert_eq!(before, 2, "the first version and its correction");
    delete_profile(&mut c, pid).await?;
    let left: i64 = sqlx::query_scalar("select count(*) from medication_statement where profile_id = $1")
        .bind(pid)
        .fetch_one(&mut *c)
        .await?;
    assert_eq!(left, 0);
    Ok(())
}
