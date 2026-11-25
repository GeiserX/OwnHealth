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

/// The account profile of a real export (`Your Profile/Profile.csv`); every field but the zone is
/// synthetic or null, and only the zone is read.
fn profile(zone: &str) -> String {
    format!(
        "id,full_name,first_name,last_name,display_name_setting,display_name,username,email_address,\
         date_of_birth,child,country,state,city,timezone,locale,member_since\n\
         ABC123,Lucía Rivera,Lucía,Rivera,NAME,Lucía,null,null,null,false,ES,null,null,{zone},es_ES,2019-03-02\n"
    )
}

#[tokio::test]
async fn legacy_heart_rate_json_is_utc_and_the_profile_csv_gives_the_local_offset() -> R {
    let env = Env::new().await?;
    let zone = profile("Europe/Madrid");
    let (state, error, result) = upload(
        &env,
        &[
            ("Takeout/Fitbit/Your Profile/Profile.csv", &zone),
            (
                "Takeout/Fitbit/Global Export Data/heart_rate-2024-07-04.json",
                r#"[{"dateTime":"07/04/24 06:10:00","value":{"bpm":72,"confidence":3}}]"#,
            ),
            (
                "Takeout/Fitbit/Global Export Data/resting_heart_rate-2024-07-04.json",
                r#"[{"dateTime":"07/04/24 00:00:00","value":{"date":"07/04/24","value":55.4,"error":6.8}}]"#,
            ),
            (
                "Takeout/Fitbit/Heart Rate Variability/Heart Rate Variability Details - 2024-07-04.csv",
                "timestamp,rmssd,coverage,low_frequency,high_frequency\n2024-07-04T01:15:00,43.5,0.9,900.1,300.2\n",
            ),
            (
                "Takeout/Fitbit/Heart Rate Variability/Daily Heart Rate Variability Summary - 2024-07-(4).csv",
                "timestamp,rmssd,nremhr,entropy\n2024-07-04T00:00:00,41.2,55.1,3.1\n",
            ),
        ],
    )
    .await?;
    assert_eq!(state, "done", "{error}");
    assert_readings(
        &env,
        result,
        &[
            // heart rate in Global Export Data is UTC; the zone only gives the local offset
            ("hr", 72.0, "2024-07-04T06:10:00Z", 120),
            // daily values keep their calendar day
            ("hr_resting", 55.4, "2024-07-04T00:00:00Z", 0),
            // the HRV detail rows are local times
            ("hrv_rmssd", 43.5, "2024-07-03T23:15:00Z", 120),
            ("hrv_rmssd", 41.2, "2024-07-04T00:00:00Z", 0),
        ],
    )
    .await
}

#[tokio::test]
async fn one_reading_in_both_layouts_of_one_export_is_stored_once() -> R {
    let env = Env::new().await?;
    let zone = profile("Europe/Madrid");
    let root = "Takeout/Google Health";
    let (state, error, result) = upload(
        &env,
        &[
            (&format!("{root}/Your Profile/Profile.csv"), &zone),
            (
                &format!("{root}/Global Export Data/heart_rate-2024-07-12.json"),
                r#"[{"dateTime":"07/12/24 08:00:00","value":{"bpm":70,"confidence":3}}]"#,
            ),
            (
                &format!("{root}/Physical Activity_GoogleData/Heart Rate/heart_rate_2024-07-01.csv"),
                "timestamp,beats per minute,data source\n2024-07-12T08:00:00Z,70.0,Google Fitbit Air\n",
            ),
            (
                &format!("{root}/Global Export Data/resting_heart_rate-2024-07-12.json"),
                r#"[{"dateTime":"07/12/24 00:00:00","value":{"date":"07/12/24","value":55.0,"error":6.8}}]"#,
            ),
            (
                &format!("{root}/Physical Activity_GoogleData/Daily Resting Heart Rate/daily_resting_heart_rate.csv"),
                "timestamp,beats per minute,data source\n2024-07-12T00:00:00Z,55.0,Google Health App\n",
            ),
        ],
    )
    .await?;
    assert_eq!(state, "done", "{error}");
    assert_readings(
        &env,
        result,
        &[
            ("hr", 70.0, "2024-07-12T08:00:00Z", 120),
            ("hr_resting", 55.0, "2024-07-12T00:00:00Z", 0),
        ],
    )
    .await
}

#[tokio::test]
async fn a_clock_change_or_an_unreadable_time_leaves_out_the_row_not_the_zip() -> R {
    let env = Env::new().await?;
    let zone = profile("Europe/Madrid");
    let (state, error, result) = upload(
        &env,
        &[
            ("Takeout/Fitbit/Your Profile/Profile.csv", &zone),
            (
                "Takeout/Fitbit/Heart Rate Variability/Heart Rate Variability Details - 2024-10-27.csv",
                // 02:30 happens twice on 27 October in Madrid and never on 31 March
                "timestamp,rmssd,coverage,low_frequency,high_frequency\n\
                 2024-10-27T01:30:00,40.0,0.9,1,1\n\
                 2024-10-27T02:30:00,41.0,0.9,1,1\n\
                 2024-03-31T02:30:00,42.0,0.9,1,1\n\
                 yesterday,43.0,0.9,1,1\n",
            ),
            (
                "Takeout/Fitbit/Physical Activity_GoogleData/Heart Rate/heart_rate_2024-10-01.csv",
                "timestamp,beats per minute,data source\n2024-10-27T06:10,74,watch\n2024-10-27T06:11:00Z,75,watch\n",
            ),
        ],
    )
    .await?;
    assert_eq!(state, "done", "{error}");
    assert_eq!(result["fitbit_takeout"]["rows_left_out"], 4, "{result}");
    assert_readings(
        &env,
        result,
        &[
            ("hr", 75.0, "2024-10-27T06:11:00Z", 60),
            ("hrv_rmssd", 40.0, "2024-10-26T23:30:00Z", 120),
        ],
    )
    .await
}

#[tokio::test]
async fn without_a_profile_utc_times_import_and_local_times_are_left_out() -> R {
    let env = Env::new().await?;
    let (state, error, result) = upload(
        &env,
        &[
            (
                "Takeout/Fitbit/Global Export Data/heart_rate-2024-07-04.json",
                r#"[{"dateTime":"07/04/24 08:10:00","value":{"bpm":72}}]"#,
            ),
            (
                "Takeout/Fitbit/Heart Rate Variability/Heart Rate Variability Details - 2024-07-04.csv",
                "timestamp,rmssd,coverage,low_frequency,high_frequency\n2024-07-04T01:15:00,43.5,0.9,1,1\n",
            ),
        ],
    )
    .await?;
    assert_eq!(state, "done", "{error}");
    assert_eq!(result["fitbit_takeout"]["rows_left_out"], 1, "{result}");
    assert_readings(&env, result, &[("hr", 72.0, "2024-07-04T08:10:00Z", 0)]).await
}

#[tokio::test]
async fn a_utc_time_takes_the_offset_of_the_profile_zone_and_a_written_offset_stays() -> R {
    let env = Env::new().await?;
    let zone = profile("Europe/Madrid");
    let (state, error, result) = upload(
        &env,
        &[
            ("Takeout/Google Health/Your Profile/Profile.csv", &zone),
            (
                "Takeout/Google Health/Physical Activity_GoogleData/Heart Rate/heart_rate_2026-11-01.csv",
                // 23:30 UTC is 00:30 on 14 November in Madrid; the second row says where it was taken
                "timestamp,beats per minute,data source\n2026-11-13T23:30:00Z,61,watch\n2026-11-13T23:40:00-05:00,62,watch\n",
            ),
        ],
    )
    .await?;
    assert_eq!(state, "done", "{error}");
    assert_readings(
        &env,
        result,
        &[
            ("hr", 61.0, "2026-11-13T23:30:00Z", 60),
            ("hr", 62.0, "2026-11-14T04:40:00Z", -300),
        ],
    )
    .await?;
    let days: Vec<(chrono::NaiveDate,)> = sqlx::query_as(
        "select (t at time zone 'UTC')::date from wearable_rollup where profile_id = $1 and metric = 'hr' \
         and bucket = 'day' order by t",
    )
    .bind(env.fam.profile("lucia"))
    .fetch_all(&env.db.pool)
    .await?;
    assert_eq!(days, [("2026-11-13".parse()?,), ("2026-11-14".parse()?,)]);
    Ok(())
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
async fn an_unknown_fitbit_layout_fails_without_writing_samples() -> R {
    let env = Env::new().await?;
    for (path, data, code) in [(
        "Takeout/Fitbit/Google Data/new-vitals.csv",
        "time,value\n",
        "fitbit_takeout_unknown_layout",
    )] {
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
