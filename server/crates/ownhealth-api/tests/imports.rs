//! Lane I: `api/imports.py` (tests/test_app.py): uploads land in the blob store and enqueue the
//! import job; the job list is the caller's and its limit is checked at the edge.

mod lane_i;

use axum::body::Body;
use http::{Request, StatusCode, header};
use lane_i::{R, app, glucose_pdf};
use serde_json::json;

fn blob_count(dir: &std::path::Path) -> anyhow::Result<usize> {
    let mut n = 0;
    for e in std::fs::read_dir(dir)? {
        let p = e?.path();
        n += if p.is_dir() { blob_count(&p)? } else { 1 };
    }
    Ok(n)
}

#[tokio::test]
async fn test_upload_pdf_enqueues_import_and_job_list() -> R {
    let a = app().await?;
    let pdf = glucose_pdf("LN-API");
    let (s, v) = a
        .upload("alex", &[("profile", "alex")], &[("labnorte.pdf", &pdf)])
        .await?;
    assert_eq!(s, StatusCode::OK, "{v}");
    let item = v["items"][0].clone();
    assert_eq!(item["kind"], "pdf");
    assert!(item["job_id"].as_i64().is_some(), "{item}");
    assert_eq!(item["duplicate"], json!(false));
    assert_eq!(item["filename"], "labnorte.pdf");
    let sha = item["sha256"].as_str().unwrap_or_default().to_string();
    assert!(a.rt.blobs.exists(
        ownhealth_core::blobs::BlobOwner::Profile(a.family.profile("alex")),
        &sha
    ));
    // the spooled copies are gone
    let left: Vec<_> = std::fs::read_dir(a.rt.settings.blobs_path().join("tmp"))?.collect();
    assert!(left.is_empty(), "{left:?}");

    let (_, again) = a.upload("alex", &[("profile", "alex")], &[("copy.pdf", &pdf)]).await?;
    assert_eq!(again["items"][0]["duplicate"], json!(true));
    assert_eq!(again["items"][0]["document_id"], item["document_id"]);

    let job = item["job_id"].as_i64().unwrap_or_default();
    let (s, jobs) = a.get("alex", &format!("/jobs?ids={job}")).await?;
    assert_eq!(s, StatusCode::OK, "{jobs}");
    let j = &jobs["jobs"][0];
    assert_eq!(j["kind"], "import.pdf");
    assert_eq!(j["state"], "queued");
    // `-> dict`: pydantic's JSON mode, so UTC datetimes end in `Z`
    assert!(j["created_at"].as_str().is_some_and(|t| t.ends_with('Z')), "{j}");
    assert_eq!(j["document_id"], item["document_id"]);
    assert_eq!(j["profile_id"], json!(a.family.profile("alex")));
    let keys: Vec<&String> = j.as_object().map(|m| m.keys().collect()).unwrap_or_default();
    assert_eq!(
        keys,
        [
            "id",
            "kind",
            "state",
            "attempts",
            "max_attempts",
            "last_error",
            "result",
            "created_at",
            "finished_at",
            "profile_id",
            "filename",
            "document_id"
        ]
    );
    // sam has no grant on carmen: a job of carmen's is not his to see
    let carmen_job: i64 = sqlx::query_scalar(
        "insert into job (kind, payload) values ('import.pdf', jsonb_build_object('profile_id', $1::text)) returning id",
    )
    .bind(a.family.profile("carmen").to_string())
    .fetch_one(&a.rt.pool)
    .await?;
    let (_, theirs) = a.get("sam", &format!("/jobs?ids={carmen_job},{job}")).await?;
    let seen: Vec<i64> = theirs["jobs"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .filter_map(|j| j["id"].as_i64())
        .collect();
    assert_eq!(seen, [job]);
    let (_, all) = a.get("alex", &format!("/jobs?ids={carmen_job},{job}")).await?;
    assert_eq!(all["jobs"][0]["id"], json!(carmen_job)); // newest first

    // viewers cannot import, and nothing lands in the store for them
    let before = blob_count(&a.rt.settings.blobs_path().join("sha256"))?;
    let other = glucose_pdf("LN-SAM");
    let (s, v) = a.upload("sam", &[("profile", "alex")], &[("x.pdf", &other)]).await?;
    assert_eq!(s, StatusCode::FORBIDDEN, "{v}");
    assert_eq!(blob_count(&a.rt.settings.blobs_path().join("sha256"))?, before);

    let (s, v) = a.upload("alex", &[("profile", "alex")], &[("x.txt", b"hello")]).await?;
    assert_eq!(s, StatusCode::UNSUPPORTED_MEDIA_TYPE, "{v}");
    assert_eq!(v["detail"]["code"], "unsupported_file");

    let audits = a
        .count("select count(*) from audit_event where action = 'import' and tool = 'POST /api/imports'")
        .await?;
    assert_eq!(audits, 2);
    Ok(())
}

#[tokio::test]
async fn test_upload_needs_both_fields() -> R {
    let a = app().await?;
    let (s, v) = a.upload("alex", &[], &[]).await?;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(
        v,
        json!({"detail": [
            {"type": "missing", "loc": ["body", "profile"], "msg": "Field required", "input": null},
            {"type": "missing", "loc": ["body", "files"], "msg": "Field required", "input": null},
        ]})
    );
    let (s, v) = a.upload("alex", &[("profile", "alex")], &[]).await?;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(v["detail"].as_array().map(Vec::len), Some(1));
    // a JSON body is not a form
    let req = Request::post("/imports")
        .header(header::COOKIE, a.cookie("alex").await?)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from("{}"))?;
    let (s, v) = a.send(req).await?;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(v["detail"].as_array().map(Vec::len), Some(2));
    Ok(())
}

#[tokio::test]
async fn test_upload_form_limits_match_starlette() -> R {
    let a = app().await?;
    let bad = json!({"detail": {"code": "bad_request", "message": "There was an error parsing the body"}});
    let pdf = glucose_pdf("LN-LIMIT");
    // a 2 MiB `profile` field is refused before anything reads it, and is never echoed back
    let big = "a".repeat(2 * 1024 * 1024);
    let (s, v) = a.upload("alex", &[("profile", &big)], &[("x.pdf", &pdf)]).await?;
    assert_eq!((s, &v), (StatusCode::BAD_REQUEST, &bad));
    // so is a large unknown field
    let (s, v) = a
        .upload("alex", &[("profile", "alex"), ("note", &big)], &[("x.pdf", &pdf)])
        .await?;
    assert_eq!((s, &v), (StatusCode::BAD_REQUEST, &bad));
    // 1000 non-file parts pass the parser, 1001 do not
    let mut fields = vec![("profile", "alex")];
    fields.extend(std::iter::repeat_n(("note", "x"), 999));
    let (s, v) = a.upload("alex", &fields, &[("x.pdf", &pdf)]).await?;
    assert_eq!(s, StatusCode::OK, "{v}");
    fields.push(("note", "x"));
    let (s, v) = a.upload("alex", &fields, &[("x.pdf", &pdf)]).await?;
    assert_eq!((s, &v), (StatusCode::BAD_REQUEST, &bad));
    Ok(())
}

#[tokio::test]
async fn test_jobs_limit_is_checked_at_the_edge() -> R {
    let a = app().await?;
    for bad in ["-1", "0", "201"] {
        let (s, v) = a.get("alex", &format!("/jobs?limit={bad}")).await?;
        assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{bad}");
        assert_eq!(v["detail"][0]["loc"], json!(["query", "limit"]));
    }
    let (s, v) = a.get("alex", "/jobs?limit=abc").await?;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(
        v["detail"][0],
        json!({"type": "int_parsing", "loc": ["query", "limit"],
               "msg": "Input should be a valid integer, unable to parse string as an integer", "input": "abc"})
    );
    let (s, v) = a.get("alex", "/jobs?limit=200").await?;
    assert_eq!((s, v), (StatusCode::OK, json!({"jobs": []})));
    let (s, _) = a.get("alex", "/jobs?ids=x,,3").await?;
    assert_eq!(s, StatusCode::OK);
    Ok(())
}

/// A wearables import's result counts samples and names devices: only a member who may read wearables
/// on the profile sees it. Anyone else with a grant there still sees the job, with no result.
#[tokio::test]
async fn a_wearables_import_result_shows_only_to_members_who_may_read_wearables() -> R {
    let a = app().await?;
    let alex = a.family.profile("alex");
    let result = json!({"accepted": 32, "fit": {"heart_rate_minutes": 30, "device": "OwnHealth Test Watch"}});
    let mut ids = Vec::new();
    for kind in ["import.fit", "import.gadgetbridge", "import.pdf"] {
        let id: i64 = sqlx::query_scalar(
            "insert into job (kind, state, payload, result) values ($1, 'done', \
             jsonb_build_object('profile_id', $2::text, 'filename', 'run.fit'), $3) returning id",
        )
        .bind(kind)
        .bind(alex.to_string())
        .bind(&result)
        .fetch_one(&a.rt.pool)
        .await?;
        ids.push(id);
    }
    let uri = format!(
        "/jobs?ids={}",
        ids.iter().map(i64::to_string).collect::<Vec<_>>().join(",")
    );
    let results = |v: &serde_json::Value| -> Vec<(String, serde_json::Value)> {
        v["jobs"]
            .as_array()
            .cloned()
            .unwrap_or_default()
            .iter()
            .map(|j| (j["kind"].as_str().unwrap_or_default().to_string(), j["result"].clone()))
            .collect()
    };
    let grant = |sql: &'static str| {
        let pool = a.rt.pool.clone();
        let (sam, alex) = (a.family.profile("sam"), alex);
        async move {
            let user: uuid::Uuid = sqlx::query_scalar("select user_id from profile where id = $1")
                .bind(sam)
                .fetch_one(&pool)
                .await?;
            sqlx::query(sqlx::AssertSqlSafe(sql))
                .bind(user)
                .bind(alex)
                .execute(&pool)
                .await?;
            anyhow::Ok(())
        }
    };
    // the owner, and sam while his grant on alex holds wearables: every result
    grant(
        "update profile_grant set categories = array_append(array_remove(categories, 'read:wearables'), \
         'read:wearables') where user_id = $1 and profile_id = $2",
    )
    .await?;
    for who in ["alex", "sam"] {
        let (s, v) = a.get(who, &uri).await?;
        assert_eq!(s, StatusCode::OK, "{v}");
        assert!(results(&v).iter().all(|(_, r)| *r == result), "{who}: {v}");
    }
    // without wearables, sam sees the jobs, and the wearables results are gone
    grant(
        "update profile_grant set categories = array_remove(categories, 'read:wearables') \
         where user_id = $1 and profile_id = $2",
    )
    .await?;
    let (s, v) = a.get("sam", &uri).await?;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(
        results(&v),
        [
            ("import.pdf".to_string(), result.clone()),
            ("import.gadgetbridge".to_string(), serde_json::Value::Null),
            ("import.fit".to_string(), serde_json::Value::Null),
        ]
    );
    let (_, owner) = a.get("alex", &uri).await?;
    assert!(results(&owner).iter().all(|(_, r)| *r == result), "{owner}");
    Ok(())
}
