//! v1.0 imaging: DICOM studies from hospital CDs and zips (slice 41).
//!
//! ```text
//! POST /api/imaging/import   multipart: profile, files (one zip, or the files of a CD folder)
//! ```
//!
//! One part that is a zip is kept as it is. Anything else is a folder: its parts (at most
//! [`MAX_FOLDER_FILES`], each named by its path inside the folder) are written into one zip. The zip
//! is stored for the profile before anything reads it and recorded as an `imaging_import`; the
//! `import.dicom` job (ownhealth-imaging) then sends its studies to Orthanc and records each one. The
//! answer is `{import_id, duplicate, job_id, files}`; `GET /api/jobs?ids=` follows the job. The same
//! zip again is the same import, and its job sends nothing new. It needs the `import` action on the
//! profile and imaging on (503 `imaging_off`).

use std::path::PathBuf;

use axum::extract::{DefaultBodyLimit, FromRequest, Multipart, Request, State};
use axum::http::StatusCode;
use axum::routing::post;
use axum::{Json, Router};
use ownhealth_core::audit::{AuditEvent, write_audit};
use ownhealth_core::blobs::BlobOwner;
use ownhealth_core::documents::is_zip;
use ownhealth_core::{Error, Runtime, ids};
use ownhealth_imaging::upload::{self, MAX_FOLDER_FILES, zip_files, zip_folder};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::Caller;
use crate::error::ApiError;
use crate::routes::auth::{Fail, R};
use crate::validate::Errors;

/// The audit action of an upload.
pub const AUDIT_ACTION: &str = "imaging.import";
/// A form holds at most this many parts that are not files, each of at most 1 MiB.
const MAX_FIELDS: usize = 1000;
const MAX_FIELD: usize = 1024 * 1024;

pub fn router() -> Router<Runtime> {
    // a CD runs to hundreds of megabytes: no body cap, as /api/imports
    Router::new().route("/imaging/import", post(import).layer(DefaultBodyLimit::disable()))
}

/// A spooled part, removed from disk when dropped.
struct Spool {
    name: String,
    path: PathBuf,
}

impl Drop for Spool {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

fn bad_body() -> Fail {
    Fail::Api(ApiError::new(
        StatusCode::BAD_REQUEST,
        "bad_request",
        "There was an error parsing the body",
    ))
}

fn too_many_files() -> Fail {
    Fail::Api(ApiError::new(
        StatusCode::PAYLOAD_TOO_LARGE,
        "too_many_files",
        format!("A folder upload holds at most {MAX_FOLDER_FILES} files. Zip the disc and upload the zip."),
    ))
}

async fn read_field(field: &mut axum::extract::multipart::Field<'_>) -> R<Vec<u8>> {
    let mut buf = Vec::new();
    while let Some(c) = field.chunk().await.map_err(|_| bad_body())? {
        if buf.len() + c.len() > MAX_FIELD {
            return Err(bad_body());
        }
        buf.extend_from_slice(&c);
    }
    Ok(buf)
}

async fn spool(field: &mut axum::extract::multipart::Field<'_>, path: &std::path::Path) -> R<()> {
    // plaintext until the store takes it: readable by this user only
    let mut out = ownhealth_core::blobs::create_spool(path).await.map_err(Error::from)?;
    while let Some(chunk) = field.chunk().await.map_err(|_| bad_body())? {
        out.write_all(&chunk).await.map_err(Error::from)?;
    }
    out.flush().await.map_err(Error::from)?;
    Ok(())
}

async fn import(State(rt): State<Runtime>, caller: Caller, req: Request) -> R<Json<Value>> {
    if rt.settings.orthanc.is_none() {
        return Err(Fail::Api(ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "imaging_off",
            "Imaging is off: it needs the imaging compose profile and OWNHEALTH_ORTHANC_URL.",
        )));
    }
    let principal = &caller.0;
    let mut profile: Option<String> = None;
    let mut files: Vec<Spool> = Vec::new();
    if let Ok(mut form) = Multipart::from_request(req, &rt).await {
        let mut fields = 0usize;
        while let Some(mut field) = form.next_field().await.map_err(|_| bad_body())? {
            let is_file = field.file_name().is_some();
            let field_name = field.name().map(str::to_string);
            match field_name.as_deref() {
                Some("files") if is_file => {
                    if files.len() >= MAX_FOLDER_FILES {
                        return Err(too_many_files());
                    }
                    let name = field.file_name().unwrap_or_default().to_string();
                    let path = rt
                        .blobs
                        .staging_path(&format!("imaging-{}", ids::token_hex(16)))
                        .map_err(Error::from)?;
                    // the Spool owns the file before it exists, so a failed read still removes it
                    files.push(Spool {
                        name,
                        path: path.clone(),
                    });
                    spool(&mut field, &path).await?;
                }
                _ if is_file => return Err(bad_body()),
                name => {
                    fields += 1;
                    if fields > MAX_FIELDS {
                        return Err(bad_body());
                    }
                    let is_profile = name == Some("profile");
                    let bytes = read_field(&mut field).await?;
                    if is_profile {
                        profile = Some(String::from_utf8(bytes).map_err(|_| bad_body())?);
                    }
                }
            }
        }
    }
    let mut e = Errors::new();
    if profile.is_none() {
        e.missing(vec![json!("body"), json!("profile")], Value::Null);
    }
    if files.is_empty() {
        e.missing(vec![json!("body"), json!("files")], Value::Null);
    }
    e.done()?;
    let profile = profile.unwrap_or_default();

    // A short check first: no connection or transaction is held while a CD of hundreds of megabytes
    // is zipped and stored.
    let profile_id = {
        let mut conn = rt.pool.acquire().await?;
        caller
            .require(&mut conn, &profile, None, Some("import"))
            .await?
            .profile
            .id
    };
    let one_zip = match files.as_slice() {
        [only] => {
            let mut head = [0u8; 4];
            let n = tokio::fs::File::open(&only.path)
                .await
                .map_err(Error::from)?
                .read(&mut head)
                .await
                .map_err(Error::from)?;
            is_zip(&head[..n])
        }
        _ => false,
    };
    // One zip is kept as it came; a folder becomes one zip of its files.
    let parts: Vec<(String, PathBuf)> = files.iter().map(|f| (f.name.clone(), f.path.clone())).collect();
    let folder = (!one_zip)
        .then(|| rt.blobs.staging_path(&format!("imaging-{}.zip", ids::token_hex(16))))
        .transpose()
        .map_err(Error::from)?
        .map(|path| Spool {
            name: String::new(),
            path,
        });
    let zip_path = folder
        .as_ref()
        .map_or_else(|| files[0].path.clone(), |f| f.path.clone());
    let filename = if one_zip {
        files[0].name.rsplit(['/', '\\']).next().unwrap_or_default().to_string()
    } else {
        folder_name(&files[0].name)
    };
    let (blobs, owner) = (rt.blobs.clone(), BlobOwner::Profile(profile_id));
    let (count, sha256, size) = tokio::task::spawn_blocking(move || -> Result<_, Fail> {
        let count = if one_zip {
            zip_files(&zip_path).map_err(|why| {
                Fail::Api(ApiError::new(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "invalid_zip",
                    format!("This zip does not open: {why}"),
                ))
            })?
        } else {
            zip_folder(&parts, &zip_path).map_err(Error::from)?;
            parts.len()
        };
        let (sha256, size) = blobs.put_file(owner, &zip_path).map_err(Error::from)?;
        Ok((count, sha256, size))
    })
    .await
    .map_err(|e| Error::Other(e.into()))??;
    drop(folder);
    drop(files);
    // The same check again in the transaction that records the upload.
    let mut tx = rt.pool.begin().await?;
    let perm = caller.require(&mut tx, &profile, None, Some("import")).await?;
    if perm.profile.id != profile_id {
        return Err(Fail::Api(ApiError::new(
            StatusCode::CONFLICT,
            "profile_changed",
            "The profile changed during the upload. Upload the disc again.",
        )));
    }
    let recorded = upload::record(
        &mut tx,
        perm.profile.id,
        &sha256,
        i64::try_from(size).unwrap_or(i64::MAX),
        Some(&filename).filter(|f| !f.is_empty()).map(String::as_str),
        count,
        principal.user_id,
    )
    .await?;
    let mut audit = AuditEvent::new(principal.actor_kind(), "ui", AUDIT_ACTION);
    audit.actor_id = principal.user_id;
    audit.tool = Some("POST /api/imaging/import".into());
    audit.profile_id = Some(perm.profile.id);
    audit.resource_type = Some("imaging_import".into());
    audit.resource_ids = Some(vec![recorded.import_id.to_string()]);
    write_audit(&mut tx, &audit).await?;
    tx.commit().await?;
    Ok(Json(json!({
        "import_id": recorded.import_id,
        "duplicate": recorded.duplicate,
        "job_id": recorded.job_id,
        "files": count,
    })))
}

/// A folder upload's name: the folder the first part sits in (`CD/DICOM/IM1` is `CD.zip`).
fn folder_name(first: &str) -> String {
    let parts: Vec<&str> = first
        .split(['/', '\\'])
        .filter(|p| !p.is_empty() && *p != "." && *p != "..")
        .collect();
    match parts.as_slice() {
        [top, _, ..] => format!("{top}.zip"),
        _ => "folder.zip".into(),
    }
}
