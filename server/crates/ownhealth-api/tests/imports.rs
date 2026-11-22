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

/// A synthetic Dexcom Clarity export with one reading.
const CLARITY: &[u8] = b"Index,Timestamp (YYYY-MM-DDThh:mm:ss),Event Type,Event Subtype,Patient Info,Device Info,\
Source Device ID,Glucose Value (mg/dL),Insulin Value (u),Carb Value (grams),Duration (hh:mm:ss),\
Glucose Rate of Change (mg/dL/min),Transmitter Time (Long Integer),Transmitter ID\n\
1,2026-11-14T20:00:17,EGV,,,,iOS G7,120,,,,,3456000,7Q2RLX\n";

#[tokio::test]
async fn a_cgm_upload_carries_the_browser_zone_and_an_unknown_zone_is_refused() -> R {
    let a = app().await?;
    let (s, v) = a
        .upload(
            "alex",
            &[("profile", "lucia"), ("timezone", "Europe/Madrid")],
            &[("clarity.csv", CLARITY)],
        )
        .await?;
    assert_eq!(s, StatusCode::OK, "{v}");
    let job = v["items"][0]["job_id"].as_i64().unwrap_or_default();
    let zone: Option<String> = sqlx::query_scalar("select payload->>'timezone' from job where id = $1")
        .bind(job)
        .fetch_one(&a.rt.pool)
        .await?;
    assert_eq!(zone.as_deref(), Some("Europe/Madrid"));
    // a zone the time zone database does not know is refused, and nothing is stored or queued
    let jobs = a.count("select count(*) from job").await?;
    let (s, v) = a
        .upload(
            "alex",
            &[("profile", "lucia"), ("timezone", "Mars/Olympus")],
            &[("other.csv", b"x")],
        )
        .await?;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{v}");
    assert_eq!(v["detail"]["code"], "unknown_timezone", "{v}");
    assert_eq!(a.count("select count(*) from job").await?, jobs);
    // no zone, or an empty one, is an upload as before
    let (s, v) = a
        .upload(
            "alex",
            &[("profile", "lucia"), ("timezone", "")],
            &[("pdf.pdf", &glucose_pdf("LN-TZ"))],
        )
        .await?;
    assert_eq!(s, StatusCode::OK, "{v}");
    Ok(())
}

/// A finished CGM import job of a profile, with the details its result carries.
async fn cgm_job(a: &lane_i::App, slug: &str) -> anyhow::Result<i64> {
    Ok(sqlx::query_scalar(
        "insert into job (kind, state, payload, result) values ('import.cgm_csv', 'done', \
         jsonb_build_object('profile_id', $1::text, 'filename', 'clarity.csv'), \
         '{\"accepted\": 8, \"skipped\": 2, \"reasons\": {\"below_sensor_range\": 1}, \
           \"cgm_csv\": {\"vendor\": \"dexcom_clarity\", \"readings\": 8, \"devices\": [\"Dexcom G7\"]}}') returning id",
    )
    .bind(a.family.profile(slug).to_string())
    .fetch_one(&a.rt.pool)
    .await?)
}

#[tokio::test]
async fn a_cgm_import_result_reaches_only_those_who_may_read_wearables() -> R {
    let a = app().await?;
    // alex is carmen's caregiver (no read:wearables) and lucia's guardian (every scope)
    let (carmen, lucia) = (cgm_job(&a, "carmen").await?, cgm_job(&a, "lucia").await?);
    let (s, v) = a.get("alex", &format!("/jobs?ids={carmen},{lucia}")).await?;
    assert_eq!(s, StatusCode::OK, "{v}");
    let by_id = |id: i64| {
        v["jobs"]
            .as_array()
            .and_then(|j| j.iter().find(|x| x["id"] == json!(id)).cloned())
            .unwrap_or_default()
    };
    // the caregiver keeps what any member saw before (the job, its state and file), not what it brought
    assert_eq!(by_id(carmen)["state"], "done", "{v}");
    assert_eq!(by_id(carmen)["filename"], "clarity.csv", "{v}");
    assert_eq!(by_id(carmen)["result"], serde_json::Value::Null, "{v}");
    assert_eq!(by_id(lucia)["result"]["cgm_csv"]["readings"], json!(8), "{v}");
    // with read:wearables on carmen, the result is there
    let mut c = a.conn().await?;
    let cats = ownhealth_core::access::scope_set(["read:summary", "read:records", "read:wearables"]);
    ownhealth_core::access::set_grant(
        &mut c,
        a.family.user("alex"),
        a.family.profile("carmen"),
        "caregiver",
        Some(&cats),
        None,
        None,
    )
    .await?;
    drop(c);
    let (_, v) = a.get("alex", &format!("/jobs?ids={carmen}")).await?;
    assert_eq!(v["jobs"][0]["result"]["reasons"]["below_sensor_range"], json!(1), "{v}");
    Ok(())
}

#[tokio::test]
async fn a_cgm_document_shows_its_counts_only_to_those_who_may_read_wearables() -> R {
    let a = app().await?;
    let doc: uuid::Uuid = sqlx::query_scalar(
        "insert into document (household_id, profile_id, blob_sha256, mime, size_bytes, filename, doc_type, flags) \
         values ($1, $2, sha256('x'::bytea), 'text/csv', 1, 'clarity.csv', 'other', \
         '{\"import\": {\"accepted\": 8, \"reasons\": {\"below_sensor_range\": 1}}, \
           \"cgm_csv\": {\"vendor\": \"dexcom_clarity\", \"readings\": 8, \"devices\": [\"Dexcom G7\"]}}') returning id",
    )
    .bind(a.family.household_id)
    .bind(a.family.profile("carmen"))
    .fetch_one(&a.rt.pool)
    .await?;
    let (s, v) = a.get("alex", &format!("/documents/{doc}")).await?;
    assert_eq!(s, StatusCode::OK, "{v}");
    // the caregiver reads what the file is, not what it brought
    assert_eq!(v["flags"]["cgm_csv"], json!({"vendor": "dexcom_clarity"}), "{v}");
    assert_eq!(v["flags"].get("import"), None, "{v}");
    let (_, l) = a.get("alex", "/profiles/carmen/documents").await?;
    assert_eq!(
        l["items"][0]["flags"]["cgm_csv"],
        json!({"vendor": "dexcom_clarity"}),
        "{l}"
    );
    // on lucia, whose guardian holds every scope, alex reads it all
    sqlx::query("update document set profile_id = $1 where id = $2")
        .bind(a.family.profile("lucia"))
        .bind(doc)
        .execute(&a.rt.pool)
        .await?;
    let (_, v) = a.get("alex", &format!("/documents/{doc}")).await?;
    assert_eq!(v["flags"]["cgm_csv"]["readings"], json!(8), "{v}");
    assert_eq!(v["flags"]["import"]["accepted"], json!(8), "{v}");
    Ok(())
}
