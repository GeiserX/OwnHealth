//! Synthetic Garmin account exports through the upload dispatcher and worker.
//! Member layout follows https://knownpace.com/articles/garmin-account-export-what-is-inside/.
//! The Garmin Local Archive inventory documents the healthStatusData and userBioMetrics fields:
//! https://github.com/Wewoc/Garmin_Local_Archive/blob/5f65d71e28ff89299f8febef2169cf94808b3303/src/docs/REFERENCE_GARMIN_EXPORT.md
//! The daily resting pulse field is documented in the supplementary data extraction code at
//! https://www.jstage.jst.go.jp/article/indhealth/63/4/63_2024-0186/_article.
mod common;

use std::io::{Cursor, Write};

use common::{Env, R};
use ownhealth_core::arrive::{Arrival, Arrived, arrive};
use ownhealth_core::blobs::BlobOwner;
use ownhealth_core::documents::{StoredFile, dispatch_upload, sniff};
use ownhealth_core::jobs;
use serde_json::{Value, json};
use sqlx::Acquire;
use uuid::Uuid;
use zip::write::SimpleFileOptions;

const RUN: &[u8] = include_bytes!("../../../../fixtures/fit/lucia-run-gps.fit");

fn zip(members: &[(&str, &[u8])]) -> anyhow::Result<Vec<u8>> {
    let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
    for (name, bytes) in members {
        if name.ends_with('/') {
            writer.add_directory(*name, SimpleFileOptions::default())?;
        } else if let Some(target) = bytes.strip_prefix(b"symlink:") {
            writer.add_symlink(*name, std::str::from_utf8(target)?, SimpleFileOptions::default())?;
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
        ("rides/lucia.rivera@example.org_7.fit", b"not FIT"),
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
        ("DI-Connect-Fitness/lucia.rivera@example.org_0_summarizedActivities.json", b"[]"),
        ("DI-Connect-Wellness/link_sleepData.json", b"symlink:123_sleepData.json"),
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
        // five JSON files and one FIT file: the UploadedFiles part that holds it is no file of its own
        assert_eq!(details["files_imported"], 6);
        assert_eq!(details["gps_left_out"], true);
        let omitted = details["not_imported"].as_array().expect("omitted files");
        // a folder is no file, so it is not listed
        assert_eq!(omitted.len(), 9, "{omitted:?}");
        for (name, reason) in [
            ("userSettings.json", "unsupported"),
            ("124_sleepData.json", "invalid_json"),
            ("UDSFile_empty.json", "no_readings"),
            ("UploadedFiles_0-_Part1.zip / rides/<email>_7.fit", "invalid_fit"),
            ("note.txt", "unsupported"),
            ("extra.zip", "nesting_limit"),
            ("escape.fit", "unsafe_path"),
            ("DI-Connect-Fitness/<email>_0_summarizedActivities.json", "unsupported"),
            ("link_sleepData.json", "unsafe_path"),
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
                    m["fit"] == true
                        && m["name"]
                            .as_str()
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
        // the account's email address is in no stored name: not in the readings, the flags or the result
        let flags: Value = sqlx::query_scalar("select flags from document where id = $1")
            .bind(doc)
            .fetch_one(&env.db.pool)
            .await?;
        for text in [
            std::str::from_utf8(&kept)?.to_string(),
            flags.to_string(),
            out.to_string(),
        ] {
            assert!(!text.contains("example.org"), "{text}");
        }

        let (again, _) = upload(&env, &archive).await?;
        let repeated = result(&env, again).await?;
        assert_eq!(repeated["document_id"], json!(doc));
        assert_eq!(repeated["accepted"], 0);
        assert_eq!(repeated["reasons"]["duplicate"], 41);
    }
    Ok(())
}

/// Each file job of an export in `env` drained; the state, result and error of job `id`.
async fn outcome(env: &Env, id: i64) -> anyhow::Result<(String, Option<Value>, Option<String>)> {
    jobs::drain(&env.ctx, "core", 5, None).await?;
    Ok(
        sqlx::query_as("select state, result, last_error from job where id = $1")
            .bind(id)
            .fetch_one(&env.db.pool)
            .await?,
    )
}

fn uds(days: &[(&str, i64)]) -> Vec<u8> {
    let rows: Vec<String> = days
        .iter()
        .map(|(d, s)| format!(r#"{{"calendarDate":"{d}","totalSteps":{s}}}"#))
        .collect();
    format!("[{}]", rows.join(",")).into_bytes()
}

const UDS: &str = "DI_CONNECT/DI-Connect-Aggregator/UDSFile_1.json";

#[test]
fn an_apple_health_marker_wins_over_a_garmin_member_in_either_order() -> R {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("a.zip");
    for order in [
        [(UDS, b"[]" as &[u8]), ("apple_health_export/export.xml", b"<x/>")],
        [("apple_health_export/export.xml", b"<x/>" as &[u8]), (UDS, b"[]")],
    ] {
        std::fs::write(&path, zip(&order)?)?;
        assert_eq!(sniff("a.zip", b"PK\x03\x04", Some(&path)), "apple_health_zip");
    }
    Ok(())
}

#[tokio::test]
async fn a_garmin_zip_from_an_inbox_keeps_its_channel_and_releases_the_raw_blob() -> R {
    let env = Env::new().await?;
    let profile = env.fam.profile("lucia");
    let inbox = env.source("folder", Some("lucia"), json!({}), None).await?;
    let archive = zip(&[(UDS, &uds(&[("2024-01-04", 6200)]))])?;
    let owner = BlobOwner::Profile(profile);
    let sha = env.blobs.put_bytes(owner, &archive)?;
    let mut c = env.db.pool.acquire().await?;
    let arrival = Arrival {
        source_id: Some(inbox.id),
        external_id: Some("first-arrival"),
        filename: Some("garmin.zip"),
        ..Arrival::new(env.fam.household_id, profile, "folder", &sha, archive.len() as i64)
    };
    let mut tx = c.begin().await?;
    let Arrived::Queued { job_id: Some(id), .. } = arrive(&mut tx, &env.blobs, &arrival).await? else {
        anyhow::bail!("first inbox arrival did not enqueue the import");
    };
    tx.commit().await?;
    let doc: Uuid = result(&env, id).await?["document_id"]
        .as_str()
        .expect("document")
        .parse()?;
    let channels: Vec<(String, String)> = sqlx::query_as(
        "select d.mime, s.channel from document d join document_source s on s.document_id = d.id \
         where d.profile_id = $1 and d.doc_type = 'wearables'",
    )
    .bind(profile)
    .fetch_all(&env.db.pool)
    .await?;
    assert_eq!(channels, [("application/x-ndjson".to_string(), "folder".to_string())]);
    assert!(!env.blobs.exists(owner, &sha), "raw ZIP still exists");
    let ledger_sha: String = sqlx::query_scalar(
        "select encode(blob_sha256, 'hex') from source_item where source_id = $1 and external_id = $2",
    )
    .bind(inbox.id)
    .bind(arrival.external_id)
    .fetch_one(&mut *c)
    .await?;
    assert_eq!(ledger_sha, sha);

    env.blobs.put_bytes(owner, &archive)?;
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
    assert!(!env.blobs.exists(owner, &sha), "duplicate stored the raw ZIP again");

    env.blobs.put_bytes(owner, &archive)?;
    let mut tx = c.begin().await?;
    assert_eq!(
        arrive(&mut tx, &env.blobs, &duplicate).await?,
        Arrived::AlreadySeen { document_id: Some(doc) },
    );
    tx.commit().await?;
    assert!(
        !env.blobs.exists(owner, &sha),
        "a repeated inbox item stored the ZIP again"
    );
    Ok(())
}

#[tokio::test]
async fn a_run_again_after_its_commit_returns_the_same_answer() -> R {
    let env = Env::new().await?;
    let (id, sha) = upload(&env, &zip(&[(UDS, &uds(&[("2024-01-04", 6200)]))])?).await?;
    let job = jobs::claim(
        &env.ctx.pool,
        "core",
        &env.ctx.worker_id,
        Some(&["import.garmin_zip".into()]),
    )
    .await?
    .expect("Garmin job");
    assert_eq!(job.id, id);
    let handler = jobs::handler_for(&job.kind).expect("Garmin handler");
    let first = handler(env.ctx.clone(), job).await?.expect("Garmin result");
    assert!(!env.blobs.exists(BlobOwner::Profile(env.fam.profile("lucia")), &sha));

    // The handler committed, but the worker stopped before completing its lease.
    sqlx::query("update job set locked_until = now() - interval '1 second' where id = $1 and state = 'running'")
        .bind(id)
        .execute(&env.db.pool)
        .await?;
    assert_eq!(jobs::reap_expired(&env.ctx.pool).await?, 1);
    assert_eq!(result(&env, id).await?, first);
    Ok(())
}

async fn steps(env: &Env) -> anyhow::Result<Vec<(String, f32, Uuid)>> {
    Ok(sqlx::query_as(
        "select to_char(t at time zone 'UTC', 'YYYY-MM-DD'), v, document_id from wearable_sample \
         where profile_id = $1 and metric = 'steps' order by t",
    )
    .bind(env.fam.profile("lucia"))
    .fetch_all(&env.db.pool)
    .await?)
}

#[tokio::test]
async fn retracting_an_older_export_keeps_the_days_a_newer_export_also_holds() -> R {
    let env = Env::new().await?;
    let old = zip(&[(UDS, &uds(&[("2024-01-04", 6200)]))])?;
    let new = zip(&[(UDS, &uds(&[("2024-01-04", 6200), ("2024-01-05", 7000)]))])?;
    let (a, _) = upload(&env, &old).await?;
    let old_doc: Uuid = result(&env, a).await?["document_id"]
        .as_str()
        .expect("document")
        .parse()?;
    let (b, _) = upload(&env, &new).await?;
    let newer = result(&env, b).await?;
    assert_eq!(newer["reasons"]["duplicate"], 1, "{newer}");
    let new_doc: Uuid = newer["document_id"].as_str().expect("document").parse()?;

    let mut tx = env.db.pool.begin().await?;
    ownhealth_ingest_structured::retract::retract_document_facts(&mut tx, old_doc, "test").await?;
    sqlx::query("update document set status = 'retracted' where id = $1")
        .bind(old_doc)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    jobs::drain(&env.ctx, "core", 5, None).await?;
    assert_eq!(
        steps(&env).await?,
        [
            ("2024-01-04".to_string(), 6200.0, new_doc),
            ("2024-01-05".to_string(), 7000.0, new_doc)
        ]
    );
    // the newer export keeps its summary of the file
    let flags: Value = sqlx::query_scalar("select flags from document where id = $1")
        .bind(new_doc)
        .fetch_one(&env.db.pool)
        .await?;
    assert_eq!(flags["garmin"]["files_imported"], 1, "{flags}");
    assert_eq!(flags["import"]["accepted"], 1, "{flags}");
    Ok(())
}

#[tokio::test]
async fn an_archive_with_two_members_of_one_name_is_refused() -> R {
    let env = Env::new().await?;
    let other = UDS.replace("UDSFile_1", "UDSFile_2");
    let mut archive = zip(&[
        (UDS, &uds(&[("2024-01-04", 6200)])),
        (&other, &uds(&[("2024-01-04", 9999)])),
    ])?;
    // the writer refuses a repeated name: rename the second member in its local and central headers
    let (from, to) = (b"UDSFile_2.json", b"UDSFile_1.json");
    let mut renamed = 0;
    for at in 0..archive.len() - from.len() {
        if &archive[at..at + from.len()] == from {
            archive[at..at + from.len()].copy_from_slice(to);
            renamed += 1;
        }
    }
    assert_eq!(renamed, 2);
    let (id, _) = upload(&env, &archive).await?;
    let (state, _, error) = outcome(&env, id).await?;
    assert_ne!(state, "done");
    assert!(
        error.as_deref().is_some_and(|e| e.contains("garmin_zip_unreadable")),
        "{error:?}"
    );
    assert!(steps(&env).await?.is_empty());
    Ok(())
}
