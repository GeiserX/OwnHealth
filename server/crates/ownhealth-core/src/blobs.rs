//! Content-addressed immutable blob store on disk. The plaintext sha256 of the bytes is a blob's
//! identity everywhere (document rows, provenance, the export manifest, state_hash); where the bytes
//! sit is this module's business.
//!
//! Two layouts, read side by side:
//!
//! - plain: `blobs/sha256/ab/cd/<sha256 hex>`, the layout of `ownhealth/blobs.py`, so a 0.1.1 data
//!   directory is read as it is. Pending items and reference data stay here.
//! - sealed (slice 46): a profile's own files, `blobs/enc/<profile id>/ab/cd/<address>`, where the
//!   address is an HMAC of the sha256 under the profile's key and the file is encrypted under it
//!   (`keys.rs`). A store with a keyring writes every profile-owned blob this way.
//!
//! A read for a profile tries its sealed copy first, then the plain one, so an install that upgrades
//! keeps every older file. The same bytes stored twice for one owner are kept once; two profiles that
//! store the same bytes each get their own sealed copy. A blob is never modified after it lands.
//! Access control lives on the rows that reference a hash (document, genome_dataset); a hash on its
//! own grants nothing.

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use sha2::{Digest, Sha256};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncSeekExt, AsyncWriteExt, ReadBuf};
use uuid::Uuid;

use crate::keys::{Dek, HEADER, Kek, KeyError, Keyring, RECORD, SealedReader, Sealer};

const CHUNK: usize = 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum BlobError {
    #[error("blob not found: {0}")]
    NotFound(String),
    #[error("not a sha256 hex digest")]
    BadHash,
    #[error("not a plain file name: {0:?}")]
    BadName(String),
    #[error(transparent)]
    Key(#[from] KeyError),
    #[error(transparent)]
    Io(#[from] io::Error),
}

impl From<BlobError> for crate::Error {
    fn from(e: BlobError) -> Self {
        match e {
            BlobError::NotFound(h) => crate::Error::NotFound(format!("blob {h}")),
            BlobError::BadHash => crate::Error::Invalid("not a sha256 hex digest".into()),
            BlobError::BadName(n) => crate::Error::Invalid(format!("not a plain file name: {n:?}")),
            BlobError::Key(k) => k.into(),
            BlobError::Io(e) => crate::Error::Io(e),
        }
    }
}

/// Whose bytes a read or a write is for. Every call names it, so the store can pick the key and the
/// S3 backend the prefix without changing a caller. Only `Profile` bytes are sealed; the other owners
/// use the plain layout. Access control stays on the rows that reference a hash: an owner here grants
/// nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlobOwner {
    /// A profile's own data.
    Profile(Uuid),
    /// Bytes that arrived but have no profile yet, keyed by the item that holds them.
    Pending(Uuid),
    /// Reference data nobody owns, and bytes whose owner is not known yet (a mail before routing).
    Reference,
}

impl BlobOwner {
    /// The owner of a document's bytes: its profile, or the document itself while it has none.
    pub fn of_document(profile_id: Option<Uuid>, document_id: Uuid) -> BlobOwner {
        profile_id.map_or(BlobOwner::Pending(document_id), BlobOwner::Profile)
    }
}

/// A blob as a file on local disk, for readers that need a path (SQLite, zip). The path is good
/// while this value lives, so keep it for as long as any reader opened from the path is in use. In
/// the plain layout it is the stored file itself; a sealed blob is decrypted into a file of the
/// store's tmp folder, readable by this user only, removed when this drops (`sweep_tmp` removes one
/// a killed process left).
#[derive(Debug)]
#[must_use = "the path is good only while the LocalCopy lives"]
pub struct LocalCopy {
    path: PathBuf,
    temporary: bool,
}

impl LocalCopy {
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for LocalCopy {
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

fn not_found(sha256: &str) -> BlobError {
    BlobError::NotFound(sha256.to_ascii_lowercase())
}

/// Where a read finds a blob.
enum Found {
    Plain(PathBuf),
    Sealed {
        path: PathBuf,
        dek: Arc<Dek>,
        profile: Uuid,
        sha: [u8; 32],
    },
}

impl BlobStore {
    /// Opens (and creates) a store at `root` that writes the plain layout for every owner: tests and
    /// tools. The app builds its store with [`BlobStore::with_keys`].
    pub fn new(root: impl Into<PathBuf>) -> io::Result<BlobStore> {
        let root = root.into();
        fs::create_dir_all(root.join("tmp"))?;
        fs::create_dir_all(root.join("sha256"))?;
        Ok(BlobStore { root, keys: None })
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
                Arc::new(Keyring::new(Kek::from_settings(settings)?, settings.sqlx_url())),
            )?,
            None => BlobStore::new(settings.blobs_path())?,
        })
    }

    /// Whether profile blobs are written sealed.
    pub fn seals(&self) -> bool {
        self.keys.is_some()
    }

    fn path(&self, sha256: &str) -> Result<PathBuf, BlobError> {
        let h = sha256_hex(sha256)?;
        Ok(self.root.join("sha256").join(&h[0..2]).join(&h[2..4]).join(&h))
    }

    fn profile_dir(&self, profile: Uuid) -> PathBuf {
        self.root.join("enc").join(profile.simple().to_string())
    }

    fn sealed_path(&self, profile: Uuid, address: &str) -> PathBuf {
        self.profile_dir(profile)
            .join(&address[0..2])
            .join(&address[2..4])
            .join(address)
    }

    /// The key a write for `owner` seals with: a profile's, when this store seals.
    fn sealing(&self, owner: BlobOwner) -> Result<Option<(Uuid, Arc<Dek>)>, BlobError> {
        match (owner, &self.keys) {
            (BlobOwner::Profile(p), Some(keys)) => Ok(keys.dek(p, true)?.map(|d| (p, d))),
            _ => Ok(None),
        }
    }

    /// Where `owner`'s blob is: the profile's sealed copy first, then the plain one. A profile with
    /// no sealed folder has never had a sealed blob, so no key is looked up for it.
    fn find(&self, owner: BlobOwner, sha256: &str) -> Result<Option<Found>, BlobError> {
        let plain = self.path(sha256)?;
        if let (BlobOwner::Profile(p), Some(keys)) = (owner, &self.keys)
            && self.profile_dir(p).is_dir()
            && let Some(dek) = keys.dek(p, false)?
        {
            let sha = sha_array(sha256)?;
            let path = self.sealed_path(p, &dek.address(&sha));
            if path.is_file() {
                return Ok(Some(Found::Sealed {
                    path,
                    dek,
                    profile: p,
                    sha,
                }));
            }
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

    fn land_sealed(&self, tmp: &Path, profile: Uuid, dek: &Dek, sha: &[u8; 32]) -> Result<(), BlobError> {
        BlobStore::land_at(tmp, &self.sealed_path(profile, &dek.address(sha)))
    }

    /// Stream into the store for `owner`. Returns (sha256 hex of the plaintext, size). Same bytes
    /// twice for one owner: one file.
    pub fn put_reader(&self, owner: BlobOwner, mut reader: impl Read) -> Result<(String, u64), BlobError> {
        let sealing = self.sealing(owner)?;
        let tmp = self.root.join("tmp").join(tmp_name());
        let result = (|| {
            let mut out = File::create(&tmp)?;
            let mut buf = vec![0u8; CHUNK];
            let (h, size) = if let Some((profile, dek)) = &sealing {
                let mut s = Sealing::new(dek, *profile);
                out.write_all(&[0u8; HEADER])?;
                loop {
                    let n = reader.read(&mut buf)?;
                    if n == 0 {
                        break;
                    }
                    out.write_all(&s.feed(&buf[..n])?)?;
                }
                let (last, header, sha, size) = s.finish()?;
                out.write_all(&last)?;
                out.seek(SeekFrom::Start(0))?;
                out.write_all(&header)?;
                out.flush()?;
                out.sync_all()?;
                self.land_sealed(&tmp, *profile, dek, &sha)?;
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
            let mut out = tokio::fs::File::create(&tmp).await?;
            let mut buf = vec![0u8; CHUNK];
            if let Some((profile, dek)) = &sealing {
                let mut s = Sealing::new(dek, *profile);
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
                self.land_sealed(&tmp, *profile, dek, &sha)?;
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
                io::ErrorKind::NotFound => not_found(sha256),
                _ => BlobError::Io(e),
            })
        };
        match self.find(owner, sha256)? {
            None => Err(not_found(sha256)),
            Some(Found::Plain(p)) => Ok(BlobReader::Plain(open(&p)?)),
            Some(Found::Sealed {
                path,
                dek,
                profile,
                sha,
            }) => Ok(BlobReader::Sealed(Box::new(SealedReader::open(
                open(&path)?,
                &dek,
                profile,
                &sha,
            )?))),
        }
    }

    /// The bytes of `owner`'s blob as an async reader, with their size, for streaming a response.
    pub async fn open_async(
        &self,
        owner: BlobOwner,
        sha256: &str,
    ) -> Result<(impl AsyncRead + Send + Unpin + 'static, u64), BlobError> {
        let (store, sha) = (self.clone(), sha256.to_string());
        let reader = tokio::task::spawn_blocking(move || store.open_seek(owner, &sha))
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
                Box::new(SealedStream {
                    rx,
                    buf: Vec::new(),
                    at: 0,
                })
            }
        };
        Ok((stream, size))
    }

    /// `owner`'s blob as a file on local disk, for readers that need a path. Prefer `open_seek`: a
    /// sealed blob is decrypted whole into a temporary file here.
    pub fn with_local_copy(&self, owner: BlobOwner, sha256: &str) -> Result<LocalCopy, BlobError> {
        match self.find(owner, sha256)? {
            None => Err(not_found(sha256)),
            Some(Found::Plain(path)) => Ok(LocalCopy { path, temporary: false }),
            Some(Found::Sealed { .. }) => {
                let mut reader = self.open_seek(owner, sha256)?;
                let copy = LocalCopy {
                    path: self.root.join("tmp").join(format!("copy-{}", tmp_name())),
                    temporary: true,
                };
                let mut out = create_private(copy.path())?;
                io::copy(&mut reader, &mut out)?;
                out.flush()?;
                Ok(copy)
            }
        }
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

    /// Store `from`'s blob for `to` as well, for a document that moved to another profile: read under
    /// `from`, written under `to`, the bytes checked against their sha256. The `from` copy stays (its
    /// profile's delete removes it). True when a copy was made; false when `to` already reads it.
    pub fn rekey(&self, from: BlobOwner, to: BlobOwner, sha256: &str) -> Result<bool, BlobError> {
        if from == to || self.find(to, sha256)?.is_some() {
            return Ok(false);
        }
        let (h, _) = self.put_reader(to, self.open_seek(from, sha256)?)?;
        if h != sha256_hex(sha256)? {
            let _ = self.delete(to, &h);
            return Err(BlobError::Io(io::Error::new(
                io::ErrorKind::InvalidData,
                "a blob read back with other bytes than its sha256",
            )));
        }
        Ok(true)
    }

    /// `owner`'s sealed copy and the plain copy of `sha256`. Only for a hard purge or the GC job,
    /// after checking no row references the hash. True when a file went.
    pub fn delete(&self, owner: BlobOwner, sha256: &str) -> Result<bool, BlobError> {
        let mut gone = self.drop_sealed(owner, sha256)?;
        let p = self.path(sha256)?;
        if p.exists() {
            set_mode(&p, 0o640)?;
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
        set_mode(&path, 0o640)?;
        fs::remove_file(path)?;
        Ok(true)
    }

    /// After a profile delete: every sealed blob of `profile` and the key this process kept for it.
    /// The key row went with the profile, so a sealed file left behind no longer opens. True when the
    /// folder was there.
    pub fn forget_profile(&self, profile: Uuid) -> io::Result<bool> {
        if let Some(keys) = &self.keys {
            keys.forget(profile);
        }
        let dir = self.profile_dir(profile);
        if !dir.exists() {
            return Ok(false);
        }
        fs::remove_dir_all(dir)?;
        Ok(true)
    }

    /// Remove upload spools a killed process left in `root/tmp`: regular files named `upload-*`
    /// (the import route), `push-*` (the push inboxes), `copy-*` (a decrypted local copy) or 32 hex
    /// characters (`put_reader`) last modified more than `older_than` ago. The age bound leaves
    /// another process's live upload alone. Returns how many went.
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
            let spool = name.starts_with("upload-") || name.starts_with("push-") || name.starts_with("copy-");
            let ours = spool || (name.len() == 32 && name.bytes().all(|c| c.is_ascii_hexdigit()));
            let meta = entry.metadata()?;
            if !ours || !meta.is_file() {
                continue;
            }
            let age = meta.modified().ok().and_then(|m| now.duration_since(m).ok());
            if age.is_some_and(|a| a > older_than) {
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
        for part in ["sha256", "enc"] {
            let src = self.root.join(part);
            if src.is_dir() {
                copy_tree(&src, &dest.join(part))?;
            }
        }
        Ok(())
    }

    /// The [`tree_files`] key of `owner`'s copy of `sha256`: `<profile>/<address>` for a sealed copy,
    /// else the sha256 itself (also when no copy is there, so a check reports it missing).
    pub fn key_of(&self, owner: BlobOwner, sha256: &str) -> Result<String, BlobError> {
        Ok(match self.find(owner, sha256)? {
            Some(Found::Sealed { profile, dek, sha, .. }) => format!("{}/{}", profile.simple(), dek.address(&sha)),
            _ => sha256_hex(sha256)?,
        })
    }

    /// Put the file `from` of a backup's blob tree where its `key` says, copied whole and then
    /// renamed, so a blob is never there half. A key that is not one ([`tree_files`]) is refused.
    /// False when the file is there already.
    pub fn place(&self, key: &str, from: &Path) -> Result<bool, BlobError> {
        let target = self
            .root
            .join(tree_rel(key).ok_or_else(|| BlobError::BadName(key.to_string()))?);
        if target.exists() {
            return Ok(false);
        }
        let parent = target.parent().unwrap_or(&self.root);
        fs::create_dir_all(parent)?;
        let part = parent.join(format!(".{}.part", key.replace('/', "-")));
        fs::copy(from, &part)?;
        fs::rename(&part, &target)?;
        set_mode(&target, 0o440)?;
        Ok(true)
    }
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

/// Where a [`tree_files`] key sits inside a blob tree; None for a string that is not a key.
fn tree_rel(key: &str) -> Option<PathBuf> {
    let hex = |s: &str, n: usize| s.len() == n && s.bytes().all(|c| matches!(c, b'0'..=b'9' | b'a'..=b'f'));
    match key.split_once('/') {
        None if hex(key, 64) => Some(Path::new("sha256").join(&key[..2]).join(&key[2..4]).join(key)),
        Some((p, a)) if hex(p, 32) && hex(a, 64) => Some(Path::new("enc").join(p).join(&a[..2]).join(&a[2..4]).join(a)),
        _ => None,
    }
}

/// Every blob file of the tree at `root` (a store, or a backup's `blobs/`), in path order, keyed the
/// way a backup manifest lists it: a plain blob by its file name (its sha256), a sealed one as
/// `<profile>/<file name>` (its address). A file elsewhere under `enc/` gets a key no manifest lists.
pub fn tree_files(root: &Path) -> io::Result<BTreeMap<String, PathBuf>> {
    let mut out = BTreeMap::new();
    let plain = root.join("sha256");
    let mut files = Vec::new();
    if plain.is_dir() {
        walk(&plain, &mut files)?;
    }
    files.sort();
    for p in files {
        if let Some(name) = p.file_name() {
            out.insert(name.to_string_lossy().into_owned(), p);
        }
    }
    let sealed = root.join("enc");
    let mut files = Vec::new();
    if sealed.is_dir() {
        walk(&sealed, &mut files)?;
    }
    files.sort();
    for p in files {
        let rel = p.strip_prefix(&sealed).unwrap_or(&p);
        let profile = rel
            .components()
            .next()
            .map(|c| c.as_os_str().to_string_lossy().into_owned());
        let name = p
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let key = match profile {
            Some(pid) if rel.components().count() > 1 => format!("{pid}/{name}"),
            _ => format!("?/{name}"),
        };
        out.insert(key, p);
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
            Err(BlobError::NotFound(_))
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
            Err(BlobError::NotFound(_))
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
            Err(BlobError::NotFound(_))
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
            ("upload-fresh".to_string(), false),
            ("notes.txt".to_string(), true),
        ] {
            let f = File::create(tmp.join(name))?;
            if old {
                f.set_modified(two_hours_ago)?;
            }
        }
        let one_hour = std::time::Duration::from_secs(3600);
        assert_eq!(store.sweep_tmp(one_hour)?, 4);
        let mut left = Vec::new();
        for e in fs::read_dir(&tmp)? {
            left.push(e?.file_name().to_string_lossy().into_owned());
        }
        left.sort();
        assert_eq!(left, ["notes.txt", "upload-fresh"]);
        assert_eq!(store.sweep_tmp(one_hour)?, 0);
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
        assert!(matches!(
            store.size(owner(), &"0".repeat(64)),
            Err(BlobError::NotFound(_))
        ));
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
            Err(BlobError::NotFound(_))
        ));
        assert!(matches!(store.path("../../etc/passwd"), Err(BlobError::BadHash)));
        let h = store.put_bytes(owner(), b"x").expect("put");
        assert!(store.delete(owner(), &h).expect("delete"));
        assert!(!store.exists(owner(), &h));
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
                matches!(store.get_bytes(other, &h), Err(BlobError::NotFound(_))),
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
    fn pending_and_reference_blobs_stay_plain() -> Result<(), BlobError> {
        let dir = tempfile::tempdir()?;
        let store = sealing_store(dir.path())?;
        let h = store.put_bytes(BlobOwner::Pending(Uuid::new_v4()), b"held LabNorte mail")?;
        let r = store.put_bytes(BlobOwner::Reference, b"ClinVar release")?;
        assert!(store.path(&h)?.is_file() && store.path(&r)?.is_file());
        assert!(!dir.path().join("enc").exists());
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
            Err(BlobError::NotFound(_))
        ));
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
        let mut bytes = fs::read(&file)?;
        let at = bytes.len() - 3;
        bytes[at] ^= 1;
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
        // a plain blob any owner reads needs no copy; a missing one is not found
        let plain = store.put_bytes(BlobOwner::Pending(Uuid::new_v4()), b"held scan")?;
        assert!(!store.rekey(BlobOwner::Pending(Uuid::nil()), to, &plain)?);
        assert!(matches!(
            store.rekey(from, to, &"0".repeat(64)),
            Err(BlobError::NotFound(_))
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
        let mut want = vec![key.clone(), plain.clone()];
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
        let climb = format!("{}/../{}", p.simple(), "0".repeat(61));
        for bad in ["../x", "ab/cd", climb.as_str(), sealed.to_uppercase().as_str()] {
            assert!(
                matches!(fresh.place(bad, &files[&plain]), Err(BlobError::BadName(_))),
                "{bad}"
            );
        }
        fs::write(backup.join("enc").join("stray"), b"x")?;
        assert!(tree_files(&backup)?.contains_key("?/stray"));
        Ok(())
    }
}
