//! The `imaging.rebuild` job: Orthanc rebuilt from the stored zips (slice 43).
//!
//! Orthanc's storage is a cache and stays out of backups: every upload is kept as one zip of its
//! profile (`imaging_import.zip_blob`), and the record says which study Orthanc should hold for each
//! UID (`imaging_study.orthanc_id`) with which series (`fhir.series`). After a restore, or when the
//! Orthanc volume is lost, the sweep ([`crate::sweep`]) finds recorded studies that Orthanc lacks
//! and queues this job for their profile.
//!
//! The job replays, for each such study, every upload that holds it (`imaging_import_study`): a
//! second disc can add series to a recorded study, so the first upload alone is not enough. It sends
//! each file of the study under the one StudyInstanceUID lock every Orthanc writer takes
//! ([`ownhealth_core::orthanc::lock_study`]), after it reads the record again under that lock.
//! Orthanc files a study by PatientID and UID, so the rebuilt study gets the id the record names; a
//! file Orthanc files under another study is taken back at once. A retracted study is rebuilt too:
//! its row stays, and so do its pixels. The record is not changed. An upload whose zip does not open
//! (its profile's key was shredded) is counted and left out.

use std::collections::{BTreeMap, BTreeSet};

use futures::future::BoxFuture;
use ownhealth_core::blobs::BlobOwner;
use ownhealth_core::jobs::{EnqueueOptions, HandlerResult, Job, JobContext, JobError, enqueue as enqueue_job};
use ownhealth_core::orthanc::lock_study;
use serde_json::{Value, json};
use sqlx::PgConnection;
use uuid::Uuid;

use crate::import::{StudyFiles, retry};
use crate::orthanc::{Orthanc, Stored};
use crate::sweep::recorded_series;

/// The job kind.
pub const JOB: &str = "imaging.rebuild";

/// This run of the app: each rebuild names the run that queued it.
static RUN: std::sync::OnceLock<String> = std::sync::OnceLock::new();

fn this_run() -> &'static str {
    RUN.get_or_init(|| ownhealth_core::ids::token_hex(8))
}

/// Queues a rebuild of `profile_id`'s studies, unless one is queued or running, or one that this run
/// of the app queued finished in the last day and could not bring every study back (the sweep runs
/// every hour: without this it would read the same zips again each time). A rebuild of an earlier run
/// does not count: after a restart, and after a restore that brought its job back, Orthanc may be
/// another one.
pub async fn enqueue(conn: &mut PgConnection, profile_id: Uuid) -> sqlx::Result<Option<i64>> {
    let key = format!("{JOB}:{profile_id}");
    let stuck: bool = sqlx::query_scalar(
        "select exists (select 1 from job where dedupe_key = $1 and state = 'done' and payload->>'run' = $2 \
         and finished_at > now() - interval '1 day' and coalesce((result->>'missing')::int, 0) > 0)",
    )
    .bind(&key)
    .bind(this_run())
    .fetch_one(&mut *conn)
    .await?;
    if stuck {
        return Ok(None);
    }
    enqueue_job(
        conn,
        JOB,
        &json!({"profile_id": profile_id.to_string(), "run": this_run()}),
        EnqueueOptions::dedupe(&key),
    )
    .await
}

pub(crate) fn job(ctx: JobContext, job: Job) -> BoxFuture<'static, HandlerResult> {
    Box::pin(run(ctx, job))
}

/// Whether Orthanc holds study `id` with at least the instances its record counts in every series.
async fn complete(orthanc: &Orthanc, id: &str, fhir: &Value) -> Result<bool, JobError> {
    let held: BTreeMap<String, usize> = match orthanc.series_ids(id).await.map_err(retry)? {
        None => return Ok(false),
        Some(_) => orthanc
            .series_of(id)
            .await
            .map_err(retry)?
            .into_iter()
            .map(|s| (s.uid, s.instances))
            .collect(),
    };
    let counted = fhir["series"].as_array().cloned().unwrap_or_default();
    Ok(counted.iter().all(|s| {
        let uid = s["uid"].as_str().unwrap_or_default();
        let want = s["numberOfInstances"].as_u64().unwrap_or_default() as usize;
        held.get(uid).is_some_and(|n| *n >= want)
    }))
}

/// Runs one rebuild. Payload: `profile_id`.
pub async fn run(ctx: JobContext, job: Job) -> HandlerResult {
    let profile_id = job.uuid("profile_id")?;
    let Some(config) = ctx.settings.orthanc.clone() else {
        return Err(JobError::retry(
            "imaging is off: the rebuild waits for the imaging compose profile and OWNHEALTH_ORTHANC_URL",
        ));
    };
    let orthanc = Orthanc::new(config).map_err(retry)?;
    let recorded: Vec<(String, String, Value)> = sqlx::query_as(
        "select study_instance_uid, orthanc_id, fhir from imaging_study \
         where profile_id = $1 and orthanc_id is not null order by study_instance_uid",
    )
    .bind(profile_id)
    .fetch_all(&ctx.pool)
    .await?;
    // uid -> (Orthanc id, FHIR) of each study Orthanc lacks in full
    let mut needed: BTreeMap<String, (String, Value)> = BTreeMap::new();
    for (uid, id, fhir) in recorded {
        if !complete(&orthanc, &id, &fhir).await? {
            needed.insert(uid, (id, fhir));
        }
    }
    let uids: Vec<String> = needed.keys().cloned().collect();
    let uploads: Vec<(Uuid, Vec<u8>, Vec<String>)> = sqlx::query_as(
        "select i.id, i.zip_blob, array_agg(l.study_instance_uid order by l.study_instance_uid) \
         from imaging_import i join imaging_import_study l on l.import_id = i.id \
         where i.profile_id = $1 and l.study_instance_uid = any($2) \
         group by i.id, i.zip_blob, i.created_at order by i.created_at, i.id",
    )
    .bind(profile_id)
    .bind(&uids)
    .fetch_all(&ctx.pool)
    .await?;
    let owner = BlobOwner::Profile(profile_id);
    let (mut unreadable, mut sent, mut refused, mut misfiled) = (0, 0, 0, 0);
    for (import_id, zip, holds) in uploads {
        let (files, scan) = match StudyFiles::open(&ctx.blobs, owner, &hex::encode(&zip)).await {
            Ok(opened) => opened,
            Err(e) => {
                tracing::warn!(%import_id, error = %e, "an imaging upload does not open: the rebuild leaves it out");
                unreadable += 1;
                continue;
            }
        };
        let holds: BTreeSet<String> = holds.into_iter().collect();
        for (uid, study) in scan.studies.iter().filter(|(uid, _)| holds.contains(*uid)) {
            let Some((id, _)) = needed.get(uid) else {
                continue;
            };
            let mut tx = ctx.pool.begin().await?;
            lock_study(&mut tx, uid).await?;
            let still: Option<Option<String>> = sqlx::query_scalar(
                "select orthanc_id from imaging_study where study_instance_uid = $1 and profile_id = $2",
            )
            .bind(uid)
            .bind(profile_id)
            .fetch_optional(&mut *tx)
            .await?;
            if still.flatten().as_ref() != Some(id) {
                tx.commit().await?;
                continue;
            }
            for instance in &study.entries {
                let Some(staged) = files.stage_instance(instance.index).await? else {
                    continue;
                };
                match orthanc.store_file(staged.file, staged.size).await.map_err(retry)? {
                    Stored::Kept { study: parent, .. } if parent == *id => sent += 1,
                    Stored::Kept { id: other, new, .. } => {
                        // not the study the record names: take back what this send added
                        if new {
                            orthanc.delete_instance(&other).await.map_err(retry)?;
                        }
                        misfiled += 1;
                    }
                    Stored::Refused => refused += 1,
                }
            }
            tx.commit().await?;
        }
    }
    let (mut rebuilt, mut missing) = (0, 0);
    for (uid, (id, fhir)) in &needed {
        if complete(&orthanc, id, fhir).await? {
            for series in recorded_series(fhir) {
                orthanc.warm_series(uid, &series).await.map_err(retry)?;
            }
            rebuilt += 1;
        } else {
            missing += 1;
        }
    }
    Ok(Some(json!({
        "studies": needed.len(),
        "rebuilt": rebuilt,
        "missing": missing,
        "unreadable_uploads": unreadable,
        "sent_files": sent,
        "refused_files": refused,
        "misfiled_files": misfiled,
    })))
}
