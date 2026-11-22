//! Withdraw everything one document brought in, for a retraction or a move to another profile
//! (`ingest/retract.py`).
//!
//! A document reaches the current view three ways: observations carry `document_id`; records,
//! encounters, notes and genome findings link through their `src.document_id`; wearable samples carry
//! `document_id` (0200); child documents (an export's ECGs) carry `flags.parent_document` and are
//! retracted with it. The facts are retracted (kept, out of the current view); samples have no
//! lifecycle, so they are deleted and the rollups they fed rebuilt. The document's import flags are
//! cleared, so importing it again runs the import instead of returning the old summary.
//!
//! Samples another still-active export also holds belong to whichever export brought them first
//! (Apple Health, CGM exports) or last (Gadgetbridge, whose exports are cumulative); they leave with
//! that one. A retracted Gadgetbridge or CGM export queues the profile's other active exports of its
//! kind for import again, so what they hold comes back.

use std::collections::{BTreeMap, BTreeSet};

use futures::future::BoxFuture;
use ownhealth_core::Result;
use ownhealth_core::lifecycle::{self, LIFECYCLE_TABLES};
use serde_json::{Map, Value, json};
use sqlx::{AssertSqlSafe, PgConnection};
use uuid::Uuid;

use crate::wearables;

/// Returns `{"observations": [ids], "records": {table: [ids]}, "wearable_samples": n, "documents": [ids]}`.
/// `documents` are the child documents the import created (an Apple Health export's ECGs), retracted
/// with it.
pub fn retract_document_facts<'c>(
    conn: &'c mut PgConnection,
    document_id: Uuid,
    reason: &'c str,
) -> BoxFuture<'c, Result<Value>> {
    Box::pin(async move {
        // a running import of a CGM export holds this lock: wait for it, so what it wrote leaves too
        lock_export_of(&mut *conn, document_id).await?;
        let mut observations: Vec<Value> = Vec::new();
        let mut records = Map::new();
        let children: Vec<Uuid> = sqlx::query_scalar(
            "update document set status = 'retracted', status_reason = $2 where flags->>'parent_document' = $1 \
             and status = 'active' returning id",
        )
        .bind(document_id.to_string())
        .bind(reason)
        .fetch_all(&mut *conn)
        .await?;
        for child in &children {
            retract_document_facts(&mut *conn, *child, reason).await?;
        }
        let mut tables: Vec<&str> = LIFECYCLE_TABLES.to_vec();
        tables.sort_unstable();
        for table in tables {
            let direct = if table == "observation" {
                " or f.document_id = $1"
            } else {
                ""
            };
            let sql = format!(
                "select f.id from {table} f left join src s on s.id = f.src_id \
                 where (s.document_id = $1{direct}) and f.status in ('current', 'provisional') order by f.id"
            );
            let ids: Vec<Uuid> = sqlx::query_scalar(AssertSqlSafe(sql.as_str()))
                .bind(document_id)
                .fetch_all(&mut *conn)
                .await?;
            for id in &ids {
                lifecycle::retract(&mut *conn, table, *id, reason).await?;
            }
            let ids: Vec<Value> = ids.iter().map(|i| json!(i.to_string())).collect();
            if table == "observation" {
                observations = ids;
            } else if !ids.is_empty() {
                records.insert(table.to_string(), Value::Array(ids));
            }
        }
        let gone: Vec<(Uuid, String, i16)> = sqlx::query_as(
            "delete from wearable_sample where document_id = $1 returning profile_id, metric, source_id",
        )
        .bind(document_id)
        .fetch_all(&mut *conn)
        .await?;
        let mut touched: BTreeMap<Uuid, (BTreeSet<String>, BTreeSet<i16>)> = BTreeMap::new();
        for (p, metric, source) in &gone {
            let e = touched.entry(*p).or_default();
            e.0.insert(metric.clone());
            e.1.insert(*source);
        }
        for (profile_id, (metrics, sources)) in touched {
            let metrics: Vec<String> = metrics.into_iter().collect();
            let sources: Vec<i16> = sources.into_iter().collect();
            wearables::recompute_rollups(&mut *conn, profile_id, Some(&metrics), Some(&sources)).await?;
        }
        sqlx::query("update document set flags = flags - 'import' - 'apple_health' - 'fhir_issuer' where id = $1")
            .bind(document_id)
            .execute(&mut *conn)
            .await?;
        if !gone.is_empty() {
            reimport_cumulative(&mut *conn, document_id).await?;
        }
        Ok(json!({
            "observations": observations,
            "records": records,
            "wearable_samples": gone.len(),
            "documents": children.iter().map(|c| c.to_string()).collect::<Vec<_>>(),
        }))
    })
}

/// The upload kinds whose other exports are imported again when one is retracted, with the flag their
/// import writes: Gadgetbridge (samples adopted with `Conflict::Adopt`) and the LibreView and Clarity
/// CSV exports (one job for both; readings kept by the first file with `Conflict::Keep`).
const REIMPORTED: [(&str, &str); 2] = [("gadgetbridge_db", "gadgetbridge"), ("libreview_csv", "cgm_csv")];

/// The exports of the same kind and profile that are still active are imported again, so the samples
/// the retracted export held in their place come back with them.
async fn reimport_cumulative(conn: &mut PgConnection, document_id: Uuid) -> Result<()> {
    for (kind, flag) in REIMPORTED {
        reimport_kind(&mut *conn, document_id, kind, flag).await?;
    }
    Ok(())
}

/// Queue the profile's other active exports of `kind` (those whose flags hold `flag`) for import again,
/// with their import summary cleared. A CGM export also calls it when its own readings are replaced.
pub async fn reimport_kind(conn: &mut PgConnection, document_id: Uuid, kind: &str, flag: &str) -> Result<()> {
    let Some(job) = ownhealth_core::documents::job_kind(kind) else {
        return Ok(());
    };
    let mime = ownhealth_core::documents::mime(kind).unwrap_or_default();
    let others: Vec<Uuid> = sqlx::query_scalar(
        "update document o set flags = o.flags - 'import' from document d where d.id = $1 and d.mime = $2 \
         and o.mime = $2 and o.profile_id = d.profile_id and o.id <> d.id and o.status = 'active' \
         and o.flags ? 'import' and o.flags ? $3 returning o.id",
    )
    .bind(document_id)
    .bind(mime)
    .bind(flag)
    .fetch_all(&mut *conn)
    .await?;
    for id in others {
        queue_after(&mut *conn, job, id).await?;
    }
    Ok(())
}

/// Queue `job` for the document `id`, to run after this transaction. A job still queued for it is
/// enough: it starts after this transaction removed its samples, so its writes see that. A job for it
/// that is running may have written before (its work commits before it is marked done), so one more run
/// is queued after the newest of them, under a key no other job holds.
async fn queue_after(conn: &mut PgConnection, job: &str, id: Uuid) -> Result<()> {
    use ownhealth_core::jobs::{EnqueueOptions, enqueue};
    let base = format!("{job}:reimport:{id}");
    let payload = json!({"document_id": id.to_string()});
    // a job that finishes between the look and the enqueue frees its key, so look again
    for _ in 0..3 {
        let held: Option<(String, i64)> = sqlx::query_as(
            "select state, id from job where (dedupe_key = $1 or starts_with(dedupe_key, $1 || ':after:')) \
             and state in ('queued', 'running') order by state = 'queued' desc, id desc limit 1",
        )
        .bind(&base)
        .fetch_optional(&mut *conn)
        .await?;
        let key = match held {
            Some((state, _)) if state == "queued" => return Ok(()),
            Some((_, running)) => format!("{base}:after:{running}"),
            None => base.clone(),
        };
        if enqueue(&mut *conn, job, &payload, EnqueueOptions::dedupe(&key))
            .await?
            .is_some()
        {
            return Ok(());
        }
    }
    let why = std::io::Error::other(format!("could not queue {job} again for {id}"));
    Err(ownhealth_core::Error::Other(why.into()))
}

/// Hold [`lock_exports`] for document `document_id` when it is a CGM export (`flags.cgm_csv`); nothing
/// for any other document. A running CGM import holds that lock and then writes its document row, so
/// whatever retracts or moves a document takes this before any lock on the document's row: a move that
/// held the row first would wait for the import while the import waits for the row (40P01). A move that
/// committed while this waited gives the export another owner, whose import the move queued: this then
/// takes that owner's lock too, and reads the owner again, until the owner it read is one whose lock it
/// holds (as the CGM import does).
pub async fn lock_export_of(conn: &mut PgConnection, document_id: Uuid) -> Result<()> {
    let mut locked: Vec<Uuid> = Vec::new();
    loop {
        let cgm: Option<Option<Uuid>> =
            sqlx::query_scalar("select profile_id from document where id = $1 and flags ? 'cgm_csv'")
                .bind(document_id)
                .fetch_optional(&mut *conn)
                .await?;
        match cgm.flatten() {
            Some(owner) if !locked.contains(&owner) => {
                lock_exports(&mut *conn, owner, "cgm_csv").await?;
                locked.push(owner);
            }
            _ => return Ok(()),
        }
    }
}

/// Hold the lock on one profile's exports of one kind (`flag`) until this transaction ends. A CGM
/// import takes it first, so two imports of one profile never interleave: one that replaces its own
/// readings and queues the others again never races another's run. A retraction or a move of a CGM
/// export takes it before anything else ([`lock_export_of`]), so it never misses the readings of an
/// import still running.
pub async fn lock_exports(conn: &mut PgConnection, profile_id: Uuid, flag: &str) -> Result<()> {
    sqlx::query("select pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(format!("ownhealth:exports:{flag}:{profile_id}"))
        .execute(conn)
        .await?;
    Ok(())
}
