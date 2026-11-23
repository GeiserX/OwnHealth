//! The `import.dicom` job: one stored upload (a zip) into Orthanc and onto the record.
//!
//! 1. Walk the zip. A file is DICOM when it has the PS3.10 magic (`DICM` after a 128-byte preamble);
//!    everything else on a hospital CD (the viewer, `autorun.inf`, readme files) is counted and left
//!    out, and so is `DICOMDIR`, the disc's own index. A file with the magic that does not read is
//!    counted as damaged. Files are grouped by StudyInstanceUID.
//! 2. For each study, under the one StudyInstanceUID lock every Orthanc writer takes
//!    ([`ownhealth_core::orthanc::lock_study`]): a study another profile holds, a second patient under
//!    one UID, or a UID Orthanc already holds under another patient goes to review, and nothing of it
//!    reaches Orthanc. A study the profile records already, whose series Orthanc holds in full, is
//!    left alone: the same disc again sends nothing. Then a study whose patient is not the profile's
//!    (`identity_match`), and on a disc with two or more patients, one that is not clearly the
//!    profile's, goes to review. The person can accept those two; the import then sends that study as
//!    it is.
//! 3. The rest is sent to Orthanc file by file; a file Orthanc refuses is counted (`refused_files`).
//!    Orthanc must then hold exactly one study under the UID, the one every file landed in, with the
//!    patient the files name; if not, the files this import added are taken back and the study goes
//!    to review.
//! 4. Each series' DICOMweb metadata is read once (Orthanc caches it), then the study is recorded:
//!    `imaging_study` (status current, src kind `dicom`), with the series and instances Orthanc holds,
//!    and an Encapsulated PDF becomes an `imaging_report` document (channel `imaging`), linked by
//!    `report_document_id`, that the PDF importer reads. A second disc of a recorded study (a series
//!    the first one did not hold) updates those counts. Only then is the study ready: a failure on
//!    the way records nothing, and the job retries.
//!
//! The UID lock is a transaction lock, so one transaction, and one pool connection, stays open while a
//! study's files go to Orthanc. That is the price of one lock that every Orthanc writer takes the same
//! way (the rebuild from the stored zips too). A session lock would need a release on every way out,
//! and a connection back in the pool that still held one would block its UID for good. A job holds
//! one connection, so the pool sees no more of them than the core queue runs jobs at once.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Read;

use chrono::NaiveDate;
use futures::future::BoxFuture;
use ownhealth_core::blobs::{BlobOwner, BlobReader, BlobStore};
use ownhealth_core::documents::{NewDocument, create_document};
use ownhealth_core::jobs::{EnqueueOptions, HandlerResult, Job, JobContext, JobError, enqueue};
use ownhealth_core::orthanc::lock_study;
use ownhealth_core::provenance::{SrcIn, create_src};
use ownhealth_core::review::{ReviewItem, ReviewItemIn, create_item};
use ownhealth_core::{Error, ids};
use ownhealth_ingest_pdf::classify::identity_match;
use serde_json::{Value, json};
use sqlx::PgConnection;
use uuid::Uuid;

use crate::orthanc::{Orthanc, OrthancError, SeriesInfo, Stored};
use crate::part10::{Header, Part10Error, encapsulated_document, is_part10, read_header};

/// The job kind.
pub const JOB: &str = "import.dicom";
/// The review kind of a study the import did not send.
pub const REVIEW_KIND: &str = "imaging_study";
/// One file larger than this is left out (counted as `too_large`).
pub const MAX_FILE_BYTES: u64 = 512 << 20;
/// A zip with more entries than this is refused.
pub const MAX_ENTRIES: usize = 100_000;
/// The header of a file is read from its first bytes; a header longer than this is read again, up to
/// [`MAX_HEADER_BYTES`].
const HEAD_BYTES: u64 = 1 << 20;
/// A header that does not end within this many bytes is counted as damaged, and the file is not sent.
const MAX_HEADER_BYTES: u64 = 16 << 20;
/// Modalities of objects that are not images: a study is named after its images when it has any.
const NOT_IMAGES: [&str; 5] = ["DOC", "SR", "KO", "PR", "OT"];

/// Why a study waits in review (`payload.reason`).
pub mod reason {
    /// The patient on the disc is not the profile's.
    pub const IDENTITY: &str = "identity_mismatch";
    /// The disc holds two or more patients, and this one is not clearly the profile's.
    pub const SECOND_PATIENT: &str = "second_patient";
    /// Another profile's record holds this StudyInstanceUID.
    pub const OTHER_PROFILE: &str = "other_profile";
    /// Two patients under one StudyInstanceUID: on the disc, or the disc's and one Orthanc holds.
    pub const UID_CONFLICT: &str = "uid_conflict";
    /// The reasons a person may accept: the study is then sent as it is.
    pub const ACCEPTABLE: [&str; 2] = [IDENTITY, SECOND_PATIENT];
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Patient {
    pub name: String,
    pub birth_date: String,
}

/// One study as the zip holds it.
#[derive(Debug, Clone, Default)]
pub struct StudyScan {
    /// By PatientID.
    pub patients: BTreeMap<String, Patient>,
    /// Instances per modality.
    pub modalities: BTreeMap<String, usize>,
    pub study_date: String,
    pub description: String,
    /// Instances per SeriesInstanceUID.
    pub series: BTreeMap<String, usize>,
    /// The zip entries of the study, in zip order.
    pub entries: Vec<usize>,
    /// Those that are Encapsulated PDF instances.
    pub pdfs: BTreeSet<usize>,
}

impl StudyScan {
    /// The modality the study is known by: its most common image modality, else its most common one.
    pub fn modality(&self) -> Option<String> {
        let best = |images: bool| {
            self.modalities
                .iter()
                .filter(|(m, _)| !m.is_empty() && NOT_IMAGES.contains(&m.as_str()) != images)
                .max_by_key(|(m, n)| (**n, std::cmp::Reverse(m.as_str())))
                .map(|(m, _)| m.clone())
        };
        best(true).or_else(|| best(false))
    }
}

/// What the walk of a zip found.
#[derive(Debug, Clone, Default)]
pub struct Scan {
    /// Files (folders left out).
    pub files: usize,
    pub not_dicom: usize,
    pub directories: usize,
    pub damaged: usize,
    pub too_large: usize,
    pub studies: BTreeMap<String, StudyScan>,
}

impl Scan {
    /// Every patient the disc holds, by PatientID.
    pub fn patients(&self) -> BTreeSet<&str> {
        self.studies
            .values()
            .flat_map(|s| s.patients.keys().map(String::as_str))
            .collect()
    }
}

fn read_entry(zip: &mut zip::ZipArchive<BlobReader>, i: usize, limit: u64) -> Result<Vec<u8>, String> {
    let entry = zip.by_index(i).map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    entry.take(limit).read_to_end(&mut out).map_err(|e| e.to_string())?;
    Ok(out)
}

/// Walks the zip (blocking: run it on a blocking thread).
pub fn scan(reader: BlobReader) -> Result<Scan, JobError> {
    let mut zip = zip::ZipArchive::new(reader)
        .map_err(|e| JobError::permanent(format!("the upload is not a zip that reads: {e}")))?;
    if zip.len() > MAX_ENTRIES {
        return Err(JobError::permanent(format!(
            "the upload holds {} files, more than {MAX_ENTRIES}",
            zip.len()
        )));
    }
    let mut s = Scan::default();
    for i in 0..zip.len() {
        let (is_dir, size) = match zip.by_index_raw(i) {
            Ok(e) => (e.is_dir(), e.size()),
            Err(_) => {
                s.files += 1;
                s.damaged += 1;
                continue;
            }
        };
        if is_dir {
            continue;
        }
        s.files += 1;
        if size > MAX_FILE_BYTES {
            s.too_large += 1;
            continue;
        }
        let Ok(mut head) = read_entry(&mut zip, i, HEAD_BYTES) else {
            s.damaged += 1;
            continue;
        };
        if !is_part10(&head) {
            s.not_dicom += 1;
            continue;
        }
        let mut header = read_header(&head);
        if matches!(header, Err(Part10Error::Corrupt(_))) && head.len() as u64 == HEAD_BYTES {
            // a header that does not end in the first bytes: read further, as far as the cap
            match read_entry(&mut zip, i, MAX_HEADER_BYTES) {
                Ok(more) => head = more,
                Err(_) => {
                    s.damaged += 1;
                    continue;
                }
            }
            header = read_header(&head);
        }
        match header {
            Ok(h) if h.is_directory() => s.directories += 1,
            Ok(h) => add(&mut s, i, h),
            Err(_) => s.damaged += 1,
        }
    }
    Ok(s)
}

fn add(s: &mut Scan, i: usize, h: Header) {
    let study = s.studies.entry(h.study_instance_uid.clone()).or_default();
    study.patients.entry(h.patient_id.clone()).or_insert_with(|| Patient {
        name: h.patient_name.clone(),
        birth_date: h.patient_birth_date.clone(),
    });
    *study.modalities.entry(h.modality.clone()).or_default() += 1;
    *study.series.entry(h.series_instance_uid.clone()).or_default() += 1;
    if study.study_date.is_empty() {
        study.study_date = h.study_date.clone();
    }
    if study.description.is_empty() {
        study.description = h.study_description.clone();
    }
    if h.is_pdf() {
        study.pdfs.insert(i);
    }
    study.entries.push(i);
}

/// `YYYYMMDD` as a date.
pub fn da(value: &str) -> Option<NaiveDate> {
    NaiveDate::parse_from_str(value.get(..8)?, "%Y%m%d").ok()
}

/// The import row the job reads.
struct ImportRow {
    id: Uuid,
    profile_id: Uuid,
    sha256: String,
    filename: Option<String>,
}

struct ProfileRow {
    household_id: Uuid,
    display_name: String,
    birth_date: Option<NaiveDate>,
}

/// The files of one study that did not reach Orthanc after the walk: refused by Orthanc, or damaged
/// on the second read.
#[derive(Debug, Default)]
struct Tally {
    refused_files: usize,
    damaged: usize,
}

/// How one study ended.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Outcome {
    Imported {
        instances: usize,
        reports: usize,
    },
    /// The profile holds it already: nothing sent.
    Already,
    /// The profile's record of it is retracted: nothing sent.
    Retracted,
    Review,
    /// Orthanc took none of its files.
    Refused,
}

pub(crate) fn job(ctx: JobContext, job: Job) -> BoxFuture<'static, HandlerResult> {
    Box::pin(run(ctx, job))
}

fn retry(e: OrthancError) -> JobError {
    JobError::typed("OrthancError", e.0)
}

/// Runs one `import.dicom` job. Payload: `import_id`, `profile_id`, and `accept` (StudyInstanceUIDs a
/// person accepted in review: only those are imported, whoever the disc names).
pub async fn run(ctx: JobContext, job: Job) -> HandlerResult {
    let import_id = job.uuid("import_id")?;
    let accepted: BTreeSet<String> = job
        .payload
        .get("accept")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect())
        .unwrap_or_default();
    let Some(config) = ctx.settings.orthanc.clone() else {
        return Err(JobError::retry(
            "imaging is off: the import waits for the imaging compose profile and OWNHEALTH_ORTHANC_URL",
        ));
    };
    let orthanc = Orthanc::new(config).map_err(retry)?;
    let row: Option<(Uuid, Vec<u8>, Option<String>)> =
        sqlx::query_as("select profile_id, zip_blob, filename from imaging_import where id = $1")
            .bind(import_id)
            .fetch_optional(&ctx.pool)
            .await?;
    let Some((profile_id, zip_blob, filename)) = row else {
        // the profile was deleted since: nothing to import
        return Ok(Some(json!({"gone": true})));
    };
    let import = ImportRow {
        id: import_id,
        profile_id,
        sha256: hex::encode(zip_blob),
        filename,
    };
    let profile: (Uuid, String, Option<NaiveDate>) =
        sqlx::query_as("select household_id, display_name, birth_date from profile where id = $1")
            .bind(profile_id)
            .fetch_one(&ctx.pool)
            .await?;
    let profile = ProfileRow {
        household_id: profile.0,
        display_name: profile.1,
        birth_date: profile.2,
    };
    let owner = BlobOwner::Profile(profile_id);
    let reader = ctx.blobs.open_seek(owner, &import.sha256).map_err(Error::from)?;
    let scan = tokio::task::spawn_blocking(move || scan(reader))
        .await
        .map_err(|e| JobError::retry(format!("the zip walk stopped: {e}")))??;
    let several = scan.patients().len() > 1;
    let (mut imported, mut already, mut review, mut refused) = (0, 0, 0, 0);
    let (mut instances, mut reports) = (0, 0);
    let mut tally = Tally::default();
    for (uid, study) in &scan.studies {
        if !accepted.is_empty() && !accepted.contains(uid) {
            continue;
        }
        let ok = accepted.contains(uid);
        let out = import_study(&ctx, &orthanc, &import, &profile, uid, study, several, ok, &mut tally).await?;
        match out {
            Outcome::Imported {
                instances: n,
                reports: r,
            } => {
                imported += 1;
                instances += n;
                reports += r;
            }
            Outcome::Already | Outcome::Retracted => already += 1,
            Outcome::Review => review += 1,
            Outcome::Refused => refused += 1,
        }
    }
    Ok(Some(json!({
        "files": scan.files,
        "not_dicom": scan.not_dicom,
        "directories": scan.directories,
        "damaged": scan.damaged + tally.damaged,
        "too_large": scan.too_large,
        "studies": scan.studies.len(),
        "imported": imported,
        "already": already,
        "in_review": review,
        "refused": refused,
        "refused_files": tally.refused_files,
        "instances": instances,
        "reports": reports,
    })))
}

/// The zip entries of one study, read on a blocking thread and handed over one at a time, so a
/// study of thousands of files holds one or two of them in memory.
fn read_study(
    blobs: BlobStore,
    owner: BlobOwner,
    sha256: String,
    entries: Vec<usize>,
) -> tokio::sync::mpsc::Receiver<(usize, Result<Vec<u8>, String>)> {
    let (tx, rx) = tokio::sync::mpsc::channel(2);
    tokio::task::spawn_blocking(move || {
        let mut zip = match blobs
            .open_seek(owner, &sha256)
            .map_err(|e| e.to_string())
            .and_then(|r| zip::ZipArchive::new(r).map_err(|e| e.to_string()))
        {
            Ok(z) => z,
            Err(e) => {
                let _ = tx.blocking_send((usize::MAX, Err(e)));
                return;
            }
        };
        for i in entries {
            let file = read_entry(&mut zip, i, MAX_FILE_BYTES + 1).and_then(|f| {
                if f.len() as u64 > MAX_FILE_BYTES {
                    Err("too large".into())
                } else {
                    Ok(f)
                }
            });
            if tx.blocking_send((i, file)).is_err() {
                return; // the import stopped
            }
        }
    });
    rx
}

/// The record of a StudyInstanceUID: profile, status, Orthanc id, date and description.
type RecordRow = (Uuid, String, Option<String>, Option<NaiveDate>, Option<String>);

/// Whether Orthanc holds every series of the disc's study, each with at least as many instances.
fn holds_all(held: &[SeriesInfo], study: &StudyScan) -> bool {
    study
        .series
        .iter()
        .all(|(uid, n)| held.iter().any(|s| s.uid == *uid && s.instances >= *n))
}

/// Puts the study in review, unsent, and ends its transaction.
async fn hold(
    mut tx: sqlx::Transaction<'static, sqlx::Postgres>,
    import: &ImportRow,
    profile: &ProfileRow,
    uid: &str,
    study: &StudyScan,
    why: &str,
) -> Result<Outcome, JobError> {
    let patient = study.patients.values().next().cloned().unwrap_or_default();
    to_review_item(&mut tx, import, profile, uid, study, &patient, why).await?;
    tx.commit().await?;
    Ok(Outcome::Review)
}

#[allow(clippy::too_many_arguments)]
async fn import_study(
    ctx: &JobContext,
    orthanc: &Orthanc,
    import: &ImportRow,
    profile: &ProfileRow,
    uid: &str,
    study: &StudyScan,
    several: bool,
    accepted: bool,
    tally: &mut Tally,
) -> Result<Outcome, JobError> {
    let mut tx = ctx.pool.begin().await?;
    lock_study(&mut tx, uid).await?;
    let record: Option<RecordRow> = sqlx::query_as(
        "select profile_id, status, orthanc_id, study_date, description from imaging_study \
         where study_instance_uid = $1 for update",
    )
    .bind(uid)
    .fetch_optional(&mut *tx)
    .await?;
    match &record {
        Some((p, ..)) if *p != import.profile_id => {
            return hold(tx, import, profile, uid, study, reason::OTHER_PROFILE).await;
        }
        Some((_, s, ..)) if s == "retracted" => return Ok(Outcome::Retracted),
        _ if study.patients.len() > 1 => return hold(tx, import, profile, uid, study, reason::UID_CONFLICT).await,
        _ => {}
    }
    // the profile's record of this study, when it has one
    let recorded = record.as_ref().filter(|r| r.2.is_some());
    let (patient_id, patient) = study
        .patients
        .iter()
        .next()
        .map(|(id, p)| (id.clone(), p.clone()))
        .unwrap_or_default();
    // What Orthanc holds under the UID: nothing, or this patient's study (the one the profile records,
    // or one an earlier run of this import sent before it stopped). Another patient's study under
    // the UID holds the disc back, whoever records it.
    for id in orthanc.studies_under(uid).await.map_err(retry)? {
        let held = orthanc.study(&id).await.map_err(retry)?;
        if held.patient_id != patient_id.trim() || held.study_instance_uid != uid {
            return hold(tx, import, profile, uid, study, reason::UID_CONFLICT).await;
        }
    }
    // A study the profile records already: a disc that adds nothing Orthanc lacks sends nothing.
    if let Some((_, _, Some(oid), ..)) = recorded
        && holds_all(&orthanc.series_of(oid).await.map_err(retry)?, study)
    {
        return Ok(Outcome::Already);
    }
    if !accepted {
        let name = patient.name.replace('^', " ");
        let identity = identity_match(
            Some(name.trim()),
            da(&patient.birth_date),
            &profile.display_name,
            profile.birth_date,
        );
        let why = match identity {
            "mismatch" => Some(reason::IDENTITY),
            "matched" => None,
            _ if several => Some(reason::SECOND_PATIENT),
            _ => None,
        };
        if let Some(why) = why {
            return hold(tx, import, profile, uid, study, why).await;
        }
    }
    let owner = BlobOwner::Profile(import.profile_id);
    let mut files = read_study(ctx.blobs.clone(), owner, import.sha256.clone(), study.entries.clone());
    let (mut study_ids, mut added, mut sent) = (BTreeSet::new(), Vec::new(), 0usize);
    let mut report: Option<Uuid> = None;
    let (mut reports, mut read) = (0, 0);
    while let Some((i, file)) = files.recv().await {
        read += 1;
        let file = match file {
            Ok(f) => f,
            Err(e) if i == usize::MAX => return Err(JobError::retry(format!("the stored zip does not open: {e}"))),
            Err(_) => {
                // damaged on the second read too (a CRC the first read did not reach)
                tally.damaged += 1;
                continue;
            }
        };
        if study.pdfs.contains(&i)
            && let Ok(Some(pdf)) = encapsulated_document(&file)
            && pdf.starts_with(b"%PDF-")
        {
            let doc = record_report(ctx, &mut tx, import, profile, study, pdf).await?;
            reports += 1;
            report = report.or(Some(doc));
        }
        match orthanc.store(file).await.map_err(retry)? {
            Stored::Kept { id, study: parent, new } => {
                sent += 1;
                study_ids.insert(parent);
                if new {
                    added.push(id);
                }
            }
            Stored::Refused => tally.refused_files += 1,
        }
    }
    if read != study.entries.len() {
        return Err(JobError::retry("the zip reader stopped before the study's last file"));
    }
    if sent == 0 {
        tx.rollback().await?;
        return Ok(Outcome::Refused);
    }
    // Orthanc must now hold exactly one study under the UID, the one every file landed in, with the
    // patient the files name. Anything else means Orthanc read the files otherwise than this import
    // did: take back what it added and leave the study to review.
    let held = orthanc.studies_under(uid).await.map_err(retry)?;
    let one = match (study_ids.len(), held.as_slice()) {
        (1, [id]) if study_ids.contains(id) => {
            let info = orthanc.study(id).await.map_err(retry)?;
            (info.patient_id == patient_id.trim() && info.study_instance_uid == uid).then(|| id.clone())
        }
        _ => None,
    };
    let Some(orthanc_id) = one else {
        tracing::warn!(
            studies = held.len(),
            "Orthanc filed a study's files otherwise than the import read them: taking them back"
        );
        for id in &added {
            orthanc.delete_instance(id).await.map_err(retry)?;
        }
        tx.rollback().await?;
        let mut tx = ctx.pool.begin().await?;
        lock_study(&mut tx, uid).await?;
        to_review_item(&mut tx, import, profile, uid, study, &patient, reason::UID_CONFLICT).await?;
        tx.commit().await?;
        return Ok(Outcome::Review);
    };
    for series in study.series.keys() {
        orthanc.warm_series(uid, series).await.map_err(retry)?;
    }
    // the record counts what Orthanc holds, not what the disc listed
    let series = orthanc.series_of(&orthanc_id).await.map_err(retry)?;
    match recorded {
        None => record_study(&mut tx, import, uid, study, &orthanc_id, report, &series).await?,
        Some((_, _, _, date, description)) => {
            let date = (*date).or_else(|| da(&study.study_date));
            let description = description
                .clone()
                .or_else(|| Some(study.description.clone()).filter(|d| !d.is_empty()));
            let fhir = study_fhir(uid, date, description.as_deref(), &series);
            sqlx::query(
                "update imaging_study set orthanc_id = $3, fhir = $4, \
                 report_document_id = coalesce(report_document_id, $5) \
                 where study_instance_uid = $1 and profile_id = $2",
            )
            .bind(uid)
            .bind(import.profile_id)
            .bind(&orthanc_id)
            .bind(fhir)
            .bind(report)
            .execute(&mut *tx)
            .await?;
        }
    }
    tx.commit().await?;
    if recorded.is_some() && added.is_empty() {
        return Ok(Outcome::Already);
    }
    Ok(Outcome::Imported {
        instances: sent,
        reports,
    })
}

/// An Encapsulated PDF of the study: stored for the profile and filed as an `imaging_report` document
/// (channel `imaging`), which the PDF importer then reads. Returns its document id.
async fn record_report(
    ctx: &JobContext,
    conn: &mut PgConnection,
    import: &ImportRow,
    profile: &ProfileRow,
    study: &StudyScan,
    pdf: Vec<u8>,
) -> Result<Uuid, JobError> {
    let blobs = ctx.blobs.clone();
    let owner = BlobOwner::Profile(import.profile_id);
    let size = i64::try_from(pdf.len()).unwrap_or(i64::MAX);
    let sha = tokio::task::spawn_blocking(move || blobs.put_bytes(owner, &pdf))
        .await
        .map_err(|e| JobError::retry(format!("storing a report stopped: {e}")))?
        .map_err(Error::from)?;
    let name = report_name(study);
    let r = create_document(
        &mut *conn,
        &NewDocument {
            filename: Some(&name),
            channel: "imaging",
            doc_type: Some("imaging_report"),
            ..NewDocument::upload(
                profile.household_id,
                Some(import.profile_id),
                &sha,
                "application/pdf",
                size,
            )
        },
    )
    .await?;
    if r.created || r.reactivated {
        let mut payload = json!({
            "document_id": r.id.to_string(),
            "profile_id": import.profile_id.to_string(),
            "household_id": profile.household_id.to_string(),
        });
        let mut key = format!("import.pdf:{}", r.id);
        if r.reactivated {
            payload["force"] = json!(true);
            key = format!("{key}:reupload:{}", ids::token_hex(4));
        }
        enqueue(&mut *conn, "import.pdf", &payload, EnqueueOptions::dedupe(&key)).await?;
    }
    Ok(r.id)
}

/// The report's file name: the study's date and description, as plain characters.
fn report_name(study: &StudyScan) -> String {
    let words: String = study
        .description
        .chars()
        .map(|c| if c.is_alphanumeric() || c == '-' { c } else { ' ' })
        .collect();
    let words = words.split_whitespace().collect::<Vec<_>>().join("-");
    let parts: Vec<&str> = [study.study_date.as_str(), words.as_str()]
        .into_iter()
        .filter(|p| !p.is_empty())
        .collect();
    let base: String = if parts.is_empty() {
        "imaging-report".into()
    } else {
        parts.join("-")
    };
    format!("{}.pdf", base.chars().take(120).collect::<String>())
}

/// The study on the record: `imaging_study`, status current, with its src and a FHIR ImagingStudy.
async fn record_study(
    conn: &mut PgConnection,
    import: &ImportRow,
    uid: &str,
    study: &StudyScan,
    orthanc_id: &str,
    report: Option<Uuid>,
    series: &[SeriesInfo],
) -> Result<(), JobError> {
    let modality = study.modality();
    let description = Some(study.description.clone()).filter(|d| !d.is_empty());
    let date = da(&study.study_date);
    let label = import
        .filename
        .clone()
        .filter(|f| !f.is_empty())
        .unwrap_or_else(|| "DICOM".into());
    let src = create_src(
        &mut *conn,
        &SrcIn {
            profile_id: Some(import.profile_id),
            import_ref: Some(format!("imaging_import/{}#{uid}", import.id)),
            label: Some(label.chars().take(120).collect()),
            ..SrcIn::new("dicom")
        },
    )
    .await?;
    let fhir = study_fhir(uid, date, description.as_deref(), series);
    sqlx::query(
        "insert into imaging_study (profile_id, study_instance_uid, orthanc_id, modality, study_date, description, \
         report_document_id, status, src_id, import_id, fhir) \
         values ($1, $2, $3, $4, $5, $6, $7, 'current', $8, $9, $10) \
         on conflict (study_instance_uid) do update set orthanc_id = excluded.orthanc_id, \
         modality = coalesce(excluded.modality, imaging_study.modality), \
         study_date = coalesce(excluded.study_date, imaging_study.study_date), \
         description = coalesce(excluded.description, imaging_study.description), \
         report_document_id = coalesce(excluded.report_document_id, imaging_study.report_document_id), \
         src_id = excluded.src_id, import_id = excluded.import_id, fhir = excluded.fhir \
         where imaging_study.profile_id = excluded.profile_id and imaging_study.orthanc_id is null",
    )
    .bind(import.profile_id)
    .bind(uid)
    .bind(orthanc_id)
    .bind(modality)
    .bind(date)
    .bind(description)
    .bind(report)
    .bind(src)
    .bind(import.id)
    .bind(fhir)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// The study as a FHIR ImagingStudy, with the series and instances Orthanc holds.
fn study_fhir(uid: &str, date: Option<NaiveDate>, description: Option<&str>, series: &[SeriesInfo]) -> Value {
    let coding = |m: &str| json!({"system": "http://dicom.nema.org/resources/ontology/DCM", "code": m});
    let modalities: BTreeSet<&str> = series
        .iter()
        .map(|s| s.modality.as_str())
        .filter(|m| !m.is_empty())
        .collect();
    let mut fhir = json!({
        "resourceType": "ImagingStudy",
        "status": "available",
        "identifier": [{"system": "urn:dicom:uid", "value": format!("urn:oid:{uid}")}],
        "modality": modalities.into_iter().map(coding).collect::<Vec<_>>(),
        "numberOfSeries": series.len(),
        "numberOfInstances": series.iter().map(|s| s.instances).sum::<usize>(),
        "series": series.iter().map(|s| json!({"uid": s.uid, "numberOfInstances": s.instances})).collect::<Vec<_>>(),
    });
    if let Some(d) = date {
        fhir["started"] = json!(d.to_string());
    }
    if let Some(d) = description {
        fhir["description"] = json!(d);
    }
    fhir
}

/// One open review item per study of an upload: a second run of the same upload adds none.
#[allow(clippy::too_many_arguments)]
async fn to_review_item(
    conn: &mut PgConnection,
    import: &ImportRow,
    profile: &ProfileRow,
    uid: &str,
    study: &StudyScan,
    patient: &Patient,
    why: &str,
) -> Result<(), JobError> {
    let open: bool = sqlx::query_scalar(
        "select exists (select 1 from review_item where kind = $1 and status = 'open' and ref_table = 'imaging_import' \
         and ref_id = $2 and payload->>'study_uid' = $3)",
    )
    .bind(REVIEW_KIND)
    .bind(import.id)
    .bind(uid)
    .fetch_one(&mut *conn)
    .await?;
    if open {
        return Ok(());
    }
    let payload = json!({
        "import_id": import.id,
        "study_uid": uid,
        "reason": why,
        "patient_name": patient.name.replace('^', " ").trim(),
        "patient_birth_date": da(&patient.birth_date).map(|d| d.to_string()),
        "modality": study.modality(),
        "study_date": da(&study.study_date).map(|d| d.to_string()),
        "description": Some(study.description.as_str()).filter(|d| !d.is_empty()),
        "series": study.series.len(),
        "instances": study.entries.len(),
        "filename": import.filename,
    });
    create_item(
        &mut *conn,
        &ReviewItemIn {
            kind: REVIEW_KIND.into(),
            household_id: Some(profile.household_id),
            profile_id: Some(import.profile_id),
            payload,
            reasons: vec![why.to_string()],
            ref_table: Some("imaging_import".into()),
            ref_id: Some(import.id),
            ..ReviewItemIn::default()
        },
    )
    .await?;
    Ok(())
}

/// Accepting an `imaging_study` item: a study whose patient did not match the profile, or that came on
/// a disc with other patients, is imported as it is. The other reasons cannot be accepted: another
/// profile or another patient holds the UID. The refusal names neither, since the item's reviewer may
/// see neither.
pub async fn accept(
    conn: &mut PgConnection,
    item: &ReviewItem,
    _user: Uuid,
    _edits: Option<&Value>,
) -> Result<Vec<Uuid>, Error> {
    let why = item.payload.get("reason").and_then(Value::as_str).unwrap_or_default();
    if !reason::ACCEPTABLE.contains(&why) {
        return Err(Error::Review(
            "This study cannot be added to this profile. Dismiss it instead.".into(),
        ));
    }
    let (Some(import_id), Some(uid), Some(profile_id)) = (
        item.ref_id,
        item.payload.get("study_uid").and_then(Value::as_str),
        item.profile_id,
    ) else {
        return Err(Error::Review("this item names no upload and study".into()));
    };
    let filename: Option<String> = sqlx::query_scalar("select filename from imaging_import where id = $1")
        .bind(import_id)
        .fetch_optional(&mut *conn)
        .await?
        .flatten();
    let payload = json!({
        "import_id": import_id.to_string(),
        "profile_id": profile_id.to_string(),
        "filename": filename,
        "accept": [uid],
    });
    let key = format!("{JOB}:{import_id}:accept:{uid}");
    enqueue(&mut *conn, JOB, &payload, EnqueueOptions::dedupe(&key)).await?;
    Ok(Vec::new())
}
