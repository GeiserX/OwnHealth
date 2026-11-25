//! Synthetic Fitbit Takeout ZIPs through the upload dispatcher and the real worker.
mod common;

use std::io::{Cursor, Write};

use chrono::{DateTime, Utc};
use common::{Env, R};
use ownhealth_core::blobs::BlobOwner;
use ownhealth_core::documents::{StoredFile, dispatch_upload};
use ownhealth_core::jobs;
use serde_json::Value;
use uuid::Uuid;
use zip::write::SimpleFileOptions;

fn archive(members: &[(&str, &str)]) -> anyhow::Result<Vec<u8>> {
    let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
    for (name, contents) in members {
        zip.start_file(*name, SimpleFileOptions::default())?;
        zip.write_all(contents.as_bytes())?;
    }
    Ok(zip.finish()?.into_inner())
}

async fn upload(env: &Env, members: &[(&str, &str)]) -> anyhow::Result<(String, String, Value)> {
    let bytes = archive(members)?;
    let profile = env.fam.profile("lucia");
    let hash = env.blobs.put_bytes(BlobOwner::Profile(profile), &bytes)?;
    let mut db = env.db.pool.acquire().await?;
    let out = dispatch_upload(
        &mut db,
        &env.blobs,
        env.fam.household_id,
        profile,
        &[StoredFile {
            filename: "takeout.zip".into(),
            sha256: hash.clone(),
            size: bytes.len() as i64,
        }],
        None,
    )
    .await?;
    let item = &out["items"][0];
    assert_eq!(item["kind"], "fitbit_takeout", "{out}");
    let id = item["job_id"].as_i64().expect("Fitbit import job");
    jobs::drain(&env.ctx, "core", 10, None).await?;
    let (state, error, result): (String, Option<String>, Option<Value>) =
        sqlx::query_as("select state, last_error, result from job where id = $1")
            .bind(id)
            .fetch_one(&env.db.pool)
            .await?;
    Ok((state, error.unwrap_or_default(), result.unwrap_or_default()))
}

async fn assert_readings(env: &Env, result: Value, expected: &[(&str, f32, &str, i16)]) -> R {
    assert_eq!(result["accepted"], expected.len(), "{result}");
    let doc: Uuid = result["document_id"].as_str().expect("document id").parse()?;
    type Point = (String, f32, DateTime<Utc>, Option<i16>, String);
    let points: Vec<Point> = sqlx::query_as(
        "select s.metric, s.v, s.t, s.tz_offset_min, w.provider from wearable_sample s \
         join wearable_source w on w.id = s.source_id and w.profile_id = s.profile_id \
         where s.document_id = $1 order by s.metric, s.t",
    )
    .bind(doc)
    .fetch_all(&env.db.pool)
    .await?;
    let expected: Vec<_> = expected
        .iter()
        .map(|(metric, value, time, offset)| {
            (
                (*metric).to_string(),
                *value,
                time.parse().expect("fixture timestamp"),
                Some(*offset),
                "fitbit_takeout".to_string(),
            )
        })
        .collect();
    assert_eq!(points, expected);
    Ok(())
}

#[tokio::test]
async fn legacy_json_uses_the_export_profile_zone() -> R {
    let env = Env::new().await?;
    let (state, error, result) = upload(
        &env,
        &[
            (
                "Takeout/Fitbit/Global Export Data/heart_rate-2024-07-04.json",
                r#"[{"dateTime":"07/04/24 08:10:15","value":{"bpm":72,"confidence":3}}]"#,
            ),
            (
                "Takeout/Fitbit/Global Export Data/resting_heart_rate-2024-07-04.json",
                r#"[{"dateTime":"07/04/24 00:00:00","value":{"date":"2024-07-04","value":55.4}}]"#,
            ),
            (
                "Takeout/Fitbit/Global Export Data/heart_rate_variability-2024-07-04.json",
                r#"[{"dateTime":"07/04/24 01:15:00","value":{"rmssd":43.5,"sdnn":90}}]"#,
            ),
            (
                "Takeout/Fitbit/Global Export Data/spo2-2024-07-04.json",
                r#"[{"dateTime":"07/04/24 02:00:00","value":{"avg":97.5,"min":90}}]"#,
            ),
            (
                "Takeout/Fitbit/Profile/Profile.json",
                r#"{"profile":{"timezone":"Europe/Madrid"}}"#,
            ),
        ],
    )
    .await?;
    assert_eq!(state, "done", "{error}");
    assert_readings(
        &env,
        result,
        &[
            ("hr", 72.0, "2024-07-04T06:10:15Z", 120),
            ("hr_resting", 55.4, "2024-07-03T22:00:00Z", 120),
            ("hrv_rmssd", 43.5, "2024-07-03T23:15:00Z", 120),
            ("spo2", 97.5, "2024-07-04T00:00:00Z", 120),
        ],
    )
    .await
}

#[tokio::test]
async fn google_csv_keeps_explicit_offsets_and_projects_rmssd() -> R {
    let env = Env::new().await?;
    let (state, error, result) = upload(&env, &[
        ("Takeout/Fitbit/Physical Activity_GoogleData/Heart Rate/2024-07.csv", "timestamp,beats per minute,data source\n2024-07-04T08:10:15+02:00,74,watch\n"),
        ("Takeout/Fitbit/Physical Activity_GoogleData/Daily Resting Heart Rate/2024-07.csv", "date,beats per minute,data source\n2024-07-04,54,watch\n"),
        ("Takeout/Fitbit/Physical Activity_GoogleData/Heart Rate Variability/2024-07.csv", "timestamp,root mean square of successive differences milliseconds,standard deviation milliseconds,data source\n2024-07-04T01:15:00Z,44.1,90,watch\n"),
        ("Takeout/Fitbit/Physical Activity_GoogleData/Oxygen Saturation/2024-07.csv", "timestamp,oxygen saturation percentage,data source\n2024-07-04T02:00:00Z,96.6,watch\n"),
    ]).await?;
    assert_eq!(state, "done", "{error}");
    assert_readings(
        &env,
        result,
        &[
            ("hr", 74.0, "2024-07-04T06:10:15Z", 120),
            ("hr_resting", 54.0, "2024-07-04T00:00:00Z", 0),
            ("hrv_rmssd", 44.1, "2024-07-04T01:15:00Z", 0),
            ("spo2", 96.6, "2024-07-04T02:00:00Z", 0),
        ],
    )
    .await
}

#[tokio::test]
async fn unknown_fitbit_layout_and_missing_profile_zone_fail_without_writing_samples() -> R {
    let env = Env::new().await?;
    for (path, data, code) in [
        (
            "Takeout/Fitbit/Google Data/new-vitals.csv",
            "time,value\n",
            "fitbit_takeout_unknown_layout",
        ),
        (
            "Takeout/Fitbit/Global Export Data/heart_rate-2024-07-04.json",
            r#"[{"dateTime":"07/04/24 08:10:15","value":{"bpm":72}}]"#,
            "fitbit_takeout_timezone_required",
        ),
    ] {
        let (state, error, result) = upload(&env, &[(path, data)]).await?;
        assert_eq!(state, "dead", "{error}");
        assert_eq!(error, format!("PermanentJobError: {code}"));
        assert!(result.is_null());
    }
    let count: i64 = sqlx::query_scalar("select count(*) from wearable_sample")
        .fetch_one(&env.db.pool)
        .await?;
    assert_eq!(count, 0);
    Ok(())
}

#[tokio::test]
async fn retraction_restores_overlapping_samples_from_the_other_readings_document() -> R {
    use ownhealth_core::documents::FITBIT_READINGS_MIME;
    use ownhealth_ingest_structured::retract::{lock_export_of, retract_document_facts};

    let env = Env::new().await?;
    let profile = env.fam.profile("lucia");
    let member = "Takeout/Fitbit/Google Data/heart_rate_2024-07-04.csv";
    let first = [(member, "timestamp,bpm\n2024-07-04T08:00:00Z,72\n")];
    let second = [(member, "timestamp,bpm\n2024-07-04T08:00:00Z,74\n")];
    let raw = env.blobs.put_bytes(BlobOwner::Profile(profile), &archive(&first)?)?;
    let (state, error, first) = upload(&env, &first).await?;
    assert_eq!(state, "done", "{error}");
    let first_id: Uuid = first["document_id"].as_str().expect("first document").parse()?;
    assert!(!env.blobs.exists(BlobOwner::Profile(profile), &raw));
    let (state, error, second) = upload(&env, &second).await?;
    assert_eq!(state, "done", "{error}");
    assert_eq!(second["skipped"], 1);
    let second_id: Uuid = second["document_id"].as_str().expect("second document").parse()?;

    let mut tx = env.db.pool.begin().await?;
    lock_export_of(&mut tx, first_id).await?;
    sqlx::query("update document set status = 'retracted' where id = $1")
        .bind(first_id)
        .execute(&mut *tx)
        .await?;
    retract_document_facts(&mut tx, first_id, "retracted").await?;
    tx.commit().await?;
    jobs::drain(&env.ctx, "core", 10, None).await?;

    let (doc, value, mime, sha): (Uuid, f32, String, String) = sqlx::query_as(
        "select d.id, s.v, d.mime, encode(d.blob_sha256, 'hex') from wearable_sample s
         join document d on d.id = s.document_id where s.profile_id = $1",
    )
    .bind(profile)
    .fetch_one(&env.db.pool)
    .await?;
    assert_eq!((doc, value, mime.as_str()), (second_id, 74.0, FITBIT_READINGS_MIME));
    let stored = String::from_utf8(env.blobs.get_bytes(BlobOwner::Profile(profile), &sha)?)?;
    let header: Value = serde_json::from_str(stored.lines().next().expect("header"))?;
    assert_eq!(header["members"][0]["name"], member);
    Ok(())
}
