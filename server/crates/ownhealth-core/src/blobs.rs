//! Content-addressed immutable blob store on disk. The plaintext sha256 of the bytes is a blob's
//! identity everywhere (document rows, provenance, the export manifest, state_hash); where the bytes
//! sit is this module's business.
//!
//! Two layouts, read side by side:
//!
//! - plain: `blobs/sha256/ab/cd/<sha256 hex>`, the layout of `ownhealth/blobs.py`, so a 0.1.1 data
//!   directory is read as it is. Reference data stays here.
//! - sealed (slice 46): a profile's own files, `blobs/enc/<profile id>/ab/cd/<address>`, where the
//!   address is an HMAC of the sha256 under the profile's key and the file is encrypted under it
//!   (`keys.rs`), and a pending item's files (a held mail, a document with no profile yet) the same
//!   way under the item's own key, `blobs/pending/<item id>/ab/cd/<address>`. A store with a keyring
//!   writes every profile and pending blob this way.
//!
//! A read for a profile or an item tries its sealed copy first, then the plain one, so an install that
//! upgrades keeps every older file. A store that seals hashes such a plain file before it serves it
//! for a profile or an item (once per process while the file stays the same), so a file planted at the
//! plain path never stands in for a sealed one. A sealed folder whose key is gone (a deleted profile, a
//! settled item) is removed by [`BlobStore::sweep_unkeyed`].
//!
//! A backup copies the files after its database snapshot, so a file a row of that snapshot names must
//! stay until the copy has it. The backup holds the copy lock ([`hold_for_copy`]) from before the
//! snapshot until its copy ends; every removal of a file that such a row may name takes the other side
//! of it ([`removal_lock`]), and leaves the file for later while a backup holds it. The same bytes stored twice for one owner are kept once; two profiles that
//! store the same bytes each get their own sealed copy. A blob is never modified after it lands.
//! Access control lives on the rows that reference a hash (document, genome_dataset); a hash on its
//! own grants nothing.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex, Weak};
use std::task::{Context, Poll};

use sha2::{Digest, Sha256};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncSeekExt, AsyncWriteExt, ReadBuf};
use uuid::Uuid;

use crate::keys::{Dek, HEADER, Kek, KeyError, Keyring, RECORD, SealedReader, Sealer, Slot};

const CHUNK: usize = 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum BlobError {
    /// The file is not stored. The error names no hash: a sealed file's name hides its sha256, and
    /// neither a person nor a log line should see it.
    #[error("a stored file is missing")]
    NotFound,
    #[error("not a sha256 hex digest")]
    BadHash,
    #[error("not a plain file name: {0:?}")]
    BadName(String),
    #[error(transparent)]
    Key(#[from] KeyError),
    #[error(transparent)]
    Io(#[from] io::Error),
}

/// What a caller hears when a stored file is gone. The REST layer answers it with its own code,
/// `blob_missing` (HTTP 404), so a page says it in the person's language.
pub const MISSING_FILE: &str = "A stored file is missing.";

impl From<BlobError> for crate::Error {
    fn from(e: BlobError) -> Self {
        match e {
            BlobError::NotFound => crate::Error::NotFound(MISSING_FILE.into()),
            BlobError::BadHash => crate::Error::Invalid("not a sha256 hex digest".into()),
            BlobError::BadName(n) => crate::Error::Invalid(format!("not a plain file name: {n:?}")),
            BlobError::Key(k) => k.into(),
            BlobError::Io(e) => crate::Error::Io(e),
        }
    }
}

/// Whose bytes a read or a write is for. Every call names it, so the store can pick the key and the
/// S3 backend the prefix without changing a caller. `Profile` and `Pending` bytes are sealed, each
/// under its own key; `Reference` bytes use the plain layout. Access control stays on the rows that
/// reference a hash: an owner here grants nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlobOwner {
    /// A profile's own data.
    Profile(Uuid),
    /// Bytes that arrived but have no profile yet, under the id of the item that holds them: a held
    /// mail's `quarantine_item.id`, a document's id while it has no profile.
    Pending(Uuid),
    /// Reference data nobody owns.
    Reference,
}

impl BlobOwner {
    /// The owner of a document's bytes: its profile, or the document itself while it has none.
    pub fn of_document(profile_id: Option<Uuid>, document_id: Uuid) -> BlobOwner {
        profile_id.map_or(BlobOwner::Pending(document_id), BlobOwner::Profile)
    }

    /// The owner of an inbox item's raw bytes (`source_item.blob_sha256`): the profile it was filed
    /// to, else the held mail it is, else nobody (rows from before slice 46, in the plain layout). The
    /// backup reads the owner of every referenced row this way (`ownhealth_ops` `referenced_blobs`).
    pub fn of_item(profile_id: Option<Uuid>, quarantine_id: Option<Uuid>) -> BlobOwner {
        match (profile_id, quarantine_id) {
            (Some(p), _) => BlobOwner::Profile(p),
            (None, Some(q)) => BlobOwner::Pending(q),
            (None, None) => BlobOwner::Reference,
        }
    }

    /// The key this owner's bytes are sealed under, if any.
    pub fn slot(self) -> Option<Slot> {
        match self {
            BlobOwner::Profile(p) => Some(Slot::Profile(p)),
            BlobOwner::Pending(i) => Some(Slot::Pending(i)),
            BlobOwner::Reference => None,
        }
    }
}

/// A blob as a file on local disk, for readers that need a path (SQLite, zip). The path is good
/// while this value lives, so keep it for as long as any reader opened from the path is in use. In
/// the plain layout it is the stored file itself, in a store that seals a hard link to it in the
/// store's tmp folder (`ownhealth seal-legacy` may remove the stored file while a reader still opens
/// the path; the link keeps the bytes). A sealed blob is decrypted into a file of the tmp folder,
/// readable by this user only. A file in the tmp folder is removed when this drops (`sweep_tmp`
/// removes one a killed process left), and holds a lock on its file while it lives, so the sweep of
/// any process leaves it alone however long a reader takes.
#[derive(Debug)]
#[must_use = "the path is good only while the LocalCopy lives"]
pub struct LocalCopy(Arc<CopyFile>);

#[derive(Debug)]
struct CopyFile {
    path: PathBuf,
    temporary: bool,
    /// The copy's own handle, locked while the copy lives.
    held: Option<File>,
}

impl LocalCopy {
    pub fn path(&self) -> &Path {
        &self.0.path
    }
}

impl Drop for CopyFile {
    fn drop(&mut self) {
        if self.temporary {
            let _ = fs::remove_file(&self.path);
        }
    }
}

/// A sha256 given as hex (any case) or as 32 raw bytes, normalised to lowercase hex.
pub fn sha256_hex(value: &str) -> Result<String, BlobError> {
    let v = value.to_ascii_lowercase();
    if v.len() != 64 || !v.bytes().all(|c| c.is_ascii_hexdigit()) {
        return Err(BlobError::BadHash);
    }
    Ok(v)
}

/// The 32 bytes of a hex digest (the `bytea` columns hold these).
pub fn sha256_hex_to_bytes(h: &str) -> Result<Vec<u8>, BlobError> {
    hex::decode(sha256_hex(h)?).map_err(|_| BlobError::BadHash)
}

fn sha_array(h: &str) -> Result<[u8; 32], BlobError> {
    let mut out = [0u8; 32];
    hex::decode_to_slice(sha256_hex(h)?, &mut out).map_err(|_| BlobError::BadHash)?;
    Ok(out)
}

/// The bytes of a blob, from either layout, with seeks.
#[derive(Debug)]
pub enum BlobReader {
    Plain(File),
    Sealed(Box<SealedReader<File>>),
}

impl BlobReader {
    /// The plaintext size.
    pub fn len(&self) -> io::Result<u64> {
        match self {
            BlobReader::Plain(f) => Ok(f.metadata()?.len()),
            BlobReader::Sealed(r) => Ok(r.len()),
        }
    }

    pub fn is_empty(&self) -> io::Result<bool> {
        Ok(self.len()? == 0)
    }
}

impl Read for BlobReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            BlobReader::Plain(f) => f.read(buf),
            BlobReader::Sealed(r) => r.read(buf),
        }
    }
}

impl Seek for BlobReader {
    fn seek(&mut self, to: SeekFrom) -> io::Result<u64> {
        match self {
            BlobReader::Plain(f) => f.seek(to),
            BlobReader::Sealed(r) => r.seek(to),
        }
    }
}

/// A sealed blob streamed to an async caller: a blocking task decrypts, this end hands the bytes on.
/// A record that fails its check ends the stream with that error, never with short bytes.
struct SealedStream {
    rx: tokio::sync::mpsc::Receiver<io::Result<Vec<u8>>>,
    buf: Vec<u8>,
    at: usize,
}

impl AsyncRead for SealedStream {
    fn poll_read(mut self: Pin<&mut Self>, cx: &mut Context<'_>, out: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        loop {
            if self.at < self.buf.len() {
                let n = out.remaining().min(self.buf.len() - self.at);
                let at = self.at;
                out.put_slice(&self.buf[at..at + n]);
                self.at += n;
                return Poll::Ready(Ok(()));
            }
            match self.rx.poll_recv(cx) {
                Poll::Ready(Some(Ok(next))) => {
                    self.buf = next;
                    self.at = 0;
                }
                Poll::Ready(Some(Err(e))) => return Poll::Ready(Err(e)),
                Poll::Ready(None) => return Poll::Ready(Ok(())),
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

/// The plaintext of one sealed file, fed in any pieces: hashes it and seals it record by record. A
/// full record is held back until more bytes come, so the last record is always the one sealed last.
struct Sealing {
    sealer: Sealer,
    digest: Sha256,
    size: u64,
    pending: Vec<u8>,
}

/// What [`Sealing::finish`] gives: the last record, the header, the plaintext's sha256 and size.
type Tail = (Vec<u8>, [u8; HEADER], [u8; 32], u64);

impl Sealing {
    fn new(dek: &Dek, profile: Uuid) -> Sealing {
        Sealing {
            sealer: Sealer::new(dek, profile),
            digest: Sha256::new(),
            size: 0,
            pending: Vec::with_capacity(2 * RECORD),
        }
    }

    /// Records ready to write after `data`.
    fn feed(&mut self, data: &[u8]) -> io::Result<Vec<u8>> {
        self.digest.update(data);
        self.size += data.len() as u64;
        self.pending.extend_from_slice(data);
        let mut out = Vec::new();
        let mut used = 0;
        while self.pending.len() - used > RECORD {
            out.extend(self.sealer.seal(&self.pending[used..used + RECORD], false)?);
            used += RECORD;
        }
        self.pending.drain(..used);
        Ok(out)
    }

    /// The last record, the header to write at offset 0, and the sha256 and size of the plaintext.
    fn finish(mut self) -> io::Result<Tail> {
        let last = self.sealer.seal(&self.pending, true)?;
        let sha: [u8; 32] = self.digest.finalize().into();
        Ok((last, self.sealer.header(&sha), sha, self.size))
    }
}

/// Where blobs live is this module's business: no caller gets a raw path or the store's root, so the
/// S3 backend can change the layout without touching callers. `tests/blob_paths.rs` holds the rest of
/// the code to that.
#[derive(Debug, Clone)]
pub struct BlobStore {
    root: PathBuf,
    keys: Option<Arc<Keyring>>,
    /// Plain files this process read for a profile and found to hash to their name, by sha256 and by
    /// what changes when the file does ([`stamp`]).
    checked: Arc<Mutex<HashMap<String, Stamp>>>,
    /// Reuse a live link across readers of the same inode. The last reader still removes it.
    plain_links: Arc<Mutex<HashMap<PathBuf, Weak<CopyFile>>>>,
}

/// What changes when a file is rewritten or replaced: device, inode, length, and the modification
/// and change times in nanoseconds (a change time cannot be set back by hand).
type Stamp = (u64, u64, u64, i128, i128);

#[cfg(unix)]
fn stamp(meta: &fs::Metadata) -> Option<Stamp> {
    use std::os::unix::fs::MetadataExt;
    let ns = |s: i64, n: i64| i128::from(s) * 1_000_000_000 + i128::from(n);
    Some((
        meta.dev(),
        meta.ino(),
        meta.len(),
        ns(meta.mtime(), meta.mtime_nsec()),
        ns(meta.ctime(), meta.ctime_nsec()),
    ))
}

/// Without a change time, every read hashes the file again.
#[cfg(not(unix))]
fn stamp(_meta: &fs::Metadata) -> Option<Stamp> {
    None
}

/// Test support: every plain file [`BlobStore::checked_plain`] hashed in this process, with the name of
/// the thread that hashed it.
#[cfg(any(test, feature = "testing"))]
static PLAIN_HASHED: Mutex<Vec<(String, String)>> = Mutex::new(Vec::new());

/// Test support: the names of the threads that hashed the plain file `sha256` before serving it for an
/// owner, so a test shows the hash never ran on an async runtime's own thread.
#[cfg(any(test, feature = "testing"))]
pub fn plain_hashed_on(sha256: &str) -> Vec<String> {
    let h = sha256.to_ascii_lowercase();
    PLAIN_HASHED
        .lock()
        .map(|seen| seen.iter().filter(|(s, _)| *s == h).map(|(_, t)| t.clone()).collect())
        .unwrap_or_default()
}

/// Whether a live [`LocalCopy`] (of this process or another) holds the file at `path`: it keeps a
/// lock on it while it lives (shared on a link to a plain file, which several readers may hold), and
/// a killed process's lock went with it.
fn in_use(path: &Path) -> bool {
    File::open(path).is_ok_and(|f| matches!(f.try_lock(), Err(fs::TryLockError::WouldBlock)))
}

fn tmp_name() -> String {
    let mut b = [0u8; 16];
    rand::fill(&mut b);
    hex::encode(b)
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
}

#[cfg(not(unix))]
fn set_mode(_path: &Path, _mode: u32) -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
thread_local! {
    // Tests on this thread see every hard link fail, as on a filesystem without them.
    static NO_LINKS: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    // ... and, with this too, the stored file is removed first, as a racing `seal-legacy` would
    // after the reader opened it and before the bytes are hashed and copied.
    static SEALED_MEANWHILE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

fn link_file(path: &Path, link: &Path) -> io::Result<()> {
    #[cfg(test)]
    if NO_LINKS.with(std::cell::Cell::get) {
        if SEALED_MEANWHILE.with(std::cell::Cell::get) {
            fs::remove_file(path)?;
        }
        return Err(io::Error::new(io::ErrorKind::Unsupported, "no hard links in this test"));
    }
    fs::hard_link(path, link)
}

/// A new file only this user may read, for spools and decrypted copies.
fn create_private(path: &Path) -> io::Result<File> {
    let mut o = fs::OpenOptions::new();
    o.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.mode(0o600);
    }
    o.open(path)
}

/// A new spool file at a [`BlobStore::staging_path`], readable by this user only: an upload sits
/// there as plaintext until the store takes it.
pub async fn create_spool(path: &Path) -> io::Result<tokio::fs::File> {
    let mut o = tokio::fs::OpenOptions::new();
    o.write(true).create_new(true);
    #[cfg(unix)]
    o.mode(0o600);
    o.open(path).await
}

/// Where a read finds a blob.
enum Found {
    Plain(PathBuf),
    Sealed {
        path: PathBuf,
        dek: Arc<Dek>,
        slot: Slot,
        sha: [u8; 32],
    },
}

/// The folder of a slot's sealed files, under a store or a backup's blob tree.
fn slot_rel(slot: Slot) -> PathBuf {
    match slot {
        Slot::Profile(p) => Path::new("enc").join(p.simple().to_string()),
        Slot::Pending(i) => Path::new("pending").join(i.simple().to_string()),
    }
}

/// The key in a blob tree of the file `sha256` sealed under `dek` in `slot`: what a backup lists
/// for it when the key comes from the backup's own snapshot instead of the live store.
pub fn sealed_key(slot: Slot, dek: &Dek, sha256: &str) -> Result<String, BlobError> {
    Ok(slot_key(slot, &dek.address(&sha_array(sha256)?)))
}

/// The [`tree_files`] key of a slot's sealed file at `address`.
fn slot_key(slot: Slot, address: &str) -> String {
    match slot {
        Slot::Profile(p) => format!("{}/{address}", p.simple()),
        Slot::Pending(i) => format!("pending/{}/{address}", i.simple()),
    }
}

fn lock_error(e: sqlx::Error) -> BlobError {
    BlobError::Io(io::Error::other(e))
}

/// Hold the copy lock on `conn`'s own session, for a backup: it waits for the removals in progress,
/// then keeps every new one out until [`end_copy`] or until the session ends. Take it before the
/// database snapshot and end it once the blob files are copied.
pub async fn hold_for_copy(conn: &mut sqlx::PgConnection) -> Result<(), BlobError> {
    sqlx::query("select pg_catalog.pg_advisory_lock(pg_catalog.hashtext('ownhealth.blob_copy'))")
        .execute(conn)
        .await
        .map_err(lock_error)?;
    Ok(())
}

/// Let removals run again after [`hold_for_copy`].
pub async fn end_copy(conn: &mut sqlx::PgConnection) -> Result<(), BlobError> {
    sqlx::query("select pg_catalog.pg_advisory_unlock(pg_catalog.hashtext('ownhealth.blob_copy'))")
        .execute(conn)
        .await
        .map_err(lock_error)?;
    Ok(())
}

/// Take the removal side of the copy lock in `conn`'s open transaction, before removing blob files
/// that a row of a backup's snapshot may still name; it goes when the transaction ends. Removals do
/// not wait on each other. With `wait`, wait while a backup copies the files (a job); without, answer
/// false at once while one does, and leave the files for later: the sweep removes a folder whose key
/// is gone, a retry the rest.
pub async fn removal_lock(conn: &mut sqlx::PgConnection, wait: bool) -> Result<bool, BlobError> {
    if wait {
        sqlx::query("select pg_catalog.pg_advisory_xact_lock_shared(pg_catalog.hashtext('ownhealth.blob_copy'))")
            .execute(conn)
            .await
            .map_err(lock_error)?;
        return Ok(true);
    }
    sqlx::query_scalar("select pg_catalog.pg_try_advisory_xact_lock_shared(pg_catalog.hashtext('ownhealth.blob_copy'))")
        .fetch_one(conn)
        .await
        .map_err(lock_error)
}

/// Every column that names a blob, by table, with the profile a row's file belongs to (as text, null
/// when no one profile owns the row) and which of the table's rows still read the file. The same
/// columns as `ownhealth_ops::seal::COLUMNS` and `ownhealth_auth::household::BLOB_COLUMNS` (ownhealth-ops
/// `tests/seal_legacy.rs` compares them, and the auth crate compares its list with the schema), so the
/// removal check of [`remove_unheld_upload`], built from this list, counts every row a backup counts.
pub const HOLDERS: &[(&str, &[&str], &str, &str)] = &[
    ("document", &["blob_sha256"], "profile_id::text", "true"),
    (
        "document_page",
        &["words_blob", "image_blob"],
        "(select d.profile_id::text from document d where d.id = document_id)",
        "true",
    ),
    (
        "genome_dataset",
        &[
            "facts_blob",
            "vcf_blob",
            "tbi_blob",
            "coverage_blob",
            "gvcf_blob",
            "gvcf_tbi_blob",
            "sites_blob",
            "sites_tbi_blob",
        ],
        "profile_id::text",
        "true",
    ),
    ("imaging_import", &["zip_blob"], "profile_id::text", "true"),
    (
        "ocr_run",
        &["words_blob"],
        "(select d.profile_id::text from document d where d.id = document_id)",
        "true",
    ),
    // an item whose importer released its raw bytes no longer reads them
    (
        "source_item",
        &["blob_sha256"],
        "profile_id::text",
        "raw_released_at is null",
    ),
    ("quarantine_item", &["raw_blob"], "null::text", "true"),
];

/// The removal check of [`remove_unheld_upload`]: whether a row of profile `$3` names the bytes `$1`
/// (hex `$2`), and whether any row or live job does.
fn holders_query() -> &'static str {
    static SQL: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    SQL.get_or_init(|| {
        let mut rows: Vec<String> = HOLDERS
            .iter()
            .map(|(table, cols, owner, live)| {
                let names: Vec<String> = cols.iter().map(|c| format!("{c} = $1")).collect();
                format!(
                    "select {owner} as owner from {table} where ({}) and ({live})",
                    names.join(" or ")
                )
            })
            .collect();
        rows.push(
            "select payload->>'profile_id' from job where state in ('queued', 'running') \
             and payload->>'blob_sha256' = $2 and payload->'upload_released' is null"
                .into(),
        );
        format!(
            "with holders as ({}) select coalesce(bool_or(owner = $3), false), count(*) > 0 from holders",
            rows.join(" union all ")
        )
    })
}

/// Drop an upload's bytes after the importer or inbox has released its reference. The caller
/// supplies a connection in an open transaction. Other consumers and backup snapshots can still
/// hold the file, so check them before removing either copy: a row of any blob column in [`HOLDERS`]
/// or a live job. A row of `profile` keeps both copies; a row of another profile, or of none, keeps the
/// plain copy only.
pub async fn remove_unheld_upload(
    conn: &mut sqlx::PgConnection,
    blobs: &BlobStore,
    profile: Uuid,
    sha: &str,
) -> crate::Result<()> {
    removal_lock(conn, true).await?;
    let hash = sha256_hex_to_bytes(sha)?;
    let (mine, held): (bool, bool) = sqlx::query_as(sqlx::AssertSqlSafe(holders_query()))
        .bind(hash)
        .bind(sha)
        .bind(profile.to_string())
        .fetch_one(&mut *conn)
        .await?;
    let owner = BlobOwner::Profile(profile);
    if !held {
        blobs.delete(owner, sha)?;
    } else if !mine {
        blobs.drop_sealed(owner, sha)?;
    }
    Ok(())
}

// Run after the sealed lookup and before the plain lookup, in this test thread only.
#[cfg(test)]
thread_local! {
    static BETWEEN_LOOKUPS: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
        const { std::cell::RefCell::new(None) };
}

impl BlobStore {
    /// Opens (and creates) a store at `root` that writes the plain layout for every owner: tests and
    /// tools. The app builds its store with [`BlobStore::with_keys`].
    pub fn new(root: impl Into<PathBuf>) -> io::Result<BlobStore> {
        let root = root.into();
        fs::create_dir_all(root.join("tmp"))?;
        fs::create_dir_all(root.join("sha256"))?;
        Ok(BlobStore {
            root,
            keys: None,
            checked: Arc::default(),
            plain_links: Arc::default(),
        })
    }

    /// A store that seals every profile's blobs under that profile's key, and reads both layouts.
    pub fn with_keys(root: impl Into<PathBuf>, keys: Arc<Keyring>) -> io::Result<BlobStore> {
        let mut store = BlobStore::new(root)?;
        store.keys = Some(keys);
        Ok(store)
    }

    /// The store the app runs on: sealed under `OWNHEALTH_SECRETS_KEY` when it is set, plain when it
    /// is not (the binary refuses to start without it, see `Settings::require_secrets_key`).
    pub fn for_settings(settings: &crate::config::Settings) -> Result<BlobStore, BlobError> {
        Ok(match settings.secrets_key {
            Some(_) => BlobStore::with_keys(
                settings.blobs_path(),
                Arc::new(
                    Keyring::new(Kek::from_settings(settings)?, settings.sqlx_url())
                        .with_keyring_file(crate::keyfile::path(settings)),
                ),
            )?,
            None => BlobStore::new(settings.blobs_path())?,
        })
    }

    /// Whether profile blobs are written sealed.
    pub fn seals(&self) -> bool {
        self.keys.is_some()
    }

    /// The key-encryption key this store seals under, when it seals: what a start checks the stored
    /// profile keys against (`keys::check_start`).
    pub fn kek(&self) -> Option<&Kek> {
        self.keys.as_deref().map(Keyring::kek)
    }

    fn path(&self, sha256: &str) -> Result<PathBuf, BlobError> {
        let h = sha256_hex(sha256)?;
        Ok(self.root.join("sha256").join(&h[0..2]).join(&h[2..4]).join(&h))
    }

    fn slot_dir(&self, slot: Slot) -> PathBuf {
        self.root.join(slot_rel(slot))
    }

    fn sealed_path(&self, slot: Slot, address: &str) -> PathBuf {
        self.slot_dir(slot)
            .join(&address[0..2])
            .join(&address[2..4])
            .join(address)
    }

    /// The keyring this store seals with, for the secrets a profile's key seals (`secrets.rs`).
    pub fn keyring(&self) -> Option<Arc<Keyring>> {
        self.keys.clone()
    }

    /// The key a write for `owner` seals with: a profile's or a pending item's, when this store seals.
    fn sealing(&self, owner: BlobOwner) -> Result<Option<(Slot, Arc<Dek>)>, BlobError> {
        match (owner.slot(), &self.keys) {
            (Some(slot), Some(keys)) => {
                // never fall back to a plain write for a profile or an item when its key could not be had
                let dek = keys
                    .dek_for(slot, true)?
                    .ok_or_else(|| KeyError("no key to store the file under".into()))?;
                Ok(Some((slot, dek)))
            }
            _ => Ok(None),
        }
    }

    /// `file`, the plain copy of `sha256` read for `owner`, once its bytes hash to `sha256`, for a
    /// profile or a pending item in a store that seals: there every new file of theirs is sealed, so a
    /// plain one is a legacy file or a planted one. Checked once per process while the file stays the
    /// same.
    fn checked_plain(&self, owner: BlobOwner, sha256: &str, mut file: File) -> Result<File, BlobError> {
        if !(owner.slot().is_some() && self.keys.is_some()) {
            return Ok(file);
        }
        let h = sha256_hex(sha256)?;
        let before = stamp(&file.metadata()?);
        let known = |st: Stamp| self.checked.lock().is_ok_and(|c| c.get(&h) == Some(&st));
        if before.is_some_and(known) {
            return Ok(file);
        }
        #[cfg(any(test, feature = "testing"))]
        if let Ok(mut seen) = PLAIN_HASHED.lock() {
            seen.push((h.clone(), std::thread::current().name().unwrap_or_default().to_string()));
        }
        if sha256_reader(&mut file)? != h {
            return Err(BlobError::Io(io::Error::new(
                io::ErrorKind::InvalidData,
                "a stored file does not match its sha256: it was changed or replaced",
            )));
        }
        file.seek(SeekFrom::Start(0))?;
        if let Some(st) = before
            && stamp(&file.metadata()?) == before
            && let Ok(mut c) = self.checked.lock()
        {
            c.insert(h, st);
        }
        Ok(file)
    }

    /// Where `owner`'s blob is: its sealed copy first, then the plain one. An owner with no sealed
    /// folder has never had a sealed blob, so no key is looked up for it.
    fn find(&self, owner: BlobOwner, sha256: &str) -> Result<Option<Found>, BlobError> {
        let plain = self.path(sha256)?;
        if let (Some(slot), Some(keys)) = (owner.slot(), &self.keys)
            && self.slot_dir(slot).is_dir()
            && let Some(dek) = keys.dek_for(slot, false)?
        {
            let sha = sha_array(sha256)?;
            let path = self.sealed_path(slot, &dek.address(&sha));
            if path.is_file() {
                return Ok(Some(Found::Sealed { path, dek, slot, sha }));
            }
        }
        #[cfg(test)]
        if let Some(publish) = BETWEEN_LOOKUPS.with(|hook| hook.borrow_mut().take()) {
            publish();
        }
        Ok(plain.is_file().then_some(Found::Plain(plain)))
    }

    /// Ok while the store can serve; otherwise the directory that is missing, for the operator.
    pub fn health(&self) -> Result<(), String> {
        if self.root.is_dir() {
            Ok(())
        } else {
            Err(self.root.display().to_string())
        }
    }

    /// A path in the store's own tmp directory for a spool or a download, under `name` (one file
    /// name, no directory, refused otherwise). `sweep_tmp` removes leftovers named `upload-*` or
    /// `push-*`; a name reused on every run (`clinvar-<source>.vcf.gz`) is overwritten instead.
    pub fn staging_path(&self, name: &str) -> Result<PathBuf, BlobError> {
        if Path::new(name).file_name() != Some(std::ffi::OsStr::new(name)) {
            return Err(BlobError::BadName(name.to_string()));
        }
        Ok(self.root.join("tmp").join(name))
    }

    fn land_at(tmp: &Path, target: &Path) -> Result<(), BlobError> {
        if target.exists() {
            fs::remove_file(tmp)?;
        } else {
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::rename(tmp, target)?;
            set_mode(target, 0o440)?;
        }
        Ok(())
    }

    fn land(&self, tmp: &Path, h: &str) -> Result<(), BlobError> {
        BlobStore::land_at(tmp, &self.path(h)?)
    }

    fn land_sealed(&self, tmp: &Path, slot: Slot, dek: &Dek, sha: &[u8; 32]) -> Result<(), BlobError> {
        BlobStore::land_at(tmp, &self.sealed_path(slot, &dek.address(sha)))
    }

    /// Stream into the store for `owner`. Returns (sha256 hex of the plaintext, size). Same bytes
    /// twice for one owner: one file.
    pub fn put_reader(&self, owner: BlobOwner, reader: impl Read) -> Result<(String, u64), BlobError> {
        self.put_expecting(owner, reader, None)
    }

    /// `put_reader`, and with `expect` the bytes must hash to it: otherwise only the staging file goes
    /// and nothing lands, so a copy whose source holds other bytes never replaces or removes a file.
    fn put_expecting(
        &self,
        owner: BlobOwner,
        mut reader: impl Read,
        expect: Option<&str>,
    ) -> Result<(String, u64), BlobError> {
        let mismatch = |h: &str| {
            expect.is_some_and(|e| e != h).then(|| {
                BlobError::Io(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "a blob read back with other bytes than its sha256",
                ))
            })
        };
        let sealing = self.sealing(owner)?;
        let tmp = self.root.join("tmp").join(tmp_name());
        let result = (|| {
            let mut out = create_private(&tmp)?;
            let mut buf = vec![0u8; CHUNK];
            let (h, size) = if let Some((slot, dek)) = &sealing {
                let mut s = Sealing::new(dek, slot.id());
                out.write_all(&[0u8; HEADER])?;
                loop {
                    let n = reader.read(&mut buf)?;
                    if n == 0 {
                        break;
                    }
                    out.write_all(&s.feed(&buf[..n])?)?;
                }
                let (last, header, sha, size) = s.finish()?;
                if let Some(e) = mismatch(&hex::encode(sha)) {
                    return Err(e);
                }
                out.write_all(&last)?;
                out.seek(SeekFrom::Start(0))?;
                out.write_all(&header)?;
                out.flush()?;
                out.sync_all()?;
                self.land_sealed(&tmp, *slot, dek, &sha)?;
                (hex::encode(sha), size)
            } else {
                let mut digest = Sha256::new();
                let mut size = 0u64;
                loop {
                    let n = reader.read(&mut buf)?;
                    if n == 0 {
                        break;
                    }
                    digest.update(&buf[..n]);
                    out.write_all(&buf[..n])?;
                    size += n as u64;
                }
                out.flush()?;
                out.sync_all()?;
                let h = hex::encode(digest.finalize());
                if let Some(e) = mismatch(&h) {
                    return Err(e);
                }
                self.land(&tmp, &h)?;
                (h, size)
            };
            Ok((h, size))
        })();
        if result.is_err() {
            let _ = fs::remove_file(&tmp);
        }
        result
    }

    /// The async twin of `put_reader`, for request bodies.
    pub async fn put_async(
        &self,
        owner: BlobOwner,
        mut reader: impl AsyncRead + Unpin,
    ) -> Result<(String, u64), BlobError> {
        let sealing = {
            let (store, owner) = (self.clone(), owner);
            // a key lookup may reach the database on a thread of its own
            tokio::task::spawn_blocking(move || store.sealing(owner))
                .await
                .map_err(|e| BlobError::Io(io::Error::other(e)))??
        };
        let tmp = self.root.join("tmp").join(tmp_name());
        let result = async {
            let mut out = create_spool(&tmp).await?;
            let mut buf = vec![0u8; CHUNK];
            if let Some((slot, dek)) = &sealing {
                let mut s = Sealing::new(dek, slot.id());
                out.write_all(&[0u8; HEADER]).await?;
                loop {
                    let n = reader.read(&mut buf).await?;
                    if n == 0 {
                        break;
                    }
                    out.write_all(&s.feed(&buf[..n])?).await?;
                }
                let (last, header, sha, size) = s.finish()?;
                out.write_all(&last).await?;
                out.seek(SeekFrom::Start(0)).await?;
                out.write_all(&header).await?;
                out.flush().await?;
                out.sync_all().await?;
                self.land_sealed(&tmp, *slot, dek, &sha)?;
                return Ok((hex::encode(sha), size));
            }
            let mut digest = Sha256::new();
            let mut size = 0u64;
            loop {
                let n = reader.read(&mut buf).await?;
                if n == 0 {
                    break;
                }
                digest.update(&buf[..n]);
                out.write_all(&buf[..n]).await?;
                size += n as u64;
            }
            out.flush().await?;
            out.sync_all().await?;
            let h = hex::encode(digest.finalize());
            self.land(&tmp, &h)?;
            Ok((h, size))
        }
        .await;
        if result.is_err() {
            let _ = tokio::fs::remove_file(&tmp).await;
        }
        result
    }

    pub fn put_bytes(&self, owner: BlobOwner, data: &[u8]) -> Result<String, BlobError> {
        Ok(self.put_reader(owner, data)?.0)
    }

    pub fn put_file(&self, owner: BlobOwner, path: &Path) -> Result<(String, u64), BlobError> {
        self.put_reader(owner, File::open(path)?)
    }

    /// Whether `owner`'s blob is stored.
    pub fn exists(&self, owner: BlobOwner, sha256: &str) -> bool {
        matches!(self.find(owner, sha256), Ok(Some(_)))
    }

    /// The bytes of `owner`'s blob as a reader that can seek.
    pub fn open_seek(&self, owner: BlobOwner, sha256: &str) -> Result<BlobReader, BlobError> {
        let open = |p: &Path| {
            File::open(p).map_err(|e| match e.kind() {
                io::ErrorKind::NotFound => BlobError::NotFound,
                _ => BlobError::Io(e),
            })
        };
        // A seal can land between the path checks, or remove the plain file before we open it.
        for last in [false, true] {
            return match self.find(owner, sha256)? {
                None if !last => continue,
                None => Err(BlobError::NotFound),
                Some(Found::Plain(p)) => match open(&p) {
                    Err(BlobError::NotFound) if !last => continue,
                    file => Ok(BlobReader::Plain(self.checked_plain(owner, sha256, file?)?)),
                },
                Some(Found::Sealed { path, dek, slot, sha }) => Ok(BlobReader::Sealed(Box::new(SealedReader::open(
                    open(&path)?,
                    &dek,
                    slot.id(),
                    &sha,
                )?))),
            };
        }
        Err(BlobError::NotFound)
    }

    /// The bytes of `owner`'s blob as an async reader, with their size, for streaming a response. A
    /// sealed file's header, first record and last record are checked before this returns, so a file
    /// that does not open is an error here, while a route can still answer with an error status; a
    /// later record that fails ends the stream with an error.
    pub async fn open_async(
        &self,
        owner: BlobOwner,
        sha256: &str,
    ) -> Result<(impl AsyncRead + Send + Unpin + 'static, u64), BlobError> {
        let (store, sha) = (self.clone(), sha256.to_string());
        let (reader, first) = tokio::task::spawn_blocking(move || -> Result<_, BlobError> {
            let mut reader = store.open_seek(owner, &sha)?;
            let mut first = Vec::new();
            if matches!(reader, BlobReader::Sealed(_)) {
                first.resize(RECORD, 0);
                let n = reader.read(&mut first)?;
                first.truncate(n);
            }
            Ok((reader, first))
        })
        .await
        .map_err(|e| BlobError::Io(io::Error::other(e)))??;
        let size = reader.len()?;
        let stream: Box<dyn AsyncRead + Send + Unpin> = match reader {
            BlobReader::Plain(f) => Box::new(tokio::fs::File::from_std(f)),
            sealed @ BlobReader::Sealed(_) => {
                let (tx, rx) = tokio::sync::mpsc::channel(4);
                tokio::task::spawn_blocking(move || {
                    let mut r = sealed;
                    loop {
                        let mut buf = vec![0u8; RECORD];
                        let sent = match r.read(&mut buf) {
                            Ok(0) => return,
                            Ok(n) => {
                                buf.truncate(n);
                                tx.blocking_send(Ok(buf))
                            }
                            Err(e) => {
                                let _ = tx.blocking_send(Err(e));
                                return;
                            }
                        };
                        if sent.is_err() {
                            return; // the response was dropped
                        }
                    }
                });
                Box::new(SealedStream { rx, buf: first, at: 0 })
            }
        };
        Ok((stream, size))
    }

    /// `owner`'s blob as a file on local disk, for readers that need a path. Prefer `open_seek`: a
    /// sealed blob is decrypted whole into a temporary file here.
    pub fn with_local_copy(&self, owner: BlobOwner, sha256: &str) -> Result<LocalCopy, BlobError> {
        for last in [false, true] {
            match self.find(owner, sha256)? {
                None if !last => continue,
                None => return Err(BlobError::NotFound),
                Some(Found::Plain(path)) if self.keys.is_some() => match self.link_plain(owner, sha256, &path) {
                    // sealed and removed since the lookup (`seal-legacy`): look once more
                    Err(BlobError::NotFound) if !last => continue,
                    copy => return copy,
                },
                Some(found) => return self.local_copy(owner, sha256, found),
            }
        }
        Err(BlobError::NotFound)
    }

    /// A [`LocalCopy`] of the plain file at `path` as a hard link in the tmp folder, locked (shared)
    /// before the link exists, so the sweep leaves it alone from the start.
    fn link_plain(&self, owner: BlobOwner, sha256: &str, path: &Path) -> Result<LocalCopy, BlobError> {
        let not_found = |e: io::Error| match e.kind() {
            io::ErrorKind::NotFound => BlobError::NotFound,
            _ => BlobError::Io(e),
        };
        let file = File::open(path).map_err(not_found)?;
        let mut links = self
            .plain_links
            .lock()
            .map_err(|_| io::Error::other("plain link cache is unavailable"))?;
        if let Some(copy) = links.get(path).and_then(Weak::upgrade)
            && let Some(held) = &copy.held
            && let Some((dev, ino, ..)) = stamp(&held.metadata()?)
            && stamp(&file.metadata()?).is_some_and(|s| (s.0, s.1) == (dev, ino))
        {
            drop(links);
            self.checked_plain(owner, sha256, file)?;
            return Ok(LocalCopy(copy));
        }

        file.lock_shared()?;
        let link = self.root.join("tmp").join(format!("copy-{}", tmp_name()));
        if let Err(e) = link_file(path, &link) {
            if e.kind() == io::ErrorKind::NotFound {
                return Err(BlobError::NotFound);
            }
            // No link here (EPERM from protected_hardlinks, EXDEV when tmp is on another mount, a
            // filesystem without hard links): copy the bytes from the file opened above instead.
            // `drop_plain` removes the stored path without waiting for readers, and the open file
            // keeps its bytes after that, so the copy never depends on the stored path.
            drop(links);
            let mut reader = self.checked_plain(owner, sha256, file)?;
            let mut copy = CopyFile {
                path: link,
                temporary: true,
                held: None,
            };
            let mut out = create_private(&copy.path)?;
            // before a byte is in it: the sweep leaves a locked copy alone ([`in_use`])
            out.lock()?;
            io::copy(&mut reader, &mut out)?;
            out.flush()?;
            copy.held = Some(out);
            return Ok(LocalCopy(Arc::new(copy)));
        }
        let mut copy = CopyFile {
            path: link,
            temporary: true,
            held: None,
        };
        // Use the inode at the link even if the stored path was replaced before hard_link.
        let linked = File::open(&copy.path).map_err(not_found)?;
        linked.lock_shared()?;
        let reader = linked.try_clone()?;
        copy.held = Some(linked);
        let copy = Arc::new(copy);
        links.retain(|_, link| link.strong_count() != 0);
        links.insert(path.to_path_buf(), Arc::downgrade(&copy));
        drop(links);
        // Link creation can change ctime. Cache the stamp of the inode after that change.
        self.checked_plain(owner, sha256, reader)?;
        Ok(LocalCopy(copy))
    }

    fn local_copy(&self, owner: BlobOwner, sha256: &str, found: Found) -> Result<LocalCopy, BlobError> {
        let copy = match found {
            Found::Plain(path) => {
                self.checked_plain(owner, sha256, File::open(&path)?)?;
                CopyFile {
                    path,
                    temporary: false,
                    held: None,
                }
            }
            Found::Sealed { .. } => {
                let mut reader = self.open_seek(owner, sha256)?;
                let mut copy = CopyFile {
                    path: self.root.join("tmp").join(format!("copy-{}", tmp_name())),
                    temporary: true,
                    held: None,
                };
                let mut out = create_private(&copy.path)?;
                // before a byte is in it: the sweep leaves a locked copy alone ([`in_use`])
                out.lock()?;
                io::copy(&mut reader, &mut out)?;
                out.flush()?;
                copy.held = Some(out);
                copy
            }
        };
        Ok(LocalCopy(Arc::new(copy)))
    }

    /// The bytes of `owner`'s blob, all in memory.
    pub fn get_bytes(&self, owner: BlobOwner, sha256: &str) -> Result<Vec<u8>, BlobError> {
        let mut out = Vec::new();
        self.open_seek(owner, sha256)?.read_to_end(&mut out)?;
        Ok(out)
    }

    /// The size of `owner`'s blob as its reader returns it.
    pub fn size(&self, owner: BlobOwner, sha256: &str) -> Result<u64, BlobError> {
        Ok(self.open_seek(owner, sha256)?.len()?)
    }

    /// Store `from`'s blob for `to` as well, for a document that moved to another profile or a held
    /// item that got one: read under `from`, written under `to`, the bytes checked against their sha256
    /// before anything lands. The `from` copy stays (the move's drop job removes it). True when a copy
    /// was made; false when `to` already holds it: a sealed copy for an owner this store seals, any copy
    /// otherwise (a plain file of the same bytes is no copy under the profile's key).
    pub fn rekey(&self, from: BlobOwner, to: BlobOwner, sha256: &str) -> Result<bool, BlobError> {
        if from == to {
            return Ok(false);
        }
        let seals_to = to.slot().is_some() && self.keys.is_some();
        match self.find(to, sha256)? {
            Some(Found::Sealed { .. }) => return Ok(false),
            Some(Found::Plain(_)) if !seals_to => return Ok(false),
            _ => {}
        }
        let h = sha256_hex(sha256)?;
        self.put_expecting(to, self.open_seek(from, sha256)?, Some(&h))?;
        Ok(true)
    }

    /// `owner`'s sealed copy and the plain copy of `sha256`. Only for a hard purge or the GC job,
    /// after checking no row references the hash. True when a file went.
    pub fn delete(&self, owner: BlobOwner, sha256: &str) -> Result<bool, BlobError> {
        let mut gone = self.drop_sealed(owner, sha256)?;
        let p = self.path(sha256)?;
        if p.exists() {
            // no chmod first: a backup's hard link shares the inode and keeps its 0440
            fs::remove_file(p)?;
            gone = true;
        }
        Ok(gone)
    }

    /// `owner`'s sealed copy of `sha256` only: a moved document's file under the profile it left, once
    /// no row of that profile names it. The plain copy stays. True when a file went.
    pub fn drop_sealed(&self, owner: BlobOwner, sha256: &str) -> Result<bool, BlobError> {
        let Some(Found::Sealed { path, .. }) = self.find(owner, sha256)? else {
            return Ok(false);
        };
        // no chmod first: a backup's hard link shares the inode and keeps its 0440
        fs::remove_file(path)?;
        Ok(true)
    }

    /// Whether `owner` holds a sealed copy of `sha256` (never makes a key).
    pub fn is_sealed(&self, owner: BlobOwner, sha256: &str) -> Result<bool, BlobError> {
        Ok(matches!(self.find(owner, sha256)?, Some(Found::Sealed { .. })))
    }

    /// The plain copy of `sha256` only: a legacy file once every owner that reads it holds a sealed
    /// copy (`ownhealth seal-legacy`). True when a file went.
    pub fn drop_plain(&self, sha256: &str) -> Result<bool, BlobError> {
        // no chmod first: a backup's hard link shares the inode and keeps its 0440
        match fs::remove_file(self.path(sha256)?) {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(e.into()),
        }
    }

    /// The sha256 of every file in the plain layout, where its name puts it, in name order.
    pub fn plain_files(&self) -> io::Result<Vec<String>> {
        Ok(self.files()?.into_keys().filter(|k| !k.contains('/')).collect())
    }

    /// After a profile delete: every sealed blob of `profile` and the key this process kept for it.
    /// The key row went with the profile, so a sealed file left behind no longer opens. True when the
    /// folder was there.
    pub fn forget_profile(&self, profile: Uuid) -> io::Result<bool> {
        self.forget_slot(Slot::Profile(profile))
    }

    /// After a pending item's key was shredded (its mail released or deleted, its document assigned):
    /// every sealed blob of the item and the key this process kept for it. True when the folder was
    /// there.
    pub fn forget_pending(&self, item: Uuid) -> io::Result<bool> {
        self.forget_slot(Slot::Pending(item))
    }

    /// Forget the key of `slot` in this process, and remove its sealed folder.
    pub fn forget_slot(&self, slot: Slot) -> io::Result<bool> {
        if let Some(keys) = &self.keys {
            keys.forget_slot(slot);
        }
        match fs::remove_dir_all(self.slot_dir(slot)) {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(e),
        }
    }

    /// [`BlobStore::forget_slot`] for a slot whose key is gone (a deleted profile, a settled item),
    /// unless a backup is copying the files: the key leaves this process either way, and the folder
    /// stays for the sweep, which removes it once the copy is done. `Some(true)` when the folder went,
    /// `Some(false)` when there was none, `None` when it stays.
    pub async fn forget_settled(&self, conn: &mut sqlx::PgConnection, slot: Slot) -> Result<Option<bool>, BlobError> {
        use sqlx::Connection;
        if let Some(keys) = &self.keys {
            keys.forget_slot(slot);
        }
        let mut tx = conn.begin().await.map_err(lock_error)?;
        if !removal_lock(&mut tx, false).await? {
            return Ok(None);
        }
        let store = self.clone();
        let gone = tokio::task::spawn_blocking(move || store.forget_slot(slot))
            .await
            .map_err(|e| BlobError::Io(io::Error::other(e)))??;
        tx.commit().await.map_err(lock_error)?;
        Ok(Some(gone))
    }

    /// Every slot with a sealed folder in this store: `enc/<profile>` and `pending/<item>`. A folder
    /// whose name is no id is left out (and stays: nothing here made it).
    pub fn sealed_slots(&self) -> io::Result<Vec<Slot>> {
        let mut out = Vec::new();
        for (part, make) in [("enc", Slot::Profile as fn(Uuid) -> Slot), ("pending", Slot::Pending)] {
            let entries = match fs::read_dir(self.root.join(part)) {
                Ok(e) => e,
                Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
                Err(e) => return Err(e),
            };
            for entry in entries {
                let entry = entry?;
                let name = entry.file_name();
                let name = name.to_string_lossy();
                let simple = name.len() == 32 && name.bytes().all(|c| matches!(c, b'0'..=b'9' | b'a'..=b'f'));
                if simple
                    && entry.file_type()?.is_dir()
                    && let Ok(id) = Uuid::parse_str(&name)
                {
                    out.push(make(id));
                }
            }
        }
        out.sort_by_key(|s| (matches!(s, Slot::Pending(_)), s.id()));
        Ok(out)
    }

    /// Remove every sealed folder whose key is not stored or was shredded: a process that still held
    /// a deleted profile's key (the OCR worker) can land a file after the delete removed the folder,
    /// and a released or deleted held mail and an assigned document leave their item's folder. Such a
    /// file never opens again; this removes it. A folder is made only after its key is stored
    /// (`Keyring::dek_for` commits the key first), and a shredded key never comes back, so a live
    /// folder is never removed. Keys of pending items that no longer wait are shredded first
    /// (`keys::shred_pending_orphans`). While a backup copies the files, the sweep removes nothing
    /// ([`removal_lock`]): the next one does. Only a store whose keys live in the database sweeps.
    /// Returns how many folders went, or the first folder that could not go, after it tried them all.
    pub async fn sweep_unkeyed(&self, conn: &mut sqlx::PgConnection) -> Result<usize, BlobError> {
        use sqlx::Connection;
        let Some(keys) = self.keys.as_ref().filter(|k| k.stored()) else {
            return Ok(0);
        };
        crate::keys::shred_pending_orphans(&mut *conn).await?;
        let mut tx = conn.begin().await.map_err(lock_error)?;
        if !removal_lock(&mut tx, false).await? {
            return Ok(0);
        }
        let store = self.clone();
        let slots = tokio::task::spawn_blocking(move || store.sealed_slots())
            .await
            .map_err(|e| BlobError::Io(io::Error::other(e)))??;
        if slots.is_empty() {
            return Ok(0);
        }
        let live = crate::keys::live(&mut tx, &slots).await?;
        let gone: Vec<Slot> = slots.into_iter().filter(|s| !live.contains(s)).collect();
        for slot in &gone {
            keys.forget_slot(*slot);
        }
        // every folder gets its try: one that cannot go does not keep the others; the first failure
        // is the answer once all were tried
        let store = self.clone();
        let (removed, failed) = tokio::task::spawn_blocking(move || {
            let (mut removed, mut failed) = (0, None);
            for slot in gone {
                match store.forget_slot(slot) {
                    Ok(true) => removed += 1,
                    Ok(false) => {}
                    Err(e) => {
                        // a Slot prints its ids only, never a hash or a key
                        tracing::warn!(error = %e, ?slot, "a sealed folder whose key is gone could not be removed");
                        failed.get_or_insert(e);
                    }
                }
            }
            (removed, failed)
        })
        .await
        .map_err(|e| BlobError::Io(io::Error::other(e)))?;
        tx.commit().await.map_err(lock_error)?;
        match failed {
            Some(e) => Err(BlobError::Io(e)),
            None => Ok(removed),
        }
    }

    /// Remove upload spools a killed process left in `root/tmp`: regular files named `upload-*`
    /// (the import route), `push-*` (the push inboxes), `copy-*` (a decrypted local copy), `brands-*`
    /// (a provider directory vendor file) or 32 hex characters (`put_reader`) last modified more than
    /// `older_than` ago. The age bound leaves
    /// another process's live upload alone, and a local copy is left while a [`LocalCopy`] of any
    /// process still holds it. Returns how many went.
    pub fn sweep_tmp(&self, older_than: std::time::Duration) -> io::Result<usize> {
        let dir = self.root.join("tmp");
        let entries = match fs::read_dir(&dir) {
            Ok(e) => e,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(0),
            Err(e) => return Err(e),
        };
        let now = std::time::SystemTime::now();
        let mut removed = 0;
        for entry in entries {
            let entry = entry?;
            let name = entry.file_name();
            let name = name.to_string_lossy();
            let spool = ["upload-", "push-", "copy-", "brands-"]
                .iter()
                .any(|p| name.starts_with(p));
            let ours = spool || (name.len() == 32 && name.bytes().all(|c| c.is_ascii_hexdigit()));
            let meta = entry.metadata()?;
            if !ours || !meta.is_file() {
                continue;
            }
            let age = meta.modified().ok().and_then(|m| now.duration_since(m).ok());
            // a decrypted copy a reader still holds stays, however old: its LocalCopy locks it
            if age.is_some_and(|a| a > older_than) && !(name.starts_with("copy-") && in_use(&entry.path())) {
                match fs::remove_file(entry.path()) {
                    Ok(()) => removed += 1,
                    Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                    Err(e) => return Err(e),
                }
            }
        }
        Ok(removed)
    }

    /// Every blob file of this store, keyed as [`tree_files`] keys them.
    pub fn files(&self) -> io::Result<BTreeMap<String, PathBuf>> {
        tree_files(&self.root)
    }

    /// Hard-link (or copy, across disks) every blob file of this store into the blob tree at `dest`,
    /// in the same layout: a backup's `blobs/`. Blobs never change, so a link is a copy.
    pub fn copy_into(&self, dest: &Path) -> io::Result<()> {
        for part in TREE_PARTS {
            let src = self.root.join(part);
            if src.is_dir() {
                copy_tree(&src, &dest.join(part))?;
            }
        }
        Ok(())
    }

    /// The [`tree_files`] key of `owner`'s copy of `sha256`: `<profile>/<address>` or
    /// `pending/<item>/<address>` for a sealed copy, else the sha256 itself (also when no copy is
    /// there, so a check reports it missing).
    pub fn key_of(&self, owner: BlobOwner, sha256: &str) -> Result<String, BlobError> {
        Ok(match self.find(owner, sha256)? {
            Some(Found::Sealed { slot, dek, sha, .. }) => slot_key(slot, &dek.address(&sha)),
            _ => sha256_hex(sha256)?,
        })
    }

    /// Put the file `from` of a backup's blob tree where its `key` says, copied whole, made
    /// read-only and then renamed, so a blob is never there half or writable. A key that is not one
    /// ([`tree_files`]) is refused. False when the file is there already: a resumed restore checked
    /// its bytes first ([`check_restore_target`]), and it is made read-only too, in case an older
    /// placement stopped between its rename and its mode change. An error names the destination.
    pub fn place(&self, key: &str, from: &Path) -> Result<bool, BlobError> {
        let target = self
            .root
            .join(tree_rel(key).ok_or_else(|| BlobError::BadName(key.to_string()))?);
        let named = |e: io::Error| {
            BlobError::Io(io::Error::new(
                e.kind(),
                format!("cannot place blob {key} at {}: {e}", target.display()),
            ))
        };
        if target.exists() {
            set_mode(&target, 0o440).map_err(named)?;
            return Ok(false);
        }
        let parent = target.parent().unwrap_or(&self.root);
        fs::create_dir_all(parent).map_err(named)?;
        let part = restore_part_path(&target, key);
        let placed = fs::copy(from, &part)
            .and_then(|_| set_mode(&part, 0o440))
            .and_then(|_| rename_part(&part, &target));
        if let Err(e) = placed {
            let _ = fs::remove_file(&part);
            return Err(named(e));
        }
        Ok(true)
    }
}

#[cfg(test)]
type PartHook = Box<dyn FnOnce(&Path)>;

// Run with the `.part` path right before [`BlobStore::place`] renames it, in this test thread only.
#[cfg(test)]
thread_local! {
    static BEFORE_PLACE_RENAME: std::cell::RefCell<Option<PartHook>> = const { std::cell::RefCell::new(None) };
}

/// The last step of [`BlobStore::place`]: the `.part` file takes the blob's name.
fn rename_part(part: &Path, target: &Path) -> io::Result<()> {
    #[cfg(test)]
    if let Some(observe) = BEFORE_PLACE_RENAME.with(|hook| hook.borrow_mut().take()) {
        observe(part);
    }
    fs::rename(part, target)
}

fn copy_tree(src: &Path, dst: &Path) -> io::Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let (from, to) = (entry.path(), dst.join(entry.file_name()));
        if from.is_dir() {
            copy_tree(&from, &to)?;
        } else if fs::hard_link(&from, &to).is_err() {
            fs::copy(&from, &to)?;
        }
    }
    Ok(())
}

fn walk(dir: &Path, out: &mut Vec<PathBuf>) -> io::Result<()> {
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_dir() {
            walk(&path, out)?;
        } else if path.is_file() {
            out.push(path);
        }
    }
    Ok(())
}

/// The folders of a blob tree, plain then sealed.
const TREE_PARTS: [&str; 3] = ["sha256", "enc", "pending"];

/// Where a [`tree_files`] key sits inside a blob tree; None for a string that is not a key.
fn tree_rel(key: &str) -> Option<PathBuf> {
    let hex = |s: &str, n: usize| s.len() == n && s.bytes().all(|c| matches!(c, b'0'..=b'9' | b'a'..=b'f'));
    let sealed = |part: &str, id: &str, a: &str| Path::new(part).join(id).join(&a[..2]).join(&a[2..4]).join(a);
    if let Some(rest) = key.strip_prefix("pending/") {
        return match rest.split_once('/') {
            Some((i, a)) if hex(i, 32) && hex(a, 64) => Some(sealed("pending", i, a)),
            _ => None,
        };
    }
    match key.split_once('/') {
        None if hex(key, 64) => Some(Path::new("sha256").join(&key[..2]).join(&key[2..4]).join(key)),
        Some((p, a)) if hex(p, 32) && hex(a, 64) => Some(sealed("enc", p, a)),
        _ => None,
    }
}

/// Temporary sibling used by blob placement and recognized by a checkpointed retry.
fn restore_part_path(target: &Path, key: &str) -> PathBuf {
    target.with_file_name(format!(".{}.part", key.replace('/', "-")))
}

/// Profile references a restore could not recover. The file lives outside `TREE_PARTS`, so
/// only the omissions in the manifest travel with an archive.
pub type RestoreLosses = BTreeMap<Uuid, BTreeSet<String>>;

const RESTORE_LOSSES: &str = ".restore-losses.json";

/// Random bytes in the name of a staged loss record.
const STAGED_LOSSES_TOKEN: usize = 8;

/// The name [`write_restore_losses`] stages the loss record under, at the store root.
fn staged_loss_record() -> String {
    format!("{RESTORE_LOSSES}.{}.tmp", crate::ids::token_hex(STAGED_LOSSES_TOKEN))
}

fn is_staged_loss_record(rel: &Path) -> bool {
    rel.to_str()
        .and_then(|name| {
            name.strip_prefix(RESTORE_LOSSES)?
                .strip_prefix('.')?
                .strip_suffix(".tmp")
        })
        .is_some_and(|token| {
            token.len() == 2 * STAGED_LOSSES_TOKEN && token.bytes().all(|c| matches!(c, b'0'..=b'9' | b'a'..=b'f'))
        })
}

fn sha256_reader(mut reader: impl Read) -> io::Result<String> {
    let mut digest = Sha256::new();
    let mut buf = vec![0u8; CHUNK];
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        digest.update(&buf[..n]);
    }
    Ok(hex::encode(digest.finalize()))
}

/// A fresh restore needs a store without files. A checkpointed retry may find the
/// loss record and complete blobs from the same archive. Discard interrupted copies
/// of those blobs, and a loss record staged by an interrupted write, after the store
/// passes inspection.
pub fn check_restore_target(
    settings: &crate::config::Settings,
    expected: &BTreeMap<&str, &str>,
    resuming: bool,
) -> io::Result<()> {
    let root = settings.blobs_path();
    match restore_files_match(&root, expected, resuming) {
        Ok(None) => Ok(()),
        Ok(Some(path)) => {
            let action = if resuming {
                "contains files outside this restore or files that differ from the backup"
            } else {
                "is not empty"
            };
            let remedy = if resuming {
                "Use the blob store for this restore"
            } else {
                "Start a new restore with an empty blob store"
            };
            Err(io::Error::other(format!(
                "the target blob store {} {action}. Found {}. {remedy}",
                root.display(),
                path.display()
            )))
        }
        Err(error) => Err(io::Error::new(
            error.kind(),
            format!("cannot inspect the target blob store {}: {error}", root.display()),
        )),
    }
}

fn restore_files_match(root: &Path, expected: &BTreeMap<&str, &str>, resuming: bool) -> io::Result<Option<PathBuf>> {
    let mut expected_paths = BTreeMap::new();
    let mut expected_parts = BTreeSet::new();
    for (&key, &digest) in expected {
        if let Some(path) = tree_rel(key) {
            if resuming {
                expected_parts.insert(restore_part_path(&path, key));
            }
            expected_paths.insert(path, digest);
        }
    }
    let mut parts = Vec::new();
    let mut directories = vec![root.to_path_buf()];
    while let Some(dir) = directories.pop() {
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(e) if dir == root && e.kind() == io::ErrorKind::NotFound && !root.is_symlink() => return Ok(None),
            Err(e) => return Err(e),
        };
        for entry in entries {
            let entry = entry?;
            let path = entry.path();
            let kind = entry.file_type()?;
            let rel = path.strip_prefix(root).map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("{} is outside the blob root", path.display()),
                )
            })?;
            let loss_record = rel == Path::new(RESTORE_LOSSES);
            if kind.is_dir() {
                if loss_record || expected_paths.contains_key(rel) || expected_parts.contains(rel) {
                    return Ok(Some(path));
                }
                directories.push(path);
                continue;
            }
            if !resuming || !kind.is_file() {
                return Ok(Some(path));
            }
            if loss_record {
                continue;
            }
            if expected_parts.contains(rel) || is_staged_loss_record(rel) {
                parts.push(path);
                continue;
            }
            let Some(&digest) = expected_paths.get(rel) else {
                return Ok(Some(path));
            };
            if sha256_reader(File::open(&path)?)? != digest {
                return Ok(Some(path));
            }
        }
    }
    for part in parts {
        fs::remove_file(&part)
            .map_err(|error| io::Error::new(error.kind(), format!("cannot remove {}: {error}", part.display())))?;
    }
    Ok(None)
}

/// Read the omissions accepted in this store by a restore.
pub fn read_restore_losses(settings: &crate::config::Settings) -> io::Result<RestoreLosses> {
    let dir = settings.blobs_path();
    let file = dir.join(RESTORE_LOSSES);
    let read = || -> io::Result<RestoreLosses> {
        if !crate::keyfile::owner_writes_only(&file, false, "restore losses").map_err(io::Error::other)? {
            return Ok(RestoreLosses::new());
        }
        check_loss_dir(&dir)?;
        Ok(serde_json::from_reader(File::open(&file)?)?)
    };
    read().map_err(|e| {
        io::Error::new(
            e.kind(),
            format!("cannot read restore losses from {}: {e}", file.display()),
        )
    })
}

/// Record this restore's expected omissions. A matching record may already exist on retry.
pub fn record_restore_losses(settings: &crate::config::Settings, expected: &RestoreLosses) -> io::Result<()> {
    let stored = read_restore_losses(settings)?;
    if !stored.is_empty() && stored != *expected {
        return Err(io::Error::other(format!(
            "the loss record in the target blob store {} differs from the losses for this restore",
            settings.blobs_path().display()
        )));
    }
    if stored != *expected {
        write_restore_losses(settings, expected)?;
    }
    Ok(())
}

fn check_loss_dir(dir: &Path) -> io::Result<()> {
    crate::keyfile::owner_writes_only(&fs::canonicalize(dir)?, true, "blob store").map_err(io::Error::other)?;
    Ok(())
}

/// Replace the loss record while holding the keyring lock. A crash leaves the old or new record.
pub fn write_restore_losses(settings: &crate::config::Settings, losses: &RestoreLosses) -> io::Result<()> {
    let dir = settings.blobs_path();
    let staged = dir.join(staged_loss_record());
    let write = || -> io::Result<()> {
        fs::create_dir_all(&dir)?;
        check_loss_dir(&dir)?;
        let mut f = create_private(&staged)?;
        serde_json::to_writer(&mut f, losses)?;
        f.sync_all()?;
        drop(f);
        fs::rename(&staged, dir.join(RESTORE_LOSSES))?;
        #[cfg(unix)]
        File::open(&dir)?.sync_all()?;
        Ok(())
    };
    write().map_err(|e| {
        let _ = fs::remove_file(staged);
        io::Error::new(
            e.kind(),
            format!("cannot write restore losses in {}: {e}", dir.display()),
        )
    })
}

/// Whether `path` lies in the configured blob folder: the keyring folder must not, or every backup
/// would copy it (`keyfile::check_folder`).
pub fn in_blob_folder(settings: &crate::config::Settings, path: &Path) -> Result<bool, crate::keys::KeyError> {
    crate::keyfile::inside(path, &settings.blobs_path())
}

/// Every blob file of the tree at `root` (a store, or a backup's `blobs/`), in path order, keyed the
/// way a backup manifest lists it: a plain blob by its file name (its sha256), a profile's sealed one
/// as `<profile>/<file name>` (its address), a pending item's as `pending/<item>/<file name>`. A file
/// that is not where its key puts it (another shard, a name that is no key, a file elsewhere under
/// `enc/` or `pending/`) is keyed `?/<its path in the tree>`: no manifest lists that key and no restore
/// places it, and it never replaces the file its name stands for.
pub fn tree_files(root: &Path) -> io::Result<BTreeMap<String, PathBuf>> {
    let mut out = BTreeMap::new();
    for part in TREE_PARTS {
        let dir = root.join(part);
        let mut files = Vec::new();
        if dir.is_dir() {
            walk(&dir, &mut files)?;
        }
        files.sort();
        for p in files {
            let rel = p.strip_prefix(root).unwrap_or(&p).to_path_buf();
            let name = p
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            let key = match part {
                "sha256" => name,
                _ => {
                    let lead = if part == "pending" { "pending/" } else { "" };
                    match p.strip_prefix(&dir).ok().and_then(|r| r.components().next()) {
                        Some(id) => format!("{lead}{}/{name}", id.as_os_str().to_string_lossy()),
                        None => name,
                    }
                }
            };
            let key = match tree_rel(&key) {
                Some(at) if at == rel => key,
                _ => format!("?/{}", rel.to_string_lossy()),
            };
            out.insert(key, p);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn owner() -> BlobOwner {
        BlobOwner::Profile(uuid::Uuid::nil())
    }

    #[test]
    fn resumed_restore_refuses_other_bytes_at_an_expected_path() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let settings = crate::config::Settings {
            data_dir: dir.path().to_path_buf(),
            ..Default::default()
        };
        let original = b"original backup bytes";
        let digest = sha256_reader(&original[..])?;
        let expected = BTreeMap::from([(digest.as_str(), digest.as_str())]);
        let relative = tree_rel(&digest).expect("a SHA-256 digest is a plain blob key");
        let target = settings.blobs_path().join(relative);
        fs::create_dir_all(target.parent().expect("a blob path has a shard directory"))?;
        fs::write(&target, b"unrelated blob bytes")?;
        check_restore_target(&settings, &expected, true).expect_err("resume must compare the digest");
        assert_eq!(fs::read(&target)?, b"unrelated blob bytes");
        fs::write(&target, original)?;
        check_restore_target(&settings, &expected, true)?;
        Ok(())
    }

    /// A stop inside `write_restore_losses` leaves its staged record in the store root. A resumed
    /// restore removes that file and still refuses any other name.
    #[test]
    fn resumed_restore_removes_only_a_staged_loss_record() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let settings = crate::config::Settings {
            data_dir: dir.path().to_path_buf(),
            ..Default::default()
        };
        let root = settings.blobs_path();
        fs::create_dir_all(&root)?;
        let expected = BTreeMap::new();
        // The name and mode that write_restore_losses gives the file before the rename.
        let staged_name = staged_loss_record();
        let staged = root.join(&staged_name);
        drop(create_private(&staged)?);
        let strays = [
            root.join(format!("{RESTORE_LOSSES}.tmp")),
            root.join(format!("{RESTORE_LOSSES}.not-a-token.tmp")),
            root.join("sha256").join(&staged_name),
        ];
        for stray in strays {
            fs::create_dir_all(stray.parent().expect("a stray file has a folder"))?;
            fs::write(&stray, b"{}")?;
            let error = check_restore_target(&settings, &expected, true).expect_err("a stray file is refused");
            assert!(error.to_string().contains(&stray.display().to_string()), "{error}");
            assert!(staged.exists() && stray.exists());
            fs::remove_file(&stray)?;
        }
        check_restore_target(&settings, &expected, false).expect_err("a new restore needs an empty store");
        check_restore_target(&settings, &expected, true)?;
        assert!(!staged.exists(), "the staged loss record was left behind");
        Ok(())
    }

    /// A directory where a resumed restore expects an interrupted copy (`.part`) is refused by its
    /// path: placement could never write the copy there.
    #[test]
    fn resumed_restore_refuses_a_directory_at_an_expected_part_path() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let settings = crate::config::Settings {
            data_dir: dir.path().to_path_buf(),
            ..Default::default()
        };
        let digest = sha256_reader(&b"backup bytes"[..])?;
        let expected = BTreeMap::from([(digest.as_str(), digest.as_str())]);
        let relative = tree_rel(&digest).expect("a SHA-256 digest is a plain blob key");
        let part = settings.blobs_path().join(restore_part_path(&relative, &digest));
        fs::create_dir_all(&part)?;
        let error = check_restore_target(&settings, &expected, true).expect_err("a directory at a .part path");
        assert!(error.to_string().contains(&part.display().to_string()), "{error}");
        assert!(part.is_dir());
        Ok(())
    }

    #[test]
    fn resumed_restore_refuses_a_different_nonempty_loss_record() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let settings = crate::config::Settings {
            data_dir: dir.path().to_path_buf(),
            ..Default::default()
        };
        let profile = Uuid::new_v4();
        let expected = RestoreLosses::from([(profile, BTreeSet::from(["a".repeat(64)]))]);
        let stored = RestoreLosses::from([(profile, BTreeSet::from(["b".repeat(64)]))]);
        write_restore_losses(&settings, &stored)?;

        let error = record_restore_losses(&settings, &expected).expect_err("resume must refuse a different record");
        assert!(
            error.to_string().contains("differs from the losses for this restore"),
            "{error}"
        );
        assert_eq!(read_restore_losses(&settings)?, stored);
        record_restore_losses(&settings, &stored)?;
        Ok(())
    }

    #[test]
    fn open_seek_reads_from_any_offset() -> Result<(), BlobError> {
        let dir = tempfile::tempdir()?;
        let store = BlobStore::new(dir.path())?;
        let h = store.put_bytes(owner(), b"Carmen Ortega, LabNorte")?;
        let mut r = store.open_seek(owner(), &h)?;
        r.seek(io::SeekFrom::Start(15))?;
        let mut tail = String::new();
        r.read_to_string(&mut tail)?;
        assert_eq!(tail, "LabNorte");
        assert!(matches!(
            store.open_seek(owner(), &"0".repeat(64)),
            Err(BlobError::NotFound)
        ));
        assert!(matches!(
            store.open_seek(owner(), "../etc/passwd"),
            Err(BlobError::BadHash)
        ));
        Ok(())
    }

    #[test]
    fn local_copy_holds_the_bytes() -> Result<(), BlobError> {
        let dir = tempfile::tempdir()?;
        let store = BlobStore::new(dir.path())?;
        let data = b"SQLite format 3\0 synthetic Rivera family export";
        let h = store.put_bytes(owner(), data)?;
        let copy = store.with_local_copy(BlobOwner::Reference, &h.to_uppercase())?;
        assert_eq!(fs::read(copy.path())?, data);
        assert!(matches!(
            store.with_local_copy(owner(), &"0".repeat(64)),
            Err(BlobError::NotFound)
        ));
        assert!(matches!(
            store.with_local_copy(owner(), "nope"),
            Err(BlobError::BadHash)
        ));
        Ok(())
    }

    #[test]
    fn dropping_a_local_copy_leaves_the_stored_blob() -> Result<(), BlobError> {
        let dir = tempfile::tempdir()?;
        let store = BlobStore::new(dir.path())?;
        let data = b"##fileformat=VCFv4.2 synthetic Rivera calls";
        let h = store.put_bytes(owner(), data)?;
        let copy = store.with_local_copy(owner(), &h)?;
        assert_eq!(fs::read(copy.path())?, data);
        drop(copy);
        assert_eq!(store.get_bytes(owner(), &h)?, data);
        assert!(store.exists(owner(), &h));
        let again = store.with_local_copy(owner(), &h)?;
        assert_eq!(fs::read(again.path())?, data);
        Ok(())
    }

    #[test]
    fn staging_paths_sit_in_tmp_under_their_own_name() -> Result<(), BlobError> {
        let dir = tempfile::tempdir()?;
        let store = BlobStore::new(dir.path())?;
        let p = store.staging_path("upload-0123abcd")?;
        assert_eq!(p, dir.path().join("tmp").join("upload-0123abcd"));
        File::create(&p)?.set_modified(std::time::SystemTime::now() - std::time::Duration::from_secs(7200))?;
        assert_eq!(store.sweep_tmp(std::time::Duration::from_secs(3600))?, 1);
        Ok(())
    }

    #[tokio::test]
    async fn an_upload_spool_is_readable_by_its_user_only() -> Result<(), BlobError> {
        let dir = tempfile::tempdir()?;
        let store = BlobStore::new(dir.path())?;
        let p = store.staging_path("upload-0123abcd")?;
        let mut f = create_spool(&p).await?;
        f.write_all(b"Carmen Ortega, LabNorte").await?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(fs::metadata(&p)?.permissions().mode() & 0o777, 0o600);
        }
        assert!(create_spool(&p).await.is_err(), "an existing spool opened again");
        Ok(())
    }

    #[test]
    fn staging_path_takes_one_plain_file_name_only() -> Result<(), BlobError> {
        let dir = tempfile::tempdir()?;
        let store = BlobStore::new(dir.path())?;
        for bad in ["../x", "/x", "a/b", "", ".", "..", "x/"] {
            assert!(matches!(store.staging_path(bad), Err(BlobError::BadName(_))), "{bad:?}");
        }
        assert_eq!(
            store.staging_path("push-ab12")?,
            dir.path().join("tmp").join("push-ab12")
        );
        Ok(())
    }

    #[test]
    fn health_names_the_missing_directory() -> io::Result<()> {
        let dir = tempfile::tempdir()?;
        let root = dir.path().join("blobs");
        let store = BlobStore::new(&root)?;
        assert_eq!(store.health(), Ok(()));
        fs::remove_dir_all(&root)?;
        assert_eq!(store.health(), Err(root.display().to_string()));
        Ok(())
    }

    #[tokio::test]
    async fn open_async_streams_the_bytes_with_their_size() -> Result<(), BlobError> {
        let dir = tempfile::tempdir()?;
        let store = BlobStore::new(dir.path())?;
        let h = store.put_bytes(owner(), b"LabNorte lipid panel")?;
        let (mut r, size) = store.open_async(owner(), &h).await?;
        let mut out = Vec::new();
        r.read_to_end(&mut out).await?;
        assert_eq!((out.as_slice(), size), (&b"LabNorte lipid panel"[..], 20));
        assert!(matches!(
            store.open_async(owner(), &"0".repeat(64)).await,
            Err(BlobError::NotFound)
        ));
        Ok(())
    }

    #[test]
    fn sweep_tmp_removes_only_old_spools() -> io::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = BlobStore::new(dir.path())?;
        let tmp = dir.path().join("tmp");
        fs::create_dir_all(&tmp)?;
        let two_hours_ago = std::time::SystemTime::now() - std::time::Duration::from_secs(7200);
        for (name, old) in [
            ("upload-0123abcd".to_string(), true),
            ("push-0123abcd".to_string(), true),
            ("a".repeat(32), true),
            ("copy-0123abcd".to_string(), true),
            ("brands-0123abcd-epic.json".to_string(), true),
            ("upload-fresh".to_string(), false),
            ("brands-0123abcd-oracle.json".to_string(), false),
            ("notes.txt".to_string(), true),
        ] {
            let f = File::create(tmp.join(name))?;
            if old {
                f.set_modified(two_hours_ago)?;
            }
        }
        let one_hour = std::time::Duration::from_secs(3600);
        assert_eq!(store.sweep_tmp(one_hour)?, 5);
        let mut left = Vec::new();
        for e in fs::read_dir(&tmp)? {
            left.push(e?.file_name().to_string_lossy().into_owned());
        }
        left.sort();
        assert_eq!(left, ["brands-0123abcd-oracle.json", "notes.txt", "upload-fresh"]);
        assert_eq!(store.sweep_tmp(one_hour)?, 0);
        Ok(())
    }

    /// A decrypted local copy that a long import still reads (an Apple Health export, a Gadgetbridge
    /// database) outlives the sweep's age bound: the sweep leaves it while its LocalCopy lives, in
    /// this process or another, and removes it once that is gone and the copy is old.
    #[test]
    fn the_spool_sweep_leaves_a_local_copy_that_is_still_read() -> Result<(), BlobError> {
        let dir = tempfile::tempdir()?;
        let store = sealing_store(dir.path())?;
        let p = BlobOwner::Profile(Uuid::new_v4());
        let h = store.put_bytes(p, b"Apple Health export of Alex Rivera")?;
        let copy = store.with_local_copy(p, &h)?;
        let two_hours_ago = std::time::SystemTime::now() - std::time::Duration::from_secs(7200);
        File::options()
            .write(true)
            .open(copy.path())?
            .set_modified(two_hours_ago)?;
        let one_hour = std::time::Duration::from_secs(3600);
        assert_eq!(store.sweep_tmp(one_hour)?, 0, "the sweep removed a copy still in use");
        assert_eq!(fs::read(copy.path())?, b"Apple Health export of Alex Rivera");
        // a copy a killed process left holds no lock: it goes
        let left = copy.path().to_path_buf();
        fs::copy(copy.path(), dir.path().join("tmp").join("copy-killed"))?;
        File::options()
            .write(true)
            .open(dir.path().join("tmp").join("copy-killed"))?
            .set_modified(two_hours_ago)?;
        assert_eq!(store.sweep_tmp(one_hour)?, 1);
        drop(copy);
        assert!(!left.exists());
        Ok(())
    }

    #[test]
    fn every_read_names_its_owner() -> Result<(), BlobError> {
        let dir = tempfile::tempdir()?;
        let store = BlobStore::new(dir.path())?;
        let h = store.put_bytes(owner(), b"Rivera family lipid panel")?;
        assert!(store.exists(owner(), &h));
        assert_eq!(store.size(owner(), &h)?, 25);
        assert_eq!(
            store.get_bytes(BlobOwner::Pending(Uuid::nil()), &h)?,
            b"Rivera family lipid panel"
        );
        assert!(!store.exists(BlobOwner::Reference, &"0".repeat(64)));
        assert!(matches!(store.size(owner(), &"0".repeat(64)), Err(BlobError::NotFound)));
        Ok(())
    }

    #[test]
    fn put_dedupes_and_uses_sharded_path() {
        let dir = tempfile::tempdir().expect("tmp");
        let store = BlobStore::new(dir.path()).expect("store");
        let data = b"synthetic lab report";
        let h1 = store.put_bytes(owner(), data).expect("put");
        let (h2, size) = store.put_reader(owner(), &data[..]).expect("put");
        assert_eq!(h1, h2);
        assert_eq!(h1, hex::encode(Sha256::digest(data)));
        assert_eq!(size, data.len() as u64);
        let p = store.path(&h1).expect("path");
        assert_eq!(p, dir.path().join("sha256").join(&h1[..2]).join(&h1[2..4]).join(&h1));
        assert_eq!(store.get_bytes(owner(), &h1.to_uppercase()).expect("get"), data);
        assert_eq!(fs::read_dir(dir.path().join("tmp")).expect("tmp").count(), 0);
        assert_eq!(hex::encode(sha256_hex_to_bytes(&h1).expect("bytes")), h1);
    }

    #[test]
    fn missing_and_bad_hashes() {
        let dir = tempfile::tempdir().expect("tmp");
        let store = BlobStore::new(dir.path()).expect("store");
        assert!(matches!(
            store.get_bytes(owner(), &"0".repeat(64)),
            Err(BlobError::NotFound)
        ));
        assert!(matches!(store.path("../../etc/passwd"), Err(BlobError::BadHash)));
        let h = store.put_bytes(owner(), b"x").expect("put");
        assert!(store.delete(owner(), &h).expect("delete"));
        assert!(!store.exists(owner(), &h));
    }

    #[test]
    fn a_missing_blob_names_no_hash() {
        let dir = tempfile::tempdir().expect("tmp");
        let store = BlobStore::new(dir.path()).expect("store");
        let h = "ab".repeat(32);
        let e = store.get_bytes(owner(), &h).expect_err("found");
        assert!(!e.to_string().contains("ababab"), "{e}");
        let shown = crate::Error::from(e).to_string();
        assert!(!shown.contains("ababab"), "{shown}");
    }

    #[tokio::test]
    async fn async_put_matches_sync_put() {
        let dir = tempfile::tempdir().expect("tmp");
        let store = BlobStore::new(dir.path()).expect("store");
        let (h, n) = store.put_async(owner(), &b"LabNorte"[..]).await.expect("put");
        assert_eq!(h, store.put_bytes(owner(), b"LabNorte").expect("put"));
        assert_eq!(n, 8);
    }

    fn sealing_store(root: &Path) -> io::Result<BlobStore> {
        let mut secret = [0u8; 32];
        rand::fill(&mut secret[..]); // a key made at run time, never a literal
        let kek = crate::keys::Kek::from_secret(&secret).map_err(io::Error::other)?;
        BlobStore::with_keys(root, Arc::new(Keyring::in_memory(kek)))
    }

    fn files_under(dir: &Path) -> Vec<PathBuf> {
        let mut out = Vec::new();
        let mut todo = vec![dir.to_path_buf()];
        while let Some(d) = todo.pop() {
            for e in fs::read_dir(&d).into_iter().flatten().flatten() {
                let p = e.path();
                if p.is_dir() {
                    todo.push(p);
                } else {
                    out.push(p);
                }
            }
        }
        out.sort();
        out
    }

    #[test]
    fn a_profile_blob_is_sealed_under_a_name_that_is_not_its_hash() -> Result<(), BlobError> {
        let dir = tempfile::tempdir()?;
        let store = sealing_store(dir.path())?;
        let p = Uuid::new_v4();
        let data = b"Carmen Ortega, LabNorte ferritin 35 ng/mL".repeat(4000); // three records
        let h = store.put_bytes(BlobOwner::Profile(p), &data)?;
        assert_eq!(
            h,
            hex::encode(Sha256::digest(&data)),
            "the identity stays the plaintext sha256"
        );
        assert!(
            files_under(&dir.path().join("sha256")).is_empty(),
            "a plain copy was written"
        );
        let sealed = files_under(&dir.path().join("enc"));
        assert_eq!(sealed.len(), 1, "{sealed:?}");
        let name = sealed[0]
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        assert!(sealed[0].starts_with(dir.path().join("enc").join(p.simple().to_string())));
        assert_eq!(name.len(), 64);
        assert_ne!(name, h, "the file is named by its hash");
        let on_disk = fs::read(&sealed[0])?;
        assert!(!on_disk.windows(13).any(|w| w == b"Carmen Ortega"), "plaintext on disk");
        assert_eq!(store.get_bytes(BlobOwner::Profile(p), &h)?, data);
        assert_eq!(store.size(BlobOwner::Profile(p), &h)?, data.len() as u64);
        assert!(store.exists(BlobOwner::Profile(p), &h));
        let mut r = store.open_seek(BlobOwner::Profile(p), &h)?;
        r.seek(SeekFrom::Start(RECORD as u64 + 3))?;
        let mut five = [0u8; 5];
        r.read_exact(&mut five)?;
        assert_eq!(five, data[RECORD + 3..RECORD + 8]);
        // another profile, a pending item and reference reads find nothing
        for other in [
            BlobOwner::Profile(Uuid::new_v4()),
            BlobOwner::Pending(p),
            BlobOwner::Reference,
        ] {
            assert!(
                matches!(store.get_bytes(other, &h), Err(BlobError::NotFound)),
                "{other:?}"
            );
            assert!(!store.exists(other, &h), "{other:?}");
        }
        // the same bytes again: one file; another profile: its own sealed copy
        store.put_bytes(BlobOwner::Profile(p), &data)?;
        assert_eq!(files_under(&dir.path().join("enc")).len(), 1);
        let q = Uuid::new_v4();
        store.put_bytes(BlobOwner::Profile(q), &data)?;
        assert_eq!(files_under(&dir.path().join("enc")).len(), 2);
        assert_eq!(store.get_bytes(BlobOwner::Profile(q), &h)?, data);
        assert_eq!(fs::read_dir(dir.path().join("tmp"))?.count(), 0);
        Ok(())
    }

    #[test]
    fn reference_blobs_stay_plain() -> Result<(), BlobError> {
        let dir = tempfile::tempdir()?;
        let store = sealing_store(dir.path())?;
        let r = store.put_bytes(BlobOwner::Reference, b"ClinVar release")?;
        assert!(store.path(&r)?.is_file());
        assert!(!dir.path().join("enc").exists() && !dir.path().join("pending").exists());
        Ok(())
    }

    #[test]
    fn a_pending_items_blob_is_sealed_under_its_own_key_and_moves_to_its_profile() -> Result<(), BlobError> {
        let dir = tempfile::tempdir()?;
        let store = sealing_store(dir.path())?;
        let (item, other) = (Uuid::new_v4(), Uuid::new_v4());
        let data = b"From: LabNorte\r\nSubject: Carmen Ortega, ferritin 35 ng/mL".repeat(3000);
        let h = store.put_bytes(BlobOwner::Pending(item), &data)?;
        assert!(
            files_under(&dir.path().join("sha256")).is_empty(),
            "a plain copy was written"
        );
        let sealed = files_under(&dir.path().join("pending"));
        assert_eq!(sealed.len(), 1, "{sealed:?}");
        assert!(sealed[0].starts_with(dir.path().join("pending").join(item.simple().to_string())));
        assert!(
            !fs::read(&sealed[0])?.windows(13).any(|w| w == b"Carmen Ortega"),
            "plaintext on disk"
        );
        assert_eq!(store.get_bytes(BlobOwner::Pending(item), &h)?, data);
        // only the item's own key opens it: not another item, not a profile of the same id, not plain
        for who in [
            BlobOwner::Pending(other),
            BlobOwner::Profile(item),
            BlobOwner::Reference,
        ] {
            assert!(matches!(store.get_bytes(who, &h), Err(BlobError::NotFound)), "{who:?}");
        }
        let key = store.key_of(BlobOwner::Pending(item), &h)?;
        assert!(
            key.starts_with(&format!("pending/{}/", item.simple())) && !key.contains(&h),
            "{key}"
        );
        // assigned to a profile: sealed again under the profile's key, then the item's folder goes
        let p = BlobOwner::Profile(Uuid::new_v4());
        assert!(store.rekey(BlobOwner::Pending(item), p, &h)?);
        assert_eq!(store.get_bytes(p, &h)?, data);
        assert!(store.forget_pending(item)?);
        assert!(!dir.path().join("pending").join(item.simple().to_string()).exists());
        assert!(matches!(
            store.get_bytes(BlobOwner::Pending(item), &h),
            Err(BlobError::NotFound)
        ));
        assert_eq!(store.get_bytes(p, &h)?, data, "the profile's copy went with the item's");
        assert!(!store.forget_pending(item)?);
        Ok(())
    }

    #[test]
    fn sealed_slots_lists_profile_and_item_folders_only() -> Result<(), BlobError> {
        let dir = tempfile::tempdir()?;
        let store = sealing_store(dir.path())?;
        let (p, i) = (Uuid::new_v4(), Uuid::new_v4());
        store.put_bytes(BlobOwner::Profile(p), b"Alex Rivera, lipid panel")?;
        store.put_bytes(BlobOwner::Pending(i), b"held mail")?;
        store.put_bytes(BlobOwner::Reference, b"reference table")?;
        fs::create_dir_all(dir.path().join("enc").join("not-an-id"))?;
        fs::write(dir.path().join("pending").join("a".repeat(32)), b"a file, not a folder")?;
        let mut want = vec![Slot::Profile(p), Slot::Pending(i)];
        want.sort_by_key(|s| (matches!(s, Slot::Pending(_)), s.id()));
        assert_eq!(store.sealed_slots()?, want);
        Ok(())
    }

    #[test]
    fn reads_take_the_plain_layout_and_the_sealed_one_side_by_side() -> Result<(), BlobError> {
        let dir = tempfile::tempdir()?;
        // an install before slice 46 wrote plain files
        let old = BlobStore::new(dir.path())?;
        let p = Uuid::new_v4();
        let legacy = old.put_bytes(BlobOwner::Profile(p), b"0.4.0 Rivera lab report")?;
        let store = sealing_store(dir.path())?;
        assert_eq!(
            store.get_bytes(BlobOwner::Profile(p), &legacy)?,
            b"0.4.0 Rivera lab report"
        );
        let new = store.put_bytes(BlobOwner::Profile(p), b"1.0 Rivera lab report")?;
        assert_eq!(store.get_bytes(BlobOwner::Profile(p), &new)?, b"1.0 Rivera lab report");
        assert_eq!(
            store.get_bytes(BlobOwner::Profile(p), &legacy)?,
            b"0.4.0 Rivera lab report"
        );
        assert!(store.exists(BlobOwner::Profile(p), &legacy) && store.exists(BlobOwner::Profile(p), &new));
        // a store without the key cannot read the sealed one
        assert!(matches!(
            old.get_bytes(BlobOwner::Profile(p), &new),
            Err(BlobError::NotFound)
        ));
        Ok(())
    }

    #[tokio::test]
    async fn a_planted_plain_file_never_stands_in_for_a_profiles_blob() -> Result<(), BlobError> {
        let dir = tempfile::tempdir()?;
        let store = sealing_store(dir.path())?;
        let p = BlobOwner::Profile(Uuid::new_v4());
        let h = store.put_bytes(p, b"%PDF-1.4 LabNorte, Alex Rivera lipid panel")?;
        // the sealed copy goes, and other bytes sit at the plain path of its sha256
        for f in files_under(&dir.path().join("enc")) {
            set_mode(&f, 0o640)?;
            fs::remove_file(f)?;
        }
        let plain = store.path(&h)?;
        fs::create_dir_all(plain.parent().unwrap_or(dir.path()))?;
        fs::write(&plain, b"%PDF-1.4 forged: not the lab report")?;
        for (what, got) in [
            ("get_bytes", store.get_bytes(p, &h).map(|_| ())),
            ("size", store.size(p, &h).map(|_| ())),
            ("local copy", store.with_local_copy(p, &h).map(|_| ())),
        ] {
            let err = got.expect_err(what);
            assert!(
                matches!(err, BlobError::Io(ref e) if e.kind() == io::ErrorKind::InvalidData),
                "{what}: {err:?}"
            );
        }
        assert!(store.open_async(p, &h).await.is_err(), "streamed the forged bytes");
        // a legacy plain file with the right bytes still reads, for every owner, also twice
        fs::write(&plain, b"%PDF-1.4 LabNorte, Alex Rivera lipid panel")?;
        for _ in 0..2 {
            assert_eq!(store.get_bytes(p, &h)?, b"%PDF-1.4 LabNorte, Alex Rivera lipid panel");
        }
        // and a file changed after it was checked once is caught again
        fs::write(&plain, b"%PDF-1.4 forged after the first read!!")?;
        assert!(store.get_bytes(p, &h).is_err(), "a checked file changed later");
        Ok(())
    }

    #[test]
    fn reads_retry_when_sealing_lands_between_the_path_checks() -> Result<(), BlobError> {
        let dir = tempfile::tempdir()?;
        let store = sealing_store(dir.path())?;
        let owner = BlobOwner::Profile(Uuid::new_v4());
        for needs_path in [false, true] {
            let data = format!("Rivera legacy file, local path needed: {needs_path}");
            let sha = store.put_bytes(BlobOwner::Reference, data.as_bytes())?;
            let (sealing, hash) = (store.clone(), sha.clone());
            BETWEEN_LOOKUPS.with(|hook| {
                *hook.borrow_mut() = Some(Box::new(move || {
                    assert!(
                        sealing
                            .rekey(BlobOwner::Reference, owner, &hash)
                            .expect("seal the fixture")
                    );
                    assert!(sealing.drop_plain(&hash).expect("remove the plain fixture"));
                }));
            });
            let bytes = if needs_path {
                fs::read(store.with_local_copy(owner, &sha)?.path())?
            } else {
                store.get_bytes(owner, &sha)?
            };
            assert_eq!(bytes, data.as_bytes());
        }
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn overlapping_plain_copies_share_a_checked_link() -> Result<(), BlobError> {
        let dir = tempfile::tempdir()?;
        let store = sealing_store(dir.path())?;
        let owner = BlobOwner::Profile(Uuid::new_v4());
        let data = format!("Rivera legacy file for overlapping readers: {}", Uuid::new_v4());
        let sha = store.put_bytes(BlobOwner::Reference, data.as_bytes())?;
        let plain = store.path(&sha)?;
        set_mode(&plain, 0o640)?;
        let first = store.with_local_copy(owner, &sha)?;
        let second = store.clone().with_local_copy(owner, &sha)?;
        let path = first.path().to_path_buf();
        assert_eq!(second.path(), path);
        assert_eq!(plain_hashed_on(&sha).len(), 1);
        drop(first);
        assert_eq!(fs::read(&path)?, data.as_bytes());
        drop(second);
        assert!(!path.exists());
        let third = store.with_local_copy(owner, &sha)?;
        let before = fs::metadata(&plain)?.modified()?;
        let mut changed = fs::OpenOptions::new().write(true).open(&plain)?;
        changed.write_all(&vec![b'!'; data.len()])?;
        changed.set_modified(before)?;
        assert!(matches!(
            store.with_local_copy(owner, &sha),
            Err(BlobError::Io(ref e)) if e.kind() == io::ErrorKind::InvalidData
        ));
        drop(third);
        assert_eq!(store.checked.lock().expect("hash cache is available").len(), 1);
        Ok(())
    }

    #[test]
    fn a_plain_copy_outlives_its_stored_file_when_no_link_can_be_made() -> Result<(), BlobError> {
        let dir = tempfile::tempdir()?;
        let store = sealing_store(dir.path())?;
        let owner = BlobOwner::Profile(Uuid::new_v4());
        let data = format!(
            "Rivera legacy file on a store that takes no hard link: {}",
            Uuid::new_v4()
        );
        let sha = store.put_bytes(BlobOwner::Reference, data.as_bytes())?;
        let plain = store.path(&sha)?;
        NO_LINKS.with(|n| n.set(true));
        let copy = store.with_local_copy(owner, &sha);
        NO_LINKS.with(|n| n.set(false));
        let copy = copy?;
        assert_ne!(copy.path(), plain);
        // as `seal-legacy` does once the sealed copy is in place: no wait for readers
        fs::remove_file(&plain)?;
        assert_eq!(fs::read(copy.path())?, data.as_bytes());
        let path = copy.path().to_path_buf();
        drop(copy);
        assert!(!path.exists());
        Ok(())
    }

    #[test]
    fn a_plain_copy_survives_its_stored_file_going_while_it_is_copied() -> Result<(), BlobError> {
        let dir = tempfile::tempdir()?;
        let store = sealing_store(dir.path())?;
        let owner = BlobOwner::Profile(Uuid::new_v4());
        let data = format!("Rivera legacy file sealed while a reader copies it: {}", Uuid::new_v4());
        let sha = store.put_bytes(BlobOwner::Reference, data.as_bytes())?;
        let plain = store.path(&sha)?;
        NO_LINKS.with(|n| n.set(true));
        SEALED_MEANWHILE.with(|n| n.set(true));
        let copy = store.with_local_copy(owner, &sha);
        NO_LINKS.with(|n| n.set(false));
        SEALED_MEANWHILE.with(|n| n.set(false));
        let copy = copy?;
        assert!(!plain.exists(), "the stored file went before the bytes were copied");
        assert_eq!(fs::read(copy.path())?, data.as_bytes());
        Ok(())
    }

    #[test]
    fn a_local_copy_of_a_sealed_blob_is_private_and_goes_when_dropped() -> Result<(), BlobError> {
        let dir = tempfile::tempdir()?;
        let store = sealing_store(dir.path())?;
        let p = BlobOwner::Profile(Uuid::new_v4());
        let data = b"SQLite format 3\0 synthetic Gadgetbridge export".to_vec();
        let h = store.put_bytes(p, &data)?;
        let copy = store.with_local_copy(p, &h)?;
        assert_eq!(fs::read(copy.path())?, data);
        assert!(copy.path().starts_with(dir.path().join("tmp")));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(fs::metadata(copy.path())?.permissions().mode() & 0o777, 0o600);
        }
        let path = copy.path().to_path_buf();
        drop(copy);
        assert!(!path.exists(), "the decrypted copy outlived its LocalCopy");
        assert_eq!(store.get_bytes(p, &h)?, data);
        Ok(())
    }

    #[test]
    fn a_changed_sealed_blob_fails_to_read() -> Result<(), BlobError> {
        let dir = tempfile::tempdir()?;
        let store = sealing_store(dir.path())?;
        let p = BlobOwner::Profile(Uuid::new_v4());
        let data = b"LabNorte".repeat(20000);
        let h = store.put_bytes(p, &data)?;
        let file = files_under(&dir.path().join("enc")).remove(0);
        set_mode(&file, 0o640)?;
        let good = fs::read(&file)?;
        // a byte of the middle record: the file opens, its first record reads, the rest fails
        let mut bytes = good.clone();
        bytes[HEADER + RECORD + 16 + 5] ^= 1;
        fs::write(&file, &bytes)?;
        let mut r = store.open_seek(p, &h)?;
        let mut head = [0u8; 8];
        r.read_exact(&mut head)?; // the first record is intact
        assert_eq!(&head, b"LabNorte");
        let err = store.get_bytes(p, &h).expect_err("a flipped byte read");
        assert!(
            matches!(err, BlobError::Io(ref e) if e.kind() == io::ErrorKind::InvalidData),
            "{err:?}"
        );
        // a byte of the last record: the file does not even open
        let mut bytes = good.clone();
        let at = bytes.len() - 3;
        bytes[at] ^= 1;
        fs::write(&file, &bytes)?;
        assert!(store.open_seek(p, &h).is_err(), "a changed last record opened");
        // cut back to the first record only: refused, never read short
        fs::write(&file, &bytes[..HEADER + RECORD + 16])?;
        assert!(store.get_bytes(p, &h).is_err(), "a cut file read short");
        Ok(())
    }

    #[tokio::test]
    async fn open_async_streams_a_sealed_blob_and_stops_on_a_bad_record() -> Result<(), BlobError> {
        let dir = tempfile::tempdir()?;
        let store = sealing_store(dir.path())?;
        let p = BlobOwner::Profile(Uuid::new_v4());
        let data: Vec<u8> = (0..3 * RECORD + 9).map(|i| (i % 253) as u8).collect();
        let (h, n) = store.put_async(p, &data[..]).await?;
        assert_eq!(
            (h.as_str(), n),
            (hex::encode(Sha256::digest(&data)).as_str(), data.len() as u64)
        );
        let (mut r, size) = store.open_async(p, &h).await?;
        let mut out = Vec::new();
        r.read_to_end(&mut out).await?;
        assert_eq!((out == data, size), (true, data.len() as u64));
        let file = files_under(&dir.path().join("enc")).remove(0);
        set_mode(&file, 0o640)?;
        let mut bytes = fs::read(&file)?;
        bytes[HEADER + 2 * (RECORD + 16) + 1] ^= 1;
        fs::write(&file, &bytes)?;
        let (mut r, _) = store.open_async(p, &h).await?;
        let mut out = Vec::new();
        let err = r.read_to_end(&mut out).await.expect_err("a bad record streamed");
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(out.len() <= 2 * RECORD, "bytes of the bad record went out");
        // a bad first record, a bad last one or a bad header: refused before any byte, so a route
        // can still answer with an error status
        for at in [HEADER + 1, bytes.len() - 1, 30] {
            let mut bad = fs::read(&file)?;
            bad[HEADER + 2 * (RECORD + 16) + 1] ^= 1; // undo the record 2 change
            bad[at] ^= 1;
            fs::write(&file, &bad)?;
            let err = store.open_async(p, &h).await.err().expect("a bad file opened");
            assert!(
                matches!(err, BlobError::Io(ref e) if e.kind() == io::ErrorKind::InvalidData),
                "{at}: {err:?}"
            );
            bad[at] ^= 1;
            bad[HEADER + 2 * (RECORD + 16) + 1] ^= 1;
            fs::write(&file, &bad)?;
        }
        Ok(())
    }

    #[test]
    fn rekey_copies_a_blob_under_the_new_profiles_key() -> Result<(), BlobError> {
        let dir = tempfile::tempdir()?;
        let store = sealing_store(dir.path())?;
        let (from, to) = (BlobOwner::Profile(Uuid::new_v4()), BlobOwner::Profile(Uuid::new_v4()));
        let h = store.put_bytes(from, b"Sam Rivera, filed to Alex by mistake")?;
        assert!(!store.exists(to, &h));
        assert!(store.rekey(from, to, &h)?);
        assert_eq!(store.get_bytes(to, &h)?, b"Sam Rivera, filed to Alex by mistake");
        assert!(!store.rekey(from, to, &h)?, "copied twice");
        // a pending item's legacy plain file is sealed for the profile it moves to; a missing one is not found
        let plain = store.put_bytes(BlobOwner::Reference, b"held scan")?;
        assert!(store.rekey(BlobOwner::Pending(Uuid::nil()), to, &plain)?);
        assert!(store.key_of(to, &plain)?.contains('/'));
        assert!(matches!(
            store.rekey(from, to, &"0".repeat(64)),
            Err(BlobError::NotFound)
        ));
        Ok(())
    }

    #[test]
    fn delete_and_forget_remove_a_profiles_sealed_files() -> Result<(), BlobError> {
        let dir = tempfile::tempdir()?;
        let store = sealing_store(dir.path())?;
        let (p, q) = (Uuid::new_v4(), Uuid::new_v4());
        let a = store.put_bytes(BlobOwner::Profile(p), b"Alex Rivera, visit notes")?;
        let b = store.put_bytes(BlobOwner::Profile(p), b"Alex Rivera, lipid panel")?;
        store.put_bytes(BlobOwner::Profile(q), b"Alex Rivera, lipid panel")?;
        assert!(store.delete(BlobOwner::Profile(p), &a)?);
        assert!(!store.exists(BlobOwner::Profile(p), &a));
        assert!(!store.delete(BlobOwner::Profile(p), &a)?);
        assert!(store.forget_profile(p)?);
        assert!(!store.exists(BlobOwner::Profile(p), &b));
        assert!(store.exists(BlobOwner::Profile(q), &b), "another profile's copy went");
        assert!(!store.forget_profile(p)?);
        Ok(())
    }

    #[test]
    fn a_tree_copies_and_places_back_under_its_keys() -> Result<(), BlobError> {
        let dir = tempfile::tempdir()?;
        let store = sealing_store(&dir.path().join("live"))?;
        let p = Uuid::new_v4();
        let sealed = store.put_bytes(BlobOwner::Profile(p), b"Carmen Ortega, LabNorte panel")?;
        let plain = store.put_bytes(BlobOwner::Reference, b"reference table")?;
        let item = Uuid::new_v4();
        let held = store.put_bytes(BlobOwner::Pending(item), b"held LabNorte mail")?;
        let held_key = store.key_of(BlobOwner::Pending(item), &held)?;
        assert!(
            held_key.starts_with(&format!("pending/{}/", item.simple())),
            "{held_key}"
        );
        let key = store.key_of(BlobOwner::Profile(p), &sealed)?;
        assert!(key.starts_with(&format!("{}/", p.simple())) && !key.contains(&sealed));
        assert_eq!(
            store.key_of(BlobOwner::Profile(p), &plain)?,
            plain,
            "a plain copy keeps its sha"
        );
        assert_eq!(store.key_of(BlobOwner::Profile(p), &"0".repeat(64))?, "0".repeat(64));
        let backup = dir.path().join("backup");
        store.copy_into(&backup)?;
        let files = tree_files(&backup)?;
        let mut want = vec![key.clone(), plain.clone(), held_key.clone()];
        want.sort();
        assert_eq!(files.keys().cloned().collect::<Vec<_>>(), want);
        let fresh = BlobStore::new(dir.path().join("fresh"))?;
        for (k, path) in &files {
            assert!(fresh.place(k, path)?);
            assert!(!fresh.place(k, path)?, "placed twice");
        }
        assert_eq!(
            fresh.files()?.keys().collect::<Vec<_>>(),
            files.keys().collect::<Vec<_>>()
        );
        assert_eq!(fresh.get_bytes(BlobOwner::Reference, &plain)?, b"reference table");
        let climb = format!("{}/../{}", p.simple(), "0".repeat(61));
        let pending_climb = format!("pending/{}/../{}", item.simple(), "0".repeat(61));
        let pending_short = format!("pending/{}", item.simple());
        for bad in [
            "../x",
            "ab/cd",
            climb.as_str(),
            sealed.to_uppercase().as_str(),
            pending_climb.as_str(),
            pending_short.as_str(),
            "pending/x/y",
        ] {
            assert!(
                matches!(fresh.place(bad, &files[&plain]), Err(BlobError::BadName(_))),
                "{bad}"
            );
        }
        fs::write(backup.join("enc").join("stray"), b"x")?;
        fs::write(backup.join("pending").join("stray"), b"x")?;
        let strays = tree_files(&backup)?;
        assert!(strays.contains_key("?/enc/stray") && strays.contains_key("?/pending/stray"));
        Ok(())
    }

    /// A place that fails removes its `.part` file, so a stale one never blocks the next restore.
    #[test]
    fn a_failed_place_leaves_no_part_file() -> Result<(), BlobError> {
        let dir = tempfile::tempdir()?;
        let store = BlobStore::new(dir.path().join("live"))?;
        let sha = store.put_bytes(BlobOwner::Reference, b"reference table")?;
        let from = dir.path().join("from");
        fs::write(&from, b"reference table")?;
        let fresh = BlobStore::new(dir.path().join("fresh"))?;
        let target = plain_at(&dir.path().join("fresh"), &sha);
        fs::create_dir_all(target.parent().unwrap_or(dir.path()))?;
        let part = target.with_file_name(format!(".{sha}.part"));
        fs::write(&part, b"half")?;
        set_mode(&part, 0o440)?; // a copy cannot write it
        assert!(fresh.place(&sha, &from).is_err());
        assert!(!part.exists(), "the failed place left its .part file");
        assert!(fresh.place(&sha, &from)?);
        assert_eq!(fs::read(&target)?, b"reference table");
        Ok(())
    }

    /// A placed blob is read-only. So is one that a stopped placement had renamed into place before
    /// it set the mode: the retry finds it there and makes it read-only too.
    #[cfg(unix)]
    #[test]
    fn a_placed_blob_is_read_only_also_when_a_retry_reuses_it() -> Result<(), BlobError> {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir()?;
        let from = dir.path().join("from");
        fs::write(&from, b"reference table")?;
        set_mode(&from, 0o644)?;
        let sha = sha256_reader(&b"reference table"[..])?;
        let fresh = BlobStore::new(dir.path().join("fresh"))?;
        let mode = |p: &Path| fs::metadata(p).map(|m| m.permissions().mode() & 0o777);
        // the .part file is read-only before it takes the blob's name, so the blob is never there
        // writable, even for a moment
        let seen = std::rc::Rc::new(std::cell::Cell::new(None));
        let observed = seen.clone();
        BEFORE_PLACE_RENAME.with(|hook| {
            *hook.borrow_mut() = Some(Box::new(move |part: &Path| {
                observed.set(Some(fs::metadata(part).map(|m| m.permissions().mode() & 0o777).ok()));
            }));
        });
        assert!(fresh.place(&sha, &from)?);
        assert_eq!(
            seen.get(),
            Some(Some(0o440)),
            "the .part file was renamed before it was made read-only"
        );
        let target = plain_at(&dir.path().join("fresh"), &sha);
        assert_eq!(mode(&target)?, 0o440);
        // a placement stopped between its rename and its mode change left the source's mode
        set_mode(&target, 0o644)?;
        assert!(!fresh.place(&sha, &from)?);
        assert_eq!(mode(&target)?, 0o440, "the reused blob kept a writable mode");
        Ok(())
    }

    /// The plain layout's path of `sha`, for tests that plant a file there.
    fn plain_at(root: &Path, sha: &str) -> PathBuf {
        root.join("sha256").join(&sha[..2]).join(&sha[2..4]).join(sha)
    }

    #[test]
    fn a_move_to_a_keyed_profile_skips_only_when_it_holds_a_sealed_copy() -> Result<(), BlobError> {
        let dir = tempfile::tempdir()?;
        let store = sealing_store(dir.path())?;
        let (from, to) = (BlobOwner::Profile(Uuid::new_v4()), BlobOwner::Profile(Uuid::new_v4()));
        let data = b"Lucia Rivera, LabNorte panel";
        let h = store.put_bytes(from, data)?;
        // a legacy plain file of the same bytes is no copy under the new profile's key
        assert_eq!(store.put_bytes(BlobOwner::Reference, data)?, h);
        assert!(store.rekey(from, to, &h)?, "skipped on a plain copy");
        assert!(
            store.key_of(to, &h)?.contains('/'),
            "the new profile has no sealed copy"
        );
        assert!(!store.rekey(from, to, &h)?, "copied twice");
        // a store that seals nothing keeps skipping on the plain copy
        let plain = BlobStore::new(dir.path().join("plain"))?;
        let h = plain.put_bytes(from, data)?;
        assert!(!plain.rekey(from, to, &h)?);
        Ok(())
    }

    #[test]
    fn a_move_whose_source_has_other_bytes_lands_nothing() -> Result<(), BlobError> {
        let dir = tempfile::tempdir()?;
        let store = sealing_store(dir.path())?;
        let to = BlobOwner::Profile(Uuid::new_v4());
        let other = b"Carmen Ortega, a report another record names";
        // a plain copy of these bytes that another record holds
        let held = store.put_bytes(BlobOwner::Reference, other)?;
        // a file that claims another hash but holds those bytes
        let claimed = hex::encode(Sha256::digest(b"what the file should hold"));
        let at = plain_at(dir.path(), &claimed);
        fs::create_dir_all(at.parent().unwrap_or(dir.path()))?;
        fs::write(&at, other)?;
        assert!(matches!(
            store.rekey(BlobOwner::Reference, to, &claimed),
            Err(BlobError::Io(e)) if e.kind() == io::ErrorKind::InvalidData
        ));
        assert_eq!(
            store.get_bytes(BlobOwner::Reference, &held)?,
            other,
            "the held copy went"
        );
        // the plain copy still answers for `to`; a sealed one must not have landed
        assert!(files_under(&dir.path().join("enc")).is_empty(), "a copy landed");
        assert!(files_under(&dir.path().join("tmp")).is_empty(), "a staging file stayed");
        Ok(())
    }

    #[test]
    fn removing_a_copy_leaves_the_mode_of_its_backup_link() -> Result<(), BlobError> {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir()?;
        let store = sealing_store(&dir.path().join("live"))?;
        let p = BlobOwner::Profile(Uuid::new_v4());
        let sealed = store.put_bytes(p, b"Sam Rivera, visit notes")?;
        let plain = store.put_bytes(BlobOwner::Reference, b"reference table")?;
        let backup = dir.path().join("backup");
        store.copy_into(&backup)?;
        assert!(store.drop_sealed(p, &sealed)?);
        assert!(store.delete(BlobOwner::Reference, &plain)?);
        let links = files_under(&backup);
        assert_eq!(links.len(), 2);
        for f in links {
            assert_eq!(fs::metadata(&f)?.permissions().mode() & 0o777, 0o440, "{}", f.display());
        }
        Ok(())
    }

    #[test]
    fn a_misplaced_file_never_stands_in_for_the_one_its_name_says() -> Result<(), BlobError> {
        let dir = tempfile::tempdir()?;
        let store = sealing_store(dir.path())?;
        let p = Uuid::new_v4();
        let plain = store.put_bytes(BlobOwner::Reference, b"reference table")?;
        store.put_bytes(BlobOwner::Profile(p), b"Alex Rivera, lipid panel")?;
        let files = tree_files(dir.path())?;
        let sealed = files.keys().find(|k| k.contains('/')).cloned().unwrap_or_default();
        // the same names in a shard their hash does not name, sorted after the right ones
        let (pid, address) = sealed.split_once('/').unwrap_or_default();
        let stray_plain = dir.path().join("sha256/zz/zz").join(&plain);
        let stray_sealed = dir.path().join("enc").join(pid).join("zz/zz").join(address);
        for f in [&stray_plain, &stray_sealed] {
            fs::create_dir_all(f.parent().unwrap_or(dir.path()))?;
            fs::write(f, b"planted")?;
        }
        let files = tree_files(dir.path())?;
        assert_eq!(files[&plain], plain_at(dir.path(), &plain));
        assert_eq!(
            files[&sealed],
            files_under(&dir.path().join("enc").join(pid).join(&address[..2]))[0]
        );
        let misplaced: Vec<&String> = files.keys().filter(|k| k.starts_with("?/")).collect();
        assert_eq!(misplaced.len(), 2, "{files:?}");
        Ok(())
    }
}
