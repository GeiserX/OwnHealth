//! What `POST /api/imaging/import` keeps: one zip per upload, stored for the profile before anything
//! reads it, and its `imaging_import` row and job.

use std::collections::BTreeSet;
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
    let mut zip = zip::ZipWriter::new(std::fs::File::create(out)?);
    let mut seen = BTreeSet::new();
    for (name, path) in files {
        let base = entry_name(name);
        let mut name = base.clone();
        let mut n = 1;
        while !seen.insert(name.clone()) {
            n += 1;
            name = format!("{base}~{n}");
        }
        let mut file = std::fs::File::open(path)?;
        let size = file.metadata()?.len();
        let opts = SimpleFileOptions::default()
            .compression_method(CompressionMethod::Stored)
            .large_file(size >= u64::from(u32::MAX));
        zip.start_file(name, opts).map_err(io::Error::other)?;
        io::copy(&mut file, &mut zip)?;
    }
    zip.finish().map_err(io::Error::other)?;
    Ok(())
}

/// How many files the zip at `path` holds (folders left out), or why it does not read as a zip.
pub fn zip_files(path: &Path) -> std::result::Result<usize, String> {
    let file = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let mut zip = zip::ZipArchive::new(file).map_err(|e| e.to_string())?;
    let mut n = 0;
    for i in 0..zip.len() {
        if !zip.by_index_raw(i).map_err(|e| e.to_string())?.is_dir() {
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
}
