//! Slice 41: `POST /api/imaging/import` keeps a hospital CD, as a zip or as a folder of up to 20,000
//! files, as one zip of the profile before anything reads it, and queues its import. `/api/imports`
//! keeps its 1,000-part cap.

mod lane_i;

use std::io::{Read, Write};

use axum::body::Body;
use http::{Request, StatusCode, header};
use lane_i::{App, R, app, item};
use ownhealth_core::Runtime;
use ownhealth_core::blobs::BlobOwner;
use ownhealth_core::orthanc::OrthancConfig;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use uuid::Uuid;

/// The app with imaging on (an Orthanc nobody calls: the route only stores and queues).
async fn imaging_app() -> anyhow::Result<App> {
    let mut a = app().await?;
    let mut settings = (*a.rt.settings).clone();
    settings.orthanc = Some(OrthancConfig {
        url: "http://127.0.0.1:9".into(),
        user: "ownhealth".into(),
        password: ownhealth_core::ids::token_hex(16),
    });
    a.rt = Runtime::with_pool(settings, a.db.pool.clone())?;
    Ok(a)
}

/// A multipart form: text fields, then `files` parts (name as the browser sends it, bytes).
fn form(fields: &[(&str, &str)], files: &[(String, Vec<u8>)]) -> (String, Vec<u8>) {
    let boundary = "ownhealth-imaging-boundary";
    let mut body = Vec::new();
    for (name, value) in fields {
        body.extend_from_slice(
            format!("--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n").as_bytes(),
        );
    }
    for (name, bytes) in files {
        body.extend_from_slice(
            format!(
                "--{boundary}\r\nContent-Disposition: form-data; name=\"files\"; filename=\"{name}\"\r\n\
                 Content-Type: application/octet-stream\r\n\r\n"
            )
            .as_bytes(),
        );
        body.extend_from_slice(bytes);
        body.extend_from_slice(b"\r\n");
    }
    body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
    (format!("multipart/form-data; boundary={boundary}"), body)
}

async fn post(
    a: &App,
    who: &str,
    uri: &str,
    fields: &[(&str, &str)],
    files: &[(String, Vec<u8>)],
) -> anyhow::Result<(StatusCode, Value)> {
    let (ctype, body) = form(fields, files);
    let req = Request::post(uri)
        .header(header::COOKIE, a.cookie(who).await?)
        .header(header::CONTENT_TYPE, ctype)
        .body(Body::from(body))?;
    a.send(req).await
}

fn zip_of(files: &[(&str, &[u8])]) -> anyhow::Result<Vec<u8>> {
    let mut out = std::io::Cursor::new(Vec::new());
    let mut zip = zip::ZipWriter::new(&mut out);
    for (name, bytes) in files {
        zip.start_file(*name, zip::write::SimpleFileOptions::default())?;
        zip.write_all(bytes)?;
    }
    zip.finish()?;
    Ok(out.into_inner())
}

/// What `files` should be: synthetic bytes, the route reads none of them.
fn disc() -> Vec<(String, Vec<u8>)> {
    vec![
        ("CD/DICOMDIR".into(), b"synthetic directory".to_vec()),
        ("CD/DICOM/IM0001".into(), b"synthetic image one".to_vec()),
        ("CD/viewer.exe".into(), b"synthetic viewer".to_vec()),
    ]
}

async fn imports(a: &App, pid: Uuid) -> anyhow::Result<Vec<(Uuid, Vec<u8>, Option<String>, i64, i32)>> {
    Ok(sqlx::query_as(
        "select id, zip_blob, filename, size_bytes, files from imaging_import where profile_id = $1 order by created_at",
    )
    .bind(pid)
    .fetch_all(&a.rt.pool)
    .await?)
}

#[tokio::test]
async fn a_zip_is_kept_as_one_blob_of_the_profile_and_its_import_is_queued() -> R {
    let a = imaging_app().await?;
    let alex = a.family.profile("alex");
    let zip = zip_of(&[
        ("DICOM/IM1", b"one"),
        ("DICOM/IM2", b"two"),
        ("autorun.inf", b"[autorun]"),
    ])?;
    let (s, v) = post(
        &a,
        "alex",
        "/imaging/import",
        &[("profile", "alex")],
        &[("cd.zip".into(), zip.clone())],
    )
    .await?;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["duplicate"], json!(false));
    assert_eq!(v["files"], json!(3));
    let job = v["job_id"].as_i64().ok_or_else(|| anyhow::anyhow!("no job: {v}"))?;
    let rows = imports(&a, alex).await?;
    assert_eq!(rows.len(), 1);
    let (id, sha, filename, size, files) = rows[0].clone();
    assert_eq!(v["import_id"], json!(id));
    assert_eq!(sha, Sha256::digest(&zip).to_vec(), "the zip is kept as it came");
    assert_eq!(filename.as_deref(), Some("cd.zip"));
    assert_eq!(size, i64::try_from(zip.len())?);
    assert_eq!(files, 3);
    assert_eq!(a.rt.blobs.get_bytes(BlobOwner::Profile(alex), &hex(&sha))?, zip);
    // the import job, as /api/jobs lists it
    let (s, jobs) = a.get("alex", &format!("/jobs?ids={job}")).await?;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(jobs["jobs"][0]["kind"], "import.dicom");
    assert_eq!(jobs["jobs"][0]["filename"], "cd.zip");
    assert_eq!(jobs["jobs"][0]["profile_id"], json!(alex));
    let audited = a
        .count(&format!(
            "select count(*) from audit_event where action = 'imaging.import' and profile_id = '{alex}' \
             and resource_ids = array['{id}']"
        ))
        .await?;
    assert_eq!(audited, 1);
    // the spooled copies are gone
    let left: Vec<_> = std::fs::read_dir(a.rt.settings.blobs_path().join("tmp"))?.collect();
    assert!(left.is_empty(), "{left:?}");
    // the same zip again is the same import
    let (s, again) = post(
        &a,
        "alex",
        "/imaging/import",
        &[("profile", "alex")],
        &[("copy.zip".into(), zip)],
    )
    .await?;
    assert_eq!(s, StatusCode::OK, "{again}");
    assert_eq!(again["duplicate"], json!(true));
    assert_eq!(again["import_id"], v["import_id"]);
    assert_eq!(imports(&a, alex).await?.len(), 1);
    Ok(())
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

#[tokio::test]
async fn a_folder_becomes_one_zip_of_its_files() -> R {
    let a = imaging_app().await?;
    let alex = a.family.profile("alex");
    let (s, v) = post(&a, "alex", "/imaging/import", &[("profile", "alex")], &disc()).await?;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["files"], json!(3));
    let rows = imports(&a, alex).await?;
    assert_eq!(rows[0].2.as_deref(), Some("CD.zip"));
    let zip = a.rt.blobs.get_bytes(BlobOwner::Profile(alex), &hex(&rows[0].1))?;
    let mut archive = zip::ZipArchive::new(std::io::Cursor::new(zip))?;
    let names: Vec<String> = archive
        .file_names()
        .filter_map(|n| n.ok())
        .map(|n| n.to_string())
        .collect();
    assert_eq!(names, ["CD/DICOMDIR", "CD/DICOM/IM0001", "CD/viewer.exe"]);
    let mut image = Vec::new();
    archive.by_name("CD/DICOM/IM0001")?.read_to_end(&mut image)?;
    assert_eq!(image, b"synthetic image one");
    Ok(())
}

#[tokio::test]
async fn a_folder_holds_at_most_20000_files() -> R {
    let a = imaging_app().await?;
    let files: Vec<(String, Vec<u8>)> = (0..20_001)
        .map(|i| (format!("CD/DICOM/IM{i:05}"), vec![b'x']))
        .collect();
    let (s, v) = post(&a, "alex", "/imaging/import", &[("profile", "alex")], &files).await?;
    assert_eq!(s, StatusCode::PAYLOAD_TOO_LARGE, "{v}");
    assert_eq!(v["detail"]["code"], "too_many_files", "{v}");
    assert!(imports(&a, a.family.profile("alex")).await?.is_empty());
    let left: Vec<_> = std::fs::read_dir(a.rt.settings.blobs_path().join("tmp"))?.collect();
    assert!(left.is_empty(), "{} spooled files left", left.len());
    Ok(())
}

#[tokio::test]
async fn the_general_upload_keeps_its_1000_part_cap() -> R {
    let a = imaging_app().await?;
    let files: Vec<(String, Vec<u8>)> = (0..1001).map(|i| (format!("p{i}.pdf"), b"%PDF-1.4".to_vec())).collect();
    let (s, v) = post(&a, "alex", "/imports", &[("profile", "alex")], &files).await?;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{v}");
    Ok(())
}

#[tokio::test]
async fn a_zip_that_does_not_open_is_refused() -> R {
    let a = imaging_app().await?;
    let mut zip = zip_of(&[("DICOM/IM1", b"one")])?;
    zip.truncate(zip.len() - 10);
    let (s, v) = post(
        &a,
        "alex",
        "/imaging/import",
        &[("profile", "alex")],
        &[("cd.zip".into(), zip)],
    )
    .await?;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{v}");
    assert_eq!(v["detail"]["code"], "invalid_zip", "{v}");
    assert!(imports(&a, a.family.profile("alex")).await?.is_empty());
    Ok(())
}

#[tokio::test]
async fn only_someone_who_may_import_on_the_profile_imports_and_only_with_imaging_on() -> R {
    let a = imaging_app().await?;
    // sam is a viewer on alex, and has no grant on carmen
    let (s, v) = post(&a, "sam", "/imaging/import", &[("profile", "alex")], &disc()).await?;
    assert_eq!(s, StatusCode::FORBIDDEN, "{v}");
    let (s, v) = post(&a, "sam", "/imaging/import", &[("profile", "carmen")], &disc()).await?;
    assert_eq!(s, StatusCode::NOT_FOUND, "{v}");
    let (s, v) = post(&a, "alex", "/imaging/import", &[("profile", "alex")], &[]).await?;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{v}");
    let n = a.count("select count(*) from imaging_import").await?;
    assert_eq!(n, 0);
    // imaging off: nothing is read or kept
    let off = app().await?;
    let (s, v) = post(&off, "alex", "/imaging/import", &[("profile", "alex")], &disc()).await?;
    assert_eq!(s, StatusCode::SERVICE_UNAVAILABLE, "{v}");
    assert_eq!(v["detail"]["code"], "imaging_off", "{v}");
    Ok(())
}

/// Narrows `who`'s grant on `profile` to these categories.
async fn narrow(a: &App, who: &str, profile: &str, categories: &[&str]) -> R {
    sqlx::query("update profile_grant set categories = $3 where user_id = $1 and profile_id = $2")
        .bind(a.family.user(who))
        .bind(a.family.profile(profile))
        .bind(categories.iter().map(|c| c.to_string()).collect::<Vec<_>>())
        .execute(&a.rt.pool)
        .await?;
    Ok(())
}

#[tokio::test]
async fn a_held_study_shows_in_review_only_to_those_who_may_read_records() -> R {
    let a = imaging_app().await?;
    let payload = json!({"import_id": Uuid::new_v4(), "study_uid": "1.2.826.0.1.3680043.10.999.41.5",
        "reason": "identity_mismatch", "patient_name": "ORTEGA CARMEN", "patient_birth_date": "1955-01-30",
        "modality": "MR", "study_date": "2026-09-14", "description": "RM CEREBRO", "series": 2,
        "instances": 9, "filename": "cd.zip"});
    let id = item(&a, "carmen", "imaging_study", Some(payload), json!({})).await?;
    // alex is carmen's caregiver: with read:records he reviews the study
    let (s, listed) = a.get("alex", "/review?profile=carmen").await?;
    assert_eq!(s, StatusCode::OK, "{listed}");
    assert!(listed.to_string().contains(&id.to_string()), "{listed}");
    // without it, the item is withheld like the study itself
    narrow(&a, "alex", "carmen", &["read:summary", "read:labs"]).await?;
    let (s, listed) = a.get("alex", "/review?profile=carmen").await?;
    assert_eq!(s, StatusCode::OK, "{listed}");
    assert!(!listed.to_string().contains(&id.to_string()), "{listed}");
    assert!(!listed.to_string().contains("ORTEGA"), "{listed}");
    let (s, one) = a.get("alex", &format!("/review/{id}")).await?;
    assert_eq!(s, StatusCode::NOT_FOUND, "{one}");
    let (s, v) = a.post("alex", &format!("/review/{id}/accept"), Some(json!({}))).await?;
    assert_eq!(s, StatusCode::NOT_FOUND, "{v}");
    let (s, v) = a.post("alex", &format!("/review/{id}/dismiss"), None).await?;
    assert_eq!(s, StatusCode::NOT_FOUND, "{v}");
    Ok(())
}

#[tokio::test]
async fn a_zip_of_more_files_than_the_import_reads_is_refused_before_it_is_kept() -> R {
    let a = imaging_app().await?;
    let mut out = std::io::Cursor::new(Vec::new());
    {
        let mut zip = zip::ZipWriter::new(&mut out);
        let opts = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
        for i in 0..=ownhealth_imaging::import::MAX_ENTRIES {
            zip.start_file(format!("{i}"), opts)?;
        }
        zip.finish()?;
    }
    let (s, v) = post(
        &a,
        "alex",
        "/imaging/import",
        &[("profile", "alex")],
        &[("big.zip".into(), out.into_inner())],
    )
    .await?;
    assert_eq!(s, StatusCode::PAYLOAD_TOO_LARGE, "{v}");
    assert_eq!(v["detail"]["code"], "too_many_files", "{v}");
    assert!(imports(&a, a.family.profile("alex")).await?.is_empty());
    assert_eq!(
        a.count("select count(*) from job where kind = 'import.dicom'").await?,
        0
    );
    Ok(())
}

#[tokio::test]
async fn a_dicom_import_job_shows_its_file_name_and_counts_only_to_those_who_may_read_records() -> R {
    let a = imaging_app().await?;
    let (s, v) = post(&a, "alex", "/imaging/import", &[("profile", "alex")], &disc()).await?;
    assert_eq!(s, StatusCode::OK, "{v}");
    let job = v["job_id"].as_i64().ok_or_else(|| anyhow::anyhow!("no job: {v}"))?;
    sqlx::query("update job set result = '{\"imported\": 1}'::jsonb where id = $1")
        .bind(job)
        .execute(&a.rt.pool)
        .await?;
    // sam views alex's labs only
    narrow(&a, "sam", "alex", &["read:summary", "read:labs"]).await?;
    let (s, jobs) = a.get("sam", &format!("/jobs?ids={job}")).await?;
    assert_eq!(s, StatusCode::OK, "{jobs}");
    let seen = &jobs["jobs"][0];
    assert_eq!(seen["kind"], "import.dicom", "{jobs}");
    assert_eq!(seen["filename"], Value::Null, "{jobs}");
    assert_eq!(seen["result"], Value::Null, "{jobs}");
    // alex reads records: he sees both
    let (_, jobs) = a.get("alex", &format!("/jobs?ids={job}")).await?;
    assert_eq!(jobs["jobs"][0]["filename"], "CD.zip", "{jobs}");
    assert_eq!(jobs["jobs"][0]["result"]["imported"], 1, "{jobs}");
    Ok(())
}

#[tokio::test]
async fn a_single_file_upload_is_named_after_the_file() -> R {
    let a = imaging_app().await?;
    let (s, v) = post(
        &a,
        "alex",
        "/imaging/import",
        &[("profile", "alex")],
        &[("IM0001".into(), b"synthetic image".to_vec())],
    )
    .await?;
    assert_eq!(s, StatusCode::OK, "{v}");
    let rows = imports(&a, a.family.profile("alex")).await?;
    assert_eq!(rows[0].2.as_deref(), Some("IM0001"));
    Ok(())
}
