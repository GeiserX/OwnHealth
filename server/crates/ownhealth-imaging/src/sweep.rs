//! The `imaging.sweep` job: Orthanc holds only what the record names (slice 43).
//!
//! Orthanc knows nothing of profiles, so a profile delete (`ownhealth_auth::household`) never calls
//! it. This job brings Orthanc back to the record instead, at every start of the app, every hour, and
//! right after a profile delete. It works whether Orthanc was down at the delete or not:
//!
//! 1. The POST journal of an upload that is gone (its profile was purged while an Orthanc POST lost
//!    its reply, and no job of it runs again) is drained: its new instances are taken back.
//! 2. Every Orthanc study that no `imaging_study.orthanc_id` names is deleted. A retracted study keeps
//!    its row, so it keeps its pixels: the viewer refuses it, and a restore of the row finds it whole.
//!    A study that another profile's record names is never touched.
//! 3. A recorded study keeps only the series its record counts (`fhir.series`). A series an import
//!    added and then could not record (a retry whose check after the send failed) is deleted.
//! 4. A recorded study that Orthanc lacks, or holds without a series its record counts (a restore of
//!    the database, a lost Orthanc volume), queues `imaging.rebuild` for its profile
//!    ([`crate::rebuild`]).
//!
//! Every deletion happens under the one StudyInstanceUID lock every Orthanc writer takes
//! ([`ownhealth_core::orthanc::lock_study`]), after the record is read again under it, so a study an
//! import is recording right now is never deleted. When Orthanc does not answer, the sweep ends
//! without a failure and says so in its result: the next one finishes the work.

use std::collections::{BTreeMap, BTreeSet};

use futures::future::BoxFuture;
use ownhealth_core::jobs::{EnqueueOptions, HandlerResult, Job, JobContext, JobError, enqueue as enqueue_job};
use ownhealth_core::orthanc::lock_study;
use serde_json::{Value, json};
use sqlx::PgConnection;
use uuid::Uuid;

use crate::import::{recover_posts, retry};
use crate::orthanc::Orthanc;

/// The job kind.
pub const JOB: &str = "imaging.sweep";
/// How often the app queues a sweep.
pub const EVERY_SECONDS: u64 = 3600;

/// Queues a sweep in the caller's transaction, unless one is queued or running already.
pub async fn enqueue(conn: &mut PgConnection) -> sqlx::Result<Option<i64>> {
    enqueue_job(
        conn,
        JOB,
        &json!({}),
        EnqueueOptions {
            max_attempts: 3,
            ..EnqueueOptions::dedupe(JOB)
        },
    )
    .await
}

pub(crate) fn job(ctx: JobContext, job: Job) -> BoxFuture<'static, HandlerResult> {
    Box::pin(run(ctx, job))
}

/// Runs one sweep. Imaging off: nothing to do.
pub async fn run(ctx: JobContext, _job: Job) -> HandlerResult {
    let Some(config) = ctx.settings.orthanc.clone() else {
        return Ok(Some(json!({"skipped": "imaging_off"})));
    };
    let orthanc = Orthanc::new(config).map_err(retry)?;
    match sweep(&ctx, &orthanc).await {
        Ok(done) => Ok(Some(done)),
        // the next sweep (hourly, and at every start) finishes the work
        Err(e) if e.type_name == "OrthancError" => {
            tracing::warn!(error = %e.message, "the imaging sweep stopped: Orthanc did not answer");
            Ok(Some(json!({"skipped": "orthanc_unreachable"})))
        }
        Err(e) => Err(e),
    }
}

/// A recorded study: its profile, StudyInstanceUID and the series its record counts.
struct Named {
    profile_id: Uuid,
    uid: String,
    series: BTreeSet<String>,
}

/// The SeriesInstanceUIDs of a record's FHIR ImagingStudy.
pub(crate) fn recorded_series(fhir: &Value) -> BTreeSet<String> {
    fhir["series"]
        .as_array()
        .map(|all| {
            all.iter()
                .filter_map(|s| s["uid"].as_str())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

async fn sweep(ctx: &JobContext, orthanc: &Orthanc) -> Result<Value, JobError> {
    // 1. the journal of a purged upload
    let orphaned: Vec<String> = sqlx::query_scalar(
        "select distinct p.study_instance_uid from imaging_import_post p \
         where not exists (select 1 from imaging_import i where i.id = p.import_id) order by 1",
    )
    .fetch_all(&ctx.pool)
    .await?;
    let mut journal = None;
    for uid in &orphaned {
        let mut tx = ctx.pool.begin().await?;
        lock_study(&mut tx, uid).await?;
        recover_posts(&mut tx, &mut journal, &ctx.pool, orthanc, uid).await?;
        tx.commit().await?;
    }
    drop(journal);

    // 2. what the record names, and what Orthanc holds
    let rows: Vec<(String, Uuid, String, Value)> = sqlx::query_as(
        "select orthanc_id, profile_id, study_instance_uid, fhir from imaging_study where orthanc_id is not null",
    )
    .fetch_all(&ctx.pool)
    .await?;
    let named: BTreeMap<String, Named> = rows
        .into_iter()
        .map(|(id, profile_id, uid, fhir)| {
            let series = recorded_series(&fhir);
            (
                id,
                Named {
                    profile_id,
                    uid,
                    series,
                },
            )
        })
        .collect();
    let held = orthanc.study_ids().await.map_err(retry)?;
    let mut removed_studies = 0;
    for id in held.iter().filter(|id| !named.contains_key(*id)) {
        let uid = orthanc.study(id).await.map_err(retry)?.study_instance_uid;
        let mut tx = ctx.pool.begin().await?;
        lock_study(&mut tx, &uid).await?;
        let still: bool = sqlx::query_scalar("select exists (select 1 from imaging_study where orthanc_id = $1)")
            .bind(id)
            .fetch_one(&mut *tx)
            .await?;
        if !still {
            orthanc.delete_study(id).await.map_err(retry)?;
            removed_studies += 1;
        }
        tx.commit().await?;
    }

    // 3 and 4. the recorded studies
    let held: BTreeSet<&String> = held.iter().collect();
    let mut removed_series = 0;
    let mut rebuild: BTreeSet<Uuid> = BTreeSet::new();
    for (id, study) in &named {
        let series = if held.contains(id) {
            orthanc.series_ids(id).await.map_err(retry)?
        } else {
            None
        };
        let Some(series) = series else {
            rebuild.insert(study.profile_id);
            continue;
        };
        let uids: BTreeSet<&String> = series.iter().map(|(_, uid)| uid).collect();
        if study.series.iter().any(|s| !uids.contains(s)) {
            rebuild.insert(study.profile_id);
        }
        if series.iter().all(|(_, uid)| study.series.contains(uid)) {
            continue;
        }
        // read the record again under the lock: an import may have just counted the series
        let mut tx = ctx.pool.begin().await?;
        lock_study(&mut tx, &study.uid).await?;
        let fhir: Option<Value> = sqlx::query_scalar("select fhir from imaging_study where orthanc_id = $1")
            .bind(id)
            .fetch_optional(&mut *tx)
            .await?;
        if let Some(fhir) = fhir {
            let counted = recorded_series(&fhir);
            for (series_id, uid) in &series {
                if !counted.contains(uid) {
                    orthanc.delete_series(series_id).await.map_err(retry)?;
                    removed_series += 1;
                }
            }
        }
        tx.commit().await?;
    }
    let mut queued = 0;
    for profile_id in &rebuild {
        let mut conn = ctx.pool.acquire().await?;
        if crate::rebuild::enqueue(&mut conn, *profile_id).await?.is_some() {
            queued += 1;
        }
    }
    Ok(json!({
        "drained_journals": orphaned.len(),
        "removed_studies": removed_studies,
        "removed_series": removed_series,
        "rebuild_profiles": rebuild.len(),
        "rebuilds_queued": queued,
    }))
}
