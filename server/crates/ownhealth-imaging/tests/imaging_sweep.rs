//! Slice 43: Orthanc follows deletion, retraction and restore.
//!
//! Each test imports synthetic hospital CDs (support/mod.rs) into a fake Orthanc that files studies by
//! PatientID and UID as Orthanc does, then changes the record (a profile delete, a retraction, a
//! restore that comes back to an empty Orthanc) and runs the `imaging.sweep` and `imaging.rebuild`
//! jobs. Synthetic people only.

mod support;

use std::sync::atomic::Ordering;

use ownhealth_core::Runtime;
use ownhealth_core::blobs::BlobOwner;
use ownhealth_core::demo::{Family, create_rivera_family};
use ownhealth_core::jobs::{self, JobContext};
use ownhealth_core::testing::TestDb;
use ownhealth_imaging::import::{JOB, REVIEW_KIND};
use ownhealth_imaging::orthanc::Orthanc;
use ownhealth_imaging::remove::{self, RELEASE_JOB, Removal};
use ownhealth_imaging::upload::{self, Recorded};
use ownhealth_imaging::{rebuild, sweep};
use serde_json::Value;
use support::{ALEX, CARMEN, EXPLICIT_LE, FakeOrthanc, SAM, Study, study_files, zip_files};
use uuid::Uuid;

type R = anyhow::Result<()>;

const U1: &str = "1.2.826.0.1.3680043.10.999.43.1";
const U2: &str = "1.2.826.0.1.3680043.10.999.43.2";
const U3: &str = "1.2.826.0.1.3680043.10.999.43.3";

struct Env {
    _db: TestDb,
    _dir: tempfile::TempDir,
    rt: Runtime,
    ctx: JobContext,
    fam: Family,
    orthanc: FakeOrthanc,
}

async fn env() -> anyhow::Result<Env> {
    ownhealth_imaging::register();
    let orthanc = FakeOrthanc::start().await?;
    let db = TestDb::new().await?;
    let dir = tempfile::tempdir()?;
    let mut settings = db.settings(dir.path())?;
    settings.orthanc = Some(orthanc.config());
    let rt = Runtime::with_pool(settings, db.pool.clone())?;
    let mut c = db.pool.acquire().await?;
    let fam = create_rivera_family(&mut c, &format!("Rivera {}", ownhealth_core::ids::token_hex(3))).await?;
    drop(c);
    let ctx = rt.job_context();
    Ok(Env {
        _db: db,
        _dir: dir,
        rt,
        ctx,
        fam,
        orthanc,
    })
}

impl Env {
    /// Stores the disc as `POST /api/imaging/import` does and queues its import.
    async fn upload(&self, slug: &str, files: &[(String, Vec<u8>)]) -> anyhow::Result<(Recorded, String)> {
        let zip = zip_files(files);
        let profile = self.fam.profile(slug);
        let sha = self.rt.blobs.put_bytes(BlobOwner::Profile(profile), &zip)?;
        let mut tx = self.rt.pool.begin().await?;
        let r = upload::record(
            &mut tx,
            profile,
            &sha,
            i64::try_from(zip.len())?,
            Some("cd.zip"),
            files.len(),
            None,
        )
        .await?;
        tx.commit().await?;
        Ok((r, sha))
    }

    /// Runs the queued jobs of one kind.
    async fn run(&self, kind: &str) -> anyhow::Result<usize> {
        Ok(jobs::drain(&self.ctx, "core", 20, Some(&[kind.to_string()])).await?)
    }

    /// Imports every queued upload.
    async fn import(&self) -> anyhow::Result<usize> {
        self.run(JOB).await
    }

    /// The result of the newest finished job of `kind`.
    async fn result(&self, kind: &str) -> anyhow::Result<(String, Value)> {
        Ok(sqlx::query_as(
            "select state, coalesce(result, 'null'::jsonb) from job where kind = $1 order by id desc limit 1",
        )
        .bind(kind)
        .fetch_one(&self.rt.pool)
        .await?)
    }

    /// Queues a sweep, as the app does every hour and after a profile delete, and runs it.
    async fn sweep(&self) -> anyhow::Result<Value> {
        let mut conn = self.rt.pool.acquire().await?;
        sweep::enqueue(&mut conn).await?;
        drop(conn);
        assert_eq!(self.run(sweep::JOB).await?, 1);
        let (state, result) = self.result(sweep::JOB).await?;
        assert_eq!(state, "done", "{result}");
        Ok(result)
    }

    /// Deletes a profile as the delete route does (`household::delete_profile`): no Orthanc call.
    async fn delete_profile(&self, slug: &str) -> R {
        let mut tx = self.rt.pool.begin().await?;
        ownhealth_auth::household::delete_profile(&mut tx, self.fam.profile(slug)).await?;
        tx.commit().await?;
        Ok(())
    }

    /// The profile that records `uid`, its Orthanc id, status and FHIR.
    async fn recorded(&self, uid: &str) -> anyhow::Result<Option<(Uuid, Option<String>, String, Value)>> {
        Ok(sqlx::query_as(
            "select profile_id, orthanc_id, status, fhir from imaging_study where study_instance_uid = $1",
        )
        .bind(uid)
        .fetch_optional(&self.rt.pool)
        .await?)
    }

    async fn count(&self, sql: &str) -> anyhow::Result<i64> {
        Ok(sqlx::query_scalar(sqlx::AssertSqlSafe(sql.to_string()))
            .fetch_one(&self.rt.pool)
            .await?)
    }

    /// The open `imaging_study` items of a profile: (item id, study uid, reason).
    async fn in_review(&self, slug: &str) -> anyhow::Result<Vec<(Uuid, String, String)>> {
        Ok(sqlx::query_as(
            "select id, payload->>'study_uid', payload->>'reason' from review_item where kind = $1 \
             and status = 'open' and profile_id = $2 order by created_at, id",
        )
        .bind(REVIEW_KIND)
        .bind(self.fam.profile(slug))
        .fetch_all(&self.rt.pool)
        .await?)
    }
}

/// The files of only some series of a study: a second disc of it.
fn series_files(st: &Study, keep: &[usize], dir: &str) -> Vec<(String, Vec<u8>)> {
    study_files(st, dir)
        .into_iter()
        .filter(|(name, _)| keep.iter().any(|s| name.contains(&format!("/S{s:02}/"))))
        .collect()
}

#[tokio::test]
async fn a_profile_deleted_while_orthanc_is_down_loses_its_study_at_the_next_sweep() -> R {
    let e = env().await?;
    e.upload("alex", &study_files(&Study::new(U1, ALEX), "DICOM")).await?;
    e.upload("sam", &study_files(&Study::new(U2, SAM), "DICOM")).await?;
    assert_eq!(e.import().await?, 2);
    assert_eq!(e.orthanc.patients_under(U1), [ALEX.id.to_string()].into());

    // Orthanc is down while Alex is deleted: the delete does not need it, and the sweep waits
    e.orthanc.down.store(true, Ordering::SeqCst);
    e.delete_profile("alex").await?;
    let waited = e.sweep().await?;
    assert_eq!(waited["skipped"], "orthanc_unreachable", "{waited}");
    assert_eq!(
        e.orthanc.patients_under(U1).len(),
        1,
        "nothing could go while Orthanc was down"
    );

    // Orthanc is back: the next sweep removes the study no record names, and only that one
    e.orthanc.down.store(false, Ordering::SeqCst);
    let swept = e.sweep().await?;
    assert_eq!(swept["removed_studies"], 1, "{swept}");
    assert!(
        e.orthanc.patients_under(U1).is_empty(),
        "Alex's pixels stayed in Orthanc"
    );
    assert_eq!(e.orthanc.patients_under(U2), [SAM.id.to_string()].into());
    assert_eq!(e.orthanc.instances_under(U2), 6);
    assert_eq!(swept["rebuild_profiles"], 0, "{swept}");
    Ok(())
}

#[tokio::test]
async fn a_study_another_profile_names_survives_the_delete_of_a_profile_that_held_it_in_review() -> R {
    let e = env().await?;
    let alex = Study::new(U1, ALEX);
    e.upload("alex", &study_files(&alex, "DICOM")).await?;
    e.import().await?;
    // Sam uploads the same disc, and his own study: Alex's waits in Sam's review, unsent
    e.upload("sam", &study_files(&alex, "DICOM")).await?;
    e.upload("sam", &study_files(&Study::new(U2, SAM), "DICOM")).await?;
    e.import().await?;
    let held: Vec<(String, String)> = e
        .in_review("sam")
        .await?
        .into_iter()
        .map(|(_, uid, why)| (uid, why))
        .collect();
    assert_eq!(held, [(U1.to_string(), "other_profile".to_string())]);

    e.delete_profile("sam").await?;
    let swept = e.sweep().await?;
    assert_eq!(swept["removed_studies"], 1, "{swept}");
    assert!(
        e.orthanc.patients_under(U2).is_empty(),
        "Sam's pixels stayed in Orthanc"
    );
    // Alex's study, which Sam's upload also held, is Alex's record still, whole in Orthanc
    let (owner, id, ..) = e.recorded(U1).await?.ok_or_else(|| anyhow::anyhow!("Alex lost U1"))?;
    assert_eq!(owner, e.fam.profile("alex"));
    let id = id.ok_or_else(|| anyhow::anyhow!("no Orthanc id"))?;
    assert!(e.orthanc.held().studies.contains_key(&id));
    assert_eq!(e.orthanc.instances_under(U1), 6);
    Ok(())
}

#[tokio::test]
async fn restore_then_rebuild_brings_every_recorded_study_back_under_the_same_ids() -> R {
    let e = env().await?;
    // the first disc holds two series of U1; a second disc adds a third series and a second study
    let three = Study {
        series: 3,
        ..Study::new(U1, ALEX)
    };
    let (first, _) = e.upload("alex", &series_files(&three, &[0, 1], "DICOM")).await?;
    e.import().await?;
    let mut second = series_files(&three, &[2], "DICOM");
    second.extend(study_files(&Study::new(U2, ALEX), "DICOM/P2"));
    let (later, _) = e.upload("alex", &second).await?;
    e.import().await?;
    let (_, u1_id, _, u1_fhir) = e
        .recorded(U1)
        .await?
        .ok_or_else(|| anyhow::anyhow!("U1 not recorded"))?;
    assert_eq!(u1_fhir["numberOfSeries"], 3, "{u1_fhir}");
    // U1 came from both uploads; the record names only the first one
    let holders = e
        .count(&format!(
            "select count(*) from imaging_import_study where study_instance_uid = '{U1}' \
             and import_id in ('{}', '{}')",
            first.import_id, later.import_id
        ))
        .await?;
    assert_eq!(holders, 2);
    // a retracted study keeps its row, so it keeps its pixels: a restore of the row finds them
    sqlx::query("update imaging_study set status = 'retracted', status_reason = 'entered in error' where study_instance_uid = $1")
        .bind(U2)
        .execute(&e.rt.pool)
        .await?;
    let kept = e.sweep().await?;
    assert_eq!(kept["removed_studies"], 0, "{kept}");
    assert_eq!(
        e.orthanc.instances_under(U2),
        6,
        "the sweep removed a retracted study's pixels"
    );
    let ids: Vec<Option<String>> = vec![u1_id.clone(), e.recorded(U2).await?.and_then(|r| r.1)];

    // a restore on a new box: the database and the blobs come back, Orthanc's storage does not
    e.orthanc.wipe();
    let warmed_before = e.orthanc.warmed().len();
    let swept = e.sweep().await?;
    assert_eq!(swept["rebuild_profiles"], 1, "{swept}");
    assert_eq!(e.run(rebuild::JOB).await?, 1);
    let (state, rebuilt) = e.result(rebuild::JOB).await?;
    assert_eq!(state, "done", "{rebuilt}");
    assert_eq!(rebuilt["rebuilt"], 2, "{rebuilt}");
    assert_eq!(rebuilt["missing"], 0, "{rebuilt}");

    // the same studies under the same UIDs and Orthanc ids, every series of both discs, record unchanged
    let held = e.orthanc.held();
    for (uid, id) in [U1, U2].into_iter().zip(ids) {
        let id = id.ok_or_else(|| anyhow::anyhow!("no Orthanc id for {uid}"))?;
        assert_eq!(
            held.studies.get(&id).map(|(u, p)| (u.as_str(), p.as_str())),
            Some((uid, ALEX.id)),
            "{uid}"
        );
    }
    assert_eq!(held.studies.len(), 2, "{:?}", held.studies);
    assert_eq!(
        e.orthanc.series_under(U1),
        (0..3).map(|s| three.series_uid(s)).collect()
    );
    assert_eq!(e.orthanc.instances_under(U1), 9);
    let (_, after_id, status, after_fhir) = e.recorded(U1).await?.ok_or_else(|| anyhow::anyhow!("U1 gone"))?;
    assert_eq!((after_id, status.as_str(), &after_fhir), (u1_id, "current", &u1_fhir));
    assert_eq!(e.recorded(U2).await?.map(|r| r.2).as_deref(), Some("retracted"));
    // the viewer finds each series' metadata cached again: 3 series of U1, 2 of U2
    assert_eq!(e.orthanc.warmed().len() - warmed_before, 5, "{:?}", e.orthanc.warmed());

    // Orthanc is whole: the next sweep queues nothing
    let calm = e.sweep().await?;
    assert_eq!(calm["rebuild_profiles"], 0, "{calm}");
    Ok(())
}

#[tokio::test]
async fn the_sweep_removes_a_series_no_record_counts_and_a_study_no_record_names() -> R {
    let e = env().await?;
    let three = Study {
        series: 3,
        ..Study::new(U1, ALEX)
    };
    e.upload("alex", &series_files(&three, &[0, 1], "DICOM")).await?;
    e.import().await?;
    let (_, _, _, fhir) = e.recorded(U1).await?.ok_or_else(|| anyhow::anyhow!("not recorded"))?;
    // what a retry leaves when its check after the send failed: a third series under the recorded
    // study, and a study of another patient, neither of them on any record
    let client = Orthanc::new(e.orthanc.config())?;
    client.store(support::image(&three, 2, 0, EXPLICIT_LE)).await?;
    client
        .store(support::image(&Study::new(U3, CARMEN), 0, 0, EXPLICIT_LE))
        .await?;
    assert_eq!(e.orthanc.series_under(U1).len(), 3);

    let swept = e.sweep().await?;
    assert_eq!(swept["removed_series"], 1, "{swept}");
    assert_eq!(swept["removed_studies"], 1, "{swept}");
    assert_eq!(swept["rebuild_profiles"], 0, "{swept}");
    assert_eq!(
        e.orthanc.series_under(U1),
        (0..2).map(|s| three.series_uid(s)).collect()
    );
    assert_eq!(e.orthanc.instances_under(U1), 6);
    assert!(e.orthanc.patients_under(U3).is_empty());
    assert_eq!(e.recorded(U1).await?.map(|r| r.3), Some(fhir));
    Ok(())
}

#[tokio::test]
async fn the_sweep_drains_the_post_journal_of_an_upload_whose_profile_was_purged() -> R {
    let e = env().await?;
    // the first POST's reply is lost: the job retries later, and its journal names the attempt
    e.orthanc.lose_store_reply_at.store(1, Ordering::SeqCst);
    e.upload("alex", &study_files(&Study::new(U1, ALEX), "DICOM")).await?;
    e.import().await?;
    assert_eq!(e.result(JOB).await?.0, "queued");
    assert_eq!(e.count("select count(*) from imaging_import_post").await?, 1);
    // the profile is purged and its import never runs again
    sqlx::query("delete from job where kind = $1")
        .bind(JOB)
        .execute(&e.rt.pool)
        .await?;
    e.delete_profile("alex").await?;
    assert_eq!(e.count("select count(*) from imaging_import_post").await?, 1);

    let swept = e.sweep().await?;
    assert_eq!(swept["drained_journals"], 1, "{swept}");
    assert_eq!(e.count("select count(*) from imaging_import_post").await?, 0);
    assert!(e.orthanc.held().instances.is_empty());
    Ok(())
}

#[tokio::test]
async fn an_upload_held_entirely_in_review_goes_with_its_last_held_study_and_its_zip() -> R {
    let e = env().await?;
    let carmen = Study::new(U3, CARMEN);
    let (up, sha) = e.upload("alex", &study_files(&carmen, "DICOM")).await?;
    e.import().await?;
    let held = e.in_review("alex").await?;
    assert_eq!(held.len(), 1);
    let (item_id, ..) = held[0].clone();
    let owner = BlobOwner::Profile(e.fam.profile("alex"));
    assert!(e.rt.blobs.exists(owner, &sha));

    // the person rejects the held study: it was the upload's last one, and nothing of it is recorded
    let mut tx = e.rt.pool.begin().await?;
    ownhealth_core::review::reject(&mut tx, item_id, e.fam.user("alex"), "wrong person").await?;
    let item = ownhealth_core::review::get_item(&mut tx, item_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("no item"))?;
    assert!(remove::settle(&mut tx, &item).await?);
    tx.commit().await?;
    let left = e
        .count(&format!(
            "select count(*) from imaging_import where id = '{}'",
            up.import_id
        ))
        .await?;
    assert_eq!(left, 0);
    assert_eq!(e.run(RELEASE_JOB).await?, 1);
    assert_eq!(e.result(RELEASE_JOB).await?.0, "done");
    assert!(!e.rt.blobs.exists(owner, &sha), "the zip stayed");

    // reopened, the held study of the removed upload refuses Accept and says why
    let mut tx = e.rt.pool.begin().await?;
    ownhealth_core::review::reopen(&mut tx, item_id, e.fam.user("alex")).await?;
    let refused = ownhealth_core::review::accept(&mut tx, item_id, e.fam.user("alex"), None).await;
    let Err(ownhealth_core::Error::Review(why)) = refused else {
        panic!("a reopened item of a removed upload was accepted: {refused:?}");
    };
    assert!(why.contains("upload was removed"), "{why}");
    tx.rollback().await?;
    assert_eq!(
        e.count("select count(*) from job where kind = 'import.dicom' and state = 'queued'")
            .await?,
        0
    );
    Ok(())
}

#[tokio::test]
async fn an_upload_with_a_recorded_study_or_a_running_import_stays() -> R {
    let e = env().await?;
    // one disc: Alex's study, recorded, and Carmen's, held
    let mut disc = study_files(&Study::new(U1, ALEX), "DICOM/P1");
    disc.extend(study_files(&Study::new(U3, CARMEN), "DICOM/P2"));
    let (up, sha) = e.upload("alex", &disc).await?;
    // queued: the removal waits for the import
    let mut tx = e.rt.pool.begin().await?;
    assert_eq!(
        remove::remove(&mut tx, up.import_id, None, true).await?,
        Removal::Importing
    );
    tx.rollback().await?;
    e.import().await?;
    let held = e.in_review("alex").await?;
    assert_eq!(held.len(), 1);
    let mut tx = e.rt.pool.begin().await?;
    ownhealth_core::review::dismiss(&mut tx, held[0].0, e.fam.user("alex"), None).await?;
    let item = ownhealth_core::review::get_item(&mut tx, held[0].0)
        .await?
        .ok_or_else(|| anyhow::anyhow!("no item"))?;
    assert!(!remove::settle(&mut tx, &item).await?);
    assert_eq!(
        remove::remove(&mut tx, up.import_id, None, true).await?,
        Removal::Recorded
    );
    tx.commit().await?;
    assert!(e.rt.blobs.exists(BlobOwner::Profile(e.fam.profile("alex")), &sha));
    assert!(e.recorded(U1).await?.is_some());
    Ok(())
}
