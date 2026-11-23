//! What `POST /api/imaging/import` keeps: one zip per upload, stored for the profile before anything
//! reads it, and its `imaging_import` row and job.

use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::path::{Path, PathBuf};

use ownhealth_core::Result;
use ownhealth_core::blobs::sha256_hex_to_bytes;
use ownhealth_core::documents::lock_blob;
use ownhealth_core::jobs::{EnqueueOptions, enqueue};
use serde_json::json;
use sqlx::PgConnection;
use uuid::Uuid;
use zip::CompressionMethod;
use zip::write::SimpleFileOptions;

use crate::import::JOB;

/// A CD folder upload holds at most this many files (a zip upload is one file).
pub const MAX_FOLDER_FILES: usize = 20_000;

/// The name of a spool the upload route writes in the blob store's `tmp` (a folder part, or the zip
/// it makes of them): plaintext DICOM until the store takes it.
/// It starts `upload-`, so the start-up and periodic sweeps (`BlobStore::sweep_tmp`) remove one a
/// killed upload left.
pub fn spool_name(zip: bool) -> String {
    let name = format!("upload-imaging-{}", ownhealth_core::ids::token_hex(16));
    if zip { format!("{name}.zip") } else { name }
}

/// The path a folder part keeps inside the zip: its relative path as the browser sent it, with `/`
/// between parts. A `..` takes back the part before it and is dropped at the top; empty and `.` parts
/// and drive letters are dropped. So nothing names a place outside the folder. A name with nothing
/// left is `file`.
pub fn entry_name(name: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for p in name.split(['/', '\\']) {
        match p {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            _ if p.len() == 2 && p.ends_with(':') => {}
            _ => parts.push(p),
        }
    }
    if parts.is_empty() {
        "file".into()
    } else {
        parts.join("/")
    }
}

/// Writes the folder's files into one zip at `out` (blocking). The files are stored, not compressed:
/// DICOM images mostly come compressed already, and a CD of thousands of files zips in seconds. Two
/// parts with one name keep both (`name~2`).
pub fn zip_folder(files: &[(String, PathBuf)], out: &Path) -> io::Result<()> {
    let dir = out
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    // A private, create-new spool. Publish the path only after the archive is complete.
    let mut spool = tempfile::Builder::new().prefix("upload-imaging-").tempfile_in(dir)?;
    let mut zip = zip::ZipWriter::new(spool.as_file_mut());
    let mut seen = BTreeSet::new();
    let mut next_suffix = BTreeMap::new();
    for (name, path) in files {
        let base = entry_name(name);
        let name = allocate_name(base, &mut next_suffix, |name| seen.insert(name));
        let mut file = std::fs::File::open(path)?;
        let size = file.metadata()?.len();
        let opts = SimpleFileOptions::default()
            .compression_method(CompressionMethod::Stored)
            .large_file(size >= u64::from(u32::MAX));
        zip.start_file(name, opts).map_err(io::Error::other)?;
        io::copy(&mut file, &mut zip)?;
    }
    zip.finish().map_err(io::Error::other)?;
    spool.persist_noclobber(out)?;
    Ok(())
}

fn allocate_name(
    base: String,
    next_suffix: &mut BTreeMap<String, usize>,
    mut claim: impl FnMut(String) -> bool,
) -> String {
    let n = next_suffix.entry(base.clone()).or_insert(1);
    loop {
        let name = if *n == 1 { base.clone() } else { format!("{base}~{n}") };
        *n += 1;
        if claim(name.clone()) {
            return name;
        }
    }
}

/// Why an uploaded zip is refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ZipRefused {
    /// It does not read as a zip.
    Invalid(String),
    /// It holds more entries than the import reads ([`crate::import::MAX_ENTRIES`]).
    TooMany(usize),
}

/// How many files the zip at `path` holds (folders left out), or why the import would refuse it.
pub fn zip_files(path: &Path) -> std::result::Result<usize, ZipRefused> {
    let invalid = |e: &dyn std::fmt::Display| ZipRefused::Invalid(e.to_string());
    let file = std::fs::File::open(path).map_err(|e| invalid(&e))?;
    let mut zip = zip::ZipArchive::new(file).map_err(|e| invalid(&e))?;
    if zip.len() > crate::import::MAX_ENTRIES {
        return Err(ZipRefused::TooMany(zip.len()));
    }
    let mut n = 0;
    for i in 0..zip.len() {
        if !zip.by_index_raw(i).map_err(|e| invalid(&e))?.is_dir() {
            n += 1;
        }
    }
    Ok(n)
}

/// An upload on the record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recorded {
    pub import_id: Uuid,
    /// The profile had this zip already.
    pub duplicate: bool,
    /// None when the import of this zip is queued or running already.
    pub job_id: Option<i64>,
}

/// Records a stored zip as an `imaging_import` of the profile and queues its import, in the caller's
/// transaction. The same zip again is the same import; its job runs again and finds every study it
/// recorded already, so it sends nothing.
pub async fn record(
    conn: &mut PgConnection,
    profile_id: Uuid,
    sha256: &str,
    size: i64,
    filename: Option<&str>,
    files: usize,
    user: Option<Uuid>,
) -> Result<Recorded> {
    // the lock an upload of these bytes takes (documents::create_document): a profile's copy of them is
    // never removed while this upload names them
    lock_blob(&mut *conn, profile_id, sha256, true).await?;
    let sha = sha256_hex_to_bytes(sha256)?;
    let filename: Option<String> = filename.map(|f| f.chars().take(255).collect());
    let new: Option<Uuid> = sqlx::query_scalar(
        "insert into imaging_import (profile_id, zip_blob, filename, size_bytes, files, created_by) \
         values ($1, $2, $3, $4, $5, $6) on conflict (profile_id, zip_blob) do nothing returning id",
    )
    .bind(profile_id)
    .bind(&sha)
    .bind(&filename)
    .bind(size)
    .bind(i32::try_from(files).unwrap_or(i32::MAX))
    .bind(user)
    .fetch_optional(&mut *conn)
    .await?;
    let (import_id, duplicate) = match new {
        Some(id) => (id, false),
        None => (
            sqlx::query_scalar("select id from imaging_import where profile_id = $1 and zip_blob = $2")
                .bind(profile_id)
                .bind(&sha)
                .fetch_one(&mut *conn)
                .await?,
            true,
        ),
    };
    let payload = json!({
        "import_id": import_id.to_string(),
        "profile_id": profile_id.to_string(),
        "filename": filename,
    });
    let key = format!("{JOB}:{import_id}");
    let job_id = enqueue(&mut *conn, JOB, &payload, EnqueueOptions::dedupe(&key)).await?;
    Ok(Recorded {
        import_id,
        duplicate,
        job_id,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_folder_part_keeps_a_relative_path_inside_the_folder() {
        assert_eq!(entry_name("CD/DICOM/IM0001"), "CD/DICOM/IM0001");
        assert_eq!(entry_name("C:\\CD\\DICOM\\IM0001"), "CD/DICOM/IM0001");
        assert_eq!(entry_name("../../etc/passwd"), "etc/passwd");
        assert_eq!(entry_name("/abs/./x"), "abs/x");
        assert_eq!(entry_name("../.."), "file");
        assert_eq!(entry_name(""), "file");
    }

    #[test]
    fn a_spool_a_killed_upload_left_is_swept() -> io::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = ownhealth_core::blobs::BlobStore::new(dir.path())?;
        let two_hours_ago = std::time::SystemTime::now() - std::time::Duration::from_secs(7200);
        for zip in [false, true] {
            let path = store.staging_path(&spool_name(zip)).map_err(io::Error::other)?;
            std::fs::create_dir_all(path.parent().ok_or_else(|| io::Error::other("no tmp"))?)?;
            std::fs::File::create(&path)?.set_modified(two_hours_ago)?;
        }
        assert_eq!(store.sweep_tmp(std::time::Duration::from_secs(3600))?, 2);
        Ok(())
    }

    #[test]
    fn two_parts_with_one_name_keep_both() -> io::Result<()> {
        let dir = tempfile::tempdir()?;
        let (a, b) = (dir.path().join("a"), dir.path().join("b"));
        std::fs::write(&a, b"first")?;
        std::fs::write(&b, b"second")?;
        let out = dir.path().join("cd.zip");
        zip_folder(&[("CD/IM1".into(), a), ("CD/../CD/IM1".into(), b)], &out)?;
        let mut zip = zip::ZipArchive::new(std::fs::File::open(&out)?).map_err(io::Error::other)?;
        let names: Vec<String> = zip.file_names().filter_map(|n| n.ok()).map(|n| n.to_string()).collect();
        assert_eq!(names, ["CD/IM1", "CD/IM1~2"]);
        let mut second = String::new();
        io::Read::read_to_string(&mut zip.by_name("CD/IM1~2").map_err(io::Error::other)?, &mut second)?;
        assert_eq!(second, "second");
        assert_eq!(zip_files(&out), Ok(2));
        Ok(())
    }

    #[test]
    fn a_failed_zip_write_leaves_no_plaintext_archive() -> io::Result<()> {
        let dir = tempfile::tempdir()?;
        let input = dir.path().join("input");
        let missing = dir.path().join("missing");
        let out = dir.path().join("cd.zip");
        std::fs::write(&input, b"synthetic file")?;
        assert!(zip_folder(&[("IM1".into(), input), ("IM2".into(), missing)], &out).is_err());
        assert!(!out.exists());
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn the_generated_zip_is_private() -> io::Result<()> {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir()?;
        let out = dir.path().join("cd.zip");
        zip_folder(&[], &out)?;
        assert_eq!(std::fs::metadata(&out)?.permissions().mode() & 0o777, 0o600);
        Ok(())
    }

    #[test]
    fn the_generated_zip_does_not_overwrite_a_file() -> io::Result<()> {
        let dir = tempfile::tempdir()?;
        let out = dir.path().join("cd.zip");
        std::fs::write(&out, b"existing file")?;
        let result = zip_folder(&[], &out);
        assert!(
            matches!(result, Err(ref e) if e.kind() == io::ErrorKind::AlreadyExists),
            "{result:?}"
        );
        assert_eq!(std::fs::read(&out)?, b"existing file");
        Ok(())
    }

    #[test]
    fn duplicate_name_allocation_checks_linear_candidates() {
        let mut seen = BTreeSet::new();
        let mut attempts = 0;
        let mut next_suffix = BTreeMap::new();
        for _ in 0..256 {
            allocate_name("IM".into(), &mut next_suffix, |name| {
                attempts += 1;
                seen.insert(name)
            });
        }
        assert_eq!(seen.len(), 256);
        assert!(attempts <= 512, "checked {attempts} names for 256 parts");
    }

    #[test]
    fn duplicate_names_skip_occupied_suffixes() -> io::Result<()> {
        let dir = tempfile::tempdir()?;
        let input = dir.path().join("input");
        let out = dir.path().join("cd.zip");
        std::fs::write(&input, b"one")?;
        let parts = ["IM", "IM~2", "IM", "IM", "IM~3", "IM"].map(|name| (name.to_string(), input.clone()));
        zip_folder(&parts, &out)?;
        let zip = zip::ZipArchive::new(std::fs::File::open(&out)?).map_err(io::Error::other)?;
        let names: Vec<_> = zip
            .file_names()
            .collect::<std::result::Result<_, _>>()
            .map_err(io::Error::other)?;
        assert_eq!(names, ["IM", "IM~2", "IM~3", "IM~4", "IM~3~2", "IM~5"]);
        Ok(())
    }
}
