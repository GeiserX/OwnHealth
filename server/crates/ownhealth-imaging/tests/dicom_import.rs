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
async fn round3_conflicting_identity_evidence_holds_the_whole_study() -> R {
    for different in [
        Study {
            issuer: "Clinic-B",
            ..Study::new(U1, ALEX)
        },
        Study {
            person: Person {
                name: "RIVERA^SAM",
                ..ALEX
            },
            ..Study::new(U1, ALEX)
        },
        Study {
            person: Person {
                birth: "19810202",
                ..ALEX
            },
            ..Study::new(U1, ALEX)
        },
    ] {
        let e = env().await?;
        let first = Study {
            issuer: "Clinic-A",
            ..Study::new(U1, ALEX)
        };
        let disc = [
            ("first".into(), support::image(&first, 0, 0, support::EXPLICIT_LE)),
            ("second".into(), support::image(&different, 0, 1, support::EXPLICIT_LE)),
        ];
        let up = e.upload("alex", &disc).await?;
        e.run().await?;
        let (_, result, error) = e.job(up.job_id).await?;
        assert_eq!(result["in_review"], 1, "{result} {error:?}");
        assert_eq!(e.orthanc.posts(), 0);
    }
    Ok(())
}

#[tokio::test]
async fn round3_missing_optional_identity_fields_allow_consistent_files() -> R {
    let e = env().await?;
    let incomplete = Study {
        person: Person {
            name: "",
            birth: "",
            ..ALEX
        },
        ..Study::new(U1, ALEX)
    };
    let complete = Study {
        issuer: "Clinic-A",
        ..Study::new(U1, ALEX)
    };
    let disc = [
        ("first".into(), support::image(&incomplete, 0, 0, support::EXPLICIT_LE)),
        ("second".into(), support::image(&complete, 0, 1, support::EXPLICIT_LE)),
    ];
    let up = e.upload("alex", &disc).await?;
    e.run().await?;
    let (_, result, error) = e.job(up.job_id).await?;
    assert_eq!(result["instances"], 2, "{result} {error:?}");
    Ok(())
}

#[tokio::test]
async fn round3_retry_reconciles_a_second_disc_with_a_report_without_resending() -> R {
    let e = env().await?;
    let first = Study {
        series: 1,
        per_series: 1,
        ..Study::new(U1, ALEX)
    };
    e.upload("alex", &study_files(&first, "DICOM")).await?;
    e.run().await?;
    let disc = study_files(
        &Study {
            series: 2,
            report: true,
            ..first
        },
        "DICOM",
    );
    *e.orthanc.metadata_fails.lock().map_err(|_| anyhow::anyhow!("lock"))? = true;
    let up = e.upload("alex", &disc).await?;
    e.run().await?;
    assert_eq!(e.job(up.job_id).await?.0, "queued");
    assert_eq!(e.fhir(U1).await?["numberOfSeries"], 1);
    assert_eq!(e.orthanc.held().instances.len(), 3);
    let posts = e.orthanc.posts();

    *e.orthanc.metadata_fails.lock().map_err(|_| anyhow::anyhow!("lock"))? = false;
    sqlx::query("update job set run_after = now() where id = $1")
        .bind(up.job_id)
        .execute(&e.rt.pool)
        .await?;
    e.run().await?;
    let (state, result, error) = e.job(up.job_id).await?;
    assert_eq!(state, "done", "{result} {error:?}");
    assert_eq!(e.orthanc.posts(), posts);
    let fhir = e.fhir(U1).await?;
    assert_eq!(fhir["numberOfSeries"], 3, "{fhir}");
    assert_eq!(fhir["numberOfInstances"], 3, "{fhir}");
    let report: Option<Uuid> =
        sqlx::query_scalar("select report_document_id from imaging_study where study_instance_uid = $1")
            .bind(U1)
            .fetch_one(&e.rt.pool)
            .await?;
    assert!(report.is_some());

    let warmed = e.orthanc.warmed();
    let again = e.upload("alex", &disc).await?;
    e.run().await?;
    assert_eq!(e.job(again.job_id).await?.1["already"], 1);
    assert_eq!(e.orthanc.posts(), posts);
    assert_eq!(e.orthanc.warmed(), warmed);
    Ok(())
}

#[tokio::test]
async fn round3_a_second_delete_failure_resumes_without_deleting_existing_instances() -> R {
    let e = env().await?;
    let st = Study {
        series: 3,
        per_series: 1,
        ..Study::new(U1, ALEX)
    };
    let disc = study_files(&st, "DICOM");
    let client = ownhealth_imaging::orthanc::Orthanc::new(e.orthanc.config())?;
    client.store(disc[0].1.clone()).await?;
    let original = e.orthanc.held().instances;

    *e.orthanc.misread_patient.lock().map_err(|_| anyhow::anyhow!("lock"))? =
        Some((ALEX.id.into(), "LN-MISREAD".into()));
    *e.orthanc
        .misread_except_series
        .lock()
        .map_err(|_| anyhow::anyhow!("lock"))? = Some(st.series_uid(0));
    e.orthanc.fail_delete_at.store(2, std::sync::atomic::Ordering::SeqCst);
    let up = e.upload("alex", &disc).await?;
    e.run().await?;
    assert_eq!(e.job(up.job_id).await?.0, "queued");
    assert_eq!(e.orthanc.held().instances.len(), 2);
    let posts = e.orthanc.posts();
    sqlx::query("update job set run_after = now() where id = $1")
        .bind(up.job_id)
        .execute(&e.rt.pool)
        .await?;
    e.run().await?;
    let (state, result, error) = e.job(up.job_id).await?;
    assert_eq!(state, "done", "{error:?}");
    assert_eq!(result["in_review"], 1, "{result}");
    assert_eq!(e.orthanc.held().instances, original);
    assert_eq!(e.orthanc.posts(), posts);
    Ok(())
}

#[tokio::test]
async fn round3_duplicate_instances_are_counted_once() -> R {
    let e = env().await?;
    let image = support::image(&Study::new(U1, ALEX), 0, 0, support::EXPLICIT_LE);
    let up = e
        .upload("alex", &[("one".into(), image.clone()), ("two".into(), image)])
        .await?;
    e.run().await?;
    assert_eq!(e.fhir(U1).await?["numberOfInstances"], 1);
    let (_, result, _) = e.job(up.job_id).await?;
    assert_eq!(result["instances"], 1, "{result}");
    assert_eq!(result["submitted_files"], 2, "{result}");
    Ok(())
}

#[tokio::test]
async fn round3_an_open_hold_updates_when_another_profile_claims_the_uid() -> R {
    let e = env().await?;
    let disc = study_files(&Study::new(U1, ALEX), "DICOM");
    e.upload("sam", &disc).await?;
    e.run().await?;
    assert_eq!(e.in_review("sam").await?, [(U1.into(), "identity_mismatch".into())]);
    e.upload("alex", &disc).await?;
    e.run().await?;
    for _ in 0..2 {
        e.upload("sam", &disc).await?;
        e.run().await?;
        assert_eq!(e.in_review("sam").await?, [(U1.into(), "other_profile".into())]);
    }
    Ok(())
}

#[tokio::test]
async fn round3_an_entirely_refused_study_has_no_review_item() -> R {
    let e = env().await?;
    let st = Study {
        series: 1,
        per_series: 1,
        size: 64,
        ..Study::new(U1, ALEX)
    };
    let mut disc = study_files(&st, "DICOM");
    let keep = disc[0].1.len() * 2 / 3;
    disc[0].1.truncate(keep);
    let up = e.upload("alex", &disc).await?;
    e.run().await?;
    let (_, result, error) = e.job(up.job_id).await?;
    assert_eq!(result["refused"], 1, "{result} {error:?}");
    assert_eq!(result["in_review"], 0);
    assert!(e.in_review("alex").await?.is_empty());
    assert!(e.recorded(U1).await?.is_none());
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

#[tokio::test]
async fn round3_an_unsupported_text_encoding_holds_the_study_without_guessing() -> R {
    let e = env().await?;
    let study = Study {
        charset: "ISO_IR 192",
        ..Study::new(U1, ALEX)
    };
    let mut image = support::image(&study, 0, 0, support::EXPLICIT_LE);
    let at = image
        .windows(10)
        .position(|w| w == b"ISO_IR 192")
        .ok_or_else(|| anyhow::anyhow!("charset"))?;
    image[at..at + 10].copy_from_slice(b"ISO_IR 148");
    let up = e.upload("alex", &[("image".into(), image)]).await?;
    e.run().await?;
    let (_, result, error) = e.job(up.job_id).await?;
    assert_eq!(result["in_review"], 1, "{result} {error:?}");
    assert_eq!(e.in_review("alex").await?, [(U1.into(), "unsupported_charset".into())]);
    assert_eq!(e.orthanc.posts(), 0);
    Ok(())
}

/// Replaces every `from` in the files with `to`, of the same length: bytes the generator cannot write
/// (text outside the declared repertoire).
fn patch(files: &mut [(String, Vec<u8>)], from: &[u8], to: &[u8]) {
    assert_eq!(from.len(), to.len());
    for (_, bytes) in files.iter_mut() {
        let mut at = 0;
        while let Some(i) = bytes[at..].windows(from.len()).position(|w| w == from) {
            bytes[at + i..at + i + from.len()].copy_from_slice(to);
            at += i + from.len();
        }
    }
}

/// A description is not identity: one in ISO 8859-1 with no declared repertoire, as CDs write it,
/// keeps its text and holds nothing.
#[tokio::test]
async fn round4_a_latin_1_description_with_no_declared_repertoire_imports() -> R {
    let e = env().await?;
    let mut files = study_files(&Study::new(U1, ALEX), "DICOM");
    patch(&mut files, b"TC TORAX", b"TC T\xd3RAX");
    let up = e.upload("alex", &files).await?;
    e.run().await?;
    let (state, result, error) = e.job(up.job_id).await?;
    assert_eq!(state, "done", "{error:?}");
    assert_eq!(result["imported"], 1, "{result}");
    assert!(e.in_review("alex").await?.is_empty());
    assert_eq!(e.orthanc.posts(), 6);
    let description: Option<String> =
        sqlx::query_scalar("select description from imaging_study where study_instance_uid = $1")
            .bind(U1)
            .fetch_one(&e.rt.pool)
            .await?;
    assert_eq!(description.as_deref(), Some("TC TÓRAX"));
    Ok(())
}

/// A single-byte repertoire declared in its ISO 2022 form reads as its base repertoire when no value
/// switches sets.
#[tokio::test]
async fn round4_a_single_byte_iso_2022_repertoire_imports() -> R {
    let e = env().await?;
    let study = Study {
        charset: "ISO 2022 IR 100",
        ..Study::new(U1, ALEX)
    };
    let up = e.upload("alex", &study_files(&study, "DICOM")).await?;
    e.run().await?;
    let (state, result, error) = e.job(up.job_id).await?;
    assert_eq!(state, "done", "{error:?}");
    assert_eq!(result["imported"], 1, "{result}");
    assert!(e.in_review("alex").await?.is_empty());
    assert_eq!(e.orthanc.posts(), 6);
    Ok(())
}

/// A name the reader cannot decode holds the study before any send; a person may accept it, and the
/// study is then sent as it is.
#[tokio::test]
async fn round4_an_unreadable_name_waits_and_an_accept_imports_the_study() -> R {
    let e = env().await?;
    let mut files = study_files(&Study::new(U1, ALEX), "DICOM");
    patch(&mut files, b"RIVERA^ALEX", b"RIVERA^\xc1LEX");
    let up = e.upload("alex", &files).await?;
    e.run().await?;
    let (_, result, error) = e.job(up.job_id).await?;
    assert_eq!(result["in_review"], 1, "{result} {error:?}");
    assert_eq!(e.in_review("alex").await?, [(U1.into(), "unsupported_charset".into())]);
    assert_eq!(e.orthanc.posts(), 0);
    let item: Uuid = sqlx::query_scalar("select id from review_item where kind = $1")
        .bind(REVIEW_KIND)
        .fetch_one(&e.rt.pool)
        .await?;
    let mut c = e.rt.pool.acquire().await?;
    ownhealth_core::review::accept(&mut c, item, e.fam.user("alex"), None).await?;
    drop(c);
    assert_eq!(e.run().await?, 1);
    assert_eq!(e.recorded(U1).await?.map(|r| r.0), Some(e.fam.profile("alex")));
    assert_eq!(e.orthanc.patients_under(U1), [ALEX.id.to_string()].into());
    assert!(e.in_review("alex").await?.is_empty());
    Ok(())
}

/// Unreadable names do not hide a second patient under one StudyInstanceUID: the study waits as a
/// UID conflict, which no one can accept.
#[tokio::test]
async fn round4_two_unreadable_patients_under_one_uid_wait_as_a_uid_conflict() -> R {
    let e = env().await?;
    let mut files = study_files(&Study::new(U1, ALEX), "A");
    files.extend(study_files(&Study::new(U1, SAM), "B"));
    patch(&mut files, b"RIVERA^ALEX", b"RIVERA^\xc1LEX");
    patch(&mut files, b"RIVERA^SAM", b"RIVERA^S\xc1M");
    let up = e.upload("alex", &files).await?;
    e.run().await?;
    let (_, result, error) = e.job(up.job_id).await?;
    assert_eq!(result["in_review"], 1, "{result} {error:?}");
    assert_eq!(e.in_review("alex").await?, [(U1.into(), "uid_conflict".into())]);
    assert_eq!(e.orthanc.posts(), 0);
    Ok(())
}

#[tokio::test]
async fn round5_a_later_disc_with_a_different_sop_in_the_same_series_imports_it() -> R {
    let e = env().await?;
    let st = Study::new(U1, ALEX);
    for n in 0..2 {
        let files = [(format!("image-{n}"), support::image(&st, 0, n, support::EXPLICIT_LE))];
        e.upload("alex", &files).await?;
        e.run().await?;
    }
    assert_eq!(e.orthanc.held().instances.len(), 2);
    assert_eq!(e.fhir(U1).await?["numberOfInstances"], 2);
    Ok(())
}

#[tokio::test]
async fn round5_a_lost_store_reply_recovers_even_when_orthanc_used_a_different_id() -> R {
    use std::sync::atomic::Ordering::SeqCst;
    let e = env().await?;
    let st = Study {
        series: 3,
        per_series: 1,
        ..Study::new(U1, ALEX)
    };
    let disc = study_files(&st, "DICOM");
    let client = ownhealth_imaging::orthanc::Orthanc::new(e.orthanc.config())?;
    client.store(disc[0].1.clone()).await?;
    // A pre-existing copy of the exact same SOP and bytes lives under another study ID.
    *e.orthanc.misread.lock().expect("test setting") = Some((U1.into(), U2.into()));
    client.store(disc[2].1.clone()).await?;
    *e.orthanc.misread.lock().expect("test setting") = None;
    let original = e.orthanc.held().instances;
    // Orthanc keeps a new instance in the original study, then misfiles the next one and loses its reply.
    *e.orthanc.misread_patient.lock().expect("test setting") = Some((ALEX.id.into(), "LN-MISREAD".into()));
    *e.orthanc.misread_except_series.lock().expect("test setting") = Some(st.series_uid(1));
    // Include the pre-existing instance's file, with its original bytes, last in the retry upload.
    let files = [disc[1].clone(), disc[2].clone(), disc[0].clone()];
    e.orthanc.lose_store_reply_at.store(4, SeqCst);
    let up = e.upload("alex", &files).await?;
    tokio::time::timeout(Duration::from_secs(30), e.run()).await??;
    assert_eq!(e.job(up.job_id).await?.0, "queued");
    let after = e.orthanc.held().instances;
    assert_eq!(after.len(), 4);
    assert_eq!(e.orthanc.patients_under(U1).len(), 2);

    *e.orthanc.misread_patient.lock().expect("test setting") = None;
    sqlx::query("update job set run_after = now() where id = $1")
        .bind(up.job_id)
        .execute(&e.rt.pool)
        .await?;
    tokio::time::timeout(Duration::from_secs(30), e.run()).await??;
    let (state, result, error) = e.job(up.job_id).await?;
    assert_eq!(state, "done", "{result} {error:?}");
    assert_eq!(result["imported"], 1, "{result}");
    let held = e.orthanc.held();
    assert_eq!(held.instances.len(), 4);
    assert_eq!(e.orthanc.patients_under(U1), [ALEX.id.into()].into());
    let mut deleted: Vec<_> = held
        .log
        .iter()
        .filter(|(method, _)| method == "DELETE")
        .map(|(_, path)| path.trim_start_matches("/instances/"))
        .collect();
    deleted.sort_unstable();
    let wanted: Vec<_> = after
        .keys()
        .filter(|id| !original.contains_key(*id))
        .map(String::as_str)
        .collect();
    assert_eq!(deleted, wanted);
    assert!(original.keys().all(|id| held.instances.contains_key(id)));
    Ok(())
}

#[tokio::test]
async fn round5_a_refused_report_is_never_filed_even_when_its_series_count_matches() -> R {
    let e = env().await?;
    let st = Study {
        series: 1,
        per_series: 1,
        report: true,
        ..Study::new(U1, ALEX)
    };
    // A different SOP already belongs to the report's series.
    let report_series = support::image(&st, 98, 1, support::EXPLICIT_LE);
    e.upload("alex", &[("other-instance".into(), report_series)]).await?;
    e.run().await?;
    *e.orthanc.refused_sop.lock().expect("test setting") = Some(format!("{U1}.99.1"));
    let files = study_files(&st, "DICOM");
    for _ in 0..2 {
        let up = e.upload("alex", &files).await?;
        e.run().await?;
        let (state, result, error) = e.job(up.job_id).await?;
        assert_eq!(state, "done", "{result} {error:?}");
        assert_eq!(result["refused_files"], 1, "{result}");
        assert_eq!(result["reports"], 0, "{result}");
    }
    let counts: (i64, i64) = sqlx::query_as(
        "select (select count(*) from document where doc_type = 'imaging_report'), \
                (select count(*) from job where kind = 'import.pdf')",
    )
    .fetch_one(&e.rt.pool)
    .await?;
    assert_eq!(counts, (0, 0));
    assert_eq!(e.fhir(U1).await?["numberOfInstances"], 2);
    Ok(())
}

#[tokio::test]
async fn round5_an_exported_study_has_a_modality_for_every_series() -> R {
    use std::io::{Cursor, Read};
    let e = env().await?;
    let st = Study {
        series: 1,
        per_series: 1,
        modality: "",
        report: true,
        ..Study::new(U1, ALEX)
    };
    e.upload("alex", &study_files(&st, "DICOM")).await?;
    e.run().await?;
    let mut out = Cursor::new(Vec::new());
    let mut tx = e.rt.pool.begin().await?;
    ownhealth_ops::export::export_profile(&mut tx, &e.rt.blobs, e.fam.profile("alex"), &mut out, "zip").await?;
    let mut export = zip::ZipArchive::new(out)?;
    let mut resource = String::new();
    export
        .by_name("fhir/ImagingStudy.ndjson")?
        .read_to_string(&mut resource)?;
    let resource: Value = serde_json::from_str(resource.trim())?;
    for (index, code) in ["OT", "DOC"].into_iter().enumerate() {
        assert_eq!(
            resource["series"][index]["modality"],
            serde_json::json!({"system": "http://dicom.nema.org/resources/ontology/DCM", "code": code}),
        );
    }
    Ok(())
}

#[tokio::test]
async fn round5_lookup_rejects_malformed_orthanc_responses() -> R {
    use serde_json::json;
    let fake = FakeOrthanc::start().await?;
    let client = ownhealth_imaging::orthanc::Orthanc::new(fake.config())?;
    assert!(client.studies_under(U1).await?.is_empty());
    let mut accepted = Vec::new();
    for invalid in [
        json!({}),
        json!([{"Type": "Study"}]),
        json!([{"Type": "Study", "ID": "invalid"}]),
    ] {
        *fake.lookup_response.lock().expect("test setting") = Some(invalid.clone());
        if client.studies_under(U1).await.is_ok() {
            accepted.push(invalid);
        }
    }
    assert!(accepted.is_empty(), "accepted malformed responses: {accepted:?}");
    Ok(())
}

#[tokio::test]
async fn round5_an_unreadable_name_does_not_hide_an_issuer_conflict() -> R {
    let e = env().await?;
    let st = Study {
        series: 1,
        per_series: 1,
        issuer: "LABNORTE",
        ..Study::new(U1, ALEX)
    };
    let mut other = study_files(
        &Study {
            issuer: "OTHER",
            ..st.clone()
        },
        "other",
    );
    patch(&mut other, b"RIVERA^ALEX", b"RIVERA^\xc1LEX");
    let mut files = study_files(&st, "DICOM");
    files.extend(other);
    e.upload("alex", &files).await?;
    e.run().await?;
    assert_eq!(e.in_review("alex").await?, [(U1.into(), "identity_conflict".into())]);
    assert_eq!(e.orthanc.posts(), 0);
    Ok(())
}

#[tokio::test]
async fn round5_an_unreadable_name_keeps_a_utf8_id_through_accept() -> R {
    let e = env().await?;
    let st = Study {
        person: Person { id: "É-1", ..ALEX },
        charset: "ISO_IR 192",
        series: 1,
        per_series: 1,
        ..Study::new(U1, ALEX)
    };
    let mut files = study_files(&st, "DICOM");
    patch(&mut files, b"RIVERA^ALEX", b"RIVERA^\xc1LEX");
    e.upload("alex", &files).await?;
    e.run().await?;
    assert_eq!(e.in_review("alex").await?, [(U1.into(), "unsupported_charset".into())]);
    let mut c = e.rt.pool.acquire().await?;
    let item = sqlx::query_scalar("select id from review_item where kind = $1")
        .bind(REVIEW_KIND)
        .fetch_one(&mut *c)
        .await?;
    ownhealth_core::review::accept(&mut c, item, e.fam.user("alex"), None).await?;
    drop(c);
    e.run().await?;
    assert!(e.recorded(U1).await?.is_some());
    assert_eq!(e.orthanc.patients_under(U1), ["É-1".into()].into());
    Ok(())
}

#[tokio::test]
async fn round5_the_open_archive_is_reused_across_studies_and_report_extraction() -> R {
    let e = env().await?;
    let first = Study {
        series: 1,
        per_series: 1,
        ..Study::new(U1, ALEX)
    };
    let second = Study {
        uid: U2.into(),
        report: true,
        ..first.clone()
    };
    let files = hospital_cd(&[first, second]);
    let up = e.upload("alex", &files).await?;
    let sha: String = sqlx::query_scalar("select encode(zip_blob, 'hex') from imaging_import")
        .fetch_one(&e.rt.pool)
        .await?;
    let owner = BlobOwner::Profile(e.fam.profile("alex"));
    let blobs = e.rt.blobs.clone();
    // Once the first study has staged an instance, unlink the stored file. The already open
    // reader still has its original contents, but opening the archive a second time must fail.
    *e.orthanc.after_store.lock().expect("test setting") = Some(Box::new(move || {
        assert!(blobs.delete(owner, &sha).expect("unlink test ZIP"));
    }));
    tokio::time::timeout(Duration::from_secs(30), e.run()).await??;
    let (state, result, error) = e.job(up.job_id).await?;
    assert_eq!(state, "done", "{result} {error:?}");
    assert_eq!(result["imported"], 2, "{result}");
    assert_eq!(result["reports"], 1, "{result}");
    Ok(())
}

#[tokio::test]
async fn round5_series_counts_cross_the_128_row_boundary() -> R {
    use axum::{Json, Router, extract::State, routing::post};
    use serde_json::json;
    use std::sync::{Arc, Mutex};

    const ID: &str = "00000000-00000000-00000000-00000000-00000000";
    type Calls = Arc<Mutex<Vec<(u64, u64)>>>;
    let calls = Calls::default();
    let app = Router::new()
        .route(
            "/tools/find",
            post(|State(calls): State<Calls>, Json(query): Json<Value>| async move {
                let since = query["Since"].as_u64().expect("since");
                let limit = query["Limit"].as_u64().expect("limit");
                calls.lock().expect("calls").push((since, limit));
                let page: Vec<Value> = (since..129)
                    .take(limit as usize)
                    .map(|n| {
                        json!({
                            "MainDicomTags": {"SeriesInstanceUID": format!("{U1}.{n:03}"), "Modality": "CT"},
                            "RequestedTags": {"NumberOfSeriesRelatedInstances": "1"},
                        })
                    })
                    .collect();
                Json(page)
            }),
        )
        .with_state(calls.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let client = ownhealth_imaging::orthanc::Orthanc::new(ownhealth_core::orthanc::OrthancConfig {
        url: format!("http://{}", listener.local_addr()?),
        user: support::USER.into(),
        password: ownhealth_core::ids::token_hex(16),
    })?;
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    let series = tokio::time::timeout(Duration::from_secs(10), client.series_of(ID)).await??;
    assert_eq!(*calls.lock().expect("calls"), [(0, 128), (128, 128)]);
    assert_eq!(
        series.into_iter().map(|s| s.uid).collect::<Vec<_>>(),
        (0..129).map(|n| format!("{U1}.{n:03}")).collect::<Vec<_>>(),
    );
    Ok(())
}

/// A study the person accepted despite an unreadable PatientID is recorded. The same files on a new
/// upload are already stored: no new review item and no POST.
#[tokio::test]
async fn round6_an_accepted_charset_study_replays_as_a_no_op() -> R {
    let e = env().await?;
    let st = Study {
        charset: "ISO_IR 192",
        series: 1,
        per_series: 2,
        ..Study::new(U1, ALEX)
    };
    let mut files = study_files(&st, "DICOM");
    patch(&mut files, b"ISO_IR 192", b"ISO_IR 148");
    e.upload("alex", &files).await?;
    e.run().await?;
    assert_eq!(e.in_review("alex").await?, [(U1.into(), "unsupported_charset".into())]);
    let item: Uuid = sqlx::query_scalar("select id from review_item where kind = $1")
        .bind(REVIEW_KIND)
        .fetch_one(&e.rt.pool)
        .await?;
    let mut c = e.rt.pool.acquire().await?;
    ownhealth_core::review::accept(&mut c, item, e.fam.user("alex"), None).await?;
    drop(c);
    e.run().await?;
    assert!(e.recorded(U1).await?.is_some());
    let posts = e.orthanc.posts();
    files.push(("README.TXT".into(), b"same disc, copied again".to_vec()));
    let up = e.upload("alex", &files).await?;
    e.run().await?;
    let (state, result, error) = e.job(up.job_id).await?;
    assert_eq!(state, "done", "{result} {error:?}");
    assert_eq!(e.in_review("alex").await?, [], "{result}");
    assert_eq!(result["already"], 1, "{result}");
    assert_eq!(e.orthanc.posts(), posts);
    Ok(())
}

/// Orthanc with OverwriteInstances answers Success to a POST of an instance it held already. A
/// retry after a lost reply must still never delete that instance.
#[tokio::test]
async fn round6_a_retry_never_deletes_an_instance_that_existed_before_its_post() -> R {
    use std::sync::atomic::Ordering::SeqCst;
    let e = env().await?;
    let st = Study {
        series: 1,
        per_series: 3,
        ..Study::new(U1, ALEX)
    };
    let disc = study_files(&st, "DICOM");
    e.upload("alex", &disc[..2]).await?;
    e.run().await?;
    let original = e.orthanc.held().instances;
    assert_eq!(original.len(), 2);
    e.orthanc.overwrite.store(true, SeqCst);
    // The two held SOPs are POSTed again (posts 3 and 4); the reply for the new one is lost.
    e.orthanc.lose_store_reply_at.store(5, SeqCst);
    let up = e.upload("alex", &disc).await?;
    tokio::time::timeout(Duration::from_secs(30), e.run()).await??;
    assert_eq!(e.job(up.job_id).await?.0, "queued");
    sqlx::query("update job set run_after = now() where id = $1")
        .bind(up.job_id)
        .execute(&e.rt.pool)
        .await?;
    tokio::time::timeout(Duration::from_secs(30), e.run()).await??;
    let (state, result, error) = e.job(up.job_id).await?;
    assert_eq!(state, "done", "{result} {error:?}");
    let held = e.orthanc.held();
    let deleted: Vec<_> = held
        .log
        .iter()
        .filter(|(method, _)| method == "DELETE")
        .map(|(_, path)| path.trim_start_matches("/instances/"))
        .collect();
    assert!(
        original.keys().all(|id| !deleted.contains(&id.as_str())),
        "deleted {deleted:?}, held before {:?}",
        original.keys()
    );
    assert_eq!(held.instances.len(), 3);
    assert_eq!(e.fhir(U1).await?["numberOfInstances"], 3);
    Ok(())
}
