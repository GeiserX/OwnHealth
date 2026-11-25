//! The `imaging.sweep` job: Orthanc holds only what the record names (slice 43).
//!
//! Orthanc knows nothing of profiles, so a profile delete (`ownhealth_auth::household`) never calls
//! it. This job brings Orthanc back to the record instead, at every start of the app, every hour, and
//! right after a profile delete. It works whether Orthanc was down at the delete or not:
//!
//! 0. Each upload from before `imaging_import_study` gets the links its import would have made, read
//!    from its zip, and from then on is removed by the rules of a later upload
//!    ([`crate::rebuild::complete_links`]). Then an upload whose last held study a person closed while
//!    its import ran, and which that import did not remove (it failed for good, or its check came
//!    first), is removed as the close would have ([`crate::remove::settle_left_behind`]). This needs
//!    no Orthanc.
//! 1. The POST journal of an upload that is gone (its profile was purged while an Orthanc POST lost
//!    its reply, and no job of it runs again) is drained: its new instances are taken back.
//! 2. Every Orthanc study that no `imaging_study.orthanc_id` names is deleted. A retracted study keeps
//!    its row, so it keeps its pixels: the viewer refuses it, and a restore of the row finds it whole.
//!    A study that another profile's record names is never touched. A row of a profile whose key a
//!    delete shredded names nothing: a restore of a backup taken before the delete brings the row
//!    back, and its study goes from Orthanc, as the viewer refuses it.
//! 3. A recorded study keeps only the series its record counts (`fhir.series`). A series an import
//!    added and then could not record (a retry whose check after the send failed) is deleted.
//! 4. A recorded study that Orthanc lacks, or holds without a series or an instance its record counts
//!    (a restore of the database, a lost Orthanc volume), queues `imaging.rebuild` for its profile
//!    ([`crate::rebuild`]).
//!
//! Every deletion happens under the one StudyInstanceUID lock every Orthanc writer takes
//! ([`ownhealth_core::orthanc::lock_study`]), after the record is read again under it, so a study an
//! import is recording right now is never deleted. Two sweeps at once (the hourly one and one after a
//! profile delete) are safe for the same reason, and a study the other one deleted counts as gone.
//!
//! When Orthanc does not answer the list of its studies, the sweep ends without a failure, says so in
//! its result and queues one more sweep in five minutes; after that, the hourly sweep finishes the
//! work. A step that fails for one study or journal (Orthanc answers an error) does not stop the
//! others: the sweep goes on, then fails with the count, so the job retries and, when the step keeps
//! failing, ends with a `job_failed` item in review.

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
/// How long after a sweep that found Orthanc down the next one runs.
pub const RETRY_SECONDS: i32 = 300;

/// Queues a sweep in the caller's transaction, unless one is queued or running already.
pub async fn enqueue(conn: &mut PgConnection) -> sqlx::Result<Option<i64>> {
    enqueue_keyed(conn, JOB, &json!({})).await
}

/// Queues a sweep right after profile `profile_id` was deleted, in the caller's transaction. A sweep
/// that runs already may have read the record before the delete and would leave the profile's
/// studies, so this one has a key of its own.
pub async fn enqueue_after_delete(conn: &mut PgConnection, profile_id: Uuid) -> sqlx::Result<Option<i64>> {
    let key = format!("{JOB}:{profile_id}");
    enqueue_keyed(conn, &key, &json!({"profile_id": profile_id.to_string()})).await
}

async fn enqueue_keyed(conn: &mut PgConnection, key: &str, payload: &Value) -> sqlx::Result<Option<i64>> {
    enqueue_job(
        conn,
        JOB,
        payload,
        EnqueueOptions {
            max_attempts: 3,
            ..EnqueueOptions::dedupe(key)
        },
    )
    .await
}

/// One more sweep in [`RETRY_SECONDS`], after one that found Orthanc down. True when it was queued.
async fn retry_later(ctx: &JobContext) -> Result<bool, JobError> {
    let mut tx = ctx.pool.begin().await?;
    let id = enqueue_keyed(&mut tx, &format!("{JOB}:retry"), &json!({"retry": true})).await?;
    if let Some(id) = id {
        sqlx::query("update job set run_after = now() + make_interval(secs => $2) where id = $1")
            .bind(id)
            .bind(f64::from(RETRY_SECONDS))
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(id.is_some())
}

pub(crate) fn job(ctx: JobContext, job: Job) -> BoxFuture<'static, HandlerResult> {
    Box::pin(run(ctx, job))
}

/// Runs one sweep. Imaging off: nothing to do.
pub async fn run(ctx: JobContext, job: Job) -> HandlerResult {
    let Some(config) = ctx.settings.orthanc.clone() else {
        return Ok(Some(json!({"skipped": "imaging_off"})));
    };
    let orthanc = Orthanc::new(config).map_err(retry)?;
    // 0. the links of the uploads from before them, then the uploads a close left behind
    let links = crate::rebuild::complete_links(&ctx).await?;
    let settled = crate::remove::settle_left_behind(&ctx.pool).await?;
    let held = match orthanc.study_ids().await {
        Ok(held) => held,
        Err(e) => {
            tracing::warn!(error = %e, "the imaging sweep stopped: Orthanc did not answer");
            // a sweep that was itself the retry leaves the rest to the hourly one
            let again = job.payload.get("retry") != Some(&Value::Bool(true)) && retry_later(&ctx).await?;
            return Ok(Some(json!({
                "skipped": "orthanc_unreachable",
                "retry_queued": again,
                "links_completed": links,
                "settled_uploads": settled,
            })));
        }
    };
    let mut failed = Failed::default();
    let mut done = sweep(&ctx, &orthanc, &held, &mut failed).await?;
    done["links_completed"] = json!(links);
    done["settled_uploads"] = json!(settled);
    match failed.first {
        None => Ok(Some(done)),
        Some(first) => Err(JobError::typed(
            "OrthancError",
            format!(
                "the imaging sweep could not finish {} of its steps in Orthanc, and did the others ({done}); \
                 the first: {first}",
                failed.count
            ),
        )),
    }
}

/// The steps that failed in Orthanc: the sweep goes on without them.
#[derive(Default)]
struct Failed {
    count: usize,
    first: Option<String>,
}

impl Failed {
    /// Counts an Orthanc failure; any other failure stops the sweep.
    fn note(&mut self, e: JobError) -> Result<(), JobError> {
        if e.type_name != "OrthancError" {
            return Err(e);
        }
        tracing::warn!(error = %e.message, "an imaging sweep step failed in Orthanc; the sweep goes on");
        self.count += 1;
        self.first.get_or_insert(e.message);
        Ok(())
    }
}

/// A recorded study: its profile, StudyInstanceUID, the series its record counts and its FHIR
/// ImagingStudy (with the instances of each series).
struct Named {
    profile_id: Uuid,
    uid: String,
    series: BTreeSet<String>,
    fhir: Value,
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

async fn sweep(ctx: &JobContext, orthanc: &Orthanc, held: &[String], failed: &mut Failed) -> Result<Value, JobError> {
    // 1. the journal of a purged upload
    let orphaned: Vec<String> = sqlx::query_scalar(
        "select distinct p.study_instance_uid from imaging_import_post p \
         where not exists (select 1 from imaging_import i where i.id = p.import_id) order by 1",
    )
    .fetch_all(&ctx.pool)
    .await?;
    let mut journal = None;
    let mut drained = 0;
    for uid in &orphaned {
        let mut tx = ctx.pool.begin().await?;
        lock_study(&mut tx, uid).await?;
        match recover_posts(&mut tx, &mut journal, &ctx.pool, orthanc, uid).await {
            Ok(()) => {
                tx.commit().await?;
                drained += 1;
            }
            Err(e) => failed.note(e)?,
        }
    }
    drop(journal);

    // 2. what the record names, and what Orthanc holds
    // A row of a profile whose key a delete shredded names nothing: a restore of a backup taken before
    // that delete brought it back, and its images stay closed.
    let rows: Vec<(String, Uuid, String, Value)> = sqlx::query_as(
        "select s.orthanc_id, s.profile_id, s.study_instance_uid, s.fhir from imaging_study s \
         where s.orthanc_id is not null and not exists (select 1 from profile_key k \
         where k.profile_id = s.profile_id and k.wrapped_dek is null)",
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
                    fhir,
                },
            )
        })
        .collect();
    let mut removed_studies = 0;
    for id in held.iter().filter(|id| !named.contains_key(*id)) {
        match remove_unnamed(ctx, orthanc, id).await {
            Ok(true) => removed_studies += 1,
            Ok(false) => {}
            Err(e) => failed.note(e)?,
        }
    }

    // 3 and 4. the recorded studies
    let held: BTreeSet<&String> = held.iter().collect();
    let mut removed_series = 0;
    let mut rebuild: BTreeSet<Uuid> = BTreeSet::new();
    for (id, study) in &named {
        match check_recorded(ctx, orthanc, held.contains(id), id, study).await {
            Ok((lacking, removed)) => {
                if lacking {
                    rebuild.insert(study.profile_id);
                }
                removed_series += removed;
            }
            Err(e) => failed.note(e)?,
        }
    }
    let mut queued = 0;
    for profile_id in &rebuild {
        let mut conn = ctx.pool.acquire().await?;
        if crate::rebuild::enqueue(&mut conn, *profile_id).await?.is_some() {
            queued += 1;
        }
    }
    Ok(json!({
        "drained_journals": drained,
        "removed_studies": removed_studies,
        "removed_series": removed_series,
        "rebuild_profiles": rebuild.len(),
        "rebuilds_queued": queued,
    }))
}

/// Deletes Orthanc study `id`, which the record did not name, unless the record names it once read
/// again under its UID lock. True when it went.
async fn remove_unnamed(ctx: &JobContext, orthanc: &Orthanc, id: &str) -> Result<bool, JobError> {
    // another sweep may have deleted it since the list
    let Some(info) = orthanc.study_if_held(id).await.map_err(retry)? else {
        return Ok(false);
    };
    let mut tx = ctx.pool.begin().await?;
    lock_study(&mut tx, &info.study_instance_uid).await?;
    let still: bool = sqlx::query_scalar(
        "select exists (select 1 from imaging_study s where s.orthanc_id = $1 and not exists \
         (select 1 from profile_key k where k.profile_id = s.profile_id and k.wrapped_dek is null))",
    )
    .bind(id)
    .fetch_one(&mut *tx)
    .await?;
    if !still {
        orthanc.delete_study(id).await.map_err(retry)?;
    }
    tx.commit().await?;
    Ok(!still)
}

/// A recorded study: whether Orthanc lacks it, or a series or an instance its record counts (so its
/// profile needs a rebuild), and how many series no record counts it deleted.
async fn check_recorded(
    ctx: &JobContext,
    orthanc: &Orthanc,
    listed: bool,
    id: &str,
    study: &Named,
) -> Result<(bool, usize), JobError> {
    let series = if listed {
        orthanc.series_ids(id).await.map_err(retry)?
    } else {
        None
    };
    let Some(series) = series else {
        return Ok((true, 0));
    };
    let uids: BTreeSet<&String> = series.iter().map(|(_, uid)| uid).collect();
    // a counted series Orthanc lacks, or one that holds fewer instances than the record counts
    let lacking = study.series.iter().any(|s| !uids.contains(s))
        || !crate::rebuild::covers(&crate::rebuild::held_counts(orthanc, id).await?, &study.fhir);
    if series.iter().all(|(_, uid)| study.series.contains(uid)) {
        return Ok((lacking, 0));
    }
    // read the record again under the lock: an import may have just counted the series
    let mut tx = ctx.pool.begin().await?;
    lock_study(&mut tx, &study.uid).await?;
    let fhir: Option<Value> = sqlx::query_scalar("select fhir from imaging_study where orthanc_id = $1")
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?;
    let mut removed = 0;
    // one series Orthanc cannot delete does not keep the others: each is tried, then the first
    // failure fails this study's step, so the job retries
    let mut first_failure = None;
    if let Some(fhir) = fhir {
        let counted = recorded_series(&fhir);
        for (series_id, uid) in &series {
            if !counted.contains(uid) {
                match orthanc.delete_series(series_id).await.map_err(retry) {
                    Ok(_) => removed += 1,
                    Err(e) => {
                        first_failure.get_or_insert(e);
                    }
                }
            }
        }
    }
    tx.commit().await?;
    if let Some(e) = first_failure {
        return Err(e);
    }
    Ok((lacking, removed))
}
