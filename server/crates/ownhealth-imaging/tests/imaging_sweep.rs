//! Slice 43: Orthanc follows deletion, retraction and restore.
//!
//! Each test imports synthetic hospital CDs (support/mod.rs) into a fake Orthanc that files studies by
//! PatientID and UID as Orthanc does, then changes the record (a profile delete, a retraction, a
//! restore that comes back to an empty Orthanc) and runs the `imaging.sweep` and `imaging.rebuild`
//! jobs. Synthetic people only.

mod support;

use std::sync::atomic::Ordering;
use std::time::Duration;

use ownhealth_core::Runtime;
use ownhealth_core::blobs::BlobOwner;
use ownhealth_core::demo::{Family, create_rivera_family};
use ownhealth_core::documents::{NewDocument, create_document};
use ownhealth_core::jobs::{self, JobContext};
use ownhealth_core::orthanc::lock_study;
use ownhealth_core::review::{self, ReviewItem};
use ownhealth_core::testing::TestDb;
use ownhealth_imaging::import::{JOB, REVIEW_KIND};
use ownhealth_imaging::orthanc::Orthanc;
use ownhealth_imaging::remove::{self, RELEASE_JOB, Removal};
use ownhealth_imaging::upload::{self, Recorded};
use ownhealth_imaging::{rebuild, sweep};
use serde_json::Value;
use sqlx::PgConnection;
use support::{ALEX, CARMEN, EXPLICIT_LE, FakeOrthanc, SAM, Study, study_files, zip_files};
use tokio::task::JoinHandle;
use uuid::Uuid;

type R = anyhow::Result<()>;

const U1: &str = "1.2.826.0.1.3680043.10.999.43.1";
const U2: &str = "1.2.826.0.1.3680043.10.999.43.2";
const U3: &str = "1.2.826.0.1.3680043.10.999.43.3";
const U9: &str = "1.2.826.0.1.3680043.10.999.43.9";

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
            &self.rt.blobs,
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
        let id = sweep::enqueue(&mut conn)
            .await?
            .ok_or_else(|| anyhow::anyhow!("a sweep is queued already"))?;
        drop(conn);
        assert_eq!(self.run(sweep::JOB).await?, 1);
        // that sweep's own result: one that found Orthanc down queues another
        let (state, result): (String, Value) =
            sqlx::query_as("select state, coalesce(result, 'null'::jsonb) from job where id = $1")
                .bind(id)
                .fetch_one(&self.rt.pool)
                .await?;
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

/// Runs the queued jobs of one kind in a task of its own, so the test can hold a lock meanwhile.
fn spawn_run(e: &Env, kind: &str) -> JoinHandle<anyhow::Result<usize>> {
    let (ctx, kind) = (e.ctx.clone(), kind.to_string());
    tokio::spawn(async move { Ok(jobs::drain(&ctx, "core", 20, Some(&[kind])).await?) })
}

/// Waits until a session of this test's database waits on a lock (true), or `task` ended (false).
async fn until_waiting<T>(e: &Env, task: &JoinHandle<T>) -> anyhow::Result<bool> {
    for _ in 0..400 {
        if task.is_finished() {
            return Ok(false);
        }
        let waiting: i64 = sqlx::query_scalar(
            "select count(*) from pg_stat_activity where datname = current_database() and wait_event_type = 'Lock'",
        )
        .fetch_one(&e.rt.pool)
        .await?;
        if waiting > 0 {
            return Ok(true);
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    anyhow::bail!("nothing waited on a lock, and the task did not end")
}

async fn item(conn: &mut PgConnection, id: Uuid) -> anyhow::Result<ReviewItem> {
    review::get_item(conn, id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("no item {id}"))
}

/// A disc of two other patients uploaded to Alex: both studies wait in review, nothing is recorded.
async fn held_twice(e: &Env) -> anyhow::Result<(Recorded, String, Vec<Uuid>)> {
    let mut disc = study_files(&Study::new(U3, CARMEN), "DICOM/P1");
    disc.extend(study_files(&Study::new(U2, SAM), "DICOM/P2"));
    let (up, sha) = e.upload("alex", &disc).await?;
    e.import().await?;
    let held: Vec<Uuid> = e.in_review("alex").await?.into_iter().map(|(id, ..)| id).collect();
    assert_eq!(held.len(), 2, "{held:?}");
    Ok((up, sha, held))
}

async fn uploads_left(e: &Env, up: &Recorded) -> anyhow::Result<i64> {
    e.count(&format!(
        "select count(*) from imaging_import where id = '{}'",
        up.import_id
    ))
    .await
}

#[tokio::test]
async fn two_held_studies_of_one_upload_closed_at_once_both_close_and_the_upload_goes() -> R {
    let e = env().await?;
    let (up, _, held) = held_twice(&e).await?;
    let user = e.fam.user("alex");
    let mut first = e.rt.pool.begin().await?;
    review::dismiss(&mut first, held[0], user, None).await?;
    let one = item(&mut first, held[0]).await?;
    let mut second = e.rt.pool.begin().await?;
    review::dismiss(&mut second, held[1], user, None).await?;
    let two = item(&mut second, held[1]).await?;
    // the first close settles while the second is still uncommitted, then the second settles
    let a = tokio::spawn(async move {
        let removed = remove::settle(&mut first, &one).await?;
        first.commit().await?;
        anyhow::Ok(removed)
    });
    until_waiting(&e, &a).await?;
    let b = tokio::spawn(async move {
        let removed = remove::settle(&mut second, &two).await?;
        second.commit().await?;
        anyhow::Ok(removed)
    });
    let (a, b) = (a.await?, b.await?);
    assert!(matches!((&a, &b), (Ok(false), Ok(true))), "first {a:?}, second {b:?}");
    assert_eq!(uploads_left(&e, &up).await?, 0);
    assert!(e.in_review("alex").await?.is_empty());
    Ok(())
}

#[tokio::test]
async fn removing_an_upload_while_one_of_its_held_studies_is_closed_does_not_deadlock() -> R {
    let e = env().await?;
    let (up, _, held) = held_twice(&e).await?;
    let user = e.fam.user("alex");
    let mut closing = e.rt.pool.begin().await?;
    review::dismiss(&mut closing, held[0], user, None).await?;
    let one = item(&mut closing, held[0]).await?;
    let (pool, import_id) = (e.rt.pool.clone(), up.import_id);
    let removing = tokio::spawn(async move {
        let mut tx = pool.begin().await?;
        let r = remove::remove(&mut tx, import_id, Some(user), true).await?;
        tx.commit().await?;
        anyhow::Ok(r)
    });
    until_waiting(&e, &removing).await?;
    let settled = match remove::settle(&mut closing, &one).await {
        Ok(s) => {
            closing.commit().await?;
            Ok(s)
        }
        Err(err) => Err(err),
    };
    let removed = removing.await?;
    assert!(matches!(settled, Ok(false)), "the close: {settled:?}");
    assert!(
        matches!(removed, Ok(Removal::Removed { dismissed: 1 })),
        "the removal: {removed:?}"
    );
    assert_eq!(uploads_left(&e, &up).await?, 0);
    Ok(())
}

#[tokio::test]
async fn the_sweep_keeps_a_study_an_import_records_while_it_waits_for_the_lock() -> R {
    let e = env().await?;
    e.upload("alex", &study_files(&Study::new(U1, ALEX), "DICOM")).await?;
    e.import().await?;
    let (_, id, ..) = e.recorded(U1).await?.ok_or_else(|| anyhow::anyhow!("not recorded"))?;
    let id = id.ok_or_else(|| anyhow::anyhow!("no Orthanc id"))?;
    // the record does not name the study yet, as while an import records it under the UID lock
    sqlx::query("update imaging_study set orthanc_id = null where study_instance_uid = $1")
        .bind(U1)
        .execute(&e.rt.pool)
        .await?;
    let mut import = e.rt.pool.begin().await?;
    lock_study(&mut import, U1).await?;
    let mut conn = e.rt.pool.acquire().await?;
    sweep::enqueue(&mut conn).await?;
    drop(conn);
    let sweeping = spawn_run(&e, sweep::JOB);
    assert!(
        until_waiting(&e, &sweeping).await?,
        "the sweep did not wait for the UID lock"
    );
    sqlx::query("update imaging_study set orthanc_id = $1 where study_instance_uid = $2")
        .bind(&id)
        .bind(U1)
        .execute(&mut *import)
        .await?;
    import.commit().await?;
    assert_eq!(sweeping.await??, 1);
    let (state, swept) = e.result(sweep::JOB).await?;
    assert_eq!(state, "done", "{swept}");
    assert_eq!(swept["removed_studies"], 0, "{swept}");
    assert_eq!(
        e.orthanc.instances_under(U1),
        6,
        "the sweep deleted a study the record names"
    );
    Ok(())
}

#[tokio::test]
async fn the_sweep_keeps_a_series_an_import_counts_while_it_waits_for_the_lock() -> R {
    let e = env().await?;
    let three = Study {
        series: 3,
        ..Study::new(U1, ALEX)
    };
    e.upload("alex", &series_files(&three, &[0, 1], "DICOM")).await?;
    e.import().await?;
    // a second disc's import sends a third series and counts it under the UID lock
    let client = Orthanc::new(e.orthanc.config())?;
    client.store(support::image(&three, 2, 0, EXPLICIT_LE)).await?;
    let mut import = e.rt.pool.begin().await?;
    lock_study(&mut import, U1).await?;
    let mut conn = e.rt.pool.acquire().await?;
    sweep::enqueue(&mut conn).await?;
    drop(conn);
    let sweeping = spawn_run(&e, sweep::JOB);
    assert!(
        until_waiting(&e, &sweeping).await?,
        "the sweep did not wait for the UID lock"
    );
    sqlx::query(
        "update imaging_study set fhir = jsonb_set(fhir, '{series}', \
         (fhir->'series') || jsonb_build_array(jsonb_build_object('uid', $1::text))) \
         where study_instance_uid = $2",
    )
    .bind(three.series_uid(2))
    .bind(U1)
    .execute(&mut *import)
    .await?;
    import.commit().await?;
    assert_eq!(sweeping.await??, 1);
    let (state, swept) = e.result(sweep::JOB).await?;
    assert_eq!(state, "done", "{swept}");
    assert_eq!(swept["removed_series"], 0, "{swept}");
    assert_eq!(
        e.orthanc.series_under(U1).len(),
        3,
        "the sweep deleted a series the record counts"
    );
    Ok(())
}

#[tokio::test]
async fn a_rebuild_sends_nothing_for_a_study_the_record_let_go_while_it_waited() -> R {
    let e = env().await?;
    e.upload("alex", &study_files(&Study::new(U1, ALEX), "DICOM")).await?;
    e.import().await?;
    e.orthanc.wipe();
    let mut conn = e.rt.pool.acquire().await?;
    assert!(rebuild::enqueue(&mut conn, e.fam.profile("alex")).await?.is_some());
    drop(conn);
    let mut hold = e.rt.pool.begin().await?;
    lock_study(&mut hold, U1).await?;
    let rebuilding = spawn_run(&e, rebuild::JOB);
    assert!(
        until_waiting(&e, &rebuilding).await?,
        "the rebuild did not wait for the UID lock"
    );
    // the record lets the study go meanwhile
    sqlx::query("update imaging_study set orthanc_id = null where study_instance_uid = $1")
        .bind(U1)
        .execute(&mut *hold)
        .await?;
    hold.commit().await?;
    assert_eq!(rebuilding.await??, 1);
    let (state, rebuilt) = e.result(rebuild::JOB).await?;
    assert_eq!(state, "done", "{rebuilt}");
    assert_eq!(rebuilt["sent_files"], 0, "{rebuilt}");
    assert!(
        e.orthanc.held().instances.is_empty(),
        "the rebuild sent a study no record names"
    );
    Ok(())
}

#[tokio::test]
async fn closing_the_last_other_held_study_waits_for_an_accept_in_flight() -> R {
    let e = env().await?;
    let (up, _, held) = held_twice(&e).await?;
    let user = e.fam.user("alex");
    let mut accepting = e.rt.pool.begin().await?;
    review::accept(&mut accepting, held[0], user, None).await?;
    let (pool, other) = (e.rt.pool.clone(), held[1]);
    let closing = tokio::spawn(async move {
        let mut tx = pool.begin().await?;
        review::dismiss(&mut tx, other, user, None).await?;
        let it = item(&mut tx, other).await?;
        let removed = remove::settle(&mut tx, &it).await?;
        tx.commit().await?;
        anyhow::Ok(removed)
    });
    let waited = until_waiting(&e, &closing).await?;
    accepting.commit().await?;
    assert!(waited, "the removal did not wait for the accept in flight");
    assert!(!closing.await??, "the upload went while its accepted study was queued");
    assert_eq!(uploads_left(&e, &up).await?, 1);
    Ok(())
}

#[tokio::test]
async fn a_study_orthanc_holds_without_a_counted_series_is_rebuilt() -> R {
    let e = env().await?;
    let three = Study {
        series: 3,
        ..Study::new(U1, ALEX)
    };
    e.upload("alex", &study_files(&three, "DICOM")).await?;
    e.import().await?;
    let (_, id, ..) = e.recorded(U1).await?.ok_or_else(|| anyhow::anyhow!("not recorded"))?;
    let id = id.ok_or_else(|| anyhow::anyhow!("no Orthanc id"))?;
    // Orthanc lost one counted series
    let client = Orthanc::new(e.orthanc.config())?;
    let series = client.series_ids(&id).await?.unwrap_or_default();
    let lost = series
        .iter()
        .find(|(_, uid)| *uid == three.series_uid(1))
        .ok_or_else(|| anyhow::anyhow!("no series 1"))?;
    client.delete_series(&lost.0).await?;
    assert_eq!(e.orthanc.series_under(U1).len(), 2);

    let swept = e.sweep().await?;
    assert_eq!(swept["rebuild_profiles"], 1, "{swept}");
    assert_eq!(e.run(rebuild::JOB).await?, 1);
    let (state, rebuilt) = e.result(rebuild::JOB).await?;
    assert_eq!(state, "done", "{rebuilt}");
    assert_eq!(
        (rebuilt["rebuilt"].clone(), rebuilt["missing"].clone()),
        (1.into(), 0.into()),
        "{rebuilt}"
    );
    assert_eq!(
        e.orthanc.series_under(U1),
        (0..3).map(|s| three.series_uid(s)).collect()
    );
    assert_eq!(e.orthanc.instances_under(U1), 9);
    Ok(())
}

#[tokio::test]
async fn a_rebuilt_file_orthanc_files_under_another_study_is_taken_back() -> R {
    let e = env().await?;
    e.upload("alex", &study_files(&Study::new(U1, ALEX), "DICOM")).await?;
    e.import().await?;
    e.orthanc.wipe();
    // the restored Orthanc reads U1's files as another study
    if let Ok(mut m) = e.orthanc.misread.lock() {
        *m = Some((U1.to_string(), U9.to_string()));
    }
    let swept = e.sweep().await?;
    assert_eq!(swept["rebuild_profiles"], 1, "{swept}");
    assert_eq!(e.run(rebuild::JOB).await?, 1);
    let (state, rebuilt) = e.result(rebuild::JOB).await?;
    assert_eq!(state, "done", "{rebuilt}");
    assert_eq!(rebuilt["misfiled_files"], 6, "{rebuilt}");
    assert_eq!(rebuilt["missing"], 1, "{rebuilt}");
    assert!(
        e.orthanc.held().instances.is_empty(),
        "a misfiled file stayed: {:?}",
        e.orthanc.held().studies
    );
    // that rebuild could not bring the study back: the hourly sweep does not read the same zips again
    let again = e.sweep().await?;
    assert_eq!(again["rebuild_profiles"], 1, "{again}");
    assert_eq!(again["rebuilds_queued"], 0, "{again}");
    Ok(())
}

#[tokio::test]
async fn a_misfiled_rebuilt_file_whose_take_back_failed_goes_at_the_retry() -> R {
    let e = env().await?;
    let st = Study::new(U1, ALEX);
    e.upload("alex", &study_files(&st, "DICOM")).await?;
    e.import().await?;
    let named = e
        .recorded(U1)
        .await?
        .and_then(|r| r.1)
        .ok_or_else(|| anyhow::anyhow!("U1 is not recorded"))?;
    e.orthanc.wipe();
    // the restored Orthanc files the first series under another PatientID: a second study of U1
    *e.orthanc.misread_patient.lock().expect("test setting") = Some((ALEX.id.into(), "LN-MISREAD".into()));
    *e.orthanc.misread_except_series.lock().expect("test setting") = Some(st.series_uid(1));
    // and the first take-back of a misfiled file fails
    let next = e.orthanc.deletes.load(Ordering::SeqCst) + 1;
    e.orthanc.fail_delete_at.store(next, Ordering::SeqCst);
    let swept = e.sweep().await?;
    assert_eq!(swept["rebuild_profiles"], 1, "{swept}");
    assert_eq!(e.run(rebuild::JOB).await?, 1);
    let (state, _) = e.result(rebuild::JOB).await?;
    assert_eq!(state, "queued", "the rebuild did not retry");
    assert!(
        e.orthanc.held().studies.keys().any(|id| *id != named),
        "the failed take-back left no stray study"
    );
    // the retry: Orthanc answers AlreadyStored for the file it kept in the stray study
    sqlx::query("update job set run_after = now() where kind = $1 and state = 'queued'")
        .bind(rebuild::JOB)
        .execute(&e.rt.pool)
        .await?;
    assert_eq!(e.run(rebuild::JOB).await?, 1);
    let (state, rebuilt) = e.result(rebuild::JOB).await?;
    assert_eq!(state, "done", "{rebuilt}");
    let held = e.orthanc.held();
    assert_eq!(
        held.studies.keys().cloned().collect::<Vec<_>>(),
        vec![named.clone()],
        "a misfiled file stayed in another study of U1"
    );
    // the study the record names keeps the series Orthanc filed there
    let kept: Vec<&(String, String)> = held.instances.values().collect();
    assert_eq!(kept.len(), st.per_series, "{kept:?}");
    assert!(
        kept.iter()
            .all(|(study, series)| *study == named && *series == st.series_uid(1))
    );
    Ok(())
}

#[tokio::test]
async fn one_study_orthanc_cannot_delete_neither_stops_the_sweep_nor_passes_unseen() -> R {
    let e = env().await?;
    let client = Orthanc::new(e.orthanc.config())?;
    client
        .store(support::image(&Study::new(U2, SAM), 0, 0, EXPLICIT_LE))
        .await?;
    client
        .store(support::image(&Study::new(U3, CARMEN), 0, 0, EXPLICIT_LE))
        .await?;
    let ids: Vec<String> = e.orthanc.held().studies.keys().cloned().collect();
    assert_eq!(ids.len(), 2);
    // the study the sweep reaches first always fails to go
    if let Ok(mut f) = e.orthanc.fail_study_delete.lock() {
        *f = Some(ids[0].clone());
    }
    let mut conn = e.rt.pool.acquire().await?;
    sweep::enqueue(&mut conn).await?;
    drop(conn);
    assert_eq!(e.run(sweep::JOB).await?, 1);
    let held = e.orthanc.held();
    assert!(
        !held.studies.contains_key(&ids[1]),
        "one failing study kept the next one"
    );
    assert!(held.studies.contains_key(&ids[0]));
    // the sweep does not end done: it retries, and ends with a job_failed item if it keeps failing
    let (state, error): (String, Option<String>) =
        sqlx::query_as("select state, last_error from job where kind = $1 order by id desc limit 1")
            .bind(sweep::JOB)
            .fetch_one(&e.rt.pool)
            .await?;
    assert_eq!(state, "queued", "{error:?}");
    assert!(error.is_some_and(|m| m.contains("1 ")), "no error recorded");
    Ok(())
}

#[tokio::test]
async fn a_sweep_that_finds_orthanc_down_tries_again_in_five_minutes_once() -> R {
    let e = env().await?;
    e.orthanc.down.store(true, Ordering::SeqCst);
    let waited = e.sweep().await?;
    assert_eq!(waited["skipped"], "orthanc_unreachable", "{waited}");
    let later = "select count(*) from job where kind = 'imaging.sweep' and state = 'queued' \
                 and run_after > now() + interval '4 minutes' and run_after < now() + interval '6 minutes'";
    assert_eq!(e.count(later).await?, 1, "no sweep queued for later");
    // that retry finds Orthanc down too: the hourly sweep takes over, no third one is queued
    sqlx::query("update job set run_after = now() where kind = 'imaging.sweep' and state = 'queued'")
        .execute(&e.rt.pool)
        .await?;
    assert_eq!(e.run(sweep::JOB).await?, 1);
    let queued = "select count(*) from job where kind = 'imaging.sweep' and state = 'queued'";
    assert_eq!(e.count(queued).await?, 0);
    Ok(())
}

#[tokio::test]
async fn a_rebuild_that_stopped_short_before_a_restore_does_not_hold_back_the_next() -> R {
    let e = env().await?;
    let alex = e.fam.profile("alex");
    // a restore brings back the job table, with a rebuild that could not bring every study back
    sqlx::query(
        "insert into job (kind, payload, dedupe_key, state, finished_at, result) \
         values ($1, jsonb_build_object('profile_id', $2::text), $3, 'done', now() - interval '1 hour', \
         '{\"missing\": 1}')",
    )
    .bind(rebuild::JOB)
    .bind(alex.to_string())
    .bind(format!("{}:{alex}", rebuild::JOB))
    .execute(&e.rt.pool)
    .await?;
    let mut conn = e.rt.pool.acquire().await?;
    let queued = rebuild::enqueue(&mut conn, alex).await?;
    assert!(
        queued.is_some(),
        "a rebuild from before this start held the next one back"
    );
    Ok(())
}

#[tokio::test]
async fn a_rebuild_retries_when_the_stored_zip_returns() -> R {
    let e = env().await?;
    let files = study_files(&Study::new(U1, ALEX), "DICOM");
    let (_, sha) = e.upload("alex", &files).await?;
    e.import().await?;
    e.orthanc.wipe();
    let owner = BlobOwner::Profile(e.fam.profile("alex"));
    assert!(e.rt.blobs.delete(owner, &sha)?);

    let swept = e.sweep().await?;
    assert_eq!(swept["rebuilds_queued"], 1, "{swept}");
    assert_eq!(e.run(rebuild::JOB).await?, 1);
    let (state, result) = e.result(rebuild::JOB).await?;
    assert_eq!(state, "queued", "{result}");

    assert_eq!(e.rt.blobs.put_bytes(owner, &zip_files(&files))?, sha);
    sqlx::query("update job set run_after = now() where kind = $1 and state = 'queued'")
        .bind(rebuild::JOB)
        .execute(&e.rt.pool)
        .await?;
    assert_eq!(e.run(rebuild::JOB).await?, 1);
    let (state, result) = e.result(rebuild::JOB).await?;
    assert_eq!(state, "done", "{result}");
    assert_eq!(result["rebuilt"], 1, "{result}");
    assert_eq!(result["missing"], 0, "{result}");
    Ok(())
}

#[tokio::test]
async fn a_rebuild_that_exhausts_its_retries_holds_back_the_next_sweeps() -> R {
    let e = env().await?;
    let files = study_files(&Study::new(U1, ALEX), "DICOM");
    let (_, sha) = e.upload("alex", &files).await?;
    e.import().await?;
    e.orthanc.wipe();
    assert!(e.rt.blobs.delete(BlobOwner::Profile(e.fam.profile("alex")), &sha)?);

    let first = e.sweep().await?;
    assert_eq!(first["rebuilds_queued"], 1, "{first}");
    for _ in 0..5 {
        sqlx::query("update job set run_after = now() where kind = $1 and state = 'queued'")
            .bind(rebuild::JOB)
            .execute(&e.rt.pool)
            .await?;
        assert_eq!(e.run(rebuild::JOB).await?, 1);
    }
    assert_eq!(e.result(rebuild::JOB).await?.0, "dead");

    for _ in 0..2 {
        let swept = e.sweep().await?;
        assert_eq!(swept["rebuild_profiles"], 1, "{swept}");
        assert_eq!(swept["rebuilds_queued"], 0, "{swept}");
        assert_eq!(e.run(rebuild::JOB).await?, 0);
    }
    assert_eq!(
        e.count("select count(*) from review_item where kind = 'job_failed' and payload->>'kind' = 'imaging.rebuild'")
            .await?,
        1
    );
    Ok(())
}

#[tokio::test]
async fn a_reupload_of_the_same_zip_racing_the_removal_keeps_the_upload_and_its_zip() -> R {
    let e = env().await?;
    let alex = e.fam.profile("alex");
    let (up, sha) = e.upload("alex", &study_files(&Study::new(U3, CARMEN), "DICOM")).await?;
    e.import().await?;
    assert_eq!(e.in_review("alex").await?.len(), 1);
    // the same zip again, its import queued in a transaction that has not committed yet
    let mut again = e.rt.pool.begin().await?;
    let r = upload::record(&mut again, &e.rt.blobs, alex, &sha, 1, Some("cd.zip"), 6, None).await?;
    assert!(r.duplicate && r.job_id.is_some(), "{r:?}");
    let (pool, import_id) = (e.rt.pool.clone(), up.import_id);
    let removing = tokio::spawn(async move {
        let mut tx = pool.begin().await?;
        let r = remove::remove(&mut tx, import_id, None, true).await?;
        tx.commit().await?;
        anyhow::Ok(r)
    });
    until_waiting(&e, &removing).await?;
    again.commit().await?;
    let removed = removing.await??;
    assert_eq!(removed, Removal::Importing);
    assert_eq!(uploads_left(&e, &up).await?, 1);
    assert!(e.rt.blobs.exists(BlobOwner::Profile(alex), &sha));
    Ok(())
}

#[tokio::test]
async fn the_release_keeps_the_zip_while_another_row_of_the_profile_names_its_bytes() -> R {
    let e = env().await?;
    let alex = e.fam.profile("alex");
    let (up, sha) = e.upload("alex", &study_files(&Study::new(U3, CARMEN), "DICOM")).await?;
    e.import().await?;
    // a page image of one of Alex's documents with the same bytes (any column of BLOB_COLUMNS)
    let other = "ab".repeat(32);
    let mut tx = e.rt.pool.begin().await?;
    let doc = create_document(
        &mut tx,
        &NewDocument::upload(e.fam.household_id, Some(alex), &other, "application/pdf", 10),
    )
    .await?;
    sqlx::query(
        "insert into document_page (document_id, page_no, width_pt, height_pt, text_layer_ok, image_blob) \
         values ($1, 1, 595, 842, false, decode($2, 'hex'))",
    )
    .bind(doc.id)
    .bind(&sha)
    .execute(&mut *tx)
    .await?;
    assert!(matches!(
        remove::remove(&mut tx, up.import_id, None, true).await?,
        Removal::Removed { .. }
    ));
    tx.commit().await?;
    assert_eq!(e.run(RELEASE_JOB).await?, 1);
    assert_eq!(e.result(RELEASE_JOB).await?.0, "done");
    assert!(
        e.rt.blobs.exists(BlobOwner::Profile(alex), &sha),
        "the release removed bytes a document page of the profile names"
    );
    Ok(())
}

#[tokio::test]
async fn a_study_of_a_deleted_profile_a_restore_brought_back_leaves_orthanc_and_is_not_rebuilt() -> R {
    let e = env().await?;
    let alex = e.fam.profile("alex");
    e.upload("alex", &study_files(&Study::new(U1, ALEX), "DICOM")).await?;
    e.upload("sam", &study_files(&Study::new(U2, SAM), "DICOM")).await?;
    assert_eq!(e.import().await?, 2);
    // a restore of a backup taken before Alex's delete brings his rows back with a shredded key, while
    // the Orthanc volume was kept
    let mut conn = e.rt.pool.acquire().await?;
    ownhealth_core::keys::shred(&mut conn, alex).await?;
    drop(conn);
    assert!(e.recorded(U1).await?.is_some());

    let swept = e.sweep().await?;
    assert_eq!(swept["removed_studies"], 1, "{swept}");
    assert!(
        e.orthanc.patients_under(U1).is_empty(),
        "a closed profile's pixels stayed in Orthanc"
    );
    assert_eq!(e.orthanc.instances_under(U2), 6);
    // and Orthanc lacking it now queues no rebuild of its zips
    let again = e.sweep().await?;
    assert_eq!(again["rebuild_profiles"], 0, "{again}");
    let mut conn = e.rt.pool.acquire().await?;
    assert!(rebuild::enqueue(&mut conn, alex).await?.is_some());
    drop(conn);
    assert_eq!(e.run(rebuild::JOB).await?, 1);
    let (state, rebuilt) = e.result(rebuild::JOB).await?;
    assert_eq!(state, "done", "{rebuilt}");
    assert_eq!(rebuilt["studies"], 0, "{rebuilt}");
    assert!(
        e.orthanc.patients_under(U1).is_empty(),
        "the rebuild sent a closed study"
    );
    Ok(())
}

#[tokio::test]
async fn a_study_orthanc_holds_with_one_instance_missing_is_rebuilt() -> R {
    let e = env().await?;
    e.upload("alex", &study_files(&Study::new(U1, ALEX), "DICOM")).await?;
    e.import().await?;
    // Orthanc lost one instance of a counted series; every series is still there
    let lost = e
        .orthanc
        .held()
        .instances
        .keys()
        .next()
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("no instance"))?;
    Orthanc::new(e.orthanc.config())?.delete_instance(&lost).await?;
    assert_eq!(e.orthanc.instances_under(U1), 5);
    assert_eq!(e.orthanc.series_under(U1).len(), 2);

    let swept = e.sweep().await?;
    assert_eq!(swept["rebuild_profiles"], 1, "{swept}");
    assert_eq!(e.run(rebuild::JOB).await?, 1);
    let (state, rebuilt) = e.result(rebuild::JOB).await?;
    assert_eq!(state, "done", "{rebuilt}");
    assert_eq!(rebuilt["missing"], 0, "{rebuilt}");
    assert_eq!(e.orthanc.instances_under(U1), 6);
    Ok(())
}

#[tokio::test]
async fn a_study_recorded_before_the_upload_links_rebuilds_from_every_upload_that_holds_it() -> R {
    let e = env().await?;
    let three = Study {
        series: 3,
        ..Study::new(U1, ALEX)
    };
    let (first, _) = e.upload("alex", &series_files(&three, &[0, 1], "DICOM")).await?;
    e.import().await?;
    let (later, _) = e.upload("alex", &series_files(&three, &[2], "DICOM")).await?;
    e.import().await?;
    // as migration 1234 left a study recorded before it: linked to its first upload only
    as_before_the_links(&e, &[later.import_id]).await?;
    let link = format!(
        "select count(*) from imaging_import_study where study_instance_uid = '{U1}' and import_id = '{}'",
        later.import_id
    );
    e.orthanc.wipe();
    assert_eq!(e.count(&link).await?, 0);
    let rebuilt = rebuild_alex(&e).await?;
    assert_eq!(rebuilt["linked_uploads"], 1, "{rebuilt}");
    assert_eq!(
        (rebuilt["rebuilt"].clone(), rebuilt["missing"].clone()),
        (1.into(), 0.into()),
        "{rebuilt}"
    );
    assert_eq!(
        e.orthanc.series_under(U1),
        (0..3).map(|s| three.series_uid(s)).collect()
    );
    assert_eq!(e.orthanc.instances_under(U1), 9);
    // the upload that held the third series is linked now, and the first one still
    assert_eq!(e.count(&link).await?, 1);
    let first_link = format!(
        "select count(*) from imaging_import_study where study_instance_uid = '{U1}' and import_id = '{}'",
        first.import_id
    );
    assert_eq!(e.count(&first_link).await?, 1);
    Ok(())
}

/// Links of upload `import_id` to study `uid`.
async fn links(e: &Env, import_id: Uuid, uid: &str) -> anyhow::Result<i64> {
    e.count(&format!(
        "select count(*) from imaging_import_study where study_instance_uid = '{uid}' and import_id = '{import_id}'"
    ))
    .await
}

/// Runs a rebuild for Alex without first giving the legacy uploads their links in the sweep.
async fn rebuild_alex(e: &Env) -> anyhow::Result<Value> {
    let mut conn = e.rt.pool.acquire().await?;
    assert!(rebuild::enqueue(&mut conn, e.fam.profile("alex")).await?.is_some());
    drop(conn);
    assert_eq!(e.run(rebuild::JOB).await?, 1);
    let (state, rebuilt) = e.result(rebuild::JOB).await?;
    assert_eq!(state, "done", "{rebuilt}");
    Ok(rebuilt)
}

/// Queues a sweep and runs the rebuild it queues for Alex: the rebuild's result.
async fn sweep_and_rebuild(e: &Env) -> anyhow::Result<Value> {
    let swept = e.sweep().await?;
    assert_eq!(swept["rebuild_profiles"], 1, "{swept}");
    assert_eq!(e.run(rebuild::JOB).await?, 1);
    let (state, rebuilt) = e.result(rebuild::JOB).await?;
    assert_eq!(state, "done", "{rebuilt}");
    Ok(rebuilt)
}

#[tokio::test]
async fn an_upload_replayed_for_one_study_is_read_again_for_another_study_it_holds_unlinked() -> R {
    let e = env().await?;
    let b = Study {
        series: 3,
        ..Study::new(U2, ALEX)
    };
    e.upload("alex", &series_files(&b, &[0, 1], "DICOM/B")).await?;
    e.import().await?;
    // one disc with study U1 whole and the third series of U2
    let mut disc = study_files(&Study::new(U1, ALEX), "DICOM/A");
    disc.extend(series_files(&b, &[2], "DICOM/B"));
    let (x, _) = e.upload("alex", &disc).await?;
    e.import().await?;
    // as migration 1234 left it: the disc is linked to U1 only
    as_before_the_links(&e, &[]).await?;
    sqlx::query("delete from imaging_import_study where import_id = $1 and study_instance_uid = $2")
        .bind(x.import_id)
        .bind(U2)
        .execute(&e.rt.pool)
        .await?;
    assert_eq!(links(&e, x.import_id, U1).await?, 1);

    e.orthanc.wipe();
    assert_eq!(links(&e, x.import_id, U2).await?, 0);
    let rebuilt = rebuild_alex(&e).await?;
    assert_eq!(rebuilt["linked_uploads"], 1, "{rebuilt}");
    assert_eq!(
        (rebuilt["rebuilt"].clone(), rebuilt["missing"].clone()),
        (2.into(), 0.into()),
        "{rebuilt}"
    );
    assert_eq!(e.orthanc.series_under(U2), (0..3).map(|s| b.series_uid(s)).collect());
    assert_eq!(e.orthanc.instances_under(U1), 6);
    assert_eq!(links(&e, x.import_id, U2).await?, 1);
    Ok(())
}

#[tokio::test]
async fn a_legacy_upload_whose_initial_import_failed_is_neither_replayed_nor_linked() -> R {
    let e = env().await?;
    let four = Study {
        series: 4,
        ..Study::new(U1, ALEX)
    };
    e.upload("alex", &series_files(&four, &[0, 1], "DICOM")).await?;
    e.import().await?;
    let (later, _) = e.upload("alex", &series_files(&four, &[2], "DICOM")).await?;
    e.import().await?;
    let (failed, _) = e.upload("alex", &series_files(&four, &[3], "DICOM")).await?;
    sqlx::query("update job set state = 'dead', finished_at = now() where id = $1")
        .bind(failed.job_id)
        .execute(&e.rt.pool)
        .await?;
    as_before_the_links(&e, &[later.import_id, failed.import_id]).await?;

    e.orthanc.wipe();
    let rebuilt = rebuild_alex(&e).await?;
    assert_eq!(rebuilt["rebuilt"], 1, "{rebuilt}");
    let replayed = e.orthanc.series_under(U1);
    let fallback_link = links(&e, failed.import_id, U1).await?;
    sqlx::query("delete from imaging_import_study where import_id = $1")
        .bind(failed.import_id)
        .execute(&e.rt.pool)
        .await?;

    e.sweep().await?;
    let sweep_link = links(&e, failed.import_id, U1).await?;
    assert_eq!(
        (fallback_link, sweep_link),
        (0, 0),
        "rebuild and sweep links to a failed import"
    );
    assert_eq!(replayed, (0..3).map(|s| four.series_uid(s)).collect());
    assert_eq!(links(&e, later.import_id, U1).await?, 1);
    assert_eq!(remove_now(&e, failed.import_id).await?, Removal::Recorded);
    Ok(())
}

#[tokio::test]
async fn a_dead_legacy_upload_with_no_recorded_study_can_be_removed_after_the_sweep() -> R {
    let e = env().await?;
    e.upload("alex", &study_files(&Study::new(U1, ALEX), "DICOM")).await?;
    e.import().await?;
    let (failed, _) = e.upload("alex", &study_files(&Study::new(U2, ALEX), "DICOM")).await?;
    sqlx::query("update job set state = 'dead', finished_at = now() where id = $1")
        .bind(failed.job_id)
        .execute(&e.rt.pool)
        .await?;
    as_before_the_links(&e, &[failed.import_id]).await?;
    assert_eq!(remove_now(&e, failed.import_id).await?, Removal::Recorded);

    let swept = e.sweep().await?;
    assert_eq!(swept["links_completed"], 2, "{swept}");
    assert_eq!(links(&e, failed.import_id, U1).await?, 0);
    assert!(matches!(
        remove_now(&e, failed.import_id).await?,
        Removal::Removed { .. }
    ));
    Ok(())
}

#[tokio::test]
async fn a_rebuild_leaves_out_an_upload_whose_part_of_the_study_waits_in_review() -> R {
    let e = env().await?;
    let three = Study {
        series: 3,
        ..Study::new(U1, ALEX)
    };
    e.upload("alex", &series_files(&three, &[0, 1], "DICOM")).await?;
    e.import().await?;
    let (held, _) = e.upload("alex", &series_files(&three, &[2], "DICOM")).await?;
    e.import().await?;
    // the later upload's part of U1 is held for an identity conflict instead: no link, an open item
    sqlx::query("delete from imaging_import_study where import_id = $1")
        .bind(held.import_id)
        .execute(&e.rt.pool)
        .await?;
    let item_id = review::create_item(
        &mut *e.rt.pool.acquire().await?,
        &review::ReviewItemIn {
            kind: REVIEW_KIND.into(),
            household_id: Some(e.fam.household_id),
            profile_id: Some(e.fam.profile("alex")),
            payload: serde_json::json!({"import_id": held.import_id, "study_uid": U1, "reason": "identity_conflict"}),
            reasons: vec!["identity_conflict".into()],
            ref_table: Some("imaging_import".into()),
            ref_id: Some(held.import_id),
            ..Default::default()
        },
    )
    .await?;

    e.orthanc.wipe();
    let rebuilt = sweep_and_rebuild(&e).await?;
    assert_eq!(
        (rebuilt["missing"].clone(), rebuilt["linked_uploads"].clone()),
        (1.into(), 0.into()),
        "{rebuilt}"
    );
    assert_eq!(
        e.orthanc.series_under(U1),
        (0..2).map(|s| three.series_uid(s)).collect()
    );
    assert_eq!(links(&e, held.import_id, U1).await?, 0);
    assert_eq!(e.in_review("alex").await?.len(), 1);
    assert_eq!(
        e.sweep().await?["rebuilds_queued"],
        0,
        "the held part cannot be rebuilt"
    );

    // dismissed, the held part stays out too: the person said it is not this study's
    let mut tx = e.rt.pool.begin().await?;
    review::dismiss(&mut tx, item_id, e.fam.user("alex"), None).await?;
    tx.commit().await?;
    sqlx::query("delete from job where kind = $1")
        .bind(rebuild::JOB)
        .execute(&e.rt.pool)
        .await?;
    let rebuilt = sweep_and_rebuild(&e).await?;
    assert_eq!(rebuilt["linked_uploads"], 0, "{rebuilt}");
    assert_eq!(
        e.orthanc.series_under(U1),
        (0..2).map(|s| three.series_uid(s)).collect()
    );
    assert_eq!(links(&e, held.import_id, U1).await?, 0);
    Ok(())
}

#[tokio::test]
async fn a_rebuild_sends_nothing_from_an_upload_removed_while_it_waited() -> R {
    let e = env().await?;
    let three = Study {
        series: 3,
        ..Study::new(U1, ALEX)
    };
    let (first, _) = e.upload("alex", &series_files(&three, &[0, 1], "DICOM")).await?;
    e.import().await?;
    let (later, _) = e.upload("alex", &series_files(&three, &[2], "DICOM")).await?;
    e.import().await?;
    // a study recorded before the links: none at all, and the study names the first upload only
    as_before_the_links(&e, &[first.import_id, later.import_id]).await?;
    e.orthanc.wipe();
    let mut conn = e.rt.pool.acquire().await?;
    assert!(rebuild::enqueue(&mut conn, e.fam.profile("alex")).await?.is_some());
    drop(conn);
    // the rebuild reads both uploads, then waits for the UID while the later one is removed
    let mut hold = e.rt.pool.begin().await?;
    lock_study(&mut hold, U1).await?;
    let rebuilding = spawn_run(&e, rebuild::JOB);
    assert!(
        until_waiting(&e, &rebuilding).await?,
        "the rebuild did not wait for the UID lock"
    );
    // the later upload's row goes (a removal refuses an upload from before the links while its
    // profile has a recorded study, so this stands for any delete of the row)
    sqlx::query("delete from imaging_import where id = $1")
        .bind(later.import_id)
        .execute(&e.rt.pool)
        .await?;
    hold.commit().await?;
    assert_eq!(rebuilding.await??, 1);
    let (state, rebuilt) = e.result(rebuild::JOB).await?;
    assert_eq!(state, "done", "{rebuilt}");
    assert_eq!(rebuilt["sent_files"], 6, "{rebuilt}");
    assert_eq!(
        e.orthanc.series_under(U1),
        (0..2).map(|s| three.series_uid(s)).collect()
    );
    assert_eq!(links(&e, first.import_id, U1).await?, 1);
    Ok(())
}

#[tokio::test]
async fn a_held_study_an_import_adds_while_the_removal_waits_keeps_the_upload() -> R {
    let e = env().await?;
    let (up, _) = e.upload("alex", &study_files(&Study::new(U3, CARMEN), "DICOM")).await?;
    e.import().await?;
    assert_eq!(e.in_review("alex").await?.len(), 1);
    // an accept in flight reads the upload with a shared lock: the removal waits on it
    let mut reading = e.rt.pool.begin().await?;
    sqlx::query("select id from imaging_import where id = $1 for share")
        .bind(up.import_id)
        .execute(&mut *reading)
        .await?;
    let (pool, import_id, user) = (e.rt.pool.clone(), up.import_id, e.fam.user("alex"));
    let removing = tokio::spawn(async move {
        let mut tx = pool.begin().await?;
        let r = remove::remove(&mut tx, import_id, Some(user), true).await?;
        tx.commit().await?;
        anyhow::Ok(r)
    });
    assert!(
        until_waiting(&e, &removing).await?,
        "the removal did not wait for the upload lock"
    );
    // meanwhile an import of the upload holds one more study and ends
    review::create_item(
        &mut reading,
        &review::ReviewItemIn {
            kind: REVIEW_KIND.into(),
            household_id: Some(e.fam.household_id),
            profile_id: Some(e.fam.profile("alex")),
            payload: serde_json::json!({"import_id": up.import_id, "study_uid": U2, "reason": "identity_mismatch"}),
            reasons: vec!["identity_mismatch".into()],
            ref_table: Some("imaging_import".into()),
            ref_id: Some(up.import_id),
            ..Default::default()
        },
    )
    .await?;
    reading.commit().await?;
    let removed = removing.await??;
    assert_eq!(removed, Removal::Waiting);
    assert_eq!(uploads_left(&e, &up).await?, 1);
    assert_eq!(e.in_review("alex").await?.len(), 2, "a held study was closed");
    Ok(())
}

#[tokio::test]
async fn closing_the_last_held_study_while_its_import_runs_removes_the_upload_when_the_import_ends() -> R {
    let e = env().await?;
    let alex = e.fam.profile("alex");
    // Alex's own study is on the record, retracted
    e.upload("alex", &study_files(&Study::new(U3, ALEX), "DICOM")).await?;
    e.import().await?;
    sqlx::query("update imaging_study set status = 'retracted', status_reason = 'entered in error' where study_instance_uid = $1")
        .bind(U3)
        .execute(&e.rt.pool)
        .await?;
    // a disc with Sam's study, which waits in review, and Alex's retracted one, which records nothing
    let mut disc = study_files(&Study::new(U2, SAM), "DICOM/P1");
    disc.extend(study_files(&Study::new(U3, ALEX), "DICOM/P2"));
    let (up, sha) = e.upload("alex", &disc).await?;
    let mut hold = e.rt.pool.begin().await?;
    lock_study(&mut hold, U3).await?;
    let importing = spawn_run(&e, JOB);
    assert!(
        until_waiting(&e, &importing).await?,
        "the import did not wait for the UID lock"
    );
    // the import still runs when the person closes Sam's study: the upload stays for now
    let held = e.in_review("alex").await?;
    assert_eq!(held.len(), 1, "{held:?}");
    let mut tx = e.rt.pool.begin().await?;
    review::dismiss(&mut tx, held[0].0, e.fam.user("alex"), None).await?;
    let it = item(&mut tx, held[0].0).await?;
    assert!(
        !remove::settle(&mut tx, &it).await?,
        "the upload went while its import ran"
    );
    tx.commit().await?;
    hold.commit().await?;
    assert_eq!(importing.await??, 1);
    let (state, result) = e.result(JOB).await?;
    assert_eq!(state, "done", "{result}");
    // once the import ended, the upload goes with its zip
    assert_eq!(uploads_left(&e, &up).await?, 0, "{result}");
    assert_eq!(e.run(RELEASE_JOB).await?, 1);
    assert!(!e.rt.blobs.exists(BlobOwner::Profile(alex), &sha), "the zip stayed");
    Ok(())
}

#[tokio::test]
async fn an_upload_whose_stored_zip_went_before_it_was_recorded_records_nothing() -> R {
    let e = env().await?;
    let alex = e.fam.profile("alex");
    let files = study_files(&Study::new(U1, ALEX), "DICOM");
    let zip = zip_files(&files);
    let owner = BlobOwner::Profile(alex);
    let sha = e.rt.blobs.put_bytes(owner, &zip)?;
    // the release of a removed upload of the same bytes deletes them after the route stored them
    assert!(e.rt.blobs.delete(owner, &sha)?);
    let mut tx = e.rt.pool.begin().await?;
    let refused = upload::record(
        &mut tx,
        &e.rt.blobs,
        alex,
        &sha,
        i64::try_from(zip.len())?,
        Some("cd.zip"),
        files.len(),
        None,
    )
    .await;
    match refused {
        Err(ownhealth_core::Error::Review(why)) => assert_eq!(why, upload::UPLOAD_LOST),
        other => {
            tx.commit().await?;
            panic!("an upload whose zip is gone was recorded: {other:?}");
        }
    }
    // nothing was written before the refusal: the rollback below would hide it
    let uploads: i64 = sqlx::query_scalar("select count(*) from imaging_import")
        .fetch_one(&mut *tx)
        .await?;
    let imports: i64 = sqlx::query_scalar("select count(*) from job where kind = 'import.dicom'")
        .fetch_one(&mut *tx)
        .await?;
    assert_eq!((uploads, imports), (0, 0), "the refused upload wrote first");
    tx.rollback().await?;
    assert_eq!(e.count("select count(*) from imaging_import").await?, 0);
    assert_eq!(
        e.count("select count(*) from job where kind = 'import.dicom'").await?,
        0
    );
    Ok(())
}

/// As migration 1234 leaves an install: every upload from before it is marked as not linking all its
/// studies, and `unlinked` ones (a second disc that added a series) have no link at all.
async fn as_before_the_links(e: &Env, unlinked: &[Uuid]) -> R {
    sqlx::query("update imaging_import set links_complete = false")
        .execute(&e.rt.pool)
        .await?;
    sqlx::query("delete from imaging_import_study where import_id = any($1)")
        .bind(unlinked)
        .execute(&e.rt.pool)
        .await?;
    Ok(())
}

/// `DELETE /api/imaging/imports/{id}` as Alex, in a transaction of its own.
async fn remove_now(e: &Env, import_id: Uuid) -> anyhow::Result<Removal> {
    let mut tx = e.rt.pool.begin().await?;
    let r = remove::remove(&mut tx, import_id, Some(e.fam.user("alex")), true).await?;
    tx.commit().await?;
    Ok(r)
}

#[tokio::test]
async fn an_upload_from_before_the_links_is_kept_while_its_profile_has_a_recorded_study() -> R {
    let e = env().await?;
    let three = Study {
        series: 3,
        ..Study::new(U1, ALEX)
    };
    e.upload("alex", &series_files(&three, &[0, 1], "DICOM")).await?;
    e.import().await?;
    let (later, _) = e.upload("alex", &series_files(&three, &[2], "DICOM")).await?;
    e.import().await?;
    // a disc to the wrong profile, on a profile with no recorded study
    let (stray, _) = e
        .upload("lucia", &study_files(&Study::new(U3, CARMEN), "DICOM"))
        .await?;
    e.import().await?;
    // the later disc added a series before migration 1234: nothing links it to U1
    as_before_the_links(&e, &[later.import_id]).await?;
    assert_eq!(remove_now(&e, later.import_id).await?, Removal::Recorded);
    assert_eq!(
        uploads_left(&e, &later).await?,
        1,
        "the upload of a recorded series went"
    );
    // where no study is recorded, an upload from before the links still goes
    assert!(matches!(
        remove_now(&e, stray.import_id).await?,
        Removal::Removed { .. }
    ));
    Ok(())
}

#[tokio::test]
async fn the_sweep_links_an_upload_from_before_the_links_so_one_with_no_recorded_part_can_go() -> R {
    let e = env().await?;
    let three = Study {
        series: 3,
        ..Study::new(U1, ALEX)
    };
    let (first, _) = e.upload("alex", &series_files(&three, &[0, 1], "DICOM")).await?;
    e.import().await?;
    let (later, _) = e.upload("alex", &series_files(&three, &[2], "DICOM")).await?;
    e.import().await?;
    // a disc to the wrong profile, on the profile that records U1: its study waits in review
    let (stray, _) = e.upload("alex", &study_files(&Study::new(U3, CARMEN), "DICOM")).await?;
    e.import().await?;
    assert_eq!(e.in_review("alex").await?.len(), 1);
    as_before_the_links(&e, &[later.import_id]).await?;
    assert_eq!(remove_now(&e, stray.import_id).await?, Removal::Recorded);

    // the sweep reads each upload from before the links and links the recorded studies it holds
    let swept = e.sweep().await?;
    assert!(
        matches!(remove_now(&e, stray.import_id).await?, Removal::Removed { .. }),
        "an upload with no recorded study still cannot go"
    );
    // the upload of a recorded series is linked to it now, and stays
    assert_eq!(links(&e, later.import_id, U1).await?, 1);
    assert_eq!(links(&e, first.import_id, U1).await?, 1);
    assert_eq!(remove_now(&e, later.import_id).await?, Removal::Recorded);
    assert_eq!(uploads_left(&e, &later).await?, 1);
    assert_eq!(swept["links_completed"], 3, "{swept}");
    let open = "select count(*) from imaging_import where not links_complete";
    assert_eq!(e.count(open).await?, 0);
    // nothing left to read at the next sweep
    assert_eq!(e.sweep().await?["links_completed"], 0);
    Ok(())
}

#[tokio::test]
async fn the_sweep_retries_legacy_links_after_a_zip_entry_fails_to_read() -> R {
    use std::io::{Cursor, Read};

    let e = env().await?;
    let study = Study {
        series: 3,
        ..Study::new(U1, ALEX)
    };
    e.upload("alex", &series_files(&study, &[0, 1], "DICOM")).await?;
    e.import().await?;
    let mut extra = series_files(&study, &[2], "DICOM");
    extra.truncate(1);
    let (later, sha) = e.upload("alex", &extra).await?;
    e.import().await?;
    as_before_the_links(&e, &[later.import_id]).await?;

    let mut zip = zip_files(&extra);
    let header = zip
        .windows(4)
        .rposition(|bytes| bytes == b"PK\x01\x02")
        .ok_or_else(|| anyhow::anyhow!("no ZIP central directory"))?;
    zip[header + 16] ^= 1; // the image entry's central directory CRC
    let mut archive = zip::ZipArchive::new(Cursor::new(&zip))?;
    assert!(archive.by_index(1)?.read_to_end(&mut Vec::new()).is_err());
    let owner = BlobOwner::Profile(e.fam.profile("alex"));
    let damaged = e.rt.blobs.put_bytes(owner, &zip)?;
    let point_to = "update imaging_import set zip_blob = $2 where id = $1";
    sqlx::query(point_to)
        .bind(later.import_id)
        .bind(hex::decode(damaged)?)
        .execute(&e.rt.pool)
        .await?;

    let swept = e.sweep().await?;
    assert_eq!(swept["links_completed"], 1, "{swept}");
    assert_eq!(links(&e, later.import_id, U1).await?, 0);
    assert_eq!(remove_now(&e, later.import_id).await?, Removal::Recorded);

    sqlx::query(point_to)
        .bind(later.import_id)
        .bind(hex::decode(sha)?)
        .execute(&e.rt.pool)
        .await?;
    assert_eq!(e.sweep().await?["links_completed"], 1);
    assert_eq!(links(&e, later.import_id, U1).await?, 1);
    assert_eq!(remove_now(&e, later.import_id).await?, Removal::Recorded);
    Ok(())
}

#[tokio::test]
async fn the_sweep_does_not_link_an_upload_whose_part_of_a_recorded_study_waits_in_review() -> R {
    let e = env().await?;
    e.upload("alex", &study_files(&Study::new(U1, ALEX), "DICOM")).await?;
    e.import().await?;
    // a disc of another patient under the same UID, to the same profile: held, never recorded
    let other = Study {
        series: 3,
        ..Study::new(U1, CARMEN)
    };
    let (held, _) = e.upload("alex", &series_files(&other, &[2], "DICOM")).await?;
    e.import().await?;
    assert_eq!(e.in_review("alex").await?.len(), 1);
    as_before_the_links(&e, &[]).await?;
    let swept = e.sweep().await?;
    assert_eq!(swept["links_completed"], 2, "{swept}");
    assert_eq!(links(&e, held.import_id, U1).await?, 0, "a held part was linked");
    assert!(matches!(remove_now(&e, held.import_id).await?, Removal::Removed { .. }));
    Ok(())
}

#[tokio::test]
async fn an_upload_a_close_left_while_its_import_ran_goes_at_the_next_sweep() -> R {
    let e = env().await?;
    let alex = e.fam.profile("alex");
    let (up, sha) = e.upload("alex", &study_files(&Study::new(U3, CARMEN), "DICOM")).await?;
    e.import().await?;
    let held = e.in_review("alex").await?;
    assert_eq!(held.len(), 1, "{held:?}");
    let set_import = |state: &'static str| {
        sqlx::query("update job set state = $3 where kind = $1 and payload->>'import_id' = $2")
            .bind(JOB)
            .bind(up.import_id.to_string())
            .bind(state)
    };
    // a replay of the import runs when the person closes the study: the upload stays for it
    set_import("running").execute(&e.rt.pool).await?;
    let mut tx = e.rt.pool.begin().await?;
    review::dismiss(&mut tx, held[0].0, e.fam.user("alex"), None).await?;
    let it = item(&mut tx, held[0].0).await?;
    assert!(
        !remove::settle(&mut tx, &it).await?,
        "the upload went while its import ran"
    );
    tx.commit().await?;
    // that import then fails for good, so it never settles the upload itself
    set_import("dead").execute(&e.rt.pool).await?;
    let swept = e.sweep().await?;
    assert_eq!(swept["settled_uploads"], 1, "{swept}");
    assert_eq!(uploads_left(&e, &up).await?, 0);
    assert_eq!(e.run(RELEASE_JOB).await?, 1);
    assert!(!e.rt.blobs.exists(BlobOwner::Profile(alex), &sha), "the zip stayed");
    // nothing more to settle at the next sweep
    assert_eq!(e.sweep().await?["settled_uploads"], 0);
    Ok(())
}

#[tokio::test]
async fn a_rebuild_leaves_out_an_upload_whose_import_has_not_run() -> R {
    let e = env().await?;
    let four = Study {
        series: 4,
        ..Study::new(U1, ALEX)
    };
    e.upload("alex", &series_files(&four, &[0, 1], "DICOM")).await?;
    e.import().await?;
    let (later, _) = e.upload("alex", &series_files(&four, &[2], "DICOM")).await?;
    e.import().await?;
    as_before_the_links(&e, &[later.import_id]).await?;
    // a disc uploaded since, with a fourth series of the study: its import has not checked it yet
    let (fresh, _) = e.upload("alex", &series_files(&four, &[3], "DICOM")).await?;

    e.orthanc.wipe();
    let rebuilt = sweep_and_rebuild(&e).await?;
    assert_eq!(
        (rebuilt["rebuilt"].clone(), rebuilt["missing"].clone()),
        (1.into(), 0.into()),
        "{rebuilt}"
    );
    assert_eq!(
        e.orthanc.series_under(U1),
        (0..3).map(|s| four.series_uid(s)).collect(),
        "a series from an upload whose import has not run reached Orthanc"
    );
    assert_eq!(links(&e, fresh.import_id, U1).await?, 0);
    assert_eq!(links(&e, later.import_id, U1).await?, 1);
    Ok(())
}

#[tokio::test]
async fn a_removal_while_a_release_of_the_same_zip_runs_queues_a_release_of_its_own() -> R {
    let e = env().await?;
    let alex = e.fam.profile("alex");
    let files = study_files(&Study::new(U3, CARMEN), "DICOM");
    let (first, sha) = e.upload("alex", &files).await?;
    e.import().await?;
    assert!(matches!(
        remove_now(&e, first.import_id).await?,
        Removal::Removed { .. }
    ));
    // the same disc again, then the first removal's release runs: the new upload names the bytes
    let (again, same) = e.upload("alex", &files).await?;
    assert_eq!(same, sha);
    e.import().await?;
    let kinds = [RELEASE_JOB.to_string()];
    let claimed = jobs::claim(&e.rt.pool, "core", "release-test", Some(&kinds))
        .await?
        .ok_or_else(|| anyhow::anyhow!("no release queued"))?;
    let release_id = claimed.id;
    let kept = remove::release(e.ctx.clone(), claimed).await?;
    assert_eq!(kept, Some(serde_json::json!({"released": false})));
    // before that release is marked done, the new upload is removed too
    assert!(matches!(
        remove_now(&e, again.import_id).await?,
        Removal::Removed { .. }
    ));
    let queued = format!("select count(*) from job where kind = '{RELEASE_JOB}' and state = 'queued'");
    assert_eq!(e.count(&queued).await?, 1, "the removal queued no release of the zip");
    sqlx::query("update job set state = 'done', finished_at = now() where id = $1")
        .bind(release_id)
        .execute(&e.rt.pool)
        .await?;
    assert_eq!(e.run(RELEASE_JOB).await?, 1);
    assert!(!e.rt.blobs.exists(BlobOwner::Profile(alex), &sha), "the zip stayed");
    Ok(())
}

#[tokio::test]
async fn one_series_orthanc_cannot_delete_neither_keeps_the_next_one_nor_passes_unseen() -> R {
    let e = env().await?;
    let three = Study {
        series: 3,
        ..Study::new(U1, ALEX)
    };
    e.upload("alex", &series_files(&three, &[0], "DICOM")).await?;
    e.import().await?;
    let client = Orthanc::new(e.orthanc.config())?;
    let lost = e
        .orthanc
        .held()
        .instances
        .keys()
        .next()
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("no counted instance"))?;
    client.delete_instance(&lost).await?;
    // two series under the recorded study that no record counts
    client.store(support::image(&three, 1, 0, EXPLICIT_LE)).await?;
    client.store(support::image(&three, 2, 0, EXPLICIT_LE)).await?;
    let id = e
        .recorded(U1)
        .await?
        .and_then(|r| r.1)
        .ok_or_else(|| anyhow::anyhow!("not recorded"))?;
    let listed = client
        .series_ids(&id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("Orthanc lacks the study"))?;
    // the first of them the sweep reaches always fails to go
    let counted = three.series_uid(0);
    let failing = listed
        .into_iter()
        .map(|(_, uid)| uid)
        .find(|uid| *uid != counted)
        .ok_or_else(|| anyhow::anyhow!("no uncounted series"))?;
    if let Ok(mut f) = e.orthanc.fail_series_delete.lock() {
        *f = Some(failing.clone());
    }
    let mut conn = e.rt.pool.acquire().await?;
    sweep::enqueue(&mut conn).await?;
    drop(conn);
    assert_eq!(e.run(sweep::JOB).await?, 1);
    assert_eq!(
        e.orthanc.series_under(U1),
        [counted, failing].into_iter().collect(),
        "one failing series kept the next one"
    );
    assert_eq!(
        e.run(rebuild::JOB).await?,
        1,
        "the failed DELETE kept the counted series from rebuilding"
    );
    let (rebuild_state, rebuilt) = e.result(rebuild::JOB).await?;
    assert_eq!(rebuild_state, "done", "{rebuilt}");
    assert_eq!(rebuilt["rebuilt"], 1, "{rebuilt}");
    let (state, error): (String, Option<String>) =
        sqlx::query_as("select state, last_error from job where kind = $1 order by id desc limit 1")
            .bind(sweep::JOB)
            .fetch_one(&e.rt.pool)
            .await?;
    assert_eq!(state, "queued", "{error:?}");
    assert!(error.is_some(), "no error recorded");
    Ok(())
}
