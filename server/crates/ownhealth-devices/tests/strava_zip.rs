mod common;

use std::io::{Cursor, Write};

use common::{Env, R};
use flate2::{Compression, write::GzEncoder};
use ownhealth_core::blobs::BlobOwner;
use ownhealth_core::documents::{StoredFile, dispatch_upload};
use ownhealth_core::jobs;
use serde_json::Value;
use uuid::Uuid;
use zip::write::SimpleFileOptions;

const FIT: &[u8] = include_bytes!("../../../../fixtures/fit/lucia-run-gps.fit");
const GPX: &[u8] = br#"<gpx xmlns="http://www.topografix.com/GPX/1/1"
 xmlns:gpxtpx="http://www.garmin.com/xmlschemas/TrackPointExtension/v1"><trk><name>PRIVATE_ROUTE</name>
 <type>running</type><trkseg>
 <trkpt lat="40.123456" lon="-3.123456"><time>2024-01-08T08:00:01Z</time>
 <extensions><gpxtpx:TrackPointExtension><gpxtpx:hr>120</gpxtpx:hr></gpxtpx:TrackPointExtension></extensions></trkpt>
 <trkpt lat="40.124456" lon="-3.123458"><time>2024-01-08T08:00:50Z</time>
 <extensions><gpxtpx:TrackPointExtension><gpxtpx:hr>140</gpxtpx:hr></gpxtpx:TrackPointExtension></extensions></trkpt>
 </trkseg></trk></gpx>"#;
/// An indoor activity with no position. It is the last file of the test archive that imports, so the
/// route line must keep the GPS of the files read before it.
const TCX: &[u8] = br#"<TrainingCenterDatabase xmlns="http://www.garmin.com/xmlschemas/TrainingCenterDatabase/v2">
 <Activities><Activity Sport="Running"><Id>2024-01-09T08:00:00Z</Id>
 <Lap StartTime="2024-01-09T08:00:00Z"><TotalTimeSeconds>1800</TotalTimeSeconds>
 <DistanceMeters>4400</DistanceMeters><Calories>320</Calories>
 <Track><Trackpoint><Time>2024-01-09T08:00:15Z</Time>
 <HeartRateBpm><Value>147</Value></HeartRateBpm></Trackpoint></Track>
 </Lap></Activity></Activities></TrainingCenterDatabase>"#;

fn gzip(bytes: &[u8]) -> anyhow::Result<Vec<u8>> {
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(bytes)?;
    Ok(encoder.finish()?)
}

fn zip(members: &[(&str, &[u8])]) -> anyhow::Result<Vec<u8>> {
    let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
    for (name, bytes) in members {
        writer.start_file(*name, SimpleFileOptions::default())?;
        writer.write_all(bytes)?;
    }
    Ok(writer.finish()?.into_inner())
}

async fn upload(env: &Env, archive: &[u8]) -> anyhow::Result<(Value, String)> {
    let profile = env.fam.profile("lucia");
    let sha = env.blobs.put_bytes(BlobOwner::Profile(profile), archive)?;
    let mut conn = env.db.pool.acquire().await?;
    let answer = dispatch_upload(
        &mut conn,
        &env.blobs,
        env.fam.household_id,
        profile,
        &[StoredFile {
            filename: "strava.zip".into(),
            sha256: sha.clone(),
            size: archive.len() as i64,
        }],
        None,
    )
    .await?;
    assert_eq!(answer["items"][0]["kind"], "strava_zip", "{answer}");
    let id = answer["items"][0]["job_id"].as_i64().expect("Strava import job");
    drop(conn);
    jobs::drain(&env.ctx, "core", 5, None).await?;
    let (state, result, error): (String, Option<Value>, Option<String>) =
        sqlx::query_as("select state, result, last_error from job where id = $1")
            .bind(id)
            .fetch_one(&env.db.pool)
            .await?;
    assert_eq!(state, "done", "{error:?}");
    Ok((result.expect("Strava job result"), sha))
}

#[tokio::test]
async fn a_localized_export_projects_readings_and_accounts_for_missing_rows_and_files() -> R {
    let env = Env::new().await?;
    let archive = zip(&[
        ("export/activities/4.tcx.gz", &gzip(TCX)?),
        ("export/activities/3.gpx", GPX),
        ("export/activities/1.fit.gz", &gzip(FIT)?),
        ("export/activities/5.fit", b"unlisted file"),
        (
            "export/activities.csv",
            b"Identifiant,Activit\xc3\xa9,Activit\xc3\xa9,Fichier\r\n1,\"Un, deux\",Course,activities/1.fit.gz\r\n2,Missing,Course,\r\n3,Gpx,Course,activities/3.gpx\r\n4,Tcx,Course,activities/4.tcx.gz\r\n6,Missing,Course,activities/6.gpx\r\n",
        ),
    ])?;
    let (out, sha) = upload(&env, &archive).await?;
    let details = &out["strava"];
    assert_eq!(details["files_imported"], 3, "{out}");
    assert_eq!(details["gps_left_out"], true);
    assert_eq!(details["rows_without_file"], 2);
    assert_eq!(details["files_without_row"], 1);
    let doc: Uuid = out["document_id"].as_str().expect("document id").parse()?;
    let owner = BlobOwner::Profile(env.fam.profile("lucia"));
    assert!(!env.blobs.exists(owner, &sha), "original route archive still exists");
    let kept: String = sqlx::query_scalar("select encode(blob_sha256, 'hex') from document where id = $1")
        .bind(doc)
        .fetch_one(&env.db.pool)
        .await?;
    let kept = String::from_utf8(env.blobs.get_bytes(owner, &kept)?)?;
    assert!(!kept.contains("40.123456") && !kept.contains("PRIVATE_ROUTE"), "{kept}");
    let points: Vec<(String, f32, String)> = sqlx::query_as(
        "select metric, v, s.provider from wearable_sample w join wearable_source s on s.id = w.source_id \
         where document_id = $1 and t >= '2024-01-08' and t < '2024-01-10' order by t, metric",
    )
    .bind(doc)
    .fetch_all(&env.db.pool)
    .await?;
    assert_eq!(
        points,
        [
            ("hr".into(), 130.0, "strava_export".into()),
            ("distance".into(), 0.111, "strava_export".into()),
            ("active_energy".into(), 320.0, "strava_export".into()),
            ("distance".into(), 4.4, "strava_export".into()),
            ("hr".into(), 147.0, "strava_export".into()),
        ]
    );
    let (repeated, _) = upload(&env, &archive).await?;
    assert_eq!(repeated["document_id"], out["document_id"]);
    assert_eq!(repeated["accepted"], 0);
    assert_eq!(repeated["strava"], out["strava"]);
    Ok(())
}

#[tokio::test]
async fn a_later_export_restores_overlapping_readings_after_retraction() -> R {
    let env = Env::new().await?;
    let old = zip(&[
        ("activities.csv", b"Date,File\nA,activities/1.gpx\n"),
        ("activities/1.gpx", GPX),
    ])?;
    let new = zip(&[
        ("activities.csv", b"Date,File\nA,activities/1.gpx\nB,activities/2.tcx\n"),
        ("activities/1.gpx", GPX),
        ("activities/2.tcx", TCX),
    ])?;
    let (old, _) = upload(&env, &old).await?;
    let (new, _) = upload(&env, &new).await?;
    assert_eq!(new["reasons"]["duplicate"], 2);
    let old_doc: Uuid = old["document_id"].as_str().expect("old document").parse()?;
    let new_doc: Uuid = new["document_id"].as_str().expect("new document").parse()?;
    let mut tx = env.db.pool.begin().await?;
    ownhealth_ingest_structured::retract::retract_document_facts(&mut tx, old_doc, "test").await?;
    sqlx::query("update document set status = 'retracted' where id = $1")
        .bind(old_doc)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    jobs::drain(&env.ctx, "core", 5, None).await?;
    let docs: Vec<(Uuid, i64)> =
        sqlx::query_as("select document_id, count(*) from wearable_sample group by document_id")
            .fetch_all(&env.db.pool)
            .await?;
    assert_eq!(docs, [(new_doc, 5)]);
    Ok(())
}

#[tokio::test]
async fn an_inbox_drops_the_raw_strava_archive_and_links_a_second_arrival() -> R {
    use ownhealth_core::arrive::{Arrival, Arrived, arrive};
    use serde_json::json;

    let env = Env::new().await?;
    let archive = zip(&[
        ("activities.csv", b"File\nactivities/1.gpx\n"),
        ("activities/1.gpx", GPX),
    ])?;
    let profile = env.fam.profile("lucia");
    let owner = BlobOwner::Profile(profile);
    let inbox = env.source("folder", Some("lucia"), json!({}), None).await?;
    let sha = env.blobs.put_bytes(owner, &archive)?;
    let arrival = Arrival {
        source_id: Some(inbox.id),
        external_id: Some("first"),
        filename: Some("history.zip"),
        ..Arrival::new(env.fam.household_id, profile, "folder", &sha, archive.len() as i64)
    };
    let mut tx = env.db.pool.begin().await?;
    assert!(matches!(
        arrive(&mut tx, &env.blobs, &arrival).await?,
        Arrived::Queued {
            kind: "strava_zip",
            job_id: Some(_),
            ..
        }
    ));
    tx.commit().await?;
    jobs::drain(&env.ctx, "core", 5, None).await?;
    assert!(!env.blobs.exists(owner, &sha));
    let document_id = sqlx::query_scalar("select document_id from source_item where source_id = $1")
        .bind(inbox.id)
        .fetch_one(&env.db.pool)
        .await?;
    let second = Arrival {
        external_id: Some("second"),
        ..arrival
    };
    for expected in [
        Arrived::Duplicate { document_id },
        Arrived::AlreadySeen {
            document_id: Some(document_id),
        },
    ] {
        env.blobs.put_bytes(owner, &archive)?;
        let mut tx = env.db.pool.begin().await?;
        assert_eq!(arrive(&mut tx, &env.blobs, &second).await?, expected);
        tx.commit().await?;
        assert!(!env.blobs.exists(owner, &sha));
    }
    Ok(())
}

fn inbox_arrival<'a>(
    env: &Env,
    source: Uuid,
    external_id: &'a str,
    sha: &'a str,
    size: usize,
) -> ownhealth_core::arrive::Arrival<'a> {
    let profile = env.fam.profile("lucia");
    ownhealth_core::arrive::Arrival {
        source_id: Some(source),
        external_id: Some(external_id),
        filename: Some("history.zip"),
        ..ownhealth_core::arrive::Arrival::new(env.fam.household_id, profile, "folder", sha, size as i64)
    }
}

/// A refused export gives up the inbox's hold on its raw archive, so the routes in it go too.
#[tokio::test]
async fn an_inbox_releases_a_refused_strava_archive() -> R {
    use ownhealth_core::arrive::{Arrived, arrive};

    let env = Env::new().await?;
    let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
    writer.add_symlink("activities.csv", "../profile.csv", SimpleFileOptions::default())?;
    writer.start_file("activities/1.gpx", SimpleFileOptions::default())?;
    writer.write_all(GPX)?;
    let archive = writer.finish()?.into_inner();
    let owner = BlobOwner::Profile(env.fam.profile("lucia"));
    let inbox = env.source("folder", Some("lucia"), serde_json::json!({}), None).await?;
    let sha = env.blobs.put_bytes(owner, &archive)?;
    let mut tx = env.db.pool.begin().await?;
    let job = match arrive(&mut tx, &env.blobs, &inbox_arrival(&env, inbox.id, "refused", &sha, archive.len())).await? {
        Arrived::Queued {
            kind: "strava_zip",
            job_id: Some(job),
            ..
        } => job,
        other => anyhow::bail!("not queued as a Strava export: {other:?}"),
    };
    tx.commit().await?;
    jobs::drain(&env.ctx, "core", 5, None).await?;
    let (state, error): (String, Option<String>) = sqlx::query_as("select state, last_error from job where id = $1")
        .bind(job)
        .fetch_one(&env.db.pool)
        .await?;
    assert_eq!(state, "dead");
    assert!(error.is_some_and(|e| e.contains("strava_zip_unreadable")));
    let released: bool = sqlx::query_scalar("select raw_released_at is not null from source_item where source_id = $1")
        .bind(inbox.id)
        .fetch_one(&env.db.pool)
        .await?;
    assert!(released, "the inbox item still holds the refused archive");
    assert!(!env.blobs.exists(owner, &sha), "the refused archive still exists");
    Ok(())
}

/// An item seen before releases its new copy only under the profile's Strava export lock.
#[tokio::test]
async fn an_already_seen_strava_arrival_waits_for_the_export_lock() -> R {
    use ownhealth_core::arrive::{Arrived, arrive};
    use ownhealth_ingest_structured::retract::lock_exports;
    use std::time::Duration;

    let env = Env::new().await?;
    let archive = zip(&[
        ("activities.csv", b"File\nactivities/1.gpx\n"),
        ("activities/1.gpx", GPX),
    ])?;
    let profile = env.fam.profile("lucia");
    let owner = BlobOwner::Profile(profile);
    let inbox = env.source("folder", Some("lucia"), serde_json::json!({}), None).await?;
    let sha = env.blobs.put_bytes(owner, &archive)?;
    let arrival = inbox_arrival(&env, inbox.id, "first", &sha, archive.len());
    let mut tx = env.db.pool.begin().await?;
    arrive(&mut tx, &env.blobs, &arrival).await?;
    tx.commit().await?;
    jobs::drain(&env.ctx, "core", 5, None).await?;
    let document_id = sqlx::query_scalar("select document_id from source_item where source_id = $1")
        .bind(inbox.id)
        .fetch_one(&env.db.pool)
        .await?;

    env.blobs.put_bytes(owner, &archive)?;
    let mut barrier = env.db.pool.begin().await?;
    lock_exports(&mut barrier, profile, "strava").await?;
    let again = async {
        let mut tx = env.db.pool.begin().await?;
        let arrived = arrive(&mut tx, &env.blobs, &arrival).await?;
        tx.commit().await?;
        anyhow::Ok(arrived)
    };
    let probe = async {
        for _ in 0..100 {
            let waiting: i64 = sqlx::query_scalar(
                "select count(*) from pg_stat_activity where datname = current_database() \
                 and wait_event_type = 'Lock' and wait_event = 'advisory'",
            )
            .fetch_one(&env.db.pool)
            .await?;
            if waiting == 1 {
                return anyhow::Ok(());
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        anyhow::bail!("the second arrival did not wait for the Strava export lock")
    };
    tokio::pin!(again);
    tokio::select! {
        settled = &mut again => anyhow::bail!("the arrival finished while the lock was held: {settled:?}"),
        waited = probe => waited?,
    }
    barrier.rollback().await?;
    assert_eq!(
        again.await?,
        Arrived::AlreadySeen {
            document_id: Some(document_id)
        }
    );
    assert!(!env.blobs.exists(owner, &sha));
    Ok(())
}

/// An export of manual activities only holds the CSV and an empty activities folder. It is still a
/// Strava export: nothing imports, and every row is counted as a row without a file.
#[tokio::test]
async fn an_export_without_activity_files_counts_its_rows() -> R {
    let env = Env::new().await?;
    let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
    writer.start_file("activities.csv", SimpleFileOptions::default())?;
    writer.write_all(b"Activity ID,Activity Name,Filename\n1,Manual walk,\n2,Manual run,\n")?;
    writer.add_directory("activities/", SimpleFileOptions::default())?;
    writer.start_file("profile.csv", SimpleFileOptions::default())?;
    writer.write_all(b"Athlete ID\n1\n")?;
    let archive = writer.finish()?.into_inner();
    let (out, _) = upload(&env, &archive).await?;
    assert_eq!(out["accepted"], 0, "{out}");
    assert_eq!(out["strava"]["files_imported"], 0);
    assert_eq!(out["strava"]["rows_without_file"], 2);
    assert_eq!(out["strava"]["files_without_row"], 0);
    Ok(())
}
