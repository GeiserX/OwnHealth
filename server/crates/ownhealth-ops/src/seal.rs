//! `ownhealth seal-legacy`: seal what an install stored before profile keys (slice 46).
//!
//! An install older than 1.0 kept every file in the plain layout (`blobs/sha256/..`) and sealed its
//! connector refresh tokens under the secrets key alone (secret version 1). Reads take both layouts
//! and both versions, so nothing has to change at upgrade. This job, started by the operator and
//! never by the app, seals them:
//!
//! - an inbox row older than migration 1249 names no profile. The job gives it the profile its bytes
//!   are for, as a newer row would hold it: the profile of the row's document, else of the documents
//!   its released mail made (when they are all one profile's), else of its inbox when the inbox is a
//!   profile's own (Health Auto Export, a profile's mailbox). The backup then reads it as that
//!   profile's, as every other reader already did.
//! - every plain file that a row of a profile or of a pending item names is stored again under that
//!   owner's key, the bytes checked against their sha256 first, for every owner that names it. Then
//!   the plain file is removed, unless a reader with no owner may still open it: reference data, or
//!   an older inbox row the job cannot give a profile (a household inbox's file that no document or
//!   release ties to one). A plain file no row names stays too. The report lists both kinds.
//! - every version 1 refresh token of a profile's connection is sealed again under that profile's key
//!   (version 2), as its next refresh would.
//!
//! Each file is one transaction, holding the rows that name it, so a move, a release or a delete runs
//! before or after it and never in between; and holding the removal side of the backup's copy lock, so
//! a running backup keeps the plain file its snapshot names. The state lives on disk and in the rows,
//! so the job resumes where a stopped run left off and a second run changes nothing. `--dry-run`
//! reports what a run would do and writes nothing, not even a key.

use std::collections::HashMap;
use std::io;
use std::time::{Duration, Instant};

use ownhealth_core::blobs::{self, BlobOwner, BlobStore, sha256_hex_to_bytes};
use ownhealth_core::keys::Slot;
use ownhealth_core::secrets::{self, SecretBox};
use ownhealth_core::{Error, Result};
use serde_json::{Value, json};
use sqlx::{AssertSqlSafe, Connection, PgConnection, PgPool};
use uuid::Uuid;

/// Every column that names a blob, by table: the rows this job reads the owners from. The same set as
/// `ownhealth_auth::household::BLOB_COLUMNS` (`tests/seal_legacy.rs` compares them), and the job's
/// queries are built from it (a test here checks each table is read), so a new blob column cannot
/// leave its plain files behind, or lose them.
pub const COLUMNS: &[(&str, &[&str])] = &[
    ("document", &["blob_sha256"]),
    ("document_page", &["words_blob", "image_blob"]),
    ("genome_dataset", GENOME),
    ("ocr_run", &["words_blob"]),
    ("source_item", &["blob_sha256"]),
    ("quarantine_item", &["raw_blob"]),
    ("imaging_import", &["zip_blob"]),
];

const GENOME: &[&str] = &[
    "facts_blob",
    "vcf_blob",
    "tbi_blob",
    "coverage_blob",
    "gvcf_blob",
    "gvcf_tbi_blob",
    "sites_blob",
    "sites_tbi_blob",
];

/// How often the job says how far it got.
const PROGRESS_EVERY: Duration = Duration::from_secs(10);
/// How many times a file is tried again when its owners change between the key lookup and the lock.
const TRIES: usize = 3;
/// How many error lines the report keeps.
const ERRORS_KEPT: usize = 20;
/// How many kept files the report names, of each kind.
const KEPT_LISTED: usize = 100;

/// Who reads the file `sha` back: the owners of the rows that name it, whether a row names it with
/// no owner (so a read of the plain file may still come), and the older inbox rows the job gives a
/// profile: (source, external id, profile).
#[derive(Debug, Default, PartialEq, Eq)]
struct Owners {
    owners: Vec<BlobOwner>,
    ownerless: bool,
    claims: Vec<(Uuid, String, Uuid)>,
    /// An item's linked document changed after the document locks were taken.
    retry: bool,
}

impl Owners {
    fn add(&mut self, owner: BlobOwner) {
        match owner {
            BlobOwner::Reference => self.ownerless = true,
            o if !self.owners.contains(&o) => self.owners.push(o),
            _ => {}
        }
    }

    fn named(&self) -> bool {
        self.ownerless || !self.owners.is_empty()
    }
}

/// The blob columns of `table` in [`COLUMNS`].
fn columns(table: &str) -> &'static [&'static str] {
    COLUMNS.iter().find(|(t, _)| *t == table).map_or(&[], |(_, cols)| cols)
}

/// `$1 in (<every blob column of table>)`, each column after `prefix`.
fn names(table: &str, prefix: &str) -> String {
    let cols: Vec<String> = columns(table).iter().map(|c| format!("{prefix}{c}")).collect();
    if cols.is_empty() {
        "false".into()
    } else {
        format!("$1 in ({})", cols.join(", "))
    }
}

/// The five queries [`owners`] runs: documents (by their own file, their pages' and their OCR
/// layers'), genome datasets, imaging uploads, held mail and inbox rows.
fn owner_sql(lock: bool) -> [String; 5] {
    let lock_rows = if lock { " for update" } else { "" };
    let lock_docs = if lock { " for update of d" } else { "" };
    let lock_items = if lock { " for update of i" } else { "" };
    [
        format!(
            "with matched as ( \
               select id, true as names_blob from document where {} \
               or id in (select document_id from document_page where {}) \
               or id in (select document_id from ocr_run where {}) \
               union all select i.document_id, false from source_item i \
               where {} and i.raw_released_at is null and i.profile_id is null and i.document_id is not null \
             ), named as (select id, bool_or(names_blob) as names_blob from matched group by id) \
             select d.id, d.profile_id, n.names_blob from named n join document d using (id) \
             order by d.id{lock_docs}",
            names("document", ""),
            names("document_page", ""),
            names("ocr_run", ""),
            names("source_item", "i.")
        ),
        format!(
            "select profile_id from genome_dataset where {} order by id{lock_rows}",
            names("genome_dataset", "")
        ),
        format!(
            "select profile_id from imaging_import where {} order by id{lock_rows}",
            names("imaging_import", "")
        ),
        format!(
            "select id from quarantine_item where {} and status = 'held' order by id{lock_rows}",
            names("quarantine_item", "")
        ),
        // An older row uses its document's owner, then the one profile of its released mail's
        // documents, then its inbox's profile. The first query locks linked documents for claims.
        format!(
            "select i.source_id, i.external_id, i.profile_id, i.quarantine_id, q.status, i.document_id, \
             coalesce((select case when count(distinct r.profile_id) = 1 \
               then (array_agg(r.profile_id))[1] end from document r \
               where r.id = any(q.released_document_ids) and r.profile_id is not null), s.profile_id) \
             from source_item i left join quarantine_item q on q.id = i.quarantine_id \
             left join source s on s.id = i.source_id \
             where {} and i.raw_released_at is null order by i.source_id, i.external_id{lock_items}",
            names("source_item", "i.")
        ),
    ]
}

/// The owners of the rows that name `sha`, read as each reader reads them: a document and its pages
/// and OCR layers by the document (`BlobOwner::of_document`), a genome file and an imaging upload by
/// its profile, a held mail
/// by its item, an inbox row as the backup reads it (`BlobOwner::of_item`; a row of a released or
/// deleted mail with no profile is read by nobody). An inbox row older than migration 1249 is read as
/// the profile the module says, and becomes a claim. With `lock`, the rows stay locked until the
/// caller's transaction ends.
async fn owners(conn: &mut PgConnection, sha: &[u8], lock: bool) -> Result<Owners> {
    let [docs_sql, genome_sql, imaging_sql, held_sql, items_sql] = owner_sql(lock);
    let mut out = Owners::default();
    // Take all document locks in id order before any dataset lock, including documents linked
    // from inbox rows whose raw file differs from the document's own file.
    let docs: Vec<(Uuid, Option<Uuid>, bool)> = sqlx::query_as(AssertSqlSafe(docs_sql))
        .bind(sha)
        .fetch_all(&mut *conn)
        .await?;
    let mut document_profiles = HashMap::new();
    for (id, profile, names_blob) in docs {
        if names_blob {
            out.add(BlobOwner::of_document(profile, id));
        }
        document_profiles.insert(id, profile);
    }
    let genome: Vec<Uuid> = sqlx::query_scalar(AssertSqlSafe(genome_sql))
        .bind(sha)
        .fetch_all(&mut *conn)
        .await?;
    let imaging: Vec<Uuid> = sqlx::query_scalar(AssertSqlSafe(imaging_sql))
        .bind(sha)
        .fetch_all(&mut *conn)
        .await?;
    for profile in genome.into_iter().chain(imaging) {
        out.add(BlobOwner::Profile(profile));
    }
    let held: Vec<Uuid> = sqlx::query_scalar(AssertSqlSafe(held_sql))
        .bind(sha)
        .fetch_all(&mut *conn)
        .await?;
    for item in held {
        out.add(BlobOwner::Pending(item));
    }
    type Item = (
        Uuid,
        String,
        Option<Uuid>,
        Option<Uuid>,
        Option<String>,
        Option<Uuid>,
        Option<Uuid>,
    );
    let items: Vec<Item> = sqlx::query_as(AssertSqlSafe(items_sql))
        .bind(sha)
        .fetch_all(&mut *conn)
        .await?;
    for (source, external_id, profile, mail, status, document, fallback) in items {
        match (profile, mail, status.as_deref()) {
            (Some(p), _, _) => out.add(BlobOwner::Profile(p)),
            (None, Some(item), Some("held")) => out.add(BlobOwner::Pending(item)),
            // older than migration 1249: an arrival, or a mail released before it
            (None, None, _) | (None, Some(_), Some("released")) => {
                let linked = document.and_then(|id| document_profiles.get(&id));
                if lock && document.is_some() && linked.is_none() {
                    // The newly linked document was absent when we took the ordered locks.
                    out.retry = true;
                    continue;
                }
                match linked.copied().flatten().or(fallback) {
                    Some(p) => {
                        out.add(BlobOwner::Profile(p));
                        out.claims.push((source, external_id, p));
                    }
                    None if mail.is_none() => out.add(BlobOwner::Reference),
                    None => {} // a released mail that made nothing for a profile: read by nobody
                }
            }
            _ => {} // a deleted mail: read by nobody
        }
    }
    Ok(out)
}

fn task_failed(e: tokio::task::JoinError) -> Error {
    Error::Io(io::Error::other(e))
}

/// What one file came to.
enum Outcome {
    Done,
    /// A row with no owner reads it, so the plain copy stays.
    Ownerless,
    /// No row names it.
    Unnamed,
}

/// Work on disk persists even if another copy or a database write fails.
#[derive(Default)]
struct FileProgress {
    sealed: usize,
    removed: bool,
}

/// The counts of one run, and the first errors.
#[derive(Default)]
struct Tally {
    plain: usize,
    seen: usize,
    sealed: usize,
    removed: usize,
    ownerless: usize,
    unnamed: usize,
    failed: usize,
    tokens: usize,
    tokens_sealed: usize,
    tokens_failed: usize,
    errors: Vec<String>,
    /// the first plain files kept, of each kind
    kept_ownerless: Vec<String>,
    kept_unnamed: Vec<String>,
}

impl Tally {
    fn error(&mut self, what: &str, e: &Error) {
        if self.errors.len() < ERRORS_KEPT {
            // errors here name no secret (`BlobError`, `secrets`); a file by its sha256, for the operator
            self.errors.push(format!("{what}: {e}"));
        }
    }
}

/// Make the keys of `owners` (or find them) before any row is locked: a key lookup runs on a
/// connection of its own and waits on the key's row, which a profile delete holds while it waits for
/// the rows a file's transaction locks.
async fn warm(blobs: &BlobStore, owners: &[BlobOwner]) -> Result<()> {
    let Some(keys) = blobs.keyring() else {
        return Ok(());
    };
    let slots: Vec<Slot> = owners.iter().filter_map(|o| o.slot()).collect();
    tokio::task::spawn_blocking(move || {
        for slot in slots {
            keys.dek_for(slot, true)?;
        }
        Ok::<_, Error>(())
    })
    .await
    .map_err(task_failed)?
}

/// Seal the plain file `sha` for every owner that reads it, and remove it once no reader with no
/// owner remains, in one transaction that holds the rows naming it. An older inbox row gets its
/// profile in that transaction too; the plain file then goes in a transaction of its own, once the
/// row that names it says whose it is (a backup that reads the row in between finds the plain file).
async fn seal_file(pool: &PgPool, blobs: &BlobStore, sha: &str, progress: &mut FileProgress) -> Result<Outcome> {
    let bytes = sha256_hex_to_bytes(sha)?;
    for _ in 0..TRIES {
        let mut conn = pool.acquire().await?;
        let before = owners(&mut conn, &bytes, false).await?;
        warm(blobs, &before.owners).await?;
        let mut tx = conn.begin().await?;
        // a backup that copies the files now may name the plain file: wait for its copy
        if !blobs::removal_lock(&mut tx, false).await? {
            tracing::info!("waiting for a running backup to finish copying the files");
            blobs::removal_lock(&mut tx, true).await?;
        }
        let now = owners(&mut tx, &bytes, true).await?;
        if now.retry || now.owners.iter().any(|o| !before.owners.contains(o)) {
            continue; // an owner came between the lookup and the lock (a move): its key first
        }
        if !now.named() {
            return Ok(Outcome::Unnamed);
        }
        let claimed = !now.claims.is_empty();
        let (store, sha_owned, owners_now) = (blobs.clone(), sha.to_string(), now.owners.clone());
        let removed_plain = !now.ownerless && !claimed;
        let (sealed, removal) = tokio::task::spawn_blocking(move || {
            let mut sealed = 0;
            let removal = (|| -> Result<bool> {
                for owner in owners_now {
                    // Verify the plain file against its sha256 before placing each copy.
                    if store.rekey(BlobOwner::Reference, owner, &sha_owned)? {
                        sealed += 1;
                    }
                }
                Ok(removed_plain && store.drop_plain(&sha_owned)?)
            })();
            (sealed, removal)
        })
        .await
        .map_err(task_failed)?;
        progress.sealed += sealed;
        progress.removed = removal?;
        for (source, external_id, profile) in &now.claims {
            sqlx::query(
                "update source_item set profile_id = $3 \
                 where source_id = $1 and external_id = $2 and profile_id is null",
            )
            .bind(source)
            .bind(external_id)
            .bind(profile)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        if claimed {
            continue; // the rows say their profile now: the plain file goes next
        }
        return Ok(if now.ownerless {
            Outcome::Ownerless
        } else {
            Outcome::Done
        });
    }
    Err(io::Error::new(
        io::ErrorKind::WouldBlock,
        "the file's owners kept changing while it was sealed; run the job again",
    )
    .into())
}

/// What sealing `sha` would do: the copies it would make, and whether the plain file would go.
async fn plan_file(pool: &PgPool, blobs: &BlobStore, sha: &str, progress: &mut FileProgress) -> Result<Outcome> {
    let mut conn = pool.acquire().await?;
    let now = owners(&mut conn, &sha256_hex_to_bytes(sha)?, false).await?;
    if !now.named() {
        return Ok(Outcome::Unnamed);
    }
    let (store, sha_owned, list) = (blobs.clone(), sha.to_string(), now.owners.clone());
    progress.sealed = tokio::task::spawn_blocking(move || -> Result<usize> {
        let mut n = 0;
        for owner in list {
            if !store.is_sealed(owner, &sha_owned)? {
                n += 1;
            }
        }
        Ok(n)
    })
    .await
    .map_err(task_failed)??;
    progress.removed = !now.ownerless;
    Ok(if now.ownerless {
        Outcome::Ownerless
    } else {
        Outcome::Done
    })
}

/// The refresh tokens still sealed under the secrets key alone (version 1) on a connection of a
/// profile: (source, profile).
async fn legacy_tokens(conn: &mut PgConnection) -> Result<Vec<(Uuid, Uuid)>> {
    let purposes: Vec<String> = secrets::PROFILE_SEALED.iter().map(|p| (*p).to_string()).collect();
    Ok(sqlx::query_as(
        "select s.id, s.profile_id from source s join secret k on k.id = s.secret_id \
         where s.profile_id is not null and k.purpose = any($1) \
         and substring(k.ciphertext from 1 for 1) <> '\\x02'::bytea order by s.id",
    )
    .bind(&purposes)
    .fetch_all(conn)
    .await?)
}

/// Seal the refresh token of connection `source` under its profile's key, under the source row's
/// lock, as a refresh does. False when there was nothing to do any more (the connection went, or a
/// refresh sealed it first).
async fn seal_token(pool: &PgPool, sb: &SecretBox, source: Uuid, profile: Uuid) -> Result<bool> {
    // the key first, before the row lock (see `warm`)
    secrets::open_profile_key(sb, profile).await?;
    let mut tx = pool.begin().await?;
    let row: Option<(Uuid, Option<Uuid>, Option<Uuid>)> =
        sqlx::query_as("select household_id, profile_id, secret_id from source where id = $1 for update")
            .bind(source)
            .fetch_optional(&mut *tx)
            .await?;
    let Some((household, Some(p), Some(secret))) = row else {
        return Ok(false);
    };
    if p != profile || secrets::sealed_for_profile(&mut tx, secret, p).await? {
        return Ok(false);
    }
    let value = secrets::read_secret(&mut tx, sb, household, secret).await?;
    secrets::replace_profile_secret(&mut tx, sb, secret, p, &value).await?;
    tx.commit().await?;
    Ok(true)
}

/// Refuse a secrets key other than the one the install was made with, before anything is written,
/// in a dry run too: the keys the job makes would open only under the other key, and the plain copies
/// it removes would be gone. The start checks the profile keys stored, and an install upgraded from
/// before 1.0 may have none yet, so the job also opens a secret the install stored under the secrets
/// key alone (version 1), when it has one.
async fn check_secrets_key(conn: &mut PgConnection, sb: &SecretBox) -> Result<()> {
    // Read presence and ciphertext in one statement so a concurrent refresh cannot change what
    // the preflight authenticates after the legacy rows are counted.
    let stored: Vec<(Uuid, String, Option<Vec<u8>>)> = sqlx::query_as(
        "select id, purpose, case when key_fingerprint = $1 then ciphertext end \
         from secret where substring(ciphertext from 1 for 1) <> '\\x02'::bytea order by id",
    )
    .bind(&sb.fingerprint()[..])
    .fetch_all(conn)
    .await?;
    if stored.is_empty()
        || stored.iter().any(|(id, purpose, ct)| {
            ct.as_deref()
                .is_some_and(|ct| sb.open_legacy_secret(*id, purpose, ct).is_ok())
        })
    {
        return Ok(());
    }
    Err(Error::Invalid(format!(
        "OWNHEALTH_SECRETS_KEY opens none of the {} passwords and tokens this install stored. \
         Nothing was changed. Check the original key and the stored secrets, then run the job again.",
        stored.len()
    )))
}

/// The documents that name the file `sha`, for an error line the operator can act on.
async fn documents_of(pool: &PgPool, sha: &str) -> Vec<Uuid> {
    let Ok(bytes) = sha256_hex_to_bytes(sha) else {
        return Vec::new();
    };
    let Ok(mut conn) = pool.acquire().await else {
        return Vec::new();
    };
    sqlx::query_scalar("select id from document where blob_sha256 = $1 order by id limit 5")
        .bind(bytes)
        .fetch_all(&mut *conn)
        .await
        .unwrap_or_default()
}

/// Seal every plain file a profile or a pending item reads and every version 1 refresh token of a
/// profile's connection, as the module says; with `dry_run`, only count. The report goes to stdout
/// as JSON. A failure makes `ok` false. The report counts the copies that landed before a
/// failure, and a later run retries the remaining work.
pub async fn seal_legacy(pool: &PgPool, blobs: &BlobStore, sb: &SecretBox, dry_run: bool) -> Result<Value> {
    if !blobs.seals() || !sb.has_keys() {
        return Err(Error::Invalid(
            "the profile keys are not loaded: set OWNHEALTH_SECRETS_KEY to the key this install was made with".into(),
        ));
    }
    check_secrets_key(&mut *pool.acquire().await?, sb).await?;
    let store = blobs.clone();
    let plain = tokio::task::spawn_blocking(move || store.plain_files())
        .await
        .map_err(task_failed)??;
    let tokens = legacy_tokens(&mut *pool.acquire().await?).await?;
    let mut t = Tally {
        plain: plain.len(),
        tokens: tokens.len(),
        ..Tally::default()
    };
    tracing::info!(
        files = t.plain,
        tokens = t.tokens,
        dry_run,
        "sealing the files and tokens stored before profile keys"
    );
    let mut said = Instant::now();
    for sha in &plain {
        let mut progress = FileProgress::default();
        let outcome = if dry_run {
            plan_file(pool, blobs, sha, &mut progress).await
        } else {
            seal_file(pool, blobs, sha, &mut progress).await
        };
        t.sealed += progress.sealed;
        t.removed += usize::from(progress.removed);
        match outcome {
            Ok(Outcome::Done) => {}
            Ok(Outcome::Ownerless) => {
                t.ownerless += 1;
                if t.kept_ownerless.len() < KEPT_LISTED {
                    t.kept_ownerless.push(sha.clone());
                }
            }
            Ok(Outcome::Unnamed) => {
                t.unnamed += 1;
                if t.kept_unnamed.len() < KEPT_LISTED {
                    t.kept_unnamed.push(sha.clone());
                }
            }
            // gone since the list was made (a delete, or another run): nothing left to seal
            Err(Error::NotFound(_)) => {}
            Err(e) => {
                t.failed += 1;
                let docs = documents_of(pool, sha).await;
                let what = if docs.is_empty() {
                    format!("the file {sha}")
                } else {
                    let ids: Vec<String> = docs.iter().map(Uuid::to_string).collect();
                    format!("the file {sha} (document {})", ids.join(", "))
                };
                let what = if !dry_run && !progress.removed {
                    format!("{what} (plain file retained for retry)")
                } else {
                    what
                };
                t.error(&what, &e);
                tracing::warn!(%sha, sealed = progress.sealed, removed = progress.removed, error = %e, "{what}");
            }
        }
        t.seen += 1;
        if said.elapsed() >= PROGRESS_EVERY {
            said = Instant::now();
            tracing::info!(
                done = t.seen,
                of = t.plain,
                sealed = t.sealed,
                removed = t.removed,
                failed = t.failed,
                "sealing older files"
            );
        }
    }
    for (source, profile) in tokens {
        if dry_run {
            t.tokens_sealed += 1;
            continue;
        }
        match seal_token(pool, sb, source, profile).await {
            Ok(true) => t.tokens_sealed += 1,
            Ok(false) => {}
            Err(e) => {
                t.tokens_failed += 1;
                t.error(&format!("the refresh token of connection {source}"), &e);
                tracing::warn!(%source, error = %e, "a refresh token could not be sealed; it stays as it was");
            }
        }
    }
    let report = json!({
        "ok": t.failed == 0 && t.tokens_failed == 0,
        "dry_run": dry_run,
        "files": {
            "plain": t.plain,
            "sealed": t.sealed,
            "removed": t.removed,
            "kept": {"ownerless": t.ownerless, "unnamed": t.unnamed},
            // the first files of each kind: ownerless = reference data, or an inbox record from before
            // 1.0 that the job cannot give a profile; unnamed = no record names it (left for clean-up)
            "kept_files": {"ownerless": t.kept_ownerless, "unnamed": t.kept_unnamed},
            "failed": t.failed,
        },
        "tokens": {"version_1": t.tokens, "sealed": t.tokens_sealed, "failed": t.tokens_failed},
        "errors": t.errors,
    });
    tracing::info!(
        sealed = t.sealed,
        removed = t.removed,
        tokens = t.tokens_sealed,
        failed = t.failed + t.tokens_failed,
        dry_run,
        "sealing done"
    );
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inbox_claim_reads_the_linked_owner_under_the_document_lock() {
        let [locked, _, _, _, items] = owner_sql(true);
        let [plan, _, _, _, _] = owner_sql(false);
        assert!(
            locked.contains("select i.document_id, false from source_item i"),
            "{locked}"
        );
        assert!(locked.contains("order by d.id for update of d"), "{locked}");
        assert!(!plan.contains("for update"), "{plan}");
        assert!(items.contains("i.document_id"), "{items}");
        assert!(!items.contains("for update of d"), "{items}");
    }

    #[test]
    fn sealing_and_clinvar_lock_documents_before_datasets() {
        let source = include_str!("seal.rs")
            .split_once("async fn owners(")
            .expect("the sealing owner lookup exists")
            .1;
        let docs = source.find("AssertSqlSafe(docs_sql)").expect("sealing reads documents");
        let datasets = source
            .find("AssertSqlSafe(genome_sql)")
            .expect("sealing reads datasets");
        assert!(docs < datasets);
        let clinvar = include_str!("../../ownhealth-genome/src/clinvar.rs")
            .split_once("async fn diff_dataset(")
            .expect("the ClinVar dataset comparison exists")
            .1;
        let docs = clinvar
            .find("from document where id = $1 and status <> 'retracted' for share")
            .expect("ClinVar locks the document");
        let datasets = clinvar
            .find("from genome_dataset where id = $1 for update")
            .expect("ClinVar locks the dataset");
        assert!(docs < datasets);
    }

    #[test]
    fn sealing_uses_typed_task_and_retry_errors() {
        let source = include_str!("seal.rs")
            .split_once("#[cfg(test)]")
            .expect("the sealing tests follow the implementation")
            .0;
        assert!(!source.contains("anyhow"));
        assert!(source.contains("run the job again"));
    }

    #[test]
    fn key_preflight_reads_legacy_presence_and_ciphertext_in_one_statement() {
        let preflight = include_str!("seal.rs")
            .split_once("async fn check_secrets_key(")
            .expect("the sealing preflight exists")
            .1
            .split_once("async fn documents_of(")
            .expect("the document helper follows preflight")
            .0;
        assert_eq!(preflight.matches(".fetch_").count(), 1, "{preflight}");
        assert!(!preflight.contains("read_secret"), "{preflight}");
    }

    /// Every table of [`COLUMNS`] is one the job reads, with every one of its columns: a column added
    /// there joins the queries by itself, and a table added there needs a query here.
    #[test]
    fn the_queries_read_every_column() {
        for lock in [false, true] {
            let sql = owner_sql(lock).join("\n");
            for (table, cols) in COLUMNS {
                assert!(sql.contains(&format!("from {table} ")), "no query reads {table}");
                for col in *cols {
                    assert!(sql.contains(col), "no query reads {table}.{col}");
                }
            }
        }
    }
}
