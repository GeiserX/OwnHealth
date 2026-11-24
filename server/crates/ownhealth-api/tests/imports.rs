//! Lane I: `api/imports.py` (tests/test_app.py): uploads land in the blob store and enqueue the
//! import job; the job list is the caller's and its limit is checked at the edge.

mod lane_i;

use axum::body::Body;
use http::{Request, StatusCode, header};
use lane_i::{App, R, app, glucose_pdf};
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

/// The zone a CGM upload's job carries (`payload.timezone`).
async fn job_zone(a: &lane_i::App, v: &serde_json::Value) -> anyhow::Result<Option<String>> {
    let job = v["items"][0]["job_id"].as_i64().unwrap_or_default();
    Ok(sqlx::query_scalar("select payload->>'timezone' from job where id = $1")
        .bind(job)
        .fetch_one(&a.rt.pool)
        .await?)
}

#[tokio::test]
async fn a_cgm_upload_carries_the_browser_zone_and_no_zone_refuses_an_upload() -> R {
    let a = app().await?;
    let up = |zone: &'static str, name: &'static str, bytes: Vec<u8>| {
        let a = &a;
        async move {
            a.upload(
                "alex",
                &[("profile", "lucia"), ("timezone", zone)],
                &[(name, bytes.as_slice())],
            )
            .await
        }
    };
    let (s, v) = up("Europe/Madrid", "clarity.csv", CLARITY.to_vec()).await?;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(job_zone(&a, &v).await?.as_deref(), Some("Europe/Madrid"));
    // Chromium names India Asia/Calcutta, which the Postgres image does not ship: a PDF goes in, and
    // a CGM export is read in the zone's current name
    let (s, v) = up("Asia/Calcutta", "labnorte.pdf", glucose_pdf("LN-TZ-IN")).await?;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["items"][0]["kind"], "pdf", "{v}");
    let (s, v) = up(
        "Asia/Calcutta",
        "india.csv",
        [CLARITY, b"2,2026-11-14T20:05:17,EGV,,,,iOS G7,121,,,,,3456300,7Q2RLX\n"].concat(),
    )
    .await?;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(job_zone(&a, &v).await?.as_deref(), Some("Asia/Kolkata"));
    // a zone nobody knows refuses nothing: the upload goes in without one, as with no zone at all
    let (s, v) = up("Mars/Olympus", "mars.pdf", glucose_pdf("LN-TZ-MARS")).await?;
    assert_eq!(s, StatusCode::OK, "{v}");
    let (s, v) = up(
        "Mars/Olympus",
        "mars.csv",
        [CLARITY, b"2,2026-11-14T20:10:17,EGV,,,,iOS G7,122,,,,,3456600,7Q2RLX\n"].concat(),
    )
    .await?;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["items"][0]["kind"], "dexcom_clarity_csv", "{v}");
    assert_eq!(job_zone(&a, &v).await?, None);
    // no zone, or an empty one, is an upload as before
    let (s, v) = up("", "pdf.pdf", glucose_pdf("LN-TZ")).await?;
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

/// A wearables import's result counts samples and names devices: only a member who may read wearables
/// on the profile sees it. Anyone else with a grant there still sees the job, with no result.
#[tokio::test]
async fn a_wearables_import_result_shows_only_to_members_who_may_read_wearables() -> R {
    let a = app().await?;
    let alex = a.family.profile("alex");
    let result = json!({"accepted": 32, "fit": {"heart_rate_minutes": 30, "device": "OwnHealth Test Watch"}});
    let mut ids = Vec::new();
    for kind in [
        "import.fit",
        "import.gadgetbridge",
        "import.cgm_csv",
        "import.garmin_zip",
        "import.pdf",
    ] {
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
            ("import.garmin_zip".to_string(), serde_json::Value::Null),
            ("import.cgm_csv".to_string(), serde_json::Value::Null),
            ("import.gadgetbridge".to_string(), serde_json::Value::Null),
            ("import.fit".to_string(), serde_json::Value::Null),
        ]
    );
    let (_, owner) = a.get("alex", &uri).await?;
    assert!(results(&owner).iter().all(|(_, r)| *r == result), "{owner}");
    Ok(())
}

/// `GET /documents/{id}/file` as `who`: status, headers and the bytes.
async fn file_as(a: &App, who: &str, id: uuid::Uuid) -> anyhow::Result<(StatusCode, http::HeaderMap, Vec<u8>)> {
    let req = Request::get(format!("/documents/{id}/file"))
        .header(header::COOKIE, a.cookie(who).await?)
        .body(Body::empty())?;
    a.send_raw(req).await
}

/// A raw wearables export (a FIT activity file, a Gadgetbridge database) holds every reading of a device,
/// and a FIT file held the route: it is listed, opened and downloaded only with `read:wearables`;
/// `read:records` is not enough. The answer is the one every document type gives for a missing scope.
#[tokio::test]
async fn a_wearables_document_needs_read_wearables_to_be_listed_opened_or_downloaded() -> R {
    let a = app().await?;
    let alex = a.family.profile("alex");
    let household: uuid::Uuid = sqlx::query_scalar("select household_id from profile where id = $1")
        .bind(alex)
        .fetch_one(&a.rt.pool)
        .await?;
    let mut docs = Vec::new();
    for (name, mime, doc_type, bytes) in [
        ("run.fit", "application/vnd.ant.fit", "wearables", &b"a FIT file"[..]),
        (
            "Gadgetbridge.db",
            "application/vnd.sqlite3",
            "wearables",
            &b"a Gadgetbridge database"[..],
        ),
        ("letter.pdf", "application/pdf", "other", &b"a letter"[..]),
    ] {
        let sha =
            a.rt.blobs
                .put_bytes(ownhealth_core::blobs::BlobOwner::Profile(alex), bytes)?;
        let id: uuid::Uuid = sqlx::query_scalar(
            "insert into document (household_id, profile_id, blob_sha256, filename, mime, size_bytes, doc_type) \
             values ($1, $2, decode($3, 'hex'), $4, $5, $6, $7) returning id",
        )
        .bind(household)
        .bind(alex)
        .bind(&sha)
        .bind(name)
        .bind(mime)
        .bind(i64::try_from(bytes.len())?)
        .bind(doc_type)
        .fetch_one(&a.rt.pool)
        .await?;
        docs.push((id, bytes));
    }
    let [(fit, _), (gadgetbridge, _), (letter, _)] = docs[..] else {
        anyhow::bail!("three documents");
    };
    let grant = |categories: &'static [&'static str]| {
        let pool = a.rt.pool.clone();
        let (sam, alex) = (a.family.profile("sam"), alex);
        async move {
            let user: uuid::Uuid = sqlx::query_scalar("select user_id from profile where id = $1")
                .bind(sam)
                .fetch_one(&pool)
                .await?;
            sqlx::query("update profile_grant set categories = $3 where user_id = $1 and profile_id = $2")
                .bind(user)
                .bind(alex)
                .bind(categories)
                .execute(&pool)
                .await?;
            anyhow::Ok(())
        }
    };
    let listed = |v: &serde_json::Value| -> Vec<String> {
        v["items"]
            .as_array()
            .map(|a| a.iter().filter_map(|d| d["id"].as_str().map(str::to_string)).collect())
            .unwrap_or_default()
    };
    // records and labs, no wearables: the letter only
    grant(&["read:summary", "read:records", "read:labs"]).await?;
    let (s, v) = a.get("sam", "/profiles/alex/documents").await?;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(listed(&v), [letter.to_string()]);
    for (id, bytes) in &docs[..2] {
        let (s, v) = a.get("sam", &format!("/documents/{id}")).await?;
        assert_eq!(s, StatusCode::FORBIDDEN, "{v}");
        assert!(v.to_string().contains("This needs read:wearables."), "{v}");
        let (s, _, body) = file_as(&a, "sam", *id).await?;
        assert_eq!(s, StatusCode::FORBIDDEN);
        assert!(!body.windows(bytes.len()).any(|w| w == *bytes), "the bytes went out");
    }
    // with wearables: all three, and the files
    grant(&["read:summary", "read:records", "read:labs", "read:wearables"]).await?;
    let (_, v) = a.get("sam", "/profiles/alex/documents").await?;
    let mut ids = listed(&v);
    ids.sort();
    let mut want = vec![fit.to_string(), gadgetbridge.to_string(), letter.to_string()];
    want.sort();
    assert_eq!(ids, want);
    for (id, bytes) in &docs {
        let (s, v) = a.get("sam", &format!("/documents/{id}")).await?;
        assert_eq!(s, StatusCode::OK, "{v}");
        let (s, _, body) = file_as(&a, "sam", *id).await?;
        assert_eq!((s, body.as_slice()), (StatusCode::OK, *bytes));
    }
    // wearables without records: the exports, not the letter
    grant(&["read:summary", "read:labs", "read:wearables"]).await?;
    let (_, v) = a.get("sam", "/profiles/alex/documents").await?;
    let mut ids = listed(&v);
    ids.sort();
    let mut want = vec![fit.to_string(), gadgetbridge.to_string()];
    want.sort();
    assert_eq!(ids, want);
    let (s, _) = a.get("sam", &format!("/documents/{letter}")).await?;
    assert_eq!(s, StatusCode::FORBIDDEN);
    // wearables only: the list holds the exports, as each one opens by its id
    grant(&["read:summary", "read:wearables"]).await?;
    let (s, v) = a.get("sam", "/profiles/alex/documents").await?;
    assert_eq!(s, StatusCode::OK, "{v}");
    let mut ids = listed(&v);
    ids.sort();
    assert_eq!(ids, want);
    // none of the three: the refusal names every scope that lists documents
    grant(&["read:summary"]).await?;
    let (s, v) = a.get("sam", "/profiles/alex/documents").await?;
    assert_eq!(s, StatusCode::FORBIDDEN, "{v}");
    assert!(
        v.to_string()
            .contains("This needs read:labs, read:records or read:wearables."),
        "{v}"
    );
    Ok(())
}

/// Migration 1221 moves the wearables exports stored before it to `doc_type` `wearables`: a Gadgetbridge
/// import (its flag or its mime) and a FIT file; any other document keeps its type.
#[tokio::test]
async fn the_migration_moves_stored_wearables_exports_to_their_type() -> R {
    let a = app().await?;
    let alex = a.family.profile("alex");
    let household: uuid::Uuid = sqlx::query_scalar("select household_id from profile where id = $1")
        .bind(alex)
        .fetch_one(&a.rt.pool)
        .await?;
    let mut ids = Vec::new();
    for (i, (mime, flags)) in [
        ("application/octet-stream", json!({"gadgetbridge": {"devices": 1}})),
        ("application/vnd.sqlite3", json!({})),
        ("application/vnd.ant.fit", json!({})),
        ("application/pdf", json!({})),
    ]
    .into_iter()
    .enumerate()
    {
        let sha = a.rt.blobs.put_bytes(
            ownhealth_core::blobs::BlobOwner::Profile(alex),
            format!("export {i}").as_bytes(),
        )?;
        let id: uuid::Uuid = sqlx::query_scalar(
            "insert into document (household_id, profile_id, blob_sha256, mime, size_bytes, doc_type, flags) \
             values ($1, $2, decode($3, 'hex'), $4, 8, 'other', $5) returning id",
        )
        .bind(household)
        .bind(alex)
        .bind(&sha)
        .bind(mime)
        .bind(&flags)
        .fetch_one(&a.rt.pool)
        .await?;
        ids.push(id);
    }
    sqlx::raw_sql(include_str!("../../../../migrations/1221_wearables_documents.sql"))
        .execute(&a.rt.pool)
        .await?;
    let mut types = Vec::new();
    for id in &ids {
        let t: Option<String> = sqlx::query_scalar("select doc_type from document where id = $1")
            .bind(id)
            .fetch_one(&a.rt.pool)
            .await?;
        types.push(t.unwrap_or_default());
    }
    assert_eq!(types, ["wearables", "wearables", "wearables", "other"]);
    Ok(())
}

/// A document of Alex dated `day`, of `doc_type`, with an optional provider.
async fn dated_document(
    a: &App,
    name: &str,
    day: &str,
    doc_type: &str,
    provider: Option<&str>,
) -> anyhow::Result<uuid::Uuid> {
    let alex = a.family.profile("alex");
    let sha =
        a.rt.blobs
            .put_bytes(ownhealth_core::blobs::BlobOwner::Profile(alex), name.as_bytes())?;
    Ok(sqlx::query_scalar(
        "insert into document (household_id, profile_id, blob_sha256, filename, mime, size_bytes, doc_type, \
         provider_name, collected_at) \
         values ($1, $2, decode($3, 'hex'), $4, 'application/octet-stream', 8, $5, $6, ($7::date)::timestamptz) \
         returning id",
    )
    .bind(a.family.household_id)
    .bind(alex)
    .bind(&sha)
    .bind(name)
    .bind(doc_type)
    .bind(provider)
    .bind(day)
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

/// Sam's grant on Alex, as these categories.
async fn grant_sam(a: &App, categories: &[&str]) -> R {
    sqlx::query("update profile_grant set categories = $3 where user_id = $1 and profile_id = $2")
        .bind(a.family.user("sam"))
        .bind(a.family.profile("alex"))
        .bind(categories)
        .execute(&a.rt.pool)
        .await?;
    Ok(())
}

/// A FIT document is dated by its first activity, but a run is no medical record: monthly runs hide no
/// hole in the record, no gap cites one to a member who may not read wearables, a run with the
/// provider's name clears no visit, and a run is no evidence that a gap was closed.
#[tokio::test]
async fn a_wearables_export_is_no_record_fact_and_no_gap_cites_it() -> R {
    let a = app().await?;
    grant_sam(&a, &["read:summary", "read:records"]).await?;
    let letter = dated_document(&a, "letter-a.pdf", "2001-01-10", "other", None).await?;
    dated_document(&a, "letter-b.pdf", "2003-06-10", "other", None).await?;
    let mut runs = Vec::new();
    for m in 0..28 {
        let (y, mo) = (2001 + (m + 1) / 12, (m + 1) % 12 + 1);
        let day = format!("{y}-{mo:02}-15");
        runs.push(dated_document(&a, &format!("run-{m}.fit"), &day, "wearables", None).await?);
    }
    let holes = |v: &serde_json::Value| -> Vec<String> {
        v["items"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter(|i| i["rule"] == "records_hole")
                    .filter_map(|i| i["key"].as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default()
    };
    let uri = "/profiles/alex/gaps?lang=en&as_of=2004-01-01";
    for who in ["sam", "alex"] {
        let (s, v) = a.get(who, uri).await?;
        assert_eq!(s, StatusCode::OK, "{v}");
        // the 881 days between the two letters are still a hole, whatever the runs say
        assert_eq!(holes(&v), ["hole:2001-01-10:2003-06-10"], "{who}: {v}");
        let body = v.to_string();
        assert!(runs.iter().all(|r| !body.contains(&r.to_string())), "{who}: {v}");
    }
    // a run is no evidence that the hole was closed; the letter that bounds it is
    let due = "/profiles/alex/due/gap:hole:2001-01-10:2003-06-10/decision";
    let (s, v) = a
        .post(
            "alex",
            due,
            Some(json!({"kind": "done", "evidence": {"table": "document", "id": runs[20]}})),
        )
        .await?;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{v}");
    assert!(v.to_string().contains("evidence_refused"), "{v}");
    let (s, v) = a
        .post(
            "alex",
            due,
            Some(json!({"kind": "done", "evidence": {"table": "document", "id": letter}})),
        )
        .await?;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    // a visit typed by hand with no report: a run that names the clinic on that day clears nothing
    let body = json!({"period_start": "2002-03-05T09:30:00Z", "reason_text": "Knee",
                      "provider_name": "Clínica Sur"});
    let (s, v) = a.post("alex", "/profiles/alex/records/visit", Some(body)).await?;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    dated_document(&a, "clinic-run.fit", "2002-03-05", "wearables", Some("Clínica Sur")).await?;
    let (_, v) = a.get("alex", uri).await?;
    let visits: Vec<&serde_json::Value> = v["items"]
        .as_array()
        .map(|a| a.iter().filter(|i| i["rule"] == "visit_without_report").collect())
        .unwrap_or_default();
    assert_eq!(visits.len(), 1, "{v}");
    // the control: the clinic's own report clears it
    dated_document(&a, "clinic-report.pdf", "2002-03-05", "other", Some("Clínica Sur")).await?;
    let (_, v) = a.get("alex", uri).await?;
    assert!(
        v["items"]
            .as_array()
            .is_some_and(|a| a.iter().all(|i| i["rule"] != "visit_without_report")),
        "{v}"
    );
    Ok(())
}

/// The timeline shows the documents of the record, never a raw wearables export, to a member with
/// read:records and to the owner alike.
#[tokio::test]
async fn the_timeline_shows_no_wearables_export() -> R {
    let a = app().await?;
    grant_sam(&a, &["read:summary", "read:records"]).await?;
    let letter = dated_document(&a, "letter.pdf", "2024-03-01", "other", None).await?;
    let run = dated_document(&a, "run.fit", "2024-03-02", "wearables", None).await?;
    for who in ["sam", "alex"] {
        let (s, v) = a.get(who, "/profiles/alex/timeline").await?;
        assert_eq!(s, StatusCode::OK, "{v}");
        let docs: Vec<String> = v["events"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter(|e| e["type"] == "document")
                    .filter_map(|e| e["document_id"].as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        assert!(docs.contains(&letter.to_string()), "{who}: {v}");
        assert!(!v.to_string().contains(&run.to_string()), "{who}: {v}");
    }
    Ok(())
}

/// The ids in a JSON answer's text: whether `id` appears anywhere in it.
fn mentions(v: &serde_json::Value, id: uuid::Uuid) -> bool {
    v.to_string().contains(&id.to_string())
}

/// A CGM CSV export holds every glucose reading of its sensors: its import stores it as a wearables
/// document, so a member without read:wearables cannot list, open or download it, and no member sees it
/// on the timeline or in the gaps; one with read:wearables lists, opens and downloads it.
#[tokio::test]
async fn a_cgm_export_is_a_wearables_document() -> R {
    let a = app().await?;
    ownhealth_devices::register();
    let (s, v) = a
        .upload(
            "alex",
            &[("profile", "alex"), ("timezone", "Europe/Madrid")],
            &[("clarity.csv", CLARITY)],
        )
        .await?;
    assert_eq!(s, StatusCode::OK, "{v}");
    let kinds = ["import.cgm_csv".to_string()];
    ownhealth_core::jobs::drain(&a.rt.job_context(), "core", 5, Some(&kinds)).await?;
    let (doc, doc_type): (uuid::Uuid, Option<String>) =
        sqlx::query_as("select id, doc_type from document where profile_id = $1 and filename = 'clarity.csv'")
            .bind(a.family.profile("alex"))
            .fetch_one(&a.rt.pool)
            .await?;
    assert_eq!(doc_type.as_deref(), Some("wearables"));
    // dated, so the timeline and the gaps could show it
    sqlx::query("update document set collected_at = '2024-03-02' where id = $1")
        .bind(doc)
        .execute(&a.rt.pool)
        .await?;
    let listed = |v: &serde_json::Value| {
        v["items"]
            .as_array()
            .is_some_and(|i| i.iter().any(|d| d["id"] == json!(doc.to_string())))
    };
    let elsewhere = |who: &'static str| {
        let a = &a;
        async move {
            let (s, t) = a.get(who, "/profiles/alex/timeline").await?;
            assert_eq!(s, StatusCode::OK, "{t}");
            let (s, g) = a.get(who, "/profiles/alex/gaps?lang=en").await?;
            assert_eq!(s, StatusCode::OK, "{g}");
            anyhow::Ok(mentions(&t, doc) || mentions(&g, doc))
        }
    };
    // records and labs, no wearables: not listed, not opened, not downloaded
    grant_sam(&a, &["read:summary", "read:records", "read:labs"]).await?;
    let (s, v) = a.get("sam", "/profiles/alex/documents").await?;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert!(!listed(&v), "{v}");
    let (s, v) = a.get("sam", &format!("/documents/{doc}")).await?;
    assert_eq!(s, StatusCode::FORBIDDEN, "{v}");
    assert!(v.to_string().contains("This needs read:wearables."), "{v}");
    let (s, _, body) = file_as(&a, "sam", doc).await?;
    assert_eq!(s, StatusCode::FORBIDDEN);
    assert!(!body.windows(CLARITY.len()).any(|w| w == CLARITY), "the bytes went out");
    assert!(!elsewhere("sam").await?);
    // with wearables: listed, opened and downloaded; still no record fact on the timeline or in the gaps
    grant_sam(&a, &["read:summary", "read:records", "read:labs", "read:wearables"]).await?;
    let (_, v) = a.get("sam", "/profiles/alex/documents").await?;
    assert!(listed(&v), "{v}");
    let (s, v) = a.get("sam", &format!("/documents/{doc}")).await?;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["flags"]["cgm_csv"]["readings"], json!(1), "{v}");
    let (s, _, body) = file_as(&a, "sam", doc).await?;
    assert_eq!((s, body.as_slice()), (StatusCode::OK, CLARITY));
    assert!(!elsewhere("sam").await?);
    assert!(!elsewhere("alex").await?);
    // the control: the same document as a record shows on the owner's timeline
    sqlx::query("update document set doc_type = 'other' where id = $1")
        .bind(doc)
        .execute(&a.rt.pool)
        .await?;
    assert!(elsewhere("alex").await?);
    Ok(())
}

/// Migration 1292 moves the CGM CSV exports stored before it (`flags.cgm_csv`) to `doc_type`
/// `wearables`; another CSV document keeps its type.
#[tokio::test]
async fn the_migration_moves_stored_cgm_exports_to_the_wearables_type() -> R {
    let a = app().await?;
    let alex = a.family.profile("alex");
    let mut ids = Vec::new();
    for (i, flags) in [
        json!({"cgm_csv": {"vendor": "libreview", "readings": 13}, "import": {"accepted": 13}}),
        json!({"cgm_csv": {"vendor": "dexcom_clarity"}}),
        json!({"import": {"accepted": 0}}),
    ]
    .into_iter()
    .enumerate()
    {
        let sha = a.rt.blobs.put_bytes(
            ownhealth_core::blobs::BlobOwner::Profile(alex),
            format!("csv {i}").as_bytes(),
        )?;
        let id: uuid::Uuid = sqlx::query_scalar(
            "insert into document (household_id, profile_id, blob_sha256, mime, size_bytes, doc_type, flags) \
             values ($1, $2, decode($3, 'hex'), 'text/csv', 5, 'other', $4) returning id",
        )
        .bind(a.family.household_id)
        .bind(alex)
        .bind(&sha)
        .bind(&flags)
        .fetch_one(&a.rt.pool)
        .await?;
        ids.push(id);
    }
    for _ in 0..2 {
        sqlx::raw_sql(include_str!("../../../../migrations/1292_cgm_documents.sql"))
            .execute(&a.rt.pool)
            .await?;
    }
    let mut types = Vec::new();
    for id in &ids {
        let t: Option<String> = sqlx::query_scalar("select doc_type from document where id = $1")
            .bind(id)
            .fetch_one(&a.rt.pool)
            .await?;
        types.push(t.unwrap_or_default());
    }
    assert_eq!(types, ["wearables", "wearables", "other"]);
    Ok(())
}

/// Store a CGM export on a profile and import it in `zone`, as the upload job does when the server has no
/// `OWNHEALTH_TIMEZONE` and the browser sent `zone`.
async fn cgm_export_in(a: &App, slug: &str, bytes: &[u8], zone: &str) -> anyhow::Result<uuid::Uuid> {
    let profile = a.family.profile(slug);
    let sha =
        a.rt.blobs
            .put_bytes(ownhealth_core::blobs::BlobOwner::Profile(profile), bytes)?;
    let payload = json!({
        "profile_id": profile.to_string(),
        "household_id": a.family.household_id.to_string(),
        "blob_sha256": sha,
        "filename": "clarity.csv",
    });
    let mut tx = a.rt.pool.begin().await?;
    let doc = ownhealth_ingest_structured::jobs::document_of_type(
        &mut tx,
        &a.rt.blobs,
        &payload,
        "text/csv",
        "upload",
        ownhealth_core::documents::WEARABLES_DOC_TYPE,
    )
    .await
    .map_err(|e| anyhow::anyhow!("{e:?}"))?;
    ownhealth_devices::cgm_csv::import_cgm_csv(&mut tx, &a.rt.blobs, doc, None, Some(zone)).await?;
    tx.commit().await?;
    Ok(doc)
}

/// A running CGM import holds its profile's export lock and then writes the document row. Moving that
/// export through the API waits for the lock before it locks the row, so the move and the import run
/// one after the other: neither is aborted as a deadlock (40P01).
#[tokio::test]
async fn moving_a_cgm_export_while_its_import_runs_waits_for_the_import() -> R {
    let a = app().await?;
    ownhealth_devices::register();
    let alex = a.family.profile("alex");
    // the export as its first import reads it, in Madrid: the test names the zone itself, so a host that
    // sets OWNHEALTH_TIMEZONE (which an upload job reads first) gets the same instants
    let doc = cgm_export_in(&a, "alex", CLARITY, "Europe/Madrid").await?;
    // an import of the profile's exports runs: it holds the lock
    let mut import = a.rt.pool.begin().await?;
    ownhealth_ingest_structured::retract::lock_exports(&mut import, alex, "cgm_csv").await?;
    let path = format!("/documents/{doc}/assign");
    let (moved, imported) = tokio::join!(a.post("alex", &path, Some(json!({"profile": "lucia"}))), async {
        // once the move waits for the lock, the import writes the document row and commits; a move that
        // never waits fails the test
        let mut waited = false;
        for _ in 0..100 {
            let waiting: i64 = sqlx::query_scalar(
                "select count(*) from pg_stat_activity where datname = current_database() \
                 and wait_event_type = 'Lock' and wait_event = 'advisory' and query like '%pg_advisory_xact_lock%'",
            )
            .fetch_one(&a.rt.pool)
            .await?;
            if waiting > 0 {
                waited = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        anyhow::ensure!(waited, "the move never waited for the running import's export lock");
        sqlx::query("update document set flags = flags || '{\"cgm_csv_seen\": true}' where id = $1")
            .bind(doc)
            .execute(&mut *import)
            .await?;
        import.commit().await?;
        anyhow::Ok(())
    });
    imported?;
    let (s, v) = moved?;
    assert_eq!(s, StatusCode::OK, "{v}");
    let (owner, seen): (Option<uuid::Uuid>, bool) =
        sqlx::query_as("select profile_id, flags ? 'cgm_csv_seen' from document where id = $1")
            .bind(doc)
            .fetch_one(&a.rt.pool)
            .await?;
    assert_eq!((owner, seen), (Some(a.family.profile("lucia")), true));
    // the move imports the export again for Lucía, in the zone its file was read in (Madrid, +60)
    ownhealth_core::jobs::drain(&a.rt.job_context(), "core", 5, None).await?;
    let jobs: Vec<(String, String)> = sqlx::query_as(
        "select kind, state from job where payload->>'document_id' = $1 and payload->>'profile_id' = $2",
    )
    .bind(doc.to_string())
    .bind(a.family.profile("lucia").to_string())
    .fetch_all(&a.rt.pool)
    .await?;
    assert_eq!(jobs, [("import.cgm_csv".to_string(), "done".to_string())]);
    let held: Vec<(uuid::Uuid, chrono::DateTime<chrono::Utc>, Option<i16>)> =
        sqlx::query_as("select profile_id, t, tz_offset_min from wearable_sample where document_id = $1")
            .bind(doc)
            .fetch_all(&a.rt.pool)
            .await?;
    let at = chrono::DateTime::parse_from_rfc3339("2026-11-14T19:00:17Z")?.with_timezone(&chrono::Utc);
    assert_eq!(held, [(a.family.profile("lucia"), at, Some(60))]);
    let zone: serde_json::Value = sqlx::query_scalar("select flags->'cgm_csv'->'timezone' from document where id = $1")
        .bind(doc)
        .fetch_one(&a.rt.pool)
        .await?;
    assert_eq!(zone, json!("Europe/Madrid"));
    Ok(())
}

/// An export that moved on before the job of its first move ran (Lucía to Alex, then back to Lucía): that
/// job is Alex's (its payload names her, and /jobs shows its result to those who read Alex's wearables),
/// so it reads nothing of the export, now Lucía's, and its result counts none of Lucía's readings. The
/// job of the second move imports the export for Lucía.
#[tokio::test]
async fn the_job_of_a_move_the_export_has_left_reads_nothing_of_its_new_owner() -> R {
    let a = app().await?;
    ownhealth_devices::register();
    let kinds = ["import.cgm_csv".to_string()];
    let (alex, lucia) = (a.family.profile("alex"), a.family.profile("lucia"));
    // Lucía holds another export with the same reading, and the export that moves
    let longer = [CLARITY, b"2,2026-11-14T20:05:17,EGV,,,,iOS G7,121,,,,,3456300,7Q2RLX\n"].concat();
    for (name, bytes) in [("longer.csv", longer.as_slice()), ("clarity.csv", CLARITY)] {
        let (s, v) = a
            .upload(
                "alex",
                &[("profile", "lucia"), ("timezone", "Europe/Madrid")],
                &[(name, bytes)],
            )
            .await?;
        assert_eq!(s, StatusCode::OK, "{v}");
        ownhealth_core::jobs::drain(&a.rt.job_context(), "core", 5, Some(&kinds)).await?;
    }
    let doc: uuid::Uuid =
        sqlx::query_scalar("select id from document where profile_id = $1 and filename = 'clarity.csv'")
            .bind(lucia)
            .fetch_one(&a.rt.pool)
            .await?;
    // two moves before any job runs: to Alex, then back to Lucía
    let path = format!("/documents/{doc}/assign");
    for to in ["alex", "lucia"] {
        let (s, v) = a.post("alex", &path, Some(json!({"profile": to}))).await?;
        assert_eq!(s, StatusCode::OK, "{v}");
    }
    ownhealth_core::jobs::drain(&a.rt.job_context(), "core", 5, Some(&kinds)).await?;
    let result = |profile: uuid::Uuid| {
        let a = &a;
        async move {
            let r: (String, serde_json::Value) = sqlx::query_as(
                "select state, coalesce(result, 'null') from job where kind = 'import.cgm_csv' \
                 and payload->>'document_id' = $1 and payload->>'profile_id' = $2",
            )
            .bind(doc.to_string())
            .bind(profile.to_string())
            .fetch_one(&a.rt.pool)
            .await?;
            anyhow::Ok(r)
        }
    };
    let (state, moved) = result(alex).await?;
    assert_eq!(state, "done", "{moved}");
    assert_eq!(moved["cgm_csv"], serde_json::Value::Null, "{moved}");
    assert_eq!(
        (&moved["accepted"], &moved["reasons"]),
        (&json!(0), &json!({"moved": 1})),
        "{moved}"
    );
    let (state, back) = result(lucia).await?;
    assert_eq!(state, "done", "{back}");
    assert_eq!(back["cgm_csv"]["duplicates"], json!(1), "{back}");
    let (owner, imported): (Option<uuid::Uuid>, bool) =
        sqlx::query_as("select profile_id, flags ? 'cgm_csv' and flags ? 'import' from document where id = $1")
            .bind(doc)
            .fetch_one(&a.rt.pool)
            .await?;
    assert_eq!((owner, imported), (Some(lucia), true));
    Ok(())
}

/// The same bytes stored before as a document of another type (a CSV kept as a record), then uploaded as
/// a CGM export: the CGM import attaches no reading to that row and says why. A document of another type
/// that carries CGM import flags (one stored before this rule) still needs read:wearables to be listed,
/// opened or downloaded.
#[tokio::test]
async fn a_cgm_export_never_reads_into_a_document_of_another_type() -> R {
    let a = app().await?;
    ownhealth_devices::register();
    let alex = a.family.profile("alex");
    let sha =
        a.rt.blobs
            .put_bytes(ownhealth_core::blobs::BlobOwner::Profile(alex), CLARITY)?;
    let doc: uuid::Uuid = sqlx::query_scalar(
        "insert into document (household_id, profile_id, blob_sha256, filename, mime, size_bytes, doc_type) \
         values ($1, $2, decode($3, 'hex'), 'notes.csv', 'text/csv', $4, 'other') returning id",
    )
    .bind(a.family.household_id)
    .bind(alex)
    .bind(&sha)
    .bind(CLARITY.len() as i64)
    .fetch_one(&a.rt.pool)
    .await?;
    let (s, v) = a
        .upload(
            "alex",
            &[("profile", "alex"), ("timezone", "Europe/Madrid")],
            &[("clarity.csv", CLARITY)],
        )
        .await?;
    assert_eq!(s, StatusCode::OK, "{v}");
    let kinds = ["import.cgm_csv".to_string()];
    ownhealth_core::jobs::drain(&a.rt.job_context(), "core", 5, Some(&kinds)).await?;
    let (state, err): (String, Option<String>) = sqlx::query_as("select state, last_error from job where id = $1")
        .bind(v["items"][0]["job_id"].as_i64())
        .fetch_one(&a.rt.pool)
        .await?;
    assert_eq!(state, "dead", "{err:?}");
    assert!(
        err.as_deref()
            .is_some_and(|e| e.contains("already stored on this profile as another kind of document")),
        "{err:?}"
    );
    let (doc_type, cgm, held): (Option<String>, bool, i64) = sqlx::query_as(
        "select doc_type, flags ? 'cgm_csv', (select count(*) from wearable_sample where document_id = d.id) \
         from document d where id = $1",
    )
    .bind(doc)
    .fetch_one(&a.rt.pool)
    .await?;
    assert_eq!((doc_type.as_deref(), cgm, held), (Some("other"), false, 0));
    Ok(())
}

/// A document of another type that carries CGM import flags (one stored before the CGM import refused
/// such a row) holds every reading of the export: it needs read:wearables to be listed, opened or
/// downloaded, as a wearables document does.
#[tokio::test]
async fn a_document_with_cgm_flags_needs_read_wearables_whatever_its_type() -> R {
    let a = app().await?;
    let alex = a.family.profile("alex");
    let sha =
        a.rt.blobs
            .put_bytes(ownhealth_core::blobs::BlobOwner::Profile(alex), CLARITY)?;
    let doc: uuid::Uuid = sqlx::query_scalar(
        "insert into document (household_id, profile_id, blob_sha256, filename, mime, size_bytes, doc_type) \
         values ($1, $2, decode($3, 'hex'), 'notes.csv', 'text/csv', $4, 'other') returning id",
    )
    .bind(a.family.household_id)
    .bind(alex)
    .bind(&sha)
    .bind(CLARITY.len() as i64)
    .fetch_one(&a.rt.pool)
    .await?;
    sqlx::query(
        "update document set flags = flags || '{\"cgm_csv\": {\"vendor\": \"dexcom_clarity\", \"readings\": 1}}' \
         where id = $1",
    )
    .bind(doc)
    .execute(&a.rt.pool)
    .await?;
    let listed = |v: &serde_json::Value| {
        v["items"]
            .as_array()
            .is_some_and(|i| i.iter().any(|d| d["id"] == json!(doc.to_string())))
    };
    grant_sam(&a, &["read:summary", "read:records", "read:labs"]).await?;
    let (s, v) = a.get("sam", "/profiles/alex/documents").await?;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert!(!listed(&v), "{v}");
    let (s, v) = a.get("sam", &format!("/documents/{doc}")).await?;
    assert_eq!(s, StatusCode::FORBIDDEN, "{v}");
    assert!(v.to_string().contains("This needs read:wearables."), "{v}");
    let (s, _, body) = file_as(&a, "sam", doc).await?;
    assert_eq!(s, StatusCode::FORBIDDEN);
    assert!(!body.windows(CLARITY.len()).any(|w| w == CLARITY), "the bytes went out");
    // with read:wearables it is listed, opened and downloaded
    grant_sam(&a, &["read:summary", "read:records", "read:labs", "read:wearables"]).await?;
    let (_, v) = a.get("sam", "/profiles/alex/documents").await?;
    assert!(listed(&v), "{v}");
    let (s, v) = a.get("sam", &format!("/documents/{doc}")).await?;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["flags"]["cgm_csv"]["readings"], json!(1), "{v}");
    let (s, _, body) = file_as(&a, "sam", doc).await?;
    assert_eq!((s, body.as_slice()), (StatusCode::OK, CLARITY));
    Ok(())
}

/// A retraction that waits for a move of the export (the move holds the export lock until it commits)
/// checks the caller's permission on the owner the move leaves the export with. Sam may change Lucía's
/// records but only view Alex's: once the export is Alex's, his retraction is refused and the export
/// stays active.
#[tokio::test]
async fn a_retraction_that_waited_for_a_move_checks_the_permission_on_the_new_owner() -> R {
    let a = app().await?;
    let alex = a.family.profile("alex");
    let doc = cgm_export_in(&a, "lucia", CLARITY, "Europe/Madrid").await?;
    // a move to Alex, held before its commit, with its locks in the order the assign route takes them
    let mut mv = a.rt.pool.begin().await?;
    ownhealth_ingest_structured::retract::lock_export_of(&mut mv, doc).await?;
    ownhealth_core::documents::lock_owner(&mut mv, doc).await?;
    ownhealth_ingest_pdf::assign_document(&mut mv, &a.rt.blobs, doc, alex, a.family.user("alex"), None).await?;
    let path = format!("/documents/{doc}/retract");
    let (retracted, moved) = tokio::join!(a.post("sam", &path, Some(json!({"reason": "wrong file"}))), async {
        // once the retraction waits for the export lock, the move commits
        let mut waited = false;
        for _ in 0..100 {
            let waiting: i64 = sqlx::query_scalar(
                "select count(*) from pg_stat_activity where datname = current_database() \
                 and wait_event_type = 'Lock' and wait_event = 'advisory' and query like '%pg_advisory_xact_lock%'",
            )
            .fetch_one(&a.rt.pool)
            .await?;
            if waiting > 0 {
                waited = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        anyhow::ensure!(waited, "the retraction never waited for the move's export lock");
        mv.commit().await?;
        anyhow::Ok(())
    });
    moved?;
    let (s, v) = retracted?;
    let (owner, status): (Option<uuid::Uuid>, String) =
        sqlx::query_as("select profile_id, status from document where id = $1")
            .bind(doc)
            .fetch_one(&a.rt.pool)
            .await?;
    assert_eq!((owner, status.as_str()), (Some(alex), "active"), "{s} {v}");
    assert_eq!(s, StatusCode::FORBIDDEN, "{v}");
    Ok(())
}

/// A move's run is marked done only after its work commits. A move back to that run's profile in
/// between (Lucía to Alex, back to Lucía, then to Alex again while Alex's first run is still marked
/// running) queues one more run after it, so the export ends on Alex with its reading.
#[tokio::test]
async fn a_move_while_a_run_for_its_profile_is_still_marked_running_imports_after_it() -> R {
    let a = app().await?;
    ownhealth_devices::register();
    let kinds = ["import.cgm_csv".to_string()];
    let (alex, lucia) = (a.family.profile("alex"), a.family.profile("lucia"));
    // Lucía holds another export with the same reading, and the export that moves
    let longer = [CLARITY, b"2,2026-11-14T20:05:17,EGV,,,,iOS G7,121,,,,,3456300,7Q2RLX\n"].concat();
    for (name, bytes) in [("longer.csv", longer.as_slice()), ("clarity.csv", CLARITY)] {
        let (s, v) = a
            .upload(
                "alex",
                &[("profile", "lucia"), ("timezone", "Europe/Madrid")],
                &[(name, bytes)],
            )
            .await?;
        assert_eq!(s, StatusCode::OK, "{v}");
        ownhealth_core::jobs::drain(&a.rt.job_context(), "core", 5, Some(&kinds)).await?;
    }
    let doc: uuid::Uuid =
        sqlx::query_scalar("select id from document where profile_id = $1 and filename = 'clarity.csv'")
            .bind(lucia)
            .fetch_one(&a.rt.pool)
            .await?;
    let path = format!("/documents/{doc}/assign");
    for to in ["alex", "lucia"] {
        let (s, v) = a.post("alex", &path, Some(json!({"profile": to}))).await?;
        assert_eq!(s, StatusCode::OK, "{v}");
    }
    // Alex's run: the export has left Alex, so it reads nothing; its work commits, its done mark waits
    let ctx = a.rt.job_context();
    let job = ownhealth_core::jobs::claim(&ctx.pool, "core", &ctx.worker_id, Some(&kinds))
        .await?
        .ok_or_else(|| anyhow::anyhow!("no queued job"))?;
    assert_eq!(job.str("profile_id"), Some(alex.to_string().as_str()));
    let handler = ownhealth_core::jobs::handler_for("import.cgm_csv").ok_or_else(|| anyhow::anyhow!("no handler"))?;
    let out = handler(ctx.clone(), job.clone())
        .await
        .map_err(|e| anyhow::anyhow!(e.text()))?;
    // the export moves to Alex again in that window
    let (s, third) = a.post("alex", &path, Some(json!({"profile": "alex"}))).await?;
    assert_eq!(s, StatusCode::OK, "{third}");
    ownhealth_core::jobs::complete(&ctx.pool, &job, &ctx.worker_id, out.as_ref()).await?;
    ownhealth_core::jobs::drain(&ctx, "core", 5, Some(&kinds)).await?;
    let (owner, imported, held): (Option<uuid::Uuid>, bool, i64) = sqlx::query_as(
        "select profile_id, flags ? 'import', (select count(*) from wearable_sample s where s.document_id = d.id \
         and s.profile_id = d.profile_id) from document d where id = $1",
    )
    .bind(doc)
    .fetch_one(&a.rt.pool)
    .await?;
    assert_eq!((owner, imported, held), (Some(alex), true, 1), "{third}");
    assert!(third["job_id"].as_i64().is_some(), "{third}");
    Ok(())
}

/// A Garmin export's document holds the readings it read from the zip (the zip is not kept): a move
/// runs the Garmin import of that document for the new owner, so the readings follow it.
#[tokio::test]
async fn a_moved_garmin_export_takes_its_readings_to_the_new_profile() -> R {
    let a = app().await?;
    ownhealth_devices::register();
    ownhealth_ingest_pdf::register();
    let kinds = ["import.garmin_zip".to_string(), "import.pdf".to_string()];
    let bytes = std::fs::read(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../../fixtures/garmin/lucia-history.zip"
    ))?;
    let (s, v) = a
        .upload("alex", &[("profile", "alex")], &[("lucia-history.zip", &bytes)])
        .await?;
    assert_eq!(s, StatusCode::OK, "{v}");
    let ctx = a.rt.job_context();
    ownhealth_core::jobs::drain(&ctx, "core", 10, Some(&kinds)).await?;
    let job = v["items"][0]["job_id"].as_i64().expect("job");
    let result: Option<serde_json::Value> = sqlx::query_scalar("select result from job where id = $1")
        .bind(job)
        .fetch_one(&a.rt.pool)
        .await?;
    let doc = result.expect("result")["document_id"]
        .as_str()
        .expect("document")
        .to_string();
    let (alex, lucia) = (a.family.profile("alex"), a.family.profile("lucia"));
    let count = |p: uuid::Uuid| {
        let pool = a.rt.pool.clone();
        async move {
            anyhow::Ok(
                sqlx::query_scalar::<_, i64>("select count(*) from wearable_sample where profile_id = $1")
                    .bind(p)
                    .fetch_one(&pool)
                    .await?,
            )
        }
    };
    let held = count(alex).await?;
    assert!(held > 0);
    let (s, moved) = a
        .post(
            "alex",
            &format!("/documents/{doc}/assign"),
            Some(json!({"profile": "lucia"})),
        )
        .await?;
    assert_eq!(s, StatusCode::OK, "{moved}");
    ownhealth_core::jobs::drain(&ctx, "core", 10, Some(&kinds)).await?;
    assert_eq!((count(alex).await?, count(lucia).await?), (0, held));
    let items: i64 = sqlx::query_scalar("select count(*) from review_item where profile_id = $1")
        .bind(lucia)
        .fetch_one(&a.rt.pool)
        .await?;
    assert_eq!(items, 0);
    Ok(())
}
