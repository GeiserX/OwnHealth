//! The `imaging.rebuild` job: Orthanc rebuilt from the stored zips (slice 43).
//!
//! Orthanc's storage is a cache and stays out of backups: every upload is kept as one zip of its
//! profile (`imaging_import.zip_blob`), and the record says which study Orthanc should hold for each
//! UID (`imaging_study.orthanc_id`) with which series (`fhir.series`). After a restore, or when the
//! Orthanc volume is lost, the sweep ([`crate::sweep`]) finds recorded studies that Orthanc lacks
//! and queues this job for their profile.
//!
//! The job replays, for each such study, every upload that holds it (`imaging_import_study`): a
//! second disc can add series to a recorded study, so the first upload alone is not enough. A study
//! recorded before that table names only its first upload; when one is still short after those
//! replays, every upload of the profile from before that table (`links_complete` false) whose import
//! is not queued or running, and that was not yet replayed for it, is read for its UID too, and each
//! one that holds it is linked. A later upload is linked by its own import, so an unlinked one (its
//! import has not run yet) is never read for a study. An upload whose part of the study waits in
//! review, or was dismissed or rejected there, sends nothing of it. It sends each file of the study
//! under the one StudyInstanceUID lock every Orthanc writer takes
//! ([`ownhealth_core::orthanc::lock_study`]), after it reads the record again under that lock.
//! Orthanc files a study by PatientID and UID, so the rebuilt study gets the id the record names; a
//! file Orthanc files under another study is taken back at once, and so is one an earlier try left
//! there (its take-back failed) when no record names that study. A retracted study is rebuilt too:
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

use crate::import::{REVIEW_KIND, StudyFiles, retry};
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
    if orthanc.series_ids(id).await.map_err(retry)?.is_none() {
        return Ok(false);
    }
    Ok(covers(&held_counts(orthanc, id).await?, fhir))
}

/// The instances Orthanc holds in each series of study `id`, by SeriesInstanceUID.
pub(crate) async fn held_counts(orthanc: &Orthanc, id: &str) -> Result<BTreeMap<String, usize>, JobError> {
    Ok(orthanc
        .series_of(id)
        .await
        .map_err(retry)?
        .into_iter()
        .map(|s| (s.uid, s.instances))
        .collect())
}

/// Whether `held` (instances per series) has at least the instances the record counts in every
/// series of its FHIR ImagingStudy.
pub(crate) fn covers(held: &BTreeMap<String, usize>, fhir: &Value) -> bool {
    let counted = fhir["series"].as_array().cloned().unwrap_or_default();
    counted.iter().all(|s| {
        let uid = s["uid"].as_str().unwrap_or_default();
        let want = s["numberOfInstances"].as_u64().unwrap_or_default() as usize;
        held.get(uid).is_some_and(|n| *n >= want)
    })
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
        "select s.study_instance_uid, s.orthanc_id, s.fhir from imaging_study s \
         where s.profile_id = $1 and s.orthanc_id is not null and not exists (select 1 from profile_key k \
         where k.profile_id = s.profile_id and k.wrapped_dek is null) order by s.study_instance_uid",
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
    let mut tally = Tally::default();
    // upload -> the studies it was replayed for
    let mut replayed: BTreeMap<Uuid, BTreeSet<String>> = BTreeMap::new();
    for (import_id, zip, holds) in uploads {
        let holds: BTreeSet<String> = holds.into_iter().collect();
        replay(
            &ctx, &orthanc, profile_id, import_id, &zip, &holds, &needed, false, &mut tally,
        )
        .await?;
        replayed.insert(import_id, holds);
    }
    // A study recorded before `imaging_import_study` existed names only its first upload (migration
    // 1234 copied `imaging_study.import_id`), so a series a later upload added is not replayed above.
    // When a study is still short, every upload of the profile from before that table is read for its
    // UID too (an upload replayed above for one study can hold an unlinked part of another), skipping
    // the pairs already replayed and an upload whose import is queued or running, and each one that
    // held it is linked to it. A later upload holds no study its import did not link.
    let mut short: BTreeSet<String> = BTreeSet::new();
    for (uid, (id, fhir)) in &needed {
        if !complete(&orthanc, id, fhir).await? {
            short.insert(uid.clone());
        }
    }
    if !short.is_empty() {
        let all: Vec<(Uuid, Vec<u8>)> = sqlx::query_as(
            "select i.id, i.zip_blob from imaging_import i where i.profile_id = $1 and not i.links_complete \
             and not exists (select 1 from job j where j.kind = $2 and j.state in ('queued', 'running') \
             and j.payload->>'import_id' = i.id::text) order by i.created_at, i.id",
        )
        .bind(profile_id)
        .bind(crate::import::JOB)
        .fetch_all(&ctx.pool)
        .await?;
        for (import_id, zip) in all {
            let holds: BTreeSet<String> = match replayed.get(&import_id) {
                Some(done) => short.difference(done).cloned().collect(),
                None => short.clone(),
            };
            if holds.is_empty() {
                continue;
            }
            replay(
                &ctx, &orthanc, profile_id, import_id, &zip, &holds, &needed, true, &mut tally,
            )
            .await?;
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
        "unreadable_uploads": tally.unreadable,
        "sent_files": tally.sent,
        "refused_files": tally.refused,
        "misfiled_files": tally.misfiled,
        "linked_uploads": tally.linked,
    })))
}

/// For the sweep: gives each upload from before `imaging_import_study` (`links_complete` false) whose
/// import is not queued or running the links its import would have made, then marks its links
/// complete, so its removal follows the rules of a later upload. Its zip is read for the studies it
/// holds, and each one its profile records is linked, unless the upload's part of it waits in review
/// or was dismissed or rejected there (the import did not record that part). It runs under the UID
/// lock of each of those studies, so a study being recorded right now is counted. An upload whose zip
/// does not open keeps its flag and is tried again at the next sweep. The number of uploads it
/// completed.
pub(crate) async fn complete_links(ctx: &JobContext) -> Result<usize, JobError> {
    let legacy: Vec<(Uuid, Uuid, Vec<u8>)> = sqlx::query_as(
        "select i.id, i.profile_id, i.zip_blob from imaging_import i where not i.links_complete \
         and not exists (select 1 from job j where j.kind = $1 and j.state in ('queued', 'running') \
         and j.payload->>'import_id' = i.id::text) order by i.created_at, i.id",
    )
    .bind(crate::import::JOB)
    .fetch_all(&ctx.pool)
    .await?;
    let mut completed = 0;
    for (import_id, profile_id, zip) in legacy {
        let scan = match StudyFiles::open(&ctx.blobs, BlobOwner::Profile(profile_id), &hex::encode(&zip)).await {
            Ok((_, scan)) => scan,
            Err(e) => {
                tracing::warn!(%import_id, error = %e.message, "an imaging upload does not open: its links stay incomplete");
                continue;
            }
        };
        let uids: Vec<String> = scan.studies.keys().cloned().collect();
        let mut tx = ctx.pool.begin().await?;
        for uid in &uids {
            lock_study(&mut tx, uid).await?;
        }
        // a removal (`for update`) waits, then sees the links
        let open: Option<bool> =
            sqlx::query_scalar("select not links_complete from imaging_import where id = $1 for no key update")
                .bind(import_id)
                .fetch_optional(&mut *tx)
                .await?;
        if open != Some(true) {
            tx.commit().await?;
            continue;
        }
        sqlx::query(
            "insert into imaging_import_study (import_id, study_instance_uid) \
             select $1, s.study_instance_uid from imaging_study s where s.profile_id = $2 \
             and s.study_instance_uid = any($3) and not exists (select 1 from review_item r \
             where r.kind = $4 and r.ref_table = 'imaging_import' and r.ref_id = $1 \
             and r.payload->>'study_uid' = s.study_instance_uid and r.status <> 'accepted') \
             on conflict do nothing",
        )
        .bind(import_id)
        .bind(profile_id)
        .bind(&uids)
        .bind(REVIEW_KIND)
        .execute(&mut *tx)
        .await?;
        sqlx::query("update imaging_import set links_complete = true where id = $1")
            .bind(import_id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        completed += 1;
    }
    Ok(completed)
}

/// What the replays of one rebuild did.
#[derive(Default)]
struct Tally {
    unreadable: usize,
    sent: usize,
    refused: usize,
    misfiled: usize,
    /// Uploads found to hold a study that `imaging_import_study` did not link them to.
    linked: usize,
}

/// Sends again, from upload `import_id` (its stored `zip`), each study of `holds` that the rebuild
/// `needed`, under the UID lock and after the record is read again under it. The upload row is held
/// (`for key share`) from then on, so a removal waits for the link below; an upload removed meanwhile
/// sends nothing. A study whose part in this upload a person holds in review (an open item), or
/// dismissed or rejected there, is not sent from it: that decision stays the person's. With `link`,
/// an upload that held such a study (Orthanc filed a file of it under the recorded study) is linked to
/// it in `imaging_import_study`.
#[allow(clippy::too_many_arguments)]
async fn replay(
    ctx: &JobContext,
    orthanc: &Orthanc,
    profile_id: Uuid,
    import_id: Uuid,
    zip: &[u8],
    holds: &BTreeSet<String>,
    needed: &BTreeMap<String, (String, Value)>,
    link: bool,
    t: &mut Tally,
) -> Result<(), JobError> {
    let owner = BlobOwner::Profile(profile_id);
    let (files, scan) = match StudyFiles::open(&ctx.blobs, owner, &hex::encode(zip)).await {
        Ok(opened) => opened,
        Err(e) => {
            tracing::warn!(%import_id, error = %e, "an imaging upload does not open: the rebuild leaves it out");
            t.unreadable += 1;
            return Ok(());
        }
    };
    for (uid, study) in scan.studies.iter().filter(|(uid, _)| holds.contains(*uid)) {
        let Some((id, _)) = needed.get(uid) else {
            continue;
        };
        let mut tx = ctx.pool.begin().await?;
        lock_study(&mut tx, uid).await?;
        // the upload row, as the link insert below would take it: a removal (`for update`) waits
        let here: Option<Uuid> = sqlx::query_scalar("select id from imaging_import where id = $1 for key share")
            .bind(import_id)
            .fetch_optional(&mut *tx)
            .await?;
        if here.is_none() {
            tx.commit().await?;
            return Ok(());
        }
        let held: bool = sqlx::query_scalar(
            "select exists (select 1 from review_item where kind = $1 and ref_table = 'imaging_import' \
             and ref_id = $2 and payload->>'study_uid' = $3 and status <> 'accepted')",
        )
        .bind(REVIEW_KIND)
        .bind(import_id)
        .bind(uid)
        .fetch_one(&mut *tx)
        .await?;
        if held {
            tx.commit().await?;
            continue;
        }
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
        let mut kept = 0;
        for instance in &study.entries {
            let Some(staged) = files.stage_instance(instance.index).await? else {
                continue;
            };
            match orthanc.store_file(staged.file, staged.size).await.map_err(retry)? {
                Stored::Kept { study: parent, .. } if parent == *id => {
                    t.sent += 1;
                    kept += 1;
                }
                Stored::Kept {
                    id: other,
                    study: parent,
                    new,
                } => {
                    // Not the study the record names: take back what this send added. A file Orthanc
                    // had already (a take-back of an earlier try failed) goes too when no record names
                    // its study either, as the sweep would delete that study; one inside a study a
                    // record names stays.
                    if new || !names(&mut tx, &parent).await? {
                        orthanc.delete_instance(&other).await.map_err(retry)?;
                    }
                    t.misfiled += 1;
                }
                Stored::Refused => t.refused += 1,
            }
        }
        if link && kept > 0 {
            let added = sqlx::query(
                "insert into imaging_import_study (import_id, study_instance_uid) values ($1, $2) \
                 on conflict do nothing",
            )
            .bind(import_id)
            .bind(uid)
            .execute(&mut *tx)
            .await?;
            t.linked += usize::try_from(added.rows_affected()).unwrap_or(0);
        }
        tx.commit().await?;
    }
    Ok(())
}

/// Whether a record names Orthanc study `id`.
async fn names(conn: &mut PgConnection, id: &str) -> Result<bool, JobError> {
    Ok(
        sqlx::query_scalar("select exists (select 1 from imaging_study where orthanc_id = $1)")
            .bind(id)
            .fetch_one(&mut *conn)
            .await?,
    )
}
