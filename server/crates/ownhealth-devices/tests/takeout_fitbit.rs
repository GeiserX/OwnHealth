//! Synthetic Fitbit Takeout ZIPs through the upload dispatcher and the real worker. Folder, file and
//! column names follow real exports (fixtures/fitbit/README.md names the sources).
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
    upload_bytes(env, &archive(members)?).await
}

async fn upload_bytes(env: &Env, bytes: &[u8]) -> anyhow::Result<(String, String, Value)> {
    let profile = env.fam.profile("lucia");
    let hash = env.blobs.put_bytes(BlobOwner::Profile(profile), bytes)?;
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
            ("hr", 74.0, "2024-07-04T06:10:00Z", 120),
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

fn omitted(result: &Value) -> Vec<(String, String)> {
    let mut list: Vec<_> = result["fitbit_takeout"]["not_imported"]
        .as_array()
        .expect("not_imported list")
        .iter()
        .map(|o| {
            (
                o["member"].as_str().unwrap_or("").to_string(),
                o["reason"].as_str().unwrap_or("").to_string(),
            )
        })
        .collect();
    list.sort();
    list
}

#[tokio::test]
async fn spo2_in_the_real_oxygen_saturation_folder_is_read() -> R {
    let env = Env::new().await?;
    let folder = "Takeout/Fitbit/Oxygen Saturation (SpO2)";
    let (state, error, result) = upload(
        &env,
        &[
            (
                &format!("{folder}/Daily SpO2 - 2024-07-04-2024-07-05.csv"),
                "timestamp,average_value,lower_bound,upper_bound\n\
                 2024-07-04T00:00:00Z,97.5,94.0,100.0\n2024-07-05T00:00:00Z,96.1,92.6,100.0\n",
            ),
            (
                &format!("{folder}/Minute SpO2 - 2024-07-04.csv"),
                "timestamp,value\n2024-07-04T03:53:35Z,95.0\n",
            ),
        ],
    )
    .await?;
    assert_eq!(state, "done", "{error}");
    assert_eq!(
        omitted(&result),
        [(
            format!("{folder}/Minute SpO2 - 2024-07-04.csv"),
            "unsupported".to_string()
        )]
    );
    assert_readings(
        &env,
        result,
        &[
            ("spo2", 97.5, "2024-07-04T00:00:00Z", 0),
            ("spo2", 96.1, "2024-07-05T00:00:00Z", 0),
        ],
    )
    .await
}

#[tokio::test]
async fn files_that_bring_no_readings_are_listed_with_their_reason() -> R {
    let env = Env::new().await?;
    let zone = profile("Europe/Madrid");
    let (state, error, result) = upload(
        &env,
        &[
            ("Takeout/Fitbit/Your Profile/Profile.csv", &zone),
            (
                "Takeout/Fitbit/Global Export Data/heart_rate-2024-07-04.json",
                r#"[{"dateTime":"07/04/24 08:10:00","value":{"bpm":72}}]"#,
            ),
            (
                "Takeout/Fitbit/Heart Rate Variability/Daily Respiratory Rate Summary - 2024-07-(4).csv",
                "timestamp,daily_respiratory_rate\n2024-07-04T00:00:00,14.2\n",
            ),
            (
                "Takeout/Fitbit/Global Export Data/steps-2024-07-04.json",
                r#"[{"dateTime":"07/04/24 08:10:00","value":"12"}]"#,
            ),
            (
                "Takeout/Fitbit/Global Export Data/heart_rate-2024-07-05.json",
                "not JSON",
            ),
            (
                "Takeout/Fitbit/Physical Activity_GoogleData/Heart Rate/heart_rate_2024-08-01.csv",
                "time,value\n2024-08-01T00:00:00Z,70\n",
            ),
            (
                "Takeout/Fitbit/Global Export Data/resting_heart_rate-2024-07-06.json",
                r#"[{"dateTime":"07/06/24 00:00:00","value":{"date":null,"value":0.0,"error":0.0}}]"#,
            ),
            (
                "Takeout/Google Fit/Daily activity metrics/2024-07-04.csv",
                "Start time,Steps\n",
            ),
        ],
    )
    .await?;
    assert_eq!(state, "done", "{error}");
    let ged = "Takeout/Fitbit/Global Export Data";
    assert_eq!(
        omitted(&result),
        [
            (format!("{ged}/heart_rate-2024-07-05.json"), "invalid_json".to_string()),
            (
                format!("{ged}/resting_heart_rate-2024-07-06.json"),
                "no_readings".to_string()
            ),
            (format!("{ged}/steps-2024-07-04.json"), "unsupported".to_string()),
            (
                "Takeout/Fitbit/Heart Rate Variability/Daily Respiratory Rate Summary - 2024-07-(4).csv".to_string(),
                "unsupported".to_string()
            ),
            (
                "Takeout/Fitbit/Physical Activity_GoogleData/Heart Rate/heart_rate_2024-08-01.csv".to_string(),
                "invalid_csv".to_string()
            ),
        ]
    );
    assert_eq!(result["fitbit_takeout"]["files_imported"], 1);
    assert_readings(&env, result, &[("hr", 72.0, "2024-07-04T08:10:00Z", 120)]).await
}

#[tokio::test]
async fn finder_twins_of_a_zip_made_on_a_mac_are_ignored() -> R {
    let env = Env::new().await?;
    let member = "Takeout/Fitbit/Global Export Data/heart_rate-2024-07-04.json";
    let (state, error, result) = upload(
        &env,
        &[
            (member, r#"[{"dateTime":"07/04/24 08:10:00","value":{"bpm":72}}]"#),
            (
                "__MACOSX/Takeout/Fitbit/Global Export Data/._heart_rate-2024-07-04.json",
                "\u{0}\u{5}\u{16}\u{7}\u{0}\u{2}\u{0}\u{0}Mac OS X",
            ),
            (
                "Takeout/Fitbit/Global Export Data/._heart_rate-2024-07-04.json",
                "\u{0}\u{5}\u{16}\u{7}",
            ),
        ],
    )
    .await?;
    assert_eq!(state, "done", "{error}");
    assert_eq!(omitted(&result), []);
    assert_readings(&env, result, &[("hr", 72.0, "2024-07-04T08:10:00Z", 0)]).await
}

#[tokio::test]
async fn a_zip_with_both_account_folders_reads_both_and_keeps_one_copy() -> R {
    let env = Env::new().await?;
    let zone = profile("Europe/Madrid");
    let (state, error, result) = upload(
        &env,
        &[
            ("Takeout/Fitbit/Your Profile/Profile.csv", &zone),
            (
                "Takeout/Fitbit/Global Export Data/heart_rate-2024-07-12.json",
                r#"[{"dateTime":"07/12/24 08:00:00","value":{"bpm":70}},{"dateTime":"07/12/24 08:05:00","value":{"bpm":71}}]"#,
            ),
            (
                "Takeout/Google Health/Physical Activity_GoogleData/Heart Rate/heart_rate_2024-07-01.csv",
                "timestamp,beats per minute,data source\n2024-07-12T08:00:00Z,70,watch\n2024-07-12T08:10:00Z,72,watch\n",
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
            ("hr", 71.0, "2024-07-12T08:05:00Z", 120),
            ("hr", 72.0, "2024-07-12T08:10:00Z", 120),
        ],
    )
    .await
}

#[tokio::test]
async fn overlapping_heart_rate_members_give_the_same_mean_in_either_order() -> R {
    let legacy = (
        "Takeout/Fitbit/Global Export Data/heart_rate-2024-07-12.json",
        r#"[{"dateTime":"07/12/24 08:00:05","value":{"bpm":70}},
            {"dateTime":"07/12/24 08:00:35","value":{"bpm":76}},
            {"dateTime":"07/12/24 08:01:05","value":{"bpm":60}},
            {"dateTime":"07/12/24 08:01:35","value":{"bpm":62}}]"#,
    );
    let google = (
        "Takeout/Google Health/Physical Activity_GoogleData/Heart Rate/2024-07.csv",
        "timestamp,beats per minute\n2024-07-12T08:00:05Z,70\n2024-07-12T08:00:45Z,82\n2024-07-12T08:01:05Z,90\n",
    );
    let mut orders = Vec::new();
    for members in [[legacy, google], [google, legacy]] {
        let env = Env::new().await?;
        let (state, error, result) = upload(&env, &members).await?;
        assert_eq!(state, "done", "{error}");
        assert_eq!(result["accepted"], 2, "{result}");
        assert_eq!(result["fitbit_takeout"]["files_imported"], 2, "{result}");
        let points: Vec<(DateTime<Utc>, f32)> =
            sqlx::query_as("select t, v from wearable_sample where metric = 'hr' order by t")
                .fetch_all(&env.db.pool)
                .await?;
        orders.push(points);
    }
    // At a shared instant, the first path in alphabetical order supplies the reading.
    let expected = vec![
        ("2024-07-12T08:00:00Z".parse()?, 76.0),
        ("2024-07-12T08:01:00Z".parse()?, 61.0),
    ];
    assert_eq!(orders, [expected.clone(), expected]);
    Ok(())
}

#[tokio::test]
async fn heart_rate_is_kept_as_one_reading_per_minute() -> R {
    let env = Env::new().await?;
    // Fitbit records heart rate every few seconds. As with a FIT file, each minute keeps the mean
    // of its readings, rounded to a whole beat.
    let (state, error, result) = upload(
        &env,
        &[(
            "Takeout/Fitbit/Global Export Data/heart_rate-2024-07-04.json",
            r#"[{"dateTime":"07/04/24 08:10:05","value":{"bpm":70}},{"dateTime":"07/04/24 08:10:25","value":{"bpm":71}},
                {"dateTime":"07/04/24 08:10:45","value":{"bpm":73}},{"dateTime":"07/04/24 08:11:00","value":{"bpm":80}}]"#,
        )],
    )
    .await?;
    assert_eq!(state, "done", "{error}");
    assert_readings(
        &env,
        result,
        &[
            ("hr", 71.0, "2024-07-04T08:10:00Z", 0),
            ("hr", 80.0, "2024-07-04T08:11:00Z", 0),
        ],
    )
    .await
}

/// A Google Data heart rate CSV with `per_minute` rows in each of `minutes` minutes.
fn heart_rate_csv(minutes: i64, per_minute: i64, source: &str) -> String {
    use std::fmt::Write as _;
    let start = chrono::DateTime::parse_from_rfc3339("2023-01-01T00:00:00Z")
        .expect("start")
        .to_utc();
    let mut rows = String::from("timestamp,beats per minute,data source\n");
    for minute in 0..minutes {
        for second in 0..per_minute {
            let t = start + chrono::Duration::seconds(minute * 60 + second * 5);
            writeln!(
                rows,
                "{},70,{source}",
                t.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
            )
            .expect("row");
        }
    }
    rows
}

#[tokio::test]
async fn the_reading_limit_counts_minutes_so_a_long_history_fits() -> R {
    let env = Env::new().await?;
    let member = "Takeout/Fitbit/Physical Activity_GoogleData/Heart Rate/heart_rate_2023-01-01.csv";
    // 500,010 rows, more than the limit, in 50,001 minutes.
    let (state, error, result) = upload(&env, &[(member, &heart_rate_csv(50_001, 10, ""))]).await?;
    assert_eq!(state, "done", "{error}");
    assert_eq!(result["accepted"], 50_001, "{result}");
    // 500,001 distinct minutes across two files refuse the ZIP before anything is written.
    let (state, error, _) = upload(
        &env,
        &[
            (member, &heart_rate_csv(500_000, 1, "")),
            (
                "Takeout/Fitbit/Google Data/Heart Rate/2024-07.csv",
                "timestamp,bpm\n2024-07-12T08:00:00Z,70\n",
            ),
        ],
    )
    .await?;
    assert_eq!(
        (state.as_str(), error.as_str()),
        ("dead", "PermanentJobError: fitbit_takeout_limit")
    );
    let count: i64 = sqlx::query_scalar("select count(*) from wearable_sample")
        .fetch_one(&env.db.pool)
        .await?;
    assert_eq!(count, 50_001);
    Ok(())
}

#[tokio::test]
async fn duplicate_minutes_only_count_once_toward_the_reading_limit() -> R {
    let env = Env::new().await?;
    let csv = heart_rate_csv(250_001, 1, "");
    let (state, error, result) = upload(
        &env,
        &[
            (
                "Takeout/Fitbit/Physical Activity_GoogleData/Heart Rate/2023-01.csv",
                &csv,
            ),
            (
                "Takeout/Google Health/Physical Activity_GoogleData/Heart Rate/2023-01.csv",
                &csv,
            ),
        ],
    )
    .await?;
    assert_eq!(state, "done", "{error}");
    assert_eq!(result["accepted"], 250_001, "{result}");
    assert_eq!(result["fitbit_takeout"]["files_imported"], 2, "{result}");
    Ok(())
}

#[tokio::test]
async fn a_dense_month_of_google_data_imports_above_the_json_byte_limit() -> R {
    let env = Env::new().await?;
    let minutes = 31 * 24 * 60;
    let csv = heart_rate_csv(minutes, 12, "Google Fitbit Air");
    assert!(csv.len() > 16 * 1024 * 1024);
    let (state, error, result) = upload(
        &env,
        &[(
            "Takeout/Google Health/Physical Activity_GoogleData/Heart Rate/2023-01.csv",
            &csv,
        )],
    )
    .await?;
    assert_eq!(state, "done", "{error}");
    assert_eq!(result["accepted"], minutes, "{result}");
    Ok(())
}

#[tokio::test]
async fn an_invalid_heart_rate_csv_contributes_no_partial_minute() -> R {
    let env = Env::new().await?;
    let broken = "Takeout/Fitbit/Google Data/Heart Rate/2024-07.csv";
    let (state, error, result) = upload(
        &env,
        &[
            (
                broken,
                "\u{feff}timestamp,bpm\n2024-07-12T08:00:05Z,100\n2024-07-12T08:00:35Z\n",
            ),
            (
                "Takeout/Google Health/Google Data/Heart Rate/2024-07.csv",
                "\u{feff}timestamp,bpm\n2024-07-12T08:00:15Z,70\n",
            ),
        ],
    )
    .await?;
    assert_eq!(state, "done", "{error}");
    assert_eq!(omitted(&result), [(broken.to_string(), "invalid_csv".to_string())]);
    assert_readings(&env, result, &[("hr", 70.0, "2024-07-12T08:00:00Z", 0)]).await
}

#[tokio::test]
async fn values_out_of_range_or_in_the_future_are_not_readings() -> R {
    let env = Env::new().await?;
    let future = (chrono::Utc::now() + chrono::Duration::days(3)).to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let data = "Takeout/Fitbit/Physical Activity_GoogleData";
    let (state, error, result) = upload(
        &env,
        &[
            (
                &format!("{data}/Oxygen Saturation/oxygen_saturation_2024-07-01.csv"),
                "timestamp,oxygen saturation percentage\n2024-07-04T02:00:00Z,100.5\n\
                 2024-07-04T02:01:00Z,0\n2024-07-04T02:02:00Z,97\n",
            ),
            (
                &format!("{data}/Heart Rate/heart_rate_2024-07-01.csv"),
                &format!(
                    "timestamp,beats per minute\n2024-07-04T08:00:00Z,300\n2024-07-04T08:01:00Z,20\n\
                     {future},70\n2024-07-04T08:02:00Z,70\n"
                ),
            ),
        ],
    )
    .await?;
    assert_eq!(state, "done", "{error}");
    assert_readings(
        &env,
        result,
        &[
            ("hr", 70.0, "2024-07-04T08:02:00Z", 0),
            ("spo2", 97.0, "2024-07-04T02:02:00Z", 0),
        ],
    )
    .await
}

#[tokio::test]
async fn a_profile_without_a_usable_zone_does_not_stop_times_with_an_offset() -> R {
    let env = Env::new().await?;
    let heart = (
        "Takeout/Fitbit/Physical Activity_GoogleData/Heart Rate/heart_rate_2024-07-01.csv",
        "timestamp,beats per minute\n2024-07-04T08:00:00+02:00,70\n",
    );
    for profile in [
        "\"not,csv\n\u{0}",
        "id,timezone\nABC123,Mars/Olympus\n",
        "id,city\nABC123,Murcia\n",
    ] {
        let (state, error, result) =
            upload(&env, &[("Takeout/Fitbit/Your Profile/Profile.csv", profile), heart]).await?;
        assert_eq!(state, "done", "{profile}: {error}");
        assert_eq!(
            result["fitbit_takeout"]["not_imported"],
            serde_json::json!([]),
            "{result}"
        );
    }
    let points: Vec<(chrono::DateTime<chrono::Utc>, Option<i16>)> =
        sqlx::query_as("select t, tz_offset_min from wearable_sample")
            .fetch_all(&env.db.pool)
            .await?;
    assert_eq!(points, [("2024-07-04T06:00:00Z".parse()?, Some(120))]);
    Ok(())
}

#[tokio::test]
async fn a_fitbit_zip_from_an_inbox_takes_its_own_lock_and_is_seen_once() -> R {
    use ownhealth_core::arrive::{Arrival, Arrived, arrive};
    use ownhealth_core::documents::{FITBIT_READINGS_MIME, lock_exports};
    use sqlx::Acquire;

    let env = Env::new().await?;
    let profile = env.fam.profile("lucia");
    let owner = BlobOwner::Profile(profile);
    let inbox = env.source("folder", Some("lucia"), serde_json::json!({}), None).await?;
    let member = "Takeout/Fitbit/Global Export Data/heart_rate-2024-07-04.json";
    let bytes = archive(&[(member, r#"[{"dateTime":"07/04/24 08:10:00","value":{"bpm":72}}]"#)])?;
    let sha = env.blobs.put_bytes(owner, &bytes)?;
    let arrival = Arrival {
        source_id: Some(inbox.id),
        external_id: Some("first-arrival"),
        filename: Some("takeout.zip"),
        ..Arrival::new(env.fam.household_id, profile, "folder", &sha, bytes.len() as i64)
    };

    // While a Fitbit import of the profile holds its lock, a new arrival waits for it.
    let mut holder = env.db.pool.begin().await?;
    lock_exports(&mut holder, profile, "fitbit_takeout").await?;
    let mut c = env.db.pool.acquire().await?;
    let mut tx = c.begin().await?;
    sqlx::query("set local lock_timeout = '200ms'")
        .execute(&mut *tx)
        .await?;
    assert!(
        arrive(&mut tx, &env.blobs, &arrival).await.is_err(),
        "the arrival did not wait for the Fitbit lock"
    );
    drop(tx);
    holder.rollback().await?;

    let mut tx = c.begin().await?;
    let Arrived::Queued { job_id: Some(id), .. } = arrive(&mut tx, &env.blobs, &arrival).await? else {
        anyhow::bail!("the inbox arrival did not enqueue the import");
    };
    tx.commit().await?;
    jobs::drain(&env.ctx, "core", 10, None).await?;
    let result: Value = sqlx::query_scalar("select result from job where id = $1 and state = 'done'")
        .bind(id)
        .fetch_one(&env.db.pool)
        .await?;
    let doc: Uuid = result["document_id"].as_str().expect("document").parse()?;
    let channels: Vec<(String, String)> = sqlx::query_as(
        "select d.mime, s.channel from document d join document_source s on s.document_id = d.id \
         where d.profile_id = $1 and d.doc_type = 'wearables'",
    )
    .bind(profile)
    .fetch_all(&env.db.pool)
    .await?;
    assert_eq!(channels, [(FITBIT_READINGS_MIME.to_string(), "folder".to_string())]);
    assert!(!env.blobs.exists(owner, &sha), "raw ZIP still exists");

    // The same ZIP under a new inbox id is the document already made, and its bytes go again.
    env.blobs.put_bytes(owner, &bytes)?;
    let duplicate = Arrival {
        external_id: Some("second-arrival"),
        ..arrival
    };
    let mut tx = c.begin().await?;
    assert_eq!(
        arrive(&mut tx, &env.blobs, &duplicate).await?,
        Arrived::Duplicate { document_id: doc }
    );
    tx.commit().await?;
    assert!(!env.blobs.exists(owner, &sha), "the duplicate stored the raw ZIP again");
    Ok(())
}

#[tokio::test]
async fn a_garmin_run_never_reads_a_fitbit_readings_document() -> R {
    let env = Env::new().await?;
    let (state, error, result) = upload(
        &env,
        &[(
            "Takeout/Fitbit/Global Export Data/heart_rate-2024-07-04.json",
            r#"[{"dateTime":"07/04/24 08:10:00","value":{"bpm":72}}]"#,
        )],
    )
    .await?;
    assert_eq!(state, "done", "{error}");
    let doc = result["document_id"].as_str().expect("document");
    let mut c = env.db.pool.acquire().await?;
    let id = jobs::enqueue(
        &mut c,
        ownhealth_devices::garmin_zip::JOB,
        &serde_json::json!({"document_id": doc, "profile_id": env.fam.profile("lucia").to_string()}),
        jobs::EnqueueOptions::default(),
    )
    .await?
    .expect("job");
    drop(c);
    jobs::drain(&env.ctx, "core", 10, None).await?;
    let (state, error): (String, Option<String>) = sqlx::query_as("select state, last_error from job where id = $1")
        .bind(id)
        .fetch_one(&env.db.pool)
        .await?;
    assert_eq!(
        (state.as_str(), error.as_deref()),
        ("dead", Some("PermanentJobError: the Garmin Connect document is gone"))
    );
    Ok(())
}

#[tokio::test]
async fn the_committed_fixtures_import_four_readings_each() -> R {
    let env = Env::new().await?;
    let fixtures = concat!(env!("CARGO_MANIFEST_DIR"), "/../../../fixtures/fitbit");
    for (file, expected, omitted_files) in [
        (
            "legacy.zip",
            [
                ("hr", 72.0, "2024-07-04T06:10:00Z", 120),
                ("hr_resting", 55.4, "2024-07-04T00:00:00Z", 0),
                ("hrv_rmssd", 43.5, "2024-07-03T23:15:00Z", 120),
                ("spo2", 97.5, "2024-07-04T00:00:00Z", 0),
            ],
            vec!["Takeout/Fitbit/Oxygen Saturation (SpO2)/Minute SpO2 - 2024-07-04.csv"],
        ),
        (
            "google-data.zip",
            [
                ("hr", 74.0, "2024-07-05T06:10:00Z", 120),
                ("hr_resting", 54.0, "2024-07-05T00:00:00Z", 0),
                ("hrv_rmssd", 44.1, "2024-07-05T00:00:00Z", 0),
                ("spo2", 96.6, "2024-07-05T00:00:00Z", 0),
            ],
            vec![],
        ),
    ] {
        let bytes = std::fs::read(format!("{fixtures}/{file}"))?;
        let (state, error, result) = upload_bytes(&env, &bytes).await?;
        assert_eq!(state, "done", "{file}: {error}");
        let listed: Vec<_> = omitted(&result).into_iter().map(|(member, _)| member).collect();
        assert_eq!(listed, omitted_files, "{file}");
        assert_readings(&env, result, &expected).await?;
    }
    Ok(())
}

#[tokio::test]
async fn each_product_root_uses_its_own_profile_zone() -> R {
    let env = Env::new().await?;
    let (madrid, new_york) = (profile("Europe/Madrid"), profile("America/New_York"));
    let (state, error, result) = upload(
        &env,
        &[
            ("Takeout/Fitbit/Your Profile/Profile.csv", &madrid),
            ("Takeout/Google Health/Your Profile/Profile.csv", &new_york),
            (
                "Takeout/Fitbit/Heart Rate Variability/Heart Rate Variability Details - 2024-07-04.csv",
                "timestamp,rmssd,coverage,low_frequency,high_frequency\n2024-07-04T01:15:00,43.5,0.9,1,1\n",
            ),
            (
                "Takeout/Google Health/Google Data/Heart Rate/2024-07.csv",
                "timestamp,bpm\n2024-07-04T08:10:00Z,72\n",
            ),
        ],
    )
    .await?;
    assert_eq!(state, "done", "{error}");
    assert_eq!(result["fitbit_takeout"]["rows_left_out"], 0, "{result}");
    assert_readings(
        &env,
        result,
        &[
            ("hr", 72.0, "2024-07-04T08:10:00Z", -240),
            ("hrv_rmssd", 43.5, "2024-07-03T23:15:00Z", 120),
        ],
    )
    .await
}

#[tokio::test]
async fn a_malformed_profile_does_not_suppress_another_roots_local_readings() -> R {
    let env = Env::new().await?;
    let madrid = profile("Europe/Madrid");
    let (state, error, result) = upload(
        &env,
        &[
            ("Takeout/Fitbit/Your Profile/Profile.csv", &madrid),
            (
                "Takeout/Google Health/Your Profile/Profile.csv",
                "id,timezone\nABC123,Mars/Olympus\n",
            ),
            (
                "Takeout/Fitbit/Heart Rate Variability/Heart Rate Variability Details - 2024-07-04.csv",
                "timestamp,rmssd\n2024-07-04T01:15:00,43.5\n",
            ),
            (
                "Takeout/Google Health/Heart Rate Variability/Heart Rate Variability Details - 2024-07-04.csv",
                "timestamp,rmssd\n2024-07-04T04:15:00,61.5\n",
            ),
        ],
    )
    .await?;
    assert_eq!(state, "done", "{error}");
    assert_eq!(result["fitbit_takeout"]["rows_left_out"], 1, "{result}");
    assert_readings(&env, result, &[("hrv_rmssd", 43.5, "2024-07-03T23:15:00Z", 120)]).await
}
