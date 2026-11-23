//! Slice 41: a hospital CD becomes one study on the person's record.
//!
//! Each test writes its discs at run time (support/mod.rs, with dicom-object, an implementation
//! independent of the importer's reader), stores the zip as the upload route does, runs the
//! `import.dicom` job, and reads what Orthanc (a fake that files studies by PatientID and UID, as
//! Orthanc does) and the record hold afterwards.

mod support;

use std::time::Duration;

use ownhealth_core::Runtime;
use ownhealth_core::blobs::BlobOwner;
use ownhealth_core::demo::{Family, create_rivera_family};
use ownhealth_core::jobs::{self, JobContext};
use ownhealth_core::testing::TestDb;
use ownhealth_imaging::import::{JOB, REVIEW_KIND};
use ownhealth_imaging::upload::{self, Recorded};
use serde_json::Value;
use support::{ALEX, CARMEN, FakeOrthanc, Person, SAM, Study, hospital_cd, study_files, zip_files};
use uuid::Uuid;

type R = anyhow::Result<()>;
/// An `imaging_study` row and its src: profile, Orthanc id, modality, date, description, status,
/// report document, FHIR, src kind and src profile.
type StudyRow = (
    Uuid,
    String,
    Option<String>,
    Option<chrono::NaiveDate>,
    Option<String>,
    String,
    Option<Uuid>,
    Value,
    String,
    Option<Uuid>,
);

const U1: &str = "1.2.826.0.1.3680043.10.999.41.1";
const U2: &str = "1.2.826.0.1.3680043.10.999.41.2";

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
    async fn upload(&self, slug: &str, files: &[(String, Vec<u8>)]) -> anyhow::Result<Recorded> {
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
        Ok(r)
    }

    /// Runs the queued imports (only those: the PDF importer is not under test here).
    async fn run(&self) -> anyhow::Result<usize> {
        Ok(jobs::drain(&self.ctx, "core", 10, Some(&[JOB.to_string()])).await?)
    }

    async fn job(&self, id: Option<i64>) -> anyhow::Result<(String, Value, Option<String>)> {
        Ok(
            sqlx::query_as("select state, coalesce(result, 'null'::jsonb), last_error from job where id = $1")
                .bind(id.ok_or_else(|| anyhow::anyhow!("no job was queued"))?)
                .fetch_one(&self.rt.pool)
                .await?,
        )
    }

    /// The open `imaging_study` items of a profile: (study uid, reason).
    async fn in_review(&self, slug: &str) -> anyhow::Result<Vec<(String, String)>> {
        Ok(sqlx::query_as(
            "select payload->>'study_uid', payload->>'reason' from review_item where kind = $1 and status = 'open' \
             and profile_id = $2 order by created_at, id",
        )
        .bind(REVIEW_KIND)
        .bind(self.fam.profile(slug))
        .fetch_all(&self.rt.pool)
        .await?)
    }

    /// The record's FHIR ImagingStudy of `uid`.
    async fn fhir(&self, uid: &str) -> anyhow::Result<Value> {
        Ok(
            sqlx::query_scalar("select fhir from imaging_study where study_instance_uid = $1")
                .bind(uid)
                .fetch_one(&self.rt.pool)
                .await?,
        )
    }

    /// The profile that records `uid`, and its Orthanc id.
    async fn recorded(&self, uid: &str) -> anyhow::Result<Option<(Uuid, Option<String>)>> {
        Ok(
            sqlx::query_as("select profile_id, orthanc_id from imaging_study where study_instance_uid = $1")
                .bind(uid)
                .fetch_optional(&self.rt.pool)
                .await?,
        )
    }
}

#[tokio::test]
async fn a_hospital_cd_becomes_one_study_and_a_second_import_sends_nothing() -> R {
    let e = env().await?;
    let alex = Study {
        report: true,
        ..Study::new(U1, ALEX)
    };
    let sam = Study::new(U2, SAM);
    let disc = hospital_cd(&[alex.clone(), sam.clone()]);
    let up = e.upload("alex", &disc).await?;
    assert!(!up.duplicate);
    assert_eq!(e.run().await?, 1);
    let (state, result, error) = e.job(up.job_id).await?;
    assert_eq!(state, "done", "{error:?}");
    // 4 disc files (DICOMDIR, viewer, autorun, readme), 6 + 1 of Alex's study, 6 of Sam's, 1 damaged
    assert_eq!(result["files"], 18, "{result}");
    assert_eq!(result["not_dicom"], 3, "{result}");
    assert_eq!(result["directories"], 1, "{result}");
    assert_eq!(result["damaged"], 1, "{result}");
    assert_eq!(result["studies"], 2, "{result}");
    assert_eq!(result["imported"], 1, "{result}");
    assert_eq!(result["in_review"], 1, "{result}");
    assert_eq!(result["instances"], 7, "{result}");
    assert_eq!(result["reports"], 1, "{result}");

    // Orthanc holds Alex's study only: nothing of the second patient reached it
    assert_eq!(e.orthanc.patients_under(U1), [ALEX.id.to_string()].into());
    assert!(e.orthanc.patients_under(U2).is_empty());
    assert_eq!(e.orthanc.posts(), 7);
    // every series' metadata was read once, the report's series included
    assert_eq!(e.orthanc.warmed().len(), 3, "{:?}", e.orthanc.warmed());

    // the record: one current study of Alex's, bound to the one Orthanc study, cited to a dicom src
    let row: StudyRow = sqlx::query_as(
        "select i.profile_id, i.orthanc_id, i.modality, i.study_date, i.description, i.status, \
             i.report_document_id, i.fhir, s.kind, s.profile_id from imaging_study i join src s on s.id = i.src_id \
             where i.study_instance_uid = $1",
    )
    .bind(U1)
    .fetch_one(&e.rt.pool)
    .await?;
    assert_eq!(row.0, e.fam.profile("alex"));
    let held = e.orthanc.held();
    assert_eq!(
        held.studies.get(&row.1).map(|(u, p)| (u.as_str(), p.as_str())),
        Some((U1, ALEX.id))
    );
    assert_eq!(row.2.as_deref(), Some("CT"));
    assert_eq!(row.3.map(|d| d.to_string()).as_deref(), Some("2026-09-14"));
    assert_eq!(row.4.as_deref(), Some("TC TORAX"));
    assert_eq!(row.5, "current");
    assert_eq!(row.7["resourceType"], "ImagingStudy");
    assert_eq!(row.7["numberOfSeries"], 3);
    assert_eq!(row.7["numberOfInstances"], 7);
    assert_eq!(row.8, "dicom");
    assert_eq!(row.9, Some(e.fam.profile("alex")));
    assert!(e.recorded(U2).await?.is_none());

    // the encapsulated PDF became Alex's imaging report, from the imaging channel, queued for the PDF importer
    let report = row.6.ok_or_else(|| anyhow::anyhow!("no report linked"))?;
    let doc: (Option<Uuid>, Option<String>, String, Vec<u8>, String) = sqlx::query_as(
        "select d.profile_id, d.doc_type, d.mime, d.blob_sha256, s.channel from document d \
         join document_source s on s.document_id = d.id where d.id = $1",
    )
    .bind(report)
    .fetch_one(&e.rt.pool)
    .await?;
    assert_eq!(doc.0, Some(e.fam.profile("alex")));
    assert_eq!(doc.1.as_deref(), Some("imaging_report"));
    assert_eq!(doc.2, "application/pdf");
    assert_eq!(doc.4, "imaging");
    let pdf =
        e.rt.blobs
            .get_bytes(BlobOwner::Profile(e.fam.profile("alex")), &hex::encode(&doc.3))?;
    assert_eq!(pdf, support::report_pdf(&alex));
    let queued: i64 =
        sqlx::query_scalar("select count(*) from job where kind = 'import.pdf' and payload->>'document_id' = $1")
            .bind(report.to_string())
            .fetch_one(&e.rt.pool)
            .await?;
    assert_eq!(queued, 1);

    // the second patient waits in review on Alex's profile
    assert_eq!(
        e.in_review("alex").await?,
        [(U2.to_string(), "identity_mismatch".to_string())]
    );

    // the same disc again: the same import, and its job sends nothing and files nothing new
    let again = e.upload("alex", &disc).await?;
    assert!(again.duplicate);
    assert_eq!(again.import_id, up.import_id);
    assert_eq!(e.run().await?, 1);
    let (state, result, _) = e.job(again.job_id).await?;
    assert_eq!(state, "done");
    assert_eq!(result["imported"], 0, "{result}");
    assert_eq!(result["already"], 1, "{result}");
    assert_eq!(e.orthanc.posts(), 7);
    assert_eq!(e.in_review("alex").await?.len(), 1);
    let studies: i64 = sqlx::query_scalar("select count(*) from imaging_study")
        .fetch_one(&e.rt.pool)
        .await?;
    assert_eq!(studies, 1);
    Ok(())
}

#[tokio::test]
async fn the_same_study_offered_to_a_second_profile_goes_to_review() -> R {
    let e = env().await?;
    let disc = study_files(&Study::new(U1, ALEX), "DICOM");
    e.upload("alex", &disc).await?;
    e.run().await?;
    let posts = e.orthanc.posts();
    // the same files under Carmen's profile
    let up = e.upload("carmen", &disc).await?;
    e.run().await?;
    let (state, result, _) = e.job(up.job_id).await?;
    assert_eq!(state, "done");
    assert_eq!(result["in_review"], 1, "{result}");
    assert_eq!(e.orthanc.posts(), posts, "nothing more was sent");
    assert_eq!(e.recorded(U1).await?.map(|r| r.0), Some(e.fam.profile("alex")));
    assert_eq!(
        e.in_review("carmen").await?,
        [(U1.to_string(), "other_profile".to_string())]
    );
    // and it cannot be accepted: only dismissed
    let item: Uuid = sqlx::query_scalar("select id from review_item where kind = $1 and profile_id = $2")
        .bind(REVIEW_KIND)
        .bind(e.fam.profile("carmen"))
        .fetch_one(&e.rt.pool)
        .await?;
    let mut c = e.rt.pool.acquire().await?;
    let refused = ownhealth_core::review::accept(&mut c, item, e.fam.user("alex"), None).await;
    assert!(matches!(refused, Err(ownhealth_core::Error::Review(_))), "{refused:?}");
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_imports_of_one_uid_under_two_patients_at_once_send_one_patient() -> R {
    let e = env().await?;
    // Each file takes a while in Orthanc, and the first UID lookup waits for a second one: without the
    // one UID lock, both imports would find the UID free and both would send.
    *e.orthanc.delay.lock().map_err(|_| anyhow::anyhow!("lock"))? = Duration::from_millis(40);
    e.orthanc.pair_lookups.store(true, std::sync::atomic::Ordering::SeqCst);
    let alex = e.upload("alex", &study_files(&Study::new(U1, ALEX), "DICOM")).await?;
    let sam = e.upload("sam", &study_files(&Study::new(U1, SAM), "DICOM")).await?;
    let (a, b) = (e.ctx.clone(), e.ctx.clone());
    let kinds = vec![JOB.to_string()];
    let (k1, k2) = (kinds.clone(), kinds);
    let one = tokio::spawn(async move { jobs::drain(&a, "core", 1, Some(&k1)).await });
    let two = tokio::spawn(async move { jobs::drain(&b, "core", 1, Some(&k2)).await });
    assert_eq!(one.await?? + two.await??, 2);
    for job in [alex.job_id, sam.job_id] {
        let (state, result, error) = e.job(job).await?;
        assert_eq!(state, "done", "{result} {error:?}");
    }
    // exactly one patient reached Orthanc under the UID, and that one is on the record
    let patients = e.orthanc.patients_under(U1);
    assert_eq!(patients.len(), 1, "{patients:?}");
    assert_eq!(e.orthanc.posts(), 6, "the other import sent nothing");
    let (owner, _) = e.recorded(U1).await?.ok_or_else(|| anyhow::anyhow!("not recorded"))?;
    let (winner, loser) = if owner == e.fam.profile("alex") {
        (ALEX.id, "sam")
    } else {
        (SAM.id, "alex")
    };
    assert_eq!(patients, [winner.to_string()].into());
    let waiting = e.in_review(loser).await?;
    assert_eq!(waiting.len(), 1, "{waiting:?}");
    assert_eq!(waiting[0].0, U1);
    assert!(
        ["other_profile", "uid_conflict"].contains(&waiting[0].1.as_str()),
        "{waiting:?}"
    );
    Ok(())
}

#[tokio::test]
async fn a_patient_who_is_not_the_profiles_waits_and_an_accept_imports_the_study() -> R {
    let e = env().await?;
    let up = e.upload("alex", &study_files(&Study::new(U2, CARMEN), "DICOM")).await?;
    e.run().await?;
    let (_, result, _) = e.job(up.job_id).await?;
    assert_eq!(result["in_review"], 1, "{result}");
    assert_eq!(e.orthanc.posts(), 0);
    assert_eq!(
        e.in_review("alex").await?,
        [(U2.to_string(), "identity_mismatch".to_string())]
    );
    let (item, payload): (Uuid, Value) = sqlx::query_as("select id, payload from review_item where kind = $1")
        .bind(REVIEW_KIND)
        .fetch_one(&e.rt.pool)
        .await?;
    // the item says who the disc names, so a person can decide
    assert_eq!(payload["patient_name"], "ORTEGA CARMEN");
    assert_eq!(payload["patient_birth_date"], "1955-01-30");
    assert_eq!(payload["instances"], 6);
    let mut c = e.rt.pool.acquire().await?;
    ownhealth_core::review::accept(&mut c, item, e.fam.user("alex"), None).await?;
    drop(c);
    assert_eq!(e.run().await?, 1);
    assert_eq!(e.recorded(U2).await?.map(|r| r.0), Some(e.fam.profile("alex")));
    assert_eq!(e.orthanc.patients_under(U2), [CARMEN.id.to_string()].into());
    assert!(e.in_review("alex").await?.is_empty());
    Ok(())
}

#[tokio::test]
async fn a_uid_orthanc_holds_under_another_patient_sends_nothing() -> R {
    let e = env().await?;
    e.orthanc.hold_study(U1, "LN-SOMEONE-ELSE");
    let up = e.upload("alex", &study_files(&Study::new(U1, ALEX), "DICOM")).await?;
    e.run().await?;
    let (_, result, _) = e.job(up.job_id).await?;
    assert_eq!(result["in_review"], 1, "{result}");
    assert_eq!(e.orthanc.posts(), 0);
    assert!(e.recorded(U1).await?.is_none());
    assert_eq!(
        e.in_review("alex").await?,
        [(U1.to_string(), "uid_conflict".to_string())]
    );
    Ok(())
}

#[tokio::test]
async fn a_study_is_recorded_only_once_its_metadata_is_warm() -> R {
    let e = env().await?;
    *e.orthanc.metadata_fails.lock().map_err(|_| anyhow::anyhow!("lock"))? = true;
    let up = e.upload("alex", &study_files(&Study::new(U1, ALEX), "DICOM")).await?;
    e.run().await?;
    let (state, _, error) = e.job(up.job_id).await?;
    assert_eq!(state, "queued", "a retry waits: {error:?}");
    assert!(e.recorded(U1).await?.is_none(), "not ready, not recorded");
    *e.orthanc.metadata_fails.lock().map_err(|_| anyhow::anyhow!("lock"))? = false;
    sqlx::query("update job set run_after = now() where id = $1")
        .bind(up.job_id)
        .execute(&e.rt.pool)
        .await?;
    e.run().await?;
    let (state, result, _) = e.job(up.job_id).await?;
    assert_eq!(state, "done", "{result}");
    assert!(e.recorded(U1).await?.is_some());
    // the retry sent the files again; Orthanc kept one copy of each
    assert_eq!(e.orthanc.held().instances.len(), 6);
    Ok(())
}

#[tokio::test]
async fn files_orthanc_files_otherwise_are_taken_back() -> R {
    let e = env().await?;
    // an Orthanc that reads every file of U1 as U2
    *e.orthanc.misread.lock().map_err(|_| anyhow::anyhow!("lock"))? = Some((U1.into(), U2.into()));
    let up = e.upload("alex", &study_files(&Study::new(U1, ALEX), "DICOM")).await?;
    e.run().await?;
    let (state, result, error) = e.job(up.job_id).await?;
    assert_eq!(state, "done", "{error:?}");
    assert_eq!(result["in_review"], 1, "{result}");
    let held = e.orthanc.held();
    assert!(held.instances.is_empty(), "taken back: {held:?}");
    assert!(held.studies.is_empty(), "{held:?}");
    assert!(e.recorded(U1).await?.is_none());
    assert_eq!(
        e.in_review("alex").await?,
        [(U1.to_string(), "uid_conflict".to_string())]
    );
    Ok(())
}

/// Another hospital's patient, filed under a StudyInstanceUID Alex's record holds.
const PEDRO: Person = Person {
    name: "GARCIA^PEDRO",
    id: "LN-PEDRO-09",
    birth: "19600101",
};

#[tokio::test]
async fn a_second_disc_that_adds_a_series_sends_it_and_the_record_counts_it() -> R {
    let e = env().await?;
    let first = study_files(&Study::new(U1, ALEX), "DICOM");
    e.upload("alex", &first).await?;
    e.run().await?;
    assert_eq!(e.orthanc.held().instances.len(), 6);
    // a second disc of the same study holds a third series
    let second = study_files(
        &Study {
            series: 3,
            ..Study::new(U1, ALEX)
        },
        "DICOM",
    );
    let up = e.upload("alex", &second).await?;
    e.run().await?;
    let (state, result, error) = e.job(up.job_id).await?;
    assert_eq!(state, "done", "{error:?}");
    assert_eq!(result["imported"], 1, "{result}");
    assert_eq!(result["already"], 0, "{result}");
    assert_eq!(e.orthanc.held().instances.len(), 9, "the third series reached Orthanc");
    assert!(
        e.orthanc
            .warmed()
            .iter()
            .any(|p| p.contains(&format!("/series/{U1}.3/"))),
        "{:?}",
        e.orthanc.warmed()
    );
    let fhir = e.fhir(U1).await?;
    assert_eq!(fhir["numberOfSeries"], 3, "{fhir}");
    assert_eq!(fhir["numberOfInstances"], 9, "{fhir}");
    // the same second disc again sends nothing
    let posts = e.orthanc.posts();
    let again = e.upload("alex", &second).await?;
    e.run().await?;
    let (_, result, _) = e.job(again.job_id).await?;
    assert_eq!(result["already"], 1, "{result}");
    assert_eq!(e.orthanc.posts(), posts);
    Ok(())
}

#[tokio::test]
async fn another_patients_study_under_a_uid_the_profile_holds_waits_in_review() -> R {
    let e = env().await?;
    e.upload("alex", &study_files(&Study::new(U1, ALEX), "DICOM")).await?;
    e.run().await?;
    let posts = e.orthanc.posts();
    // another patient's disc, under the UID of the study Alex's record holds, uploaded to Alex
    let up = e.upload("alex", &study_files(&Study::new(U1, PEDRO), "PEDRO")).await?;
    e.run().await?;
    let (_, result, _) = e.job(up.job_id).await?;
    assert_eq!(result["already"], 0, "{result}");
    assert_eq!(result["in_review"], 1, "{result}");
    assert_eq!(e.orthanc.posts(), posts, "nothing of it was sent");
    assert_eq!(e.orthanc.patients_under(U1), [ALEX.id.to_string()].into());
    assert_eq!(
        e.in_review("alex").await?,
        [(U1.to_string(), "uid_conflict".to_string())]
    );
    Ok(())
}

#[tokio::test]
async fn a_second_patient_the_disc_only_hints_at_waits_and_an_accept_imports_it() -> R {
    let e = env().await?;
    // the profile owner's study, and one of a patient with the owner's name but another hospital's
    // PatientID and no birth date: not clearly the owner
    let namesake = Person {
        name: "RIVERA^ALEX",
        id: "OTHER-HOSP-9",
        birth: "",
    };
    let mut disc = study_files(&Study::new(U1, ALEX), "DICOM/A");
    disc.extend(study_files(&Study::new(U2, namesake), "DICOM/B"));
    let up = e.upload("alex", &disc).await?;
    e.run().await?;
    let (_, result, _) = e.job(up.job_id).await?;
    assert_eq!(result["imported"], 1, "{result}");
    assert_eq!(result["in_review"], 1, "{result}");
    assert_eq!(
        e.in_review("alex").await?,
        [(U2.to_string(), "second_patient".to_string())]
    );
    assert!(e.orthanc.patients_under(U2).is_empty(), "nothing of it was sent");
    let item: Uuid = sqlx::query_scalar("select id from review_item where kind = $1 and status = 'open'")
        .bind(REVIEW_KIND)
        .fetch_one(&e.rt.pool)
        .await?;
    let mut c = e.rt.pool.acquire().await?;
    ownhealth_core::review::accept(&mut c, item, e.fam.user("alex"), None).await?;
    drop(c);
    e.run().await?;
    assert_eq!(e.recorded(U2).await?.map(|r| r.0), Some(e.fam.profile("alex")));
    assert_eq!(e.orthanc.patients_under(U2), ["OTHER-HOSP-9".to_string()].into());
    Ok(())
}

#[tokio::test]
async fn files_orthanc_files_under_another_patient_are_taken_back() -> R {
    let e = env().await?;
    // an Orthanc that reads Alex's PatientID as another one
    *e.orthanc.misread_patient.lock().map_err(|_| anyhow::anyhow!("lock"))? =
        Some((ALEX.id.into(), "LN-MISREAD".into()));
    let up = e.upload("alex", &study_files(&Study::new(U1, ALEX), "DICOM")).await?;
    e.run().await?;
    let (state, result, error) = e.job(up.job_id).await?;
    assert_eq!(state, "done", "{error:?}");
    assert_eq!(result["in_review"], 1, "{result}");
    let held = e.orthanc.held();
    assert!(held.instances.is_empty(), "taken back: {held:?}");
    assert!(held.studies.is_empty(), "{held:?}");
    assert!(e.recorded(U1).await?.is_none());
    assert_eq!(
        e.in_review("alex").await?,
        [(U1.to_string(), "uid_conflict".to_string())]
    );
    Ok(())
}

#[tokio::test]
async fn a_file_orthanc_refuses_is_counted_and_the_record_counts_what_orthanc_kept() -> R {
    let e = env().await?;
    let st = Study {
        series: 1,
        per_series: 4,
        size: 64,
        ..Study::new(U1, ALEX)
    };
    let mut disc = study_files(&st, "DICOM");
    // the fourth image cut in its pixel data: its header reads, Orthanc refuses the file
    let last = disc.len() - 1;
    let keep = disc[last].1.len() * 2 / 3;
    disc[last].1.truncate(keep);
    let up = e.upload("alex", &disc).await?;
    e.run().await?;
    let (state, result, error) = e.job(up.job_id).await?;
    assert_eq!(state, "done", "{error:?}");
    assert_eq!(result["damaged"], 0, "{result}");
    assert_eq!(result["refused_files"], 1, "{result}");
    assert_eq!(result["instances"], 3, "{result}");
    assert_eq!(e.orthanc.held().instances.len(), 3);
    let fhir = e.fhir(U1).await?;
    assert_eq!(fhir["numberOfInstances"], 3, "{fhir}");
    assert_eq!(fhir["series"][0]["numberOfInstances"], 3, "{fhir}");
    Ok(())
}

#[tokio::test]
async fn a_fully_refused_series_leaves_the_rest_of_the_study_ready() -> R {
    let e = env().await?;
    let st = Study {
        series: 2,
        per_series: 1,
        size: 64,
        ..Study::new(U1, ALEX)
    };
    let mut disc = study_files(&st, "DICOM");
    // Cut the only image of the second series in its pixel data.
    let keep = disc[1].1.len() * 2 / 3;
    disc[1].1.truncate(keep);
    let up = e.upload("alex", &disc).await?;
    assert_eq!(e.run().await?, 1);
    let (state, result, error) = e.job(up.job_id).await?;
    assert_eq!(state, "done", "{error:?}");
    assert_eq!(result["damaged"], 0, "{result}");
    assert_eq!(result["refused_files"], 1, "{result}");
    assert_eq!(result["instances"], 1, "{result}");
    let fhir = e.fhir(U1).await?;
    assert_eq!(fhir["numberOfSeries"], 1, "{fhir}");
    assert_eq!(fhir["numberOfInstances"], 1, "{fhir}");
    assert_eq!(fhir["series"][0]["uid"], st.series_uid(0), "{fhir}");
    assert_eq!(
        e.orthanc.warmed(),
        [format!("/dicom-web/studies/{U1}/series/{}/metadata", st.series_uid(0))]
    );
    Ok(())
}

#[tokio::test]
async fn a_header_past_the_read_cap_is_damaged_and_not_sent() -> R {
    let e = env().await?;
    let mut disc = study_files(&Study::new(U1, ALEX), "DICOM");
    // one more file of the study whose patient and study tags sit behind 17 MiB of a private element
    disc.push((
        "DICOM/BIG/I0001".into(),
        support::image_with_private_bytes(&Study::new(U1, ALEX), 17 << 20),
    ));
    let up = e.upload("alex", &disc).await?;
    e.run().await?;
    let (state, result, error) = e.job(up.job_id).await?;
    assert_eq!(state, "done", "{error:?}");
    assert_eq!(result["damaged"], 1, "{result}");
    assert_eq!(result["instances"], 6, "{result}");
    assert_eq!(e.orthanc.posts(), 6);
    Ok(())
}

#[tokio::test]
async fn round3_large_series_counts_do_not_return_instance_ids() -> R {
    use axum::{
        Json, Router,
        extract::Request,
        routing::{get, post},
    };
    use serde_json::json;

    const ID: &str = "00000000-00000000-00000000-00000000-00000000";
    const N: usize = 100_000;
    let old_answer = json!([{"Instances": vec![ID; N], "MainDicomTags": {"SeriesInstanceUID": U1}}]);
    assert!(old_answer.to_string().len() > 4 << 20);
    let app = Router::new()
        .route("/studies/{id}/series", get(|| async { Json(old_answer) }))
        .route(
            "/tools/find",
            post(|req: Request| async {
                let query = axum::body::to_bytes(req.into_body(), 8192).await.expect("query");
                let query: Value = serde_json::from_slice(&query).expect("JSON");
                assert_eq!(query["ParentStudy"], ID);
                assert_eq!(query["Level"], "Series");
                assert_eq!(query["ResponseContent"], json!(["MainDicomTags", "RequestedTags"]));
                assert_eq!(query["RequestedTags"], json!(["NumberOfSeriesRelatedInstances"]));
                Json(json!([{
                    "MainDicomTags": {"SeriesInstanceUID": U1, "Modality": "CT"},
                    "RequestedTags": {"NumberOfSeriesRelatedInstances": N.to_string()},
                }]))
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let config = ownhealth_core::orthanc::OrthancConfig {
        url: format!("http://{}", listener.local_addr()?),
        user: support::USER.into(),
        password: ownhealth_core::ids::token_hex(16),
    };
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    let client = ownhealth_imaging::orthanc::Orthanc::new(config)?;
    let held = client.series_of(ID).await?;
    assert_eq!(held.len(), 1);
    assert_eq!(held[0].uid, U1);
    assert_eq!(held[0].instances, N);
    Ok(())
}
