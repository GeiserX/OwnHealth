//! Synthetic Garmin account exports through the upload dispatcher and worker.
//! Member layout follows https://knownpace.com/articles/garmin-account-export-what-is-inside/.
//! The Garmin Local Archive inventory documents the healthStatusData and userBioMetrics fields:
//! https://github.com/Wewoc/Garmin_Local_Archive/blob/5f65d71e28ff89299f8febef2169cf94808b3303/src/docs/REFERENCE_GARMIN_EXPORT.md
//! The daily resting pulse field is documented in the supplementary data extraction code at
//! https://www.jstage.jst.go.jp/article/indhealth/63/4/63_2024-0186/_article.
mod common;

use std::io::{Cursor, Write};

use common::{Env, R};
use ownhealth_core::blobs::BlobOwner;
use ownhealth_core::documents::{StoredFile, dispatch_upload};
use ownhealth_core::jobs;
use serde_json::{Value, json};
use zip::write::SimpleFileOptions;

const RUN: &[u8] = include_bytes!("../../../../fixtures/fit/lucia-run-gps.fit");

fn zip(members: &[(&str, &[u8])]) -> anyhow::Result<Vec<u8>> {
    let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
    for (name, bytes) in members {
        if name.ends_with('/') {
            writer.add_directory(*name, SimpleFileOptions::default())?;
        } else {
            writer.start_file(*name, SimpleFileOptions::default())?;
            writer.write_all(bytes)?;
        }
    }
    Ok(writer.finish()?.into_inner())
}

fn history(root: &str) -> anyhow::Result<Vec<u8>> {
    let deeper = zip(&[("held.txt", b"never read")])?;
    let fit_zip = zip(&[
        ("rides/morning.fit", RUN),
        ("rides/broken.fit", b"not FIT"),
        ("rides/note.txt", b"not a workout"),
        ("rides/extra.zip", &deeper),
        ("../escape.fit", RUN),
    ])?;
    let members: &[(&str, &[u8])] = &[
        ("DI-Connect-Empty/", b""),
        ("DI-Connect-Aggregator/UDSFile_20240101_20240401.json",
         br#"[{"calendarDate":"2024-01-04","totalSteps":6200,"currentDayRestingHeartRate":56,"restingHeartRate":61,"activeKilocalories":210,"wellnessStartTimeGmt":"2024-01-03T23:00:00.0","wellnessStartTimeLocal":"2024-01-04T00:00:00.0"}]"#),
        ("DI-Connect-Wellness/2024-01-01_2024-04-01_123_sleepData.json",
         br#"[{"calendarDate":"2024-01-04","sleepStartTimestampGMT":"2024-01-03T22:00:00Z","sleepEndTimestampGMT":"2024-01-04T06:00:00Z","deepSleepSeconds":3600,"lightSleepSeconds":18000,"remSleepSeconds":5400}]"#),
        ("DI-Connect-Wellness/123_userBioMetrics.json", br#"[{"metaData":{"calendarDate":"2024-01-04"},"weight":{"weight":61200,"timestampGMT":"2024-01-04T08:00:00.0"}},{"metaData":{"calendarDate":"2024-01-05"},"weight":{"weight":70000,"sourceType":"USER_SETTING"}}]"#),
        ("DI-Connect-Aggregator/BloodPressureFile_20240101_20240401.json",
         br#"[{"measurementTimestampGMT":"2024-01-04T08:30:00Z","systolic":118,"diastolic":75}]"#),
        ("DI-Connect-Wellness/123_healthStatusData.json",
         br#"[{"calendarDate":"2024-01-04","metrics":[{"type":"HR","value":52},{"type":"HRV","value":42,"baselineLowerLimit":38}]}]"#),
        ("DI-Connect-Uploaded-Files/UploadedFiles_0-_Part1.zip", &fit_zip),
        ("DI-Connect-Wellness/123_userSettings.json", br#"{"private":"GPS and account marker"}"#),
        ("DI-Connect-Wellness/124_sleepData.json", b"broken JSON"),
        ("DI-Connect-Aggregator/UDSFile_empty.json", br#"[{"retro":false}]"#),
    ];
    let names: Vec<_> = members
        .iter()
        .map(|(name, bytes)| (format!("{root}{name}"), *bytes))
        .collect();
    let paths: Vec<_> = names.iter().map(|(name, bytes)| (name.as_str(), *bytes)).collect();
    zip(&paths)
}

async fn upload(env: &Env, archive: &[u8]) -> anyhow::Result<(i64, String)> {
    let profile = env.fam.profile("lucia");
    let sha = env.blobs.put_bytes(BlobOwner::Profile(profile), archive)?;
    let mut conn = env.db.pool.acquire().await?;
    let answer = dispatch_upload(
        &mut conn,
        &env.blobs,
        env.fam.household_id,
        profile,
        &[StoredFile {
            filename: "garmin.zip".into(),
            sha256: sha.clone(),
            size: archive.len() as i64,
        }],
        None,
    )
    .await?;
    assert_eq!(answer["items"][0]["kind"], "garmin_zip");
    Ok((answer["items"][0]["job_id"].as_i64().expect("Garmin import job"), sha))
}

async fn result(env: &Env, id: i64) -> anyhow::Result<Value> {
    jobs::drain(&env.ctx, "core", 5, None).await?;
    let (state, result, error): (String, Option<Value>, Option<String>) =
        sqlx::query_as("select state, result, last_error from job where id = $1")
            .bind(id)
            .fetch_one(&env.db.pool)
            .await?;
    assert_eq!(state, "done", "{error:?}");
    Ok(result.expect("Garmin job result"))
}

#[tokio::test]
async fn uploads_garmin_history_and_accounts_for_every_file() -> R {
    for root in ["DI_CONNECT/", "Account Export/DI_CONNECT/", ""] {
        let env = Env::new().await?;
        let archive = history(root)?;
        let (id, raw_sha) = upload(&env, &archive).await?;
        let out = result(&env, id).await?;
        let doc: uuid::Uuid = out["document_id"].as_str().expect("document id").parse()?;
        let profile = env.fam.profile("lucia");
        assert_eq!(out["accepted"], 41, "{out}");
        let details = &out["garmin"];
        assert_eq!(details["files_imported"], 7);
        assert_eq!(details["gps_left_out"], true);
        let omitted = details["not_imported"].as_array().expect("omitted files");
        assert_eq!(omitted.len(), 8, "{omitted:?}");
        for (name, reason) in [
            ("DI-Connect-Empty/", "directory"),
            ("userSettings.json", "unsupported"),
            ("124_sleepData.json", "invalid_json"),
            ("UDSFile_empty.json", "no_readings"),
            ("broken.fit", "invalid_fit"),
            ("note.txt", "unsupported"),
            ("extra.zip", "nesting_limit"),
            ("escape.fit", "unsafe_path"),
        ] {
            assert!(
                omitted
                    .iter()
                    .any(|e| e["member"].as_str().is_some_and(|s| s.ends_with(name)) && e["reason"] == reason),
                "{name}: {omitted:?}"
            );
        }
        let points: Vec<(String, f32)> = sqlx::query_as(
            "select metric, v from wearable_sample where profile_id = $1 and document_id = $2 and metric <> 'hr' order by metric, v"
        ).bind(profile).bind(doc).fetch_all(&env.db.pool).await?;
        assert_eq!(
            points,
            [
                ("active_energy".into(), 210.0),
                ("active_energy".into(), 312.0),
                ("bp_diastolic".into(), 75.0),
                ("bp_systolic".into(), 118.0),
                ("distance".into(), 5.23),
                ("hr_resting".into(), 56.0),
                ("hrv_rmssd".into(), 42.0),
                ("sleep_asleep".into(), 450.0),
                ("sleep_in_bed".into(), 480.0),
                ("steps".into(), 6200.0),
                ("weight".into(), 61.2),
            ]
        );
        let stored: (String, String) =
            sqlx::query_as("select doc_type, encode(blob_sha256, 'hex') from document where id = $1")
                .bind(doc)
                .fetch_one(&env.db.pool)
                .await?;
        assert_eq!(stored.0, "wearables");
        let owner = BlobOwner::Profile(profile);
        assert_ne!(stored.1, raw_sha);
        assert!(!env.blobs.exists(owner, &raw_sha));
        let kept = env.blobs.get_bytes(owner, &stored.1)?;
        let lines = std::str::from_utf8(&kept)?.lines();
        let entries: Vec<Value> = lines.map(serde_json::from_str).collect::<Result<_, _>>()?;
        assert_eq!(entries[0]["archive_sha256"], raw_sha);
        assert_eq!(entries.len(), 42);
        assert!(
            entries[0]["members"]
                .as_array()
                .expect("archive member list")
                .iter()
                .any(|m| {
                    m.as_str()
                        .is_some_and(|name| name.ends_with("UploadedFiles_0-_Part1.zip / rides/morning.fit"))
                })
        );
        for reading in &entries[1..] {
            assert_eq!(reading.as_object().expect("reading object").len(), 6, "{reading}");
            assert!(reading["value"].is_number());
            if reading["metric"] == "steps" {
                assert_eq!(reading["offset_min"], 60);
                assert_eq!(reading["start"], "2024-01-03T23:00:00Z");
            }
        }
        assert!(!std::str::from_utf8(&kept)?.contains("GPS and account marker"));

        let (again, _) = upload(&env, &archive).await?;
        let repeated = result(&env, again).await?;
        assert_eq!(repeated["document_id"], json!(doc));
        assert_eq!(repeated["accepted"], 0);
        assert_eq!(repeated["reasons"]["duplicate"], 41);
    }
    Ok(())
}
