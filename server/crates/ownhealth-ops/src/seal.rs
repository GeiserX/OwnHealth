//! `ownhealth seal-legacy`: seal what an install stored before profile keys (slice 46).
//!
//! An install older than 1.0 kept every file in the plain layout (`blobs/sha256/..`) and sealed its
//! connector refresh tokens under the secrets key alone (secret version 1). Reads take both layouts
//! and both versions, so nothing has to change at upgrade. This job, started by the operator and
//! never by the app, seals them:
//!
//! - every plain file that a row of a profile or of a pending item names is stored again under that
//!   owner's key, the bytes checked against their sha256 first, for every owner that names it. Then
//!   the plain file is removed, unless a reader with no owner may still open it (reference data, or an
//!   inbox row older than migration 1249 that names neither a profile nor a held mail). A plain file
//!   no row names stays too.
//! - every version 1 refresh token of a profile's connection is sealed again under that profile's key
//!   (version 2), as its next refresh would.
//!
//! Each file is one transaction, holding the rows that name it, so a move, a release or a delete runs
//! before or after it and never in between; and holding the removal side of the backup's copy lock, so
//! a running backup keeps the plain file its snapshot names. The state lives on disk and in the rows,
//! so the job resumes where a stopped run left off and a second run changes nothing. `--dry-run`
//! reports what a run would do and writes nothing, not even a key.

use std::time::{Duration, Instant};

use ownhealth_core::blobs::{self, BlobOwner, BlobStore, sha256_hex_to_bytes};
use ownhealth_core::keys::Slot;
use ownhealth_core::secrets::{self, SecretBox};
use ownhealth_core::{Error, Result};
use serde_json::{Value, json};
use sqlx::{AssertSqlSafe, Connection, PgConnection, PgPool};
use uuid::Uuid;

/// Every column that names a blob, by table: the rows this job reads the owners from. The same set as
/// `ownhealth_auth::household::BLOB_COLUMNS` (`tests/seal_legacy.rs` compares them), so a new blob
/// column cannot leave its plain files behind, or lose them.
pub const COLUMNS: &[(&str, &[&str])] = &[
    ("document", &["blob_sha256"]),
    ("document_page", &["words_blob", "image_blob"]),
    ("genome_dataset", GENOME),
    ("ocr_run", &["words_blob"]),
    ("source_item", &["blob_sha256"]),
    ("quarantine_item", &["raw_blob"]),
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

/// Who reads the file `sha` back: the owners of the rows that name it, and whether a row names it with
/// no owner (so a read of the plain file may still come).
#[derive(Debug, Default, PartialEq, Eq)]
struct Owners {
    owners: Vec<BlobOwner>,
    ownerless: bool,
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

/// The owners of the rows that name `sha`, read as each reader reads them: a document and its pages
/// and OCR layers by the document (`BlobOwner::of_document`), a genome file by its profile, a held mail
/// by its item, an inbox row as the backup reads it (`BlobOwner::of_item`; a row of a released or
/// deleted mail with no profile is read by nobody). With `lock`, the rows stay locked until the
/// caller's transaction ends.
async fn owners(conn: &mut PgConnection, sha: &[u8], lock: bool) -> Result<Owners> {
    let lock_rows = if lock { " for update" } else { "" };
    let mut out = Owners::default();
    let docs: Vec<(Uuid, Option<Uuid>)> = sqlx::query_as(AssertSqlSafe(format!(
        "select id, profile_id from document where blob_sha256 = $1 \
         or id in (select document_id from document_page where words_blob = $1 or image_blob = $1) \
         or id in (select document_id from ocr_run where words_blob = $1) order by id{lock_rows}"
    )))
    .bind(sha)
    .fetch_all(&mut *conn)
    .await?;
    for (id, profile) in docs {
        out.add(BlobOwner::of_document(profile, id));
    }
    let genome: Vec<Uuid> = sqlx::query_scalar(AssertSqlSafe(format!(
        "select profile_id from genome_dataset where $1 in ({}) order by id{lock_rows}",
        GENOME.join(", ")
    )))
    .bind(sha)
    .fetch_all(&mut *conn)
    .await?;
    for profile in genome {
        out.add(BlobOwner::Profile(profile));
    }
    let held: Vec<Uuid> = sqlx::query_scalar(AssertSqlSafe(format!(
        "select id from quarantine_item where raw_blob = $1 and status = 'held' order by id{lock_rows}"
    )))
    .bind(sha)
    .fetch_all(&mut *conn)
    .await?;
    for item in held {
        out.add(BlobOwner::Pending(item));
    }
    let lock_items = if lock { " for update of i" } else { "" };
    let items: Vec<(Option<Uuid>, Option<Uuid>, Option<String>)> = sqlx::query_as(AssertSqlSafe(format!(
        "select i.profile_id, i.quarantine_id, q.status from source_item i \
         left join quarantine_item q on q.id = i.quarantine_id where i.blob_sha256 = $1 \
         order by i.source_id, i.external_id{lock_items}"
    )))
    .bind(sha)
    .fetch_all(&mut *conn)
    .await?;
    for (profile, mail, status) in items {
        let read = profile.is_some() || mail.is_none() || status.as_deref() == Some("held");
        if read {
            out.add(BlobOwner::of_item(profile, mail));
        }
    }
    Ok(out)
}

fn task_failed(e: tokio::task::JoinError) -> Error {
    Error::Other(anyhow::anyhow!("blob task failed: {e}"))
}

/// What one file came to.
enum Outcome {
    /// Sealed for `sealed` owners (some may have had a copy already); the plain file went when `removed`.
    Done { sealed: usize, removed: bool },
    /// A row with no owner names it: it stays plain (its owners' copies were made).
    Ownerless { sealed: usize },
    /// No row names it.
    Unnamed,
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
}

impl Tally {
    fn error(&mut self, what: &str, e: &Error) {
        if self.errors.len() < ERRORS_KEPT {
            // errors here name no hash and no secret (`BlobError`, `secrets`)
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
/// owner remains, in one transaction that holds the rows naming it.
async fn seal_file(pool: &PgPool, blobs: &BlobStore, sha: &str) -> Result<Outcome> {
    let bytes = sha256_hex_to_bytes(sha)?;
    for _ in 0..TRIES {
        let mut conn = pool.acquire().await?;
        let before = owners(&mut conn, &bytes, false).await?;
        warm(blobs, &before.owners).await?;
        let mut tx = conn.begin().await?;
        // a backup that copies the files now may name the plain file: wait for its copy
        blobs::removal_lock(&mut tx, true).await?;
        let now = owners(&mut tx, &bytes, true).await?;
        if now.owners.iter().any(|o| !before.owners.contains(o)) {
            continue; // an owner came between the lookup and the lock (a move): its key first
        }
        if !now.named() {
            return Ok(Outcome::Unnamed);
        }
        let (store, sha_owned, owners_now) = (blobs.clone(), sha.to_string(), now.owners.clone());
        let removed_plain = !now.ownerless;
        let (sealed, removed) = tokio::task::spawn_blocking(move || -> Result<(usize, bool)> {
            let mut sealed = 0;
            for owner in owners_now {
                // read from the plain file, checked against its sha256 before anything lands
                if store.rekey(BlobOwner::Reference, owner, &sha_owned)? {
                    sealed += 1;
                }
            }
            let removed = removed_plain && store.drop_plain(&sha_owned)?;
            Ok((sealed, removed))
        })
        .await
        .map_err(task_failed)??;
        tx.commit().await?;
        return Ok(if now.ownerless {
            Outcome::Ownerless { sealed }
        } else {
            Outcome::Done { sealed, removed }
        });
    }
    Err(Error::Other(anyhow::anyhow!(
        "the file's owners kept changing while it was sealed; run the job again"
    )))
}

/// What sealing `sha` would do: the copies it would make, and whether the plain file would go.
async fn plan_file(pool: &PgPool, blobs: &BlobStore, sha: &str) -> Result<Outcome> {
    let mut conn = pool.acquire().await?;
    let now = owners(&mut conn, &sha256_hex_to_bytes(sha)?, false).await?;
    if !now.named() {
        return Ok(Outcome::Unnamed);
    }
    let (store, sha_owned, list) = (blobs.clone(), sha.to_string(), now.owners.clone());
    let sealed = tokio::task::spawn_blocking(move || -> Result<usize> {
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
    Ok(if now.ownerless {
        Outcome::Ownerless { sealed }
    } else {
        Outcome::Done { sealed, removed: true }
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

/// Seal every plain file a profile or a pending item reads and every version 1 refresh token of a
/// profile's connection, as the module says; with `dry_run`, only count. The report goes to stdout
/// as JSON; `ok` is false when a file or a token could not be sealed (it stays as it was, and a run
/// later tries it again).
pub async fn seal_legacy(pool: &PgPool, blobs: &BlobStore, sb: &SecretBox, dry_run: bool) -> Result<Value> {
    if !blobs.seals() || !sb.has_keys() {
        return Err(Error::Invalid(
            "the profile keys are not loaded: set OWNHEALTH_SECRETS_KEY to the key this install was made with".into(),
        ));
    }
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
        let outcome = if dry_run {
            plan_file(pool, blobs, sha).await
        } else {
            seal_file(pool, blobs, sha).await
        };
        match outcome {
            Ok(Outcome::Done { sealed, removed }) => {
                t.sealed += sealed;
                t.removed += usize::from(removed);
            }
            Ok(Outcome::Ownerless { sealed }) => {
                t.sealed += sealed;
                t.ownerless += 1;
            }
            Ok(Outcome::Unnamed) => t.unnamed += 1,
            // gone since the list was made (a delete, or another run): nothing left to seal
            Err(Error::NotFound(_)) => {}
            Err(e) => {
                t.failed += 1;
                t.error("a file", &e);
                tracing::warn!(error = %e, "a plain file could not be sealed; it stays as it was");
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
