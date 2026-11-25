//! Taking back an upload that went to the wrong profile (slice 43).
//!
//! A disc uploaded to the wrong profile holds a patient who is not the profile's, so its studies wait
//! in review and none of them reaches the record. Its zip stays a blob of that profile until the upload
//! is removed: when the person closes (dismisses or rejects) the last held study of it, or through
//! `DELETE /api/imaging/imports/{id}`. Either way the removal refuses while a study of the upload is
//! on the record (`imaging_import_study`; for an upload from before that table, while its profile has
//! any recorded study, until the sweep gives it its links: [`crate::rebuild::complete_links`]), or
//! while its import is queued or running. A close that found the import
//! running is settled again when that import ends ([`settle_after_import`]), and the hourly sweep
//! settles any upload such a close left ([`settle_left_behind`]). The upload row
//! goes at once; the `imaging.release_upload` job then removes the zip, once no row of the profile
//! names those bytes (and the plain copy once no row names them at all: the same check as a profile
//! delete, `household::BLOB_COLUMNS`), waiting while a backup copies the files
//! (`blobs::removal_lock`). A held study of a removed upload that is reopened refuses Accept with
//! [`crate::import::UPLOAD_REMOVED`].
//!
//! Locks, in one order for every path: a held study's review item (a close or an accept holds its own
//! item first; `DELETE` takes the open ones in id order), then the blob lock an upload of the same zip
//! holds until its queued import commits (`documents::lock_blob`), then the upload row (an accept reads
//! it with a shared lock). A close after a close reads the other items without a lock: while one is
//! still being closed in another transaction it shows as open, so that close answers Waiting, and the
//! other one, which waits on the upload's lock, removes the upload.

use futures::future::BoxFuture;
use ownhealth_auth::household::{held_by, still_referenced};
use ownhealth_core::blobs::{BlobOwner, removal_lock, sha256_hex_to_bytes};
use ownhealth_core::documents::lock_blob;
use ownhealth_core::jobs::{EnqueueOptions, HandlerResult, Job, JobContext, JobError, enqueue};
use ownhealth_core::review::ReviewItem;
use ownhealth_core::{Error, Result};
use serde_json::json;
use sqlx::{PgConnection, PgPool};
use uuid::Uuid;

use crate::import::{JOB as IMPORT_JOB, REVIEW_KIND};

/// The job that removes a removed upload's zip.
pub const RELEASE_JOB: &str = "imaging.release_upload";
/// The decision reason of a held study closed by the removal of its upload.
pub const REMOVED_REASON: &str = "upload removed";

/// What a removal did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Removal {
    /// The upload is gone, with its held studies that were still open; its zip goes by job.
    Removed { dismissed: usize },
    /// There is no such upload.
    Gone,
    /// A study of the upload is on the record.
    Recorded,
    /// Its import is queued or running.
    Importing,
    /// A held study of it still waits in review (only when the open ones are not closed with it).
    Waiting,
}

/// Removes upload `import_id` in the caller's transaction. With `close_open`, its held studies that
/// still wait in review are dismissed with it (by `user`); without, an open one keeps the upload.
pub async fn remove(conn: &mut PgConnection, import_id: Uuid, user: Option<Uuid>, close_open: bool) -> Result<Removal> {
    remove_except(conn, import_id, user, close_open, None).await
}

/// [`remove`], with `own_job` (the running `import.dicom` job that calls it) not counted as an import
/// that runs.
async fn remove_except(
    conn: &mut PgConnection,
    import_id: Uuid,
    user: Option<Uuid>,
    close_open: bool,
    own_job: Option<i64>,
) -> Result<Removal> {
    // the items first, as a close or an accept of one of them takes them
    let open = if close_open {
        open_items(&mut *conn, import_id, true).await?
    } else {
        Vec::new()
    };
    let row: Option<(Uuid, Vec<u8>)> = sqlx::query_as("select profile_id, zip_blob from imaging_import where id = $1")
        .bind(import_id)
        .fetch_optional(&mut *conn)
        .await?;
    let Some((profile_id, zip)) = row else {
        return Ok(Removal::Gone);
    };
    let sha = hex::encode(&zip);
    // An upload of the same zip to the profile holds this lock shared until its queued import commits
    // (upload::record): wait for it, so that import counts below.
    lock_blob(&mut *conn, profile_id, &sha, false).await?;
    // An accept of a held study reads the upload with a shared lock (import::accept).
    let locked: Option<Uuid> = sqlx::query_scalar("select id from imaging_import where id = $1 for update")
        .bind(import_id)
        .fetch_optional(&mut *conn)
        .await?;
    if locked.is_none() {
        return Ok(Removal::Gone);
    }
    // An upload from before `imaging_import_study` (migration 1234) can hold part of a recorded study
    // with no link (a second disc that added a series): it stays while its profile has one, until the
    // sweep gives it its links (rebuild::complete_links).
    let recorded: bool = sqlx::query_scalar(
        "select exists (select 1 from imaging_import_study where import_id = $1) \
         or exists (select 1 from imaging_study where import_id = $1) \
         or exists (select 1 from imaging_import i join imaging_study s on s.profile_id = i.profile_id \
         where i.id = $1 and not i.links_complete)",
    )
    .bind(import_id)
    .fetch_one(&mut *conn)
    .await?;
    if recorded {
        return Ok(Removal::Recorded);
    }
    let importing: bool = sqlx::query_scalar(
        "select exists (select 1 from job where kind = $1 and state in ('queued', 'running') \
         and payload->>'import_id' = $2 and id is distinct from $3)",
    )
    .bind(IMPORT_JOB)
    .bind(import_id.to_string())
    .bind(own_job)
    .fetch_one(&mut *conn)
    .await?;
    if importing {
        return Ok(Removal::Importing);
    }
    // Read again under the upload's lock, without locking them (an accept takes its item, then the
    // upload): an import that ended since the read above may have held one more study, which waits.
    // Without `close_open`, any open one keeps the upload.
    let now_open = open_items(&mut *conn, import_id, false).await?;
    if now_open.iter().any(|id| !open.contains(id)) {
        return Ok(Removal::Waiting);
    }
    sqlx::query(
        "update review_item set status = 'dismissed', decided_by = $2, decided_at = now(), decision_reason = $3 \
         where id = any($1)",
    )
    .bind(&open)
    .bind(user)
    .bind(REMOVED_REASON)
    .execute(&mut *conn)
    .await?;
    sqlx::query("delete from imaging_import where id = $1")
        .bind(import_id)
        .execute(&mut *conn)
        .await?;
    let key = format!("{RELEASE_JOB}:{profile_id}:{sha}");
    let payload = json!({"profile_id": profile_id.to_string(), "zip_sha256": sha});
    if enqueue(&mut *conn, RELEASE_JOB, &payload, EnqueueOptions::dedupe(&key))
        .await?
        .is_none()
    {
        // A release of the same zip that is still marked running may have read the rows already (and
        // found this upload naming the bytes): it would end without seeing this removal. Free its key,
        // so this removal queues a release of its own. A queued one reads the rows later, and is enough.
        sqlx::query("update job set dedupe_key = null where dedupe_key = $1 and state = 'running'")
            .bind(&key)
            .execute(&mut *conn)
            .await?;
        enqueue(&mut *conn, RELEASE_JOB, &payload, EnqueueOptions::dedupe(&key)).await?;
    }
    Ok(Removal::Removed { dismissed: open.len() })
}

/// The held studies of upload `import_id` that still wait in review, in id order; with `lock`, each
/// one locked.
async fn open_items(conn: &mut PgConnection, import_id: Uuid, lock: bool) -> Result<Vec<Uuid>> {
    let sql = if lock {
        "select id from review_item where kind = $1 and status = 'open' and ref_table = 'imaging_import' \
         and ref_id = $2 order by id for update"
    } else {
        "select id from review_item where kind = $1 and status = 'open' and ref_table = 'imaging_import' \
         and ref_id = $2 order by id"
    };
    Ok(sqlx::query_scalar(sql)
        .bind(REVIEW_KIND)
        .bind(import_id)
        .fetch_all(&mut *conn)
        .await?)
}

/// After the `import.dicom` job `own_job` of upload `import_id` ended, in a transaction of its own:
/// when a person closed the upload's last held study while that job ran (the close then found the
/// import running, and left the upload), removes the upload now, as that close would have. An upload
/// that held no study at all, or still holds an open one, stays. True when it went.
pub async fn settle_after_import(conn: &mut PgConnection, import_id: Uuid, own_job: i64) -> Result<bool> {
    let closed: bool = sqlx::query_scalar(
        "select exists (select 1 from review_item where kind = $1 and ref_table = 'imaging_import' \
         and ref_id = $2 and status in ('dismissed', 'rejected'))",
    )
    .bind(REVIEW_KIND)
    .bind(import_id)
    .fetch_one(&mut *conn)
    .await?;
    if !closed {
        return Ok(false);
    }
    Ok(matches!(
        remove_except(conn, import_id, None, false, Some(own_job)).await?,
        Removal::Removed { .. }
    ))
}

/// For the hourly sweep: removes, each in a transaction of its own, every upload a close left behind.
/// A close that found the upload's import running leaves the upload to that import
/// ([`settle_after_import`]); when the import then fails for good, or its check ran before the close
/// committed, nothing else would remove it. Each upload with a closed held study and none open is
/// settled as that close would have settled it. The number removed.
pub async fn settle_left_behind(pool: &PgPool) -> Result<usize> {
    let left: Vec<Uuid> = sqlx::query_scalar(
        "select i.id from imaging_import i where exists (select 1 from review_item r where r.kind = $1 \
         and r.ref_table = 'imaging_import' and r.ref_id = i.id and r.status in ('dismissed', 'rejected')) \
         and not exists (select 1 from review_item r where r.kind = $1 and r.ref_table = 'imaging_import' \
         and r.ref_id = i.id and r.status = 'open') order by i.created_at, i.id",
    )
    .bind(REVIEW_KIND)
    .fetch_all(pool)
    .await?;
    let mut removed = 0;
    for import_id in left {
        let mut tx = pool.begin().await?;
        if matches!(remove(&mut tx, import_id, None, false).await?, Removal::Removed { .. }) {
            removed += 1;
        }
        tx.commit().await?;
    }
    Ok(removed)
}

/// After a held study was dismissed or rejected, in the same transaction: removes its upload when
/// that was the last held study of it and nothing of the upload is on the record. True when it did.
pub async fn settle(conn: &mut PgConnection, item: &ReviewItem) -> Result<bool> {
    if item.kind != REVIEW_KIND || item.ref_table.as_deref() != Some("imaging_import") {
        return Ok(false);
    }
    let Some(import_id) = item.ref_id else {
        return Ok(false);
    };
    Ok(matches!(
        remove(conn, import_id, None, false).await?,
        Removal::Removed { .. }
    ))
}

pub(crate) fn release_job(ctx: JobContext, job: Job) -> BoxFuture<'static, HandlerResult> {
    Box::pin(release(ctx, job))
}

/// Removes a removed upload's zip, unless a row still names the bytes. Payload: `profile_id`,
/// `zip_sha256`.
pub async fn release(ctx: JobContext, job: Job) -> HandlerResult {
    let profile_id = job.uuid("profile_id")?;
    let sha = job
        .str("zip_sha256")
        .filter(|s| s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit()))
        .ok_or_else(|| JobError::permanent("the job names no zip"))?
        .to_ascii_lowercase();
    let mut tx = ctx.pool.begin().await?;
    removal_lock(&mut tx, true).await.map_err(Error::from)?;
    // the lock an upload of the same bytes to the profile takes shared (upload::record)
    lock_blob(&mut tx, profile_id, &sha, false).await?;
    // every blob column a profile delete checks (household::BLOB_COLUMNS), with the same ownership
    let bytes = sha256_hex_to_bytes(&sha).map_err(Error::from)?;
    let owner = BlobOwner::Profile(profile_id);
    let gone = if !still_referenced(&mut tx, &bytes).await? {
        ctx.blobs.delete(owner, &sha).map_err(Error::from)?
    } else if !held_by(&mut tx, profile_id, &sha).await? {
        ctx.blobs.drop_sealed(owner, &sha).map_err(Error::from)?
    } else {
        false
    };
    tx.commit().await?;
    Ok(Some(json!({"released": gone})))
}
