//! Documents (one row per stored file per profile) and the upload dispatcher (`core/documents.py`).
//!
//! The same bytes uploaded twice for one profile are one document with two `document_source`
//! rows. `dispatch_upload` sniffs a stored blob and enqueues the import job of the lane that
//! handles it.

use std::borrow::Cow;
use std::io::Read;
use std::path::Path;

use serde_json::{Map, Value, json};
use sqlx::PgConnection;
use uuid::Uuid;

use crate::blobs::{BlobError, BlobOwner, BlobStore, sha256_hex_to_bytes};
use crate::error::{Error, Result};
use crate::ids::token_hex;
use crate::jobs::{EnqueueOptions, enqueue};

/// pdf | image | apple_health_zip | fhir_json | fhir_ndjson | genome_facts | vcf | tbi | gadgetbridge_db |
/// libreview_csv | dexcom_clarity_csv | ccda | unknown
pub type UploadKind = &'static str;

/// Job kind per upload kind. Handlers: import.pdf (lane B), import.fhir_bundle and
/// import.apple_health (lane C), import.genome_facts (lane D).
pub fn job_kind(kind: &str) -> Option<&'static str> {
    Some(match kind {
        "pdf" => "import.pdf",
        "fit" | "fit_gz" => "import.fit",
        "apple_health_zip" => "import.apple_health",
        "fhir_json" | "fhir_ndjson" => "import.fhir_bundle",
        "genome_facts" => "import.genome_facts",
        "gadgetbridge_db" => "import.gadgetbridge",
        "libreview_csv" | "dexcom_clarity_csv" => "import.cgm_csv",
        "ccda" => "import.ccda",
        _ => return None,
    })
}

pub fn mime(kind: &str) -> Option<&'static str> {
    Some(match kind {
        "pdf" => "application/pdf",
        "fit" => "application/vnd.ant.fit",
        "fit_gz" => "application/gzip",
        "fhir_json" => "application/fhir+json",
        "fhir_ndjson" => "application/fhir+ndjson",
        "apple_health_zip" => "application/zip",
        "genome_facts" => "application/json",
        "vcf" => "application/gzip",
        "tbi" => "application/octet-stream",
        "gadgetbridge_db" => "application/vnd.sqlite3",
        "libreview_csv" | "dexcom_clarity_csv" => "text/csv",
        "ccda" => "application/hl7-cda+xml",
        _ => return None,
    })
}

fn zip_has_export_xml(path: &Path) -> Option<bool> {
    let file = std::fs::File::open(path).ok()?;
    let archive = zip::ZipArchive::new(file).ok()?;
    Some(archive.file_names().any(|n| n.is_ok_and(|n| n.ends_with("export.xml"))))
}

/// Whether `head` starts a zip, the one kind `sniff` reads further than the first bytes of.
pub fn is_zip(head: &[u8]) -> bool {
    head.starts_with(b"PK\x03\x04")
}

/// A FIT file header: its size (12 or 14 bytes), then `.FIT` at bytes 8 to 11.
fn is_fit_header(head: &[u8]) -> bool {
    head.len() >= 12 && matches!(head[0], 12 | 14) && &head[8..12] == b".FIT"
}

/// A gzip stream whose first unpacked bytes are a FIT header (only those 12 bytes are unpacked).
fn gzip_holds_fit(head: &[u8]) -> bool {
    let mut first = [0u8; 12];
    flate2::read::MultiGzDecoder::new(head).read_exact(&mut first).is_ok() && is_fit_header(&first)
}

/// Classify an upload from its name and first bytes (and, for zips, its member list).
pub fn sniff(filename: &str, head: &[u8], blob_path: Option<&Path>) -> UploadKind {
    let name = filename.to_lowercase();
    if head.starts_with(b"%PDF-") {
        return "pdf";
    }
    if head.starts_with(b"\x89PNG")
        || head.starts_with(b"\xff\xd8\xff")
        || name.ends_with(".heic")
        || name.ends_with(".heif")
    {
        return "image";
    }
    if head.starts_with(b"SQLite format 3\0") {
        return "gadgetbridge_db";
    }
    if is_zip(head) {
        // Truncated or corrupt: refused like any unknown file.
        return match blob_path.and_then(zip_has_export_xml) {
            Some(true) => "apple_health_zip",
            _ => "unknown",
        };
    }
    if name.ends_with(".tbi") {
        return "tbi";
    }
    // a FIT activity file (slice 29), as the device wrote it or gzipped: before the gzip rule below
    if is_fit_header(head) {
        return "fit";
    }
    // a .fit.gz name goes to the FIT importer even when its first block does not unpack, so a damaged
    // one is refused as a damaged FIT file
    if head.starts_with(b"\x1f\x8b") && (gzip_holds_fit(head) || name.ends_with(".fit.gz")) {
        return "fit_gz";
    }
    if name.ends_with(".vcf.gz") || name.ends_with(".vcf.bgz") || head.starts_with(b"\x1f\x8b") {
        return "vcf";
    }
    // Python's bytes.lstrip() strips ASCII whitespace.
    let start = head
        .iter()
        .position(|b| !b" \t\n\r\x0b\x0c".contains(b))
        .unwrap_or(head.len());
    if head[start..].starts_with(b"{") {
        let contains = |needle: &[u8]| head.windows(needle.len()).any(|w| w == needle);
        if contains(b"ownhealth.genome-facts/") {
            return "genome_facts";
        }
        if name.ends_with(".ndjson") {
            return "fhir_ndjson";
        }
        if contains(b"\"resourceType\"") {
            return "fhir_json";
        }
    }
    // csv (slice 28): a CGM export, by the text of its header row
    if let Some(kind) = sniff_cgm_csv(head) {
        return kind;
    }
    // a C-CDA document (slice 20): XML whose root element is a ClinicalDocument
    if is_clinical_document(head) {
        return "ccda";
    }
    "unknown"
}

/// Whether `head` is XML whose root element is `ClinicalDocument`, with or without a prefix. A byte
/// order mark, the XML declaration, processing instructions, comments and a DOCTYPE may come first;
/// a DOCTYPE passes here so the importer refuses the file with its reason.
fn is_clinical_document(head: &[u8]) -> bool {
    fn after(rest: &[u8], from: usize, end: &[u8]) -> Option<usize> {
        let at = rest.get(from..)?.windows(end.len()).position(|w| w == end)?;
        Some(from + at + end.len())
    }
    let mut rest = head.strip_prefix(b"\xef\xbb\xbf").unwrap_or(head);
    loop {
        let start = rest.iter().position(|b| !b" \t\r\n".contains(b)).unwrap_or(rest.len());
        rest = &rest[start..];
        let skip = if rest.starts_with(b"<?") {
            after(rest, 2, b"?>")
        } else if rest.starts_with(b"<!--") {
            after(rest, 4, b"-->")
        } else if rest.starts_with(b"<!DOCTYPE") {
            // an internal subset in brackets may hold `>` of its own
            let close = rest.iter().position(|b| *b == b'>');
            match rest.iter().position(|b| *b == b'[') {
                Some(open) if close.is_none_or(|c| open < c) => after(rest, open, b"]").and_then(|i| {
                    let gt = rest[i..].iter().position(|b| !b" \t\r\n".contains(b))?;
                    (rest[i + gt] == b'>').then_some(i + gt + 1)
                }),
                _ => close.map(|c| c + 1),
            }
        } else {
            break;
        };
        let Some(skip) = skip else { return false };
        rest = &rest[skip..];
    }
    let Some(tag) = rest.strip_prefix(b"<") else {
        return false;
    };
    let Some(end) = tag.iter().position(|b| b" \t\r\n/>".contains(b)) else {
        return false;
    };
    let name = &tag[..end];
    name.rsplit(|b| *b == b':').next() == Some(b"ClinicalDocument".as_slice())
}

/// The header cells that name a CGM export, in English, German and Spanish, as `header_key` writes
/// them: (upload kind, timestamp column, record or event type column). Clarity's timestamp column is
/// any header that ends in its format pattern (`Timestamp (YYYY-MM-DDThh:mm:ss)`, `Zeitstempel
/// (JJJJ-MM-TTThh:mm:ss)`, ...), so an empty list there. `ownhealth_devices::cgm_csv` finds the header
/// row of a file with these same names.
pub const CGM_CSV_KINDS: [(UploadKind, &[&str], &[&str]); 2] = [
    (
        "libreview_csv",
        &[
            "device timestamp",
            "gerätezeitstempel",
            "sello de tiempo del dispositivo",
        ],
        &["record type", "aufzeichnungstyp", "tipo de registro"],
    ),
    (
        "dexcom_clarity_csv",
        &[],
        &["event type", "ereignisart", "ereignistyp", "tipo de evento"],
    ),
];

/// The end of a Clarity timestamp header, whatever the language (`header_key` form).
pub const CLARITY_TIME_SUFFIX: &str = "thh:mm:ss)";

/// The lines a CGM export may hold before its header row (LibreView writes one metadata line).
pub const CGM_CSV_HEADER_LINES: usize = 8;

/// A file's text: UTF-8 without its byte order mark, or Latin-1 when it is not UTF-8 (an export saved
/// again by a spreadsheet). A UTF-8 sequence cut at the end (a file's first bytes) is dropped.
pub fn decode_text(bytes: &[u8]) -> Cow<'_, str> {
    let bytes = bytes.strip_prefix(b"\xef\xbb\xbf").unwrap_or(bytes);
    match std::str::from_utf8(bytes) {
        Ok(s) => Cow::Borrowed(s),
        Err(e) if e.error_len().is_none() => {
            Cow::Borrowed(std::str::from_utf8(&bytes[..e.valid_up_to()]).unwrap_or_default())
        }
        Err(_) => Cow::Owned(bytes.iter().map(|b| char::from(*b)).collect()),
    }
}

/// The separator of a CSV header line: the one of `,` `;` and tab that splits it into the most cells.
/// A separator inside a quoted cell (`"Notes;;"`) splits nothing, so it does not count.
pub fn csv_delimiter(line: &str) -> u8 {
    let mut quoted = false;
    let outside: Vec<u8> = line
        .bytes()
        .filter(|b| {
            if *b == b'"' {
                quoted = !quoted;
            }
            !quoted
        })
        .collect();
    (*b",;\t")
        .into_iter()
        .max_by_key(|d| outside.iter().filter(|b| *b == d).count())
        .unwrap_or(b',')
}

/// A header cell as the CGM tables write it: unquoted, without a byte order mark, trimmed, lowercase,
/// with runs of spaces as one.
pub fn header_key(cell: &str) -> String {
    let c = cell.trim().trim_start_matches('\u{feff}').trim().trim_matches('"');
    let c = c.replace("\"\"", "\"");
    c.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase()
}

/// The cells of one CSV line split at `d`, as a CSV reader splits them: a separator inside a quoted cell
/// (`"Notes,Event Type"`) splits nothing, and the quotes stay for [`header_key`] to take off.
fn csv_cells(line: &str, d: char) -> Vec<&str> {
    let (mut cells, mut start, mut quoted) = (Vec::new(), 0, false);
    for (i, c) in line.char_indices() {
        if c == '"' {
            quoted = !quoted;
        } else if c == d && !quoted {
            cells.push(&line[start..i]);
            start = i + c.len_utf8();
        }
    }
    cells.push(&line[start..]);
    cells
}

/// The CGM export kind a header line names, if it is one. A name inside a quoted cell with other text
/// (`"Notes,Event Type,Details"`) is not a header cell, as the importer's CSV reader reads it.
pub fn cgm_csv_kind(line: &str) -> Option<UploadKind> {
    let d = char::from(csv_delimiter(line));
    let cells: Vec<String> = csv_cells(line, d).into_iter().map(header_key).collect();
    let has = |names: &[&str]| cells.iter().any(|c| names.contains(&c.as_str()));
    CGM_CSV_KINDS.iter().find_map(|(kind, time, types)| {
        let time_ok = if time.is_empty() {
            cells.iter().any(|c| c.ends_with(CLARITY_TIME_SUFFIX))
        } else {
            has(time)
        };
        (time_ok && has(types)).then_some(*kind)
    })
}

fn sniff_cgm_csv(head: &[u8]) -> Option<UploadKind> {
    if head.contains(&0) {
        return None;
    }
    decode_text(head)
        .lines()
        .take(CGM_CSV_HEADER_LINES)
        .find_map(cgm_csv_kind)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DocumentRef {
    pub id: Uuid,
    pub created: bool,
    pub reactivated: bool,
}

/// The `document.doc_type` check constraint (migration 1221; `check_mirror.rs` keeps them equal).
pub const DOC_TYPES: [&str; 8] = [
    "lab_report",
    "imaging_report",
    "discharge",
    "prescription",
    "genomics",
    "ecg",
    "other",
    WEARABLES_DOC_TYPE,
];

/// The `doc_type` of a raw wearables export: a FIT activity file (`import.fit`) and a Gadgetbridge
/// database (`import.gadgetbridge`) today. Every wearables importer stores its file with this type, so a
/// wearables document needs `read:wearables` (not `read:records`) to be listed, opened or downloaded:
/// it holds every reading, and a FIT file held the route.
pub const WEARABLES_DOC_TYPE: &str = "wearables";

/// Whether the document `doc` (a table alias, like `d`) is part of the medical record, as a SQL
/// boolean: every document but a raw wearables export. A wearables export is dated by its first
/// activity, but a run is no medical record, and its readings are on the Wearables screen under
/// read:wearables. So every reader that takes documents by date or cites them (the timeline, the
/// record gaps and their visits, the evidence of a Due decision) leaves wearables exports out through
/// this one rule.
pub fn record_document_sql(doc: &str) -> String {
    format!("{doc}.doc_type is distinct from '{WEARABLES_DOC_TYPE}'")
}

/// The `document_source.channel` check constraint: how one arrival came in.
pub const CHANNELS: [&str; 22] = [
    "upload",
    "fhir",
    "apple_health",
    "email",
    "folder",
    "paperless",
    "manual",
    "genome",
    "demo",
    "share",
    "shortcut",
    "health_auto_export",
    "gadgetbridge",
    "nightscout",
    "smart",
    "bluebutton",
    "shl",
    "shc",
    "ccda",
    "openwearables",
    "dexcom",
    "imaging",
];

#[derive(Debug, Clone, PartialEq)]
pub struct NewDocument<'a> {
    pub household_id: Uuid,
    pub profile_id: Option<Uuid>,
    pub blob_sha256: &'a str,
    pub mime: &'a str,
    pub size_bytes: i64,
    pub filename: Option<&'a str>,
    /// One of [`CHANNELS`].
    pub channel: &'a str,
    pub source_ref: Option<&'a str>,
    pub pages: Option<i32>,
    pub doc_type: Option<&'a str>,
    /// The inbox it came through (`source.id`), on the document and its `document_source` row.
    pub source_id: Option<Uuid>,
    /// The original this is an unlocked copy of (a password PDF).
    pub derived_from: Option<Uuid>,
}

impl<'a> NewDocument<'a> {
    pub fn upload(
        household_id: Uuid,
        profile_id: Option<Uuid>,
        blob_sha256: &'a str,
        mime: &'a str,
        size_bytes: i64,
    ) -> Self {
        NewDocument {
            household_id,
            profile_id,
            blob_sha256,
            mime,
            size_bytes,
            filename: None,
            channel: "upload",
            source_ref: None,
            pages: None,
            doc_type: None,
            source_id: None,
            derived_from: None,
        }
    }
}

/// Python `(filename or "")[:255] or None`: at most 255 characters, empty means none.
fn short_filename(f: Option<&str>) -> Option<String> {
    let s: String = f.unwrap_or("").chars().take(255).collect();
    (!s.is_empty()).then_some(s)
}

/// The lock an upload of `sha256` for `profile` takes shared ([`create_document`], held until its
/// transaction ends) and the removal of that profile's copy of the bytes takes exclusive
/// (`ownhealth_auth::household::drop_moved_copies`): the removal never runs while an upload that names
/// the same bytes is in flight, and it sees the upload's row once it commits.
pub async fn lock_blob(conn: &mut PgConnection, profile: Uuid, sha256: &str, shared: bool) -> Result<()> {
    let sql = if shared {
        "select pg_advisory_xact_lock_shared(hashtextextended($1, 0))"
    } else {
        "select pg_advisory_xact_lock(hashtextextended($1, 0))"
    };
    sqlx::query(sql)
        .bind(format!(
            "ownhealth.blob:{}:{}",
            profile.simple(),
            sha256.to_ascii_lowercase()
        ))
        .execute(conn)
        .await?;
    Ok(())
}

/// Insert a document or return the existing one for the same profile and bytes.
pub async fn create_document(conn: &mut PgConnection, d: &NewDocument<'_>) -> Result<DocumentRef> {
    let sha = sha256_hex_to_bytes(d.blob_sha256)?;
    if let Some(p) = d.profile_id {
        lock_blob(&mut *conn, p, d.blob_sha256, true).await?;
    }
    let row: Option<(Uuid, String)> = sqlx::query_as(
        "select id, status from document where coalesce(profile_id, household_id) = coalesce($1, $2) and blob_sha256 = $3",
    )
    .bind(d.profile_id)
    .bind(d.household_id)
    .bind(&sha)
    .fetch_optional(&mut *conn)
    .await?;
    // The same bytes uploaded again after a retraction bring the document back; its import runs
    // again (the retraction cleared the import flags).
    let reactivated = row.as_ref().is_some_and(|(_, s)| s == "retracted");
    if reactivated && let Some((id, _)) = &row {
        sqlx::query("update document set status = 'active', status_reason = null where id = $1")
            .bind(id)
            .execute(&mut *conn)
            .await?;
    }
    let (id, created) = match row {
        Some((id, _)) => (id, false),
        None => {
            // A concurrent upload of the same bytes can insert between the select and here: the
            // conflict then yields no id and the row that won is the duplicate.
            let id: Option<Uuid> = sqlx::query_scalar(
                "insert into document (household_id, profile_id, blob_sha256, filename, mime, size_bytes, pages, doc_type, \
                 source_id, derived_from) values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10) \
                 on conflict ((coalesce(profile_id, household_id)), blob_sha256) do nothing returning id",
            )
            .bind(d.household_id)
            .bind(d.profile_id)
            .bind(&sha)
            .bind(short_filename(d.filename))
            .bind(d.mime)
            .bind(d.size_bytes)
            .bind(d.pages)
            .bind(d.doc_type)
            .bind(d.source_id)
            .bind(d.derived_from)
            .fetch_optional(&mut *conn)
            .await?;
            match id {
                Some(id) => (id, true),
                None => {
                    let id: Uuid = sqlx::query_scalar(
                        "select id from document where coalesce(profile_id, household_id) = coalesce($1, $2) and blob_sha256 = $3",
                    )
                    .bind(d.profile_id)
                    .bind(d.household_id)
                    .bind(&sha)
                    .fetch_one(&mut *conn)
                    .await?;
                    (id, false)
                }
            }
        }
    };
    sqlx::query("insert into document_source (document_id, channel, source_ref, source_id) values ($1, $2, $3, $4)")
        .bind(id)
        .bind(d.channel)
        .bind(d.source_ref)
        .bind(d.source_id)
        .execute(conn)
        .await?;
    Ok(DocumentRef {
        id,
        created,
        reactivated,
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredFile {
    pub filename: String,
    pub sha256: String,
    pub size: i64,
}

fn job_id_json(id: Option<i64>) -> Value {
    id.map_or(Value::Null, |i| json!(i))
}

/// Create documents and enqueue import jobs for files already in the blob store.
///
/// Returns `{"items": [{filename, kind, sha256, document_id?, duplicate?, job_id?, attached_to?}]}`
/// in the key order Python wrote. A genome-facts file takes the .vcf.gz and .tbi uploaded with
/// it; those alone are refused (`Error::Upload`).
pub async fn dispatch_upload(
    conn: &mut PgConnection,
    blobs: &BlobStore,
    household_id: Uuid,
    profile_id: Uuid,
    files: &[StoredFile],
    actor_user: Option<Uuid>,
) -> Result<Value> {
    dispatch_upload_in_zone(conn, blobs, household_id, profile_id, files, actor_user, None).await
}

/// [`dispatch_upload`] with the time zone the browser sent (an IANA name the caller checked): a CGM
/// export's job carries it (`timezone`), to read the export's wall-clock times when the server has no
/// `OWNHEALTH_TIMEZONE`, or one Postgres does not know.
pub async fn dispatch_upload_in_zone(
    conn: &mut PgConnection,
    blobs: &BlobStore,
    household_id: Uuid,
    profile_id: Uuid,
    files: &[StoredFile],
    _actor_user: Option<Uuid>,
    zone: Option<&str>,
) -> Result<Value> {
    let mut kinds: Vec<(&StoredFile, UploadKind)> = Vec::new();
    for f in files {
        let (store, owner, sha, name) = (
            blobs.clone(),
            BlobOwner::Profile(profile_id),
            f.sha256.clone(),
            f.filename.clone(),
        );
        // off the runtime: a plain file of an older install is hashed once when it opens, and a zip is
        // decrypted whole into a local copy
        let kind = tokio::task::spawn_blocking(move || -> Result<UploadKind> {
            let mut head = Vec::with_capacity(4096);
            store.open_seek(owner, &sha)?.take(4096).read_to_end(&mut head)?;
            let copy = is_zip(&head).then(|| store.with_local_copy(owner, &sha)).transpose()?;
            Ok(sniff(&name, &head, copy.as_ref().map(|c| c.path())))
        })
        .await
        .map_err(std::io::Error::other)??;
        kinds.push((f, kind));
    }
    let has_facts = kinds.iter().any(|(_, k)| *k == "genome_facts");
    let mut items = Vec::new();
    for (f, kind) in &kinds {
        let mut item = Map::new();
        item.insert("filename".into(), json!(f.filename));
        item.insert("kind".into(), json!(kind));
        item.insert("sha256".into(), json!(f.sha256));
        match *kind {
            "unknown" => {
                return Err(Error::Upload(format!(
                    "{}: not a PDF, image, Apple Health export, FHIR bundle or genome facts file",
                    f.filename
                )));
            }
            "vcf" | "tbi" => {
                if !has_facts {
                    return Err(Error::Upload(format!(
                        "{}: upload it together with its genome-facts.json",
                        f.filename
                    )));
                }
                item.insert("attached_to".into(), json!("genome_facts"));
            }
            "pdf" | "image" => {
                let m = mime(kind).unwrap_or(if f.filename.to_lowercase().ends_with(".png") {
                    "image/png"
                } else {
                    "image/jpeg"
                });
                let r = create_document(
                    &mut *conn,
                    &NewDocument {
                        filename: Some(&f.filename),
                        ..NewDocument::upload(household_id, Some(profile_id), &f.sha256, m, f.size)
                    },
                )
                .await?;
                item.insert("document_id".into(), json!(r.id.to_string()));
                item.insert("duplicate".into(), json!(!(r.created || r.reactivated)));
                // Images too: the PDF handler files them as pending_document items.
                if r.created || r.reactivated {
                    let mut payload = json!({
                        "document_id": r.id.to_string(),
                        "profile_id": profile_id.to_string(),
                        "household_id": household_id.to_string(),
                    });
                    let mut key = format!("import.pdf:{}", r.id);
                    if r.reactivated {
                        // The earlier extraction run is done; import it again.
                        payload["force"] = json!(true);
                        key = format!("{key}:reupload:{}", token_hex(4));
                    }
                    let id = enqueue(&mut *conn, "import.pdf", &payload, EnqueueOptions::dedupe(&key)).await?;
                    item.insert("job_id".into(), job_id_json(id));
                }
            }
            _ => {
                let job = job_kind(kind).ok_or_else(|| Error::Upload(format!("{}: no importer", f.filename)))?;
                let mut payload = json!({
                    "profile_id": profile_id.to_string(),
                    "household_id": household_id.to_string(),
                    "blob_sha256": f.sha256,
                    "filename": f.filename,
                });
                if *kind == "fhir_ndjson" {
                    payload["format"] = json!("ndjson");
                }
                let mut key = format!("{job}:{profile_id}:{}", f.sha256);
                if job == "import.cgm_csv"
                    && let Some(z) = zone
                {
                    payload["timezone"] = json!(z);
                    // the same file sent again in another zone is read again in it, even while the job
                    // of the first sending is still queued or running
                    key = format!("{key}:{z}");
                }
                if *kind == "genome_facts" {
                    let attached: Map<String, Value> = kinds
                        .iter()
                        .filter(|(_, k)| matches!(*k, "vcf" | "tbi"))
                        .map(|(g, _)| (g.filename.clone(), json!(g.sha256)))
                        .collect();
                    payload["files"] = Value::Object(attached);
                }
                let id = enqueue(&mut *conn, job, &payload, EnqueueOptions::dedupe(&key)).await?;
                item.insert("job_id".into(), job_id_json(id));
            }
        }
        items.push(Value::Object(item));
    }
    Ok(json!({ "items": items }))
}

const SENSITIVE_FACT_TABLES: [&str; 7] = [
    "condition",
    "medication_statement",
    "allergy",
    "immunization",
    "procedure",
    "encounter",
    "note",
];

/// The payload keys of a held review row whose value carries `sensitivity`: `candidate_row`
/// (`observation`), and a FHIR import's held record (`record`) or relative (`member`).
const HELD_VALUES: [&str; 3] = ["observation", "record", "member"];

/// The rule of `document_is_sensitive` as a SQL boolean over `doc` (a bind like `$1` or a column
/// like `d.id`), for queries that must leave sensitive documents out.
pub fn sensitive_document_sql(doc: &str) -> String {
    let via_src = SENSITIVE_FACT_TABLES
        .iter()
        .map(|t| {
            format!(
                "exists (select 1 from {t} f join src s on s.id = f.src_id where s.document_id = {doc} \
                 and cardinality(f.sensitivity) > 0)"
            )
        })
        .collect::<Vec<_>>()
        .join(" or ");
    // a held row's value: a candidate row's observation, a FHIR import's record or relative
    let held = HELD_VALUES
        .iter()
        .map(|k| {
            format!(
                "(jsonb_typeof(r.payload->'{k}'->'sensitivity') = 'array' \
                 and jsonb_array_length(r.payload->'{k}'->'sensitivity') > 0)"
            )
        })
        .collect::<Vec<_>>()
        .join(" or ");
    format!(
        "(exists (select 1 from observation o left join src s on s.id = o.src_id \
         where (o.document_id = {doc} or s.document_id = {doc}) and cardinality(o.sensitivity) > 0) \
         or exists (select 1 from review_item r where r.document_id = {doc} and ({held})) \
         or exists (select 1 from medication_label l join medication_statement m on m.id = l.medication_id \
         where l.document_id = {doc} and cardinality(m.sensitivity) > 0) \
         or exists (select 1 from document u where u.id = {doc} \
         and (u.flags->'unprojected' is not null or u.flags->'sensitive' is not null)) \
         or {via_src})"
    )
}

/// True when any value read from this document is sensitive (mental health, reproductive, sexual
/// health), whatever its status: the original file and its page text still hold it. Held review
/// rows count too: an observation, a record or a relative. So does a connector's bundle that holds
/// resources OwnHealth does not project (`flags.unprojected`, written by the FHIR pull): nothing
/// read them for sensitivity, so the file fails closed. A bundle the pull read a sensitive resource
/// in is `flags.sensitive`, even when the ledger or the dedupe imported nothing from it. Without
/// read:sensitive the file, the page renders and the words stay closed.
pub async fn document_is_sensitive(conn: &mut PgConnection, document_id: Uuid) -> Result<bool> {
    let sql = format!("select {}", sensitive_document_sql("$1::uuid"));
    let v: Option<bool> = sqlx::query_scalar(sqlx::AssertSqlSafe(sql))
        .bind(document_id)
        .fetch_one(conn)
        .await?;
    Ok(v.unwrap_or(false))
}

/// The blobs of a document that moved, and the owner they were stored for before.
/// `ownhealth_auth::household::drop_moved_copies` removes that owner's copies once the move commits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Moved {
    pub from: BlobOwner,
    pub shas: Vec<String>,
}

/// The job that removes a moved document's copies under the profile it left. [`rekey_document`] queues
/// it in the move's transaction, so a crash or an error after the commit leaves it to a retry.
pub const DROP_MOVED_JOB: &str = "blobs.drop_moved";

/// Lock document `document_id`'s row for a move, and return the profile it has now (None: no profile,
/// or no such document). A move reads the owner here, before its permission check and its re-key, so
/// two moves at once see each other's result.
pub async fn lock_owner(conn: &mut PgConnection, document_id: Uuid) -> Result<Option<Uuid>> {
    Ok(
        sqlx::query_scalar::<_, Option<Uuid>>("select profile_id from document where id = $1 for update")
            .bind(document_id)
            .fetch_optional(conn)
            .await?
            .flatten(),
    )
}

/// Lock document `document_id`'s row and return the owner its files are stored for now (None: no
/// such document). A job that stores a file of the document after slow work (an OCR layer) stores it
/// for this owner, in the transaction that records it: a move takes the same lock
/// ([`lock_owner`]), so it re-keys that file too, or sees the new owner already.
pub async fn lock_files_owner(conn: &mut PgConnection, document_id: Uuid) -> Result<Option<BlobOwner>> {
    let row: Option<Option<Uuid>> = sqlx::query_scalar("select profile_id from document where id = $1 for update")
        .bind(document_id)
        .fetch_optional(conn)
        .await?;
    Ok(row.map(|profile| BlobOwner::of_document(profile, document_id)))
}

/// After document `document_id` moved from profile `from` to `to` (`None`: no profile): every blob of
/// the document (the file, its page layers and images, its OCR layers) is stored for `to` as well, so
/// the new profile's key opens them, and the removal of `from`'s copies is queued
/// ([`DROP_MOVED_JOB`]). A document that had no profile was sealed under its own item key: that key
/// is shredded here, in the move's transaction. Run it in the transaction of the move, with `from` read under the document's
/// row lock, before the commit: the re-import the move queues reads them as `to`'s.
pub async fn rekey_document(
    conn: &mut PgConnection,
    blobs: &BlobStore,
    document_id: Uuid,
    from: Option<Uuid>,
    to: Option<Uuid>,
) -> Result<Moved> {
    let old = BlobOwner::of_document(from, document_id);
    if from == to {
        return Ok(Moved {
            from: old,
            shas: Vec::new(),
        });
    }
    let shas: Vec<String> = sqlx::query_scalar(
        "select distinct encode(h, 'hex') from (select blob_sha256 as h from document where id = $1 \
         union all select words_blob from document_page where document_id = $1 \
         union all select image_blob from document_page where document_id = $1 \
         union all select words_blob from ocr_run where document_id = $1) x where h is not null",
    )
    .bind(document_id)
    .fetch_all(&mut *conn)
    .await?;
    let new = BlobOwner::of_document(to, document_id);
    let blobs = blobs.clone();
    let moved = tokio::task::spawn_blocking(move || {
        for sha in &shas {
            match blobs.rekey(old, new, sha) {
                Ok(_) => {}
                // lost before the move: nothing to copy, and the read reports it as before
                Err(BlobError::NotFound) => {
                    tracing::warn!(%document_id, "a blob of a moved document is missing from the store");
                }
                Err(e) => return Err(Error::from(e)),
            }
        }
        Ok(Moved { from: old, shas })
    })
    .await
    .map_err(|e| Error::Other(anyhow::anyhow!("blob re-key task failed: {e}")))??;
    // the document had no profile: its own key goes in this transaction, after its files were read
    // (a key lookup waits on the row this takes). The stored key is gone once this commits. A process
    // that already holds the key in its keyring (`Keyring::dek_for` answers from its cache first) can
    // still open the item's copies until the drop job queued below removes them.
    if let BlobOwner::Pending(item) = moved.from {
        crate::keys::shred_pending(&mut *conn, &[item]).await?;
    }
    let owner = match moved.from {
        BlobOwner::Profile(p) => Some(json!({"profile_id": p})),
        BlobOwner::Pending(i) => Some(json!({"pending_id": i})),
        BlobOwner::Reference => None,
    };
    if let (Some(Value::Object(mut payload)), false) = (owner, moved.shas.is_empty()) {
        let household: Uuid = sqlx::query_scalar("select household_id from document where id = $1")
            .bind(document_id)
            .fetch_one(&mut *conn)
            .await?;
        // the household, so a retry that runs out of attempts shows in its review queue
        payload.insert("household_id".into(), json!(household));
        payload.insert("document_id".into(), json!(document_id));
        payload.insert("blobs".into(), json!(moved.shas));
        enqueue(
            &mut *conn,
            DROP_MOVED_JOB,
            &Value::Object(payload),
            EnqueueOptions::default(),
        )
        .await?;
    }
    Ok(moved)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upload_sniff_keeps_the_blocking_task_error_typed() {
        let function = include_str!("documents.rs")
            .split_once("pub async fn dispatch_upload_in_zone(")
            .expect("the upload dispatch function exists")
            .1
            .split_once("kinds.push")
            .expect("the dispatch function queues the sniff result")
            .0;
        assert!(!function.contains("anyhow"), "{function}");
    }

    #[test]
    fn a_separator_inside_quotes_does_not_pick_the_delimiter() {
        let clarity = "Index,Timestamp (YYYY-MM-DDThh:mm:ss),Event Type,Glucose Value (mg/dL)";
        assert_eq!(csv_delimiter(&format!("{clarity},\"Notes;;;;;\"")), b',');
        assert_eq!(
            csv_delimiter("Index;Timestamp (YYYY-MM-DDThh:mm:ss);Event Type;\"a,b,c,d,e\""),
            b';'
        );
        assert_eq!(csv_delimiter("Index\tTimestamp\tEvent Type"), b'\t');
        assert_eq!(
            cgm_csv_kind(&format!("{clarity},\"Notes;;;;;\"")),
            Some("dexcom_clarity_csv")
        );
    }

    #[test]
    fn a_header_name_inside_a_quoted_cell_names_no_cgm_export() {
        // one quoted cell that holds a separator and a CGM header name is one cell, as the importer's
        // CSV reader reads it: the sniffer queues no CGM job the import would then refuse
        let clarity = "Index,Timestamp (YYYY-MM-DDThh:mm:ss),\"Notes,Event Type,Details\"";
        assert_eq!(cgm_csv_kind(clarity), None);
        let libre = "Device,Device Timestamp,\"Notes,Record Type\"";
        assert_eq!(cgm_csv_kind(libre), None);
        assert_eq!(
            sniff(
                "notes.csv",
                format!("{clarity}\n1,2026-11-14T20:00:17,x\n").as_bytes(),
                None
            ),
            "unknown"
        );
        // quoted header cells, a quoted cell with a separator and an escaped quote, still name the export
        assert_eq!(
            cgm_csv_kind("\"Index\",\"Timestamp (YYYY-MM-DDThh:mm:ss)\",\"Event Type\",\"a, \"\"b\"\"\""),
            Some("dexcom_clarity_csv")
        );
        assert_eq!(
            cgm_csv_kind("Gerät;Seriennummer;\"Gerätezeitstempel\";Aufzeichnungstyp;\"x;y\""),
            Some("libreview_csv")
        );
    }

    #[test]
    fn sniff_matches_python() {
        assert_eq!(sniff("a.pdf", b"%PDF-1.7", None), "pdf");
        assert_eq!(sniff("x", b"\x89PNG\r\n", None), "image");
        assert_eq!(sniff("x.HEIC", b"....", None), "image");
        assert_eq!(sniff("x.zip", b"PK\x03\x04", None), "unknown");
        assert_eq!(sniff("x.vcf.gz", b"\x1f\x8b", None), "vcf");
        assert_eq!(sniff("x.vcf.gz.tbi", b"\x1f\x8b", None), "tbi");
        assert_eq!(
            sniff("f.json", b"  {\"schema\": \"ownhealth.genome-facts/1\"}", None),
            "genome_facts"
        );
        assert_eq!(
            sniff("b.ndjson", b"{\"resourceType\":\"Observation\"}", None),
            "fhir_ndjson"
        );
        assert_eq!(sniff("b.json", b"\n{\"resourceType\":\"Bundle\"}", None), "fhir_json");
        assert_eq!(sniff("b.json", b"{\"a\":1}", None), "unknown");
        assert_eq!(sniff("b.txt", b"hello", None), "unknown");
    }

    #[test]
    fn a_cgm_export_is_sniffed_by_its_header_row_in_each_language() {
        let libre = [
            "Glucose Data,Generated on,11-15-2026 09:00 AM UTC,Generated by,Lucía Rivera\r\nDevice,Serial Number,Device Timestamp,Record Type,Historic Glucose mg/dL\r\n",
            "Glukosewerte,Erstellt am,15-11-2026 09:00 UTC\nGerät,Seriennummer,Gerätezeitstempel,Aufzeichnungstyp,Glukosewert-Verlauf mmol/L\n",
            "Datos de glucosa,Generado el\nDispositivo,Número de serie,Sello de tiempo del dispositivo,Tipo de registro\n",
        ];
        for text in libre {
            assert_eq!(sniff("export.csv", text.as_bytes(), None), "libreview_csv", "{text}");
        }
        let clarity = [
            "Index,Timestamp (YYYY-MM-DDThh:mm:ss),Event Type,Event Subtype,Glucose Value (mg/dL)\n1,,FirstName\n",
            "\"Index\";\"Zeitstempel (JJJJ-MM-TTThh:mm:ss)\";\"Ereignisart\";\"Glukosewert (mmol/L)\"\n",
            "Índice,Marca de tiempo (AAAA-MM-DDThh:mm:ss),Tipo de evento,Valor de glucosa (mmol/L)\n",
        ];
        for text in clarity {
            assert_eq!(sniff("x.csv", text.as_bytes(), None), "dexcom_clarity_csv", "{text}");
        }
        // with a byte order mark, and saved again as Latin-1 by a spreadsheet
        let bom = [b"\xef\xbb\xbf".as_slice(), clarity[0].as_bytes()].concat();
        assert_eq!(sniff("x.csv", &bom, None), "dexcom_clarity_csv");
        let latin1: Vec<u8> = libre[1].chars().map(|c| c as u32 as u8).collect();
        assert!(std::str::from_utf8(&latin1).is_err());
        assert_eq!(sniff("x.csv", &latin1, None), "libreview_csv");
        // a CSV that only looks close: one of the two columns, or another table altogether
        for text in [
            "Device,Serial Number,Device Timestamp,Historic Glucose mg/dL\n",
            "Index,Event Type,Glucose Value (mg/dL)\n",
            "date,glucose\n2026-11-14,98\n",
            "\n\n\n\n\n\n\n\nDevice,Serial Number,Device Timestamp,Record Type\n",
        ] {
            assert_eq!(sniff("x.csv", text.as_bytes(), None), "unknown", "{text}");
        }
        assert_eq!(job_kind("libreview_csv"), Some("import.cgm_csv"));
        assert_eq!(job_kind("dexcom_clarity_csv"), Some("import.cgm_csv"));
        assert_eq!(mime("dexcom_clarity_csv"), Some("text/csv"));
    }

    #[test]
    fn a_clinical_document_root_is_sniffed_as_ccda() {
        let cases: [&[u8]; 5] = [
            b"<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<ClinicalDocument xmlns=\"urn:hl7-org:v3\">",
            b"\xef\xbb\xbf<?xml version=\"1.0\"?><?xml-stylesheet type=\"text/xsl\" href=\"CDA.xsl\"?>\n<!-- a sample -->\n<ClinicalDocument>",
            b"  <cda:ClinicalDocument xmlns:cda=\"urn:hl7-org:v3\">",
            // a DOCTYPE goes to the importer, which refuses it with a reason
            b"<?xml version=\"1.0\"?><!DOCTYPE lolz [<!ENTITY lol \"lol\">]><ClinicalDocument>",
            b"<ClinicalDocument\n  xmlns=\"urn:hl7-org:v3\">",
        ];
        for head in cases {
            assert_eq!(
                sniff("record.xml", head, None),
                "ccda",
                "{}",
                String::from_utf8_lossy(head)
            );
        }
        for head in [
            b"<?xml version=\"1.0\"?><HealthData locale=\"en_US\">".as_slice(),
            b"<ClinicalDocumentX>",
            b"<!-- <ClinicalDocument> -->",
            b"<?xml version=\"1.0\"?><!-- never closed",
            b"ClinicalDocument",
        ] {
            assert_eq!(
                sniff("record.xml", head, None),
                "unknown",
                "{}",
                String::from_utf8_lossy(head)
            );
        }
        assert_eq!(job_kind("ccda"), Some("import.ccda"));
        assert_eq!(mime("ccda"), Some("application/hl7-cda+xml"));
    }

    #[test]
    fn sniff_reads_the_zip_member_list() -> anyhow::Result<()> {
        use std::io::Write;
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("export.zip");
        let mut z = zip::ZipWriter::new(std::fs::File::create(&path)?);
        z.start_file(
            "apple_health_export/export.xml",
            zip::write::SimpleFileOptions::default(),
        )?;
        z.write_all(b"<HealthData/>")?;
        z.finish()?;
        assert_eq!(sniff("export.zip", b"PK\x03\x04", Some(&path)), "apple_health_zip");
        let bad = dir.path().join("bad.zip");
        std::fs::write(&bad, b"PK\x03\x04truncated")?;
        assert_eq!(sniff("bad.zip", b"PK\x03\x04", Some(&bad)), "unknown");
        Ok(())
    }

    #[test]
    fn filenames_are_cut_at_255_characters() {
        assert_eq!(short_filename(Some("")), None);
        assert_eq!(short_filename(None), None);
        assert_eq!(
            short_filename(Some(&"é".repeat(300))).map(|s| s.chars().count()),
            Some(255)
        );
    }
}
