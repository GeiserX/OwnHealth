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
const TCX: &[u8] = br#"<TrainingCenterDatabase xmlns="http://www.garmin.com/xmlschemas/TrainingCenterDatabase/v2">
 <Activities><Activity Sport="Running"><Id>2024-01-09T08:00:00Z</Id>
 <Lap StartTime="2024-01-09T08:00:00Z"><TotalTimeSeconds>1800</TotalTimeSeconds>
 <DistanceMeters>4400</DistanceMeters><Calories>320</Calories>
 <Track><Trackpoint><Time>2024-01-09T08:00:15Z</Time>
 <Position><LatitudeDegrees>40.123456</LatitudeDegrees><LongitudeDegrees>-3.123456</LongitudeDegrees></Position>
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
