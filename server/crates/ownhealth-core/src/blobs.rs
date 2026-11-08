//! Content-addressed immutable blob store on disk: `blobs/sha256/ab/cd/<full sha256 hex>`, the same
//! layout as `ownhealth/blobs.py`, so a 0.1.1 data directory is read as it is.
//!
//! The key is the hash of the bytes, so the same file stored twice is kept once. A blob is never
//! modified after it lands. Access control lives on the rows that reference a hash (document,
//! genome_dataset); a hash on its own grants nothing.

use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};

const CHUNK: usize = 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum BlobError {
    #[error("blob not found: {0}")]
    NotFound(String),
    #[error("not a sha256 hex digest")]
    BadHash,
    #[error(transparent)]
    Io(#[from] io::Error),
}

impl From<BlobError> for crate::Error {
    fn from(e: BlobError) -> Self {
        match e {
            BlobError::NotFound(h) => crate::Error::NotFound(format!("blob {h}")),
            BlobError::BadHash => crate::Error::Invalid("not a sha256 hex digest".into()),
            BlobError::Io(e) => crate::Error::Io(e),
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

#[derive(Debug, Clone)]
pub struct BlobStore {
    root: PathBuf,
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

impl BlobStore {
    /// Opens (and creates) the store at `root`.
    pub fn new(root: impl Into<PathBuf>) -> io::Result<BlobStore> {
        let root = root.into();
        fs::create_dir_all(root.join("tmp"))?;
        fs::create_dir_all(root.join("sha256"))?;
        Ok(BlobStore { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn path(&self, sha256: &str) -> Result<PathBuf, BlobError> {
        let h = sha256_hex(sha256)?;
        Ok(self.root.join("sha256").join(&h[0..2]).join(&h[2..4]).join(&h))
    }

    /// The path of a hash given as the 32 bytes of a `bytea` column.
    pub fn path_of_bytes(&self, sha256: &[u8]) -> Result<PathBuf, BlobError> {
        self.path(&hex::encode(sha256))
    }

    fn land(&self, tmp: &Path, h: &str) -> Result<(), BlobError> {
        let target = self.path(h)?;
        if target.exists() {
            fs::remove_file(tmp)?;
        } else {
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::rename(tmp, &target)?;
            set_mode(&target, 0o440)?;
        }
        Ok(())
    }

    /// Stream into the store. Returns (sha256 hex, size). Same bytes twice: one file.
    pub fn put_reader(&self, mut reader: impl Read) -> Result<(String, u64), BlobError> {
        let tmp = self.root.join("tmp").join(tmp_name());
        let result = (|| {
            let mut out = File::create(&tmp)?;
            let mut digest = Sha256::new();
            let mut size = 0u64;
            let mut buf = vec![0u8; CHUNK];
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
            Ok((h, size))
        })();
        if result.is_err() {
            let _ = fs::remove_file(&tmp);
        }
        result
    }

    /// The async twin of `put_reader`, for request bodies.
    pub async fn put_async(&self, mut reader: impl AsyncRead + Unpin) -> Result<(String, u64), BlobError> {
        let tmp = self.root.join("tmp").join(tmp_name());
        let result = async {
            let mut out = tokio::fs::File::create(&tmp).await?;
            let mut digest = Sha256::new();
            let mut size = 0u64;
            let mut buf = vec![0u8; CHUNK];
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

    pub fn put_bytes(&self, data: &[u8]) -> Result<String, BlobError> {
        Ok(self.put_reader(data)?.0)
    }

    pub fn put_file(&self, path: &Path) -> Result<(String, u64), BlobError> {
        self.put_reader(File::open(path)?)
    }

    pub fn exists(&self, sha256: &str) -> bool {
        self.path(sha256).map(|p| p.exists()).unwrap_or(false)
    }

    pub fn open(&self, sha256: &str) -> Result<File, BlobError> {
        let p = self.path(sha256)?;
        File::open(&p).map_err(|e| match e.kind() {
            io::ErrorKind::NotFound => BlobError::NotFound(sha256.to_ascii_lowercase()),
            _ => BlobError::Io(e),
        })
    }

    pub fn get_bytes(&self, sha256: &str) -> Result<Vec<u8>, BlobError> {
        let mut out = Vec::new();
        self.open(sha256)?.read_to_end(&mut out)?;
        Ok(out)
    }

    pub fn size(&self, sha256: &str) -> Result<u64, BlobError> {
        Ok(self.open(sha256)?.metadata()?.len())
    }

    /// Only for a hard purge or the GC job, after checking no row references the hash.
    pub fn delete(&self, sha256: &str) -> Result<bool, BlobError> {
        let p = self.path(sha256)?;
        if !p.exists() {
            return Ok(false);
        }
        set_mode(&p, 0o640)?;
        fs::remove_file(p)?;
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn put_dedupes_and_uses_sharded_path() {
        let dir = tempfile::tempdir().expect("tmp");
        let store = BlobStore::new(dir.path()).expect("store");
        let data = b"synthetic lab report";
        let h1 = store.put_bytes(data).expect("put");
        let (h2, size) = store.put_reader(&data[..]).expect("put");
        assert_eq!(h1, h2);
        assert_eq!(h1, hex::encode(Sha256::digest(data)));
        assert_eq!(size, data.len() as u64);
        let p = store.path(&h1).expect("path");
        assert_eq!(p, dir.path().join("sha256").join(&h1[..2]).join(&h1[2..4]).join(&h1));
        assert_eq!(store.get_bytes(&h1.to_uppercase()).expect("get"), data);
        assert_eq!(fs::read_dir(dir.path().join("tmp")).expect("tmp").count(), 0);
        let raw = sha256_hex_to_bytes(&h1).expect("bytes");
        assert_eq!(store.path_of_bytes(&raw).expect("path"), p);
    }

    #[test]
    fn missing_and_bad_hashes() {
        let dir = tempfile::tempdir().expect("tmp");
        let store = BlobStore::new(dir.path()).expect("store");
        assert!(matches!(store.get_bytes(&"0".repeat(64)), Err(BlobError::NotFound(_))));
        assert!(matches!(store.path("../../etc/passwd"), Err(BlobError::BadHash)));
        let h = store.put_bytes(b"x").expect("put");
        assert!(store.delete(&h).expect("delete"));
        assert!(!store.exists(&h));
    }

    #[tokio::test]
    async fn async_put_matches_sync_put() {
        let dir = tempfile::tempdir().expect("tmp");
        let store = BlobStore::new(dir.path()).expect("store");
        let (h, n) = store.put_async(&b"LabNorte"[..]).await.expect("put");
        assert_eq!(h, store.put_bytes(b"LabNorte").expect("put"));
        assert_eq!(n, 8);
    }
}
