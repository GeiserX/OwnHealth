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
use std::fs::File;
use std::io::{Read, Seek, Write};
use std::path::PathBuf;

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
use sqlx::{PgConnection, Postgres, Transaction};
use uuid::Uuid;

use crate::orthanc::{FILE_CHUNK, Orthanc, OrthancError, SeriesInfo, Stored};
use crate::part10::{Header, Part10Error, encapsulated_file, is_part10, read_header};

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
    /// Files of one study disagree about the patient identity.
    pub const IDENTITY_CONFLICT: &str = "identity_conflict";
    /// The declared encoding or text values cannot be decoded safely.
    pub const UNSUPPORTED_CHARSET: &str = "unsupported_charset";
    /// The reasons a person may accept: the study is then sent as it is.
    pub const ACCEPTABLE: [&str; 2] = [IDENTITY, SECOND_PATIENT];
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Patient {
    names: BTreeSet<String>,
    birth_dates: BTreeSet<String>,
    issuers: BTreeSet<String>,
}

impl Patient {
    fn name(&self) -> &str {
        self.names.first().map_or("", String::as_str)
    }

    fn birth_date(&self) -> &str {
        self.birth_dates.first().map_or("", String::as_str)
    }

    fn conflicting(&self) -> bool {
        self.names.len() > 1 || self.birth_dates.len() > 1 || self.issuers.len() > 1
    }
}

/// One study as the zip holds it.
#[derive(Debug, Clone, Default)]
pub struct StudyScan {
    /// By PatientID.
    pub patients: BTreeMap<String, Patient>,
    pub unsupported_charset: bool,
    /// Instances per modality.
    pub modalities: BTreeMap<String, usize>,
    pub study_date: String,
    pub description: String,
    /// Instances per SeriesInstanceUID.
    pub series: BTreeMap<String, usize>,
    sops: BTreeSet<(String, String)>,
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
    let is_pdf = h.is_pdf();
    let study = s.studies.entry(h.study_instance_uid).or_default();
    study.unsupported_charset |= h.unsupported_charset;
    if !h.unsupported_charset {
        let patient = study.patients.entry(h.patient_id).or_default();
        for (evidence, value) in [
            (&mut patient.names, h.patient_name),
            (&mut patient.birth_dates, h.patient_birth_date),
            (&mut patient.issuers, h.patient_id_issuer),
        ] {
            if !value.is_empty() {
                evidence.insert(value);
            }
        }
    }
    *study.modalities.entry(h.modality.clone()).or_default() += 1;
    if study
        .sops
        .insert((h.series_instance_uid.clone(), h.sop_instance_uid.clone()))
    {
        *study.series.entry(h.series_instance_uid.clone()).or_default() += 1;
    }
    if study.study_date.is_empty() {
        study.study_date = h.study_date.clone();
    }
    if study.description.is_empty() {
        study.description = h.study_description.clone();
    }
    if is_pdf {
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
    submitted_files: usize,
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
    /// The profile holds it already. A retry may still register a report.
    Already {
        reports: usize,
    },
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
            Outcome::Already { reports: r } => {
                already += 1;
                reports += r;
            }
            Outcome::Retracted => already += 1,
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
        "submitted_files": tally.submitted_files,
        "refused_files": tally.refused_files,
        "instances": instances,
        "reports": reports,
    })))
}

/// A seekable archive and the private directory used to stage one instance at a time.
struct StudyFiles {
    zip: zip::ZipArchive<BlobReader>,
    dir: PathBuf,
}

impl StudyFiles {
    async fn open(blobs: &BlobStore, import: &ImportRow) -> Result<Self, JobError> {
        let blobs = blobs.clone();
        let owner = BlobOwner::Profile(import.profile_id);
        let sha = import.sha256.clone();
        tokio::task::spawn_blocking(move || {
            let reader = blobs.open_seek(owner, &sha).map_err(Error::from)?;
            let zip = zip::ZipArchive::new(reader)
                .map_err(|e| JobError::retry(format!("the stored zip does not open: {e}")))?;
            let dir = blobs
                .staging_path("upload-imaging-instance")
                .map_err(Error::from)?
                .parent()
                .ok_or_else(|| JobError::retry("no imaging staging directory"))?
                .to_path_buf();
            Ok(Self { zip, dir })
        })
        .await
        .map_err(|e| JobError::retry(format!("the zip reader stopped: {e}")))?
    }

    /// Stage one entry before POST so a bad ZIP CRC can never send part of the file to Orthanc.
    /// The private temporary file is unlinked and the producer makes no other file in parallel.
    fn stage(&mut self, i: usize) -> Result<Option<(File, u64)>, JobError> {
        let Ok(entry) = self.zip.by_index(i) else {
            return Ok(None);
        };
        let expected = entry.size();
        let mut entry = entry.take(MAX_FILE_BYTES + 1);
        let mut file = tempfile::tempfile_in(&self.dir).map_err(Error::from)?;
        let mut buf = [0; FILE_CHUNK];
        let mut size = 0;
        loop {
            let Ok(n) = entry.read(&mut buf) else {
                return Ok(None);
            };
            if n == 0 {
                break;
            }
            size += n as u64;
            if size > MAX_FILE_BYTES {
                return Ok(None);
            }
            file.write_all(&buf[..n]).map_err(Error::from)?;
        }
        if size != expected {
            return Ok(None);
        }
        file.rewind().map_err(Error::from)?;
        Ok(Some((file, size)))
    }

    async fn stage_instance(self, i: usize) -> Result<(Self, Option<(File, u64)>), JobError> {
        tokio::task::spawn_blocking(move || {
            let mut files = self;
            let file = files.stage(i)?;
            Ok((files, file))
        })
        .await
        .map_err(|e| JobError::retry(format!("the zip reader stopped: {e}")))?
    }
}

#[derive(sqlx::FromRow)]
struct RecordRow {
    profile_id: Uuid,
    status: String,
    orthanc_id: Option<String>,
    study_date: Option<NaiveDate>,
    description: Option<String>,
    report_document_id: Option<Uuid>,
    fhir: Option<Value>,
}

/// Whether Orthanc holds every series of the disc's study, each with at least as many instances.
fn holds_all(held: &[SeriesInfo], study: &StudyScan) -> bool {
    study
        .series
        .iter()
        .all(|(uid, n)| held.iter().any(|s| s.uid == *uid && s.instances >= *n))
}

/// Puts the study in review, unsent, and ends its transaction.
async fn hold(
    mut tx: Transaction<'static, Postgres>,
    import: &ImportRow,
    profile: &ProfileRow,
    uid: &str,
    study: &StudyScan,
    why: &str,
) -> Result<Outcome, JobError> {
    let unknown = Patient::default();
    let patient = if matches!(why, reason::IDENTITY_CONFLICT | reason::UNSUPPORTED_CHARSET) {
        &unknown
    } else {
        study.patients.values().next().unwrap_or(&unknown)
    };
    to_review_item(&mut tx, import, profile, uid, study, patient, why).await?;
    tx.commit().await?;
    Ok(Outcome::Review)
}

/// Finish committed cleanup under the study lock before any new POST. A failed delete leaves
/// the journal intact. Orthanc treats an already-deleted ID as absent on retry.
async fn complete_cleanup(
    conn: &mut PgConnection,
    orthanc: &Orthanc,
    import_id: Uuid,
    uid: &str,
) -> Result<bool, JobError> {
    let pending: Vec<(Uuid, Vec<String>)> =
        sqlx::query_as("select import_id, instance_ids from imaging_import_cleanup where study_instance_uid = $1")
            .bind(uid)
            .fetch_all(&mut *conn)
            .await?;
    let mut needs_hold = false;
    for (owner, ids) in pending {
        for id in &ids {
            orthanc.delete_instance(id).await.map_err(retry)?;
        }
        if owner == import_id {
            sqlx::query("delete from imaging_import_cleanup where study_instance_uid = $1 and import_id = $2")
                .bind(uid)
                .bind(owner)
                .execute(&mut *conn)
                .await?;
            needs_hold = true;
        } else if !ids.is_empty() {
            sqlx::query("update imaging_import_cleanup set instance_ids = '{}' where study_instance_uid = $1 and import_id = $2")
                .bind(uid).bind(owner).execute(&mut *conn).await?;
        }
    }
    Ok(needs_hold)
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
    if complete_cleanup(&mut tx, orthanc, import.id, uid).await? {
        return hold(tx, import, profile, uid, study, reason::UID_CONFLICT).await;
    }
    let record: Option<RecordRow> = sqlx::query_as(
        "select profile_id, status, orthanc_id, study_date, description, report_document_id, fhir \
         from imaging_study where study_instance_uid = $1 for update",
    )
    .bind(uid)
    .fetch_optional(&mut *tx)
    .await?;
    if let Some(r) = &record {
        if r.profile_id != import.profile_id {
            return hold(tx, import, profile, uid, study, reason::OTHER_PROFILE).await;
        }
        if r.status == "retracted" {
            tx.commit().await?;
            return Ok(Outcome::Retracted);
        }
    }
    if study.unsupported_charset {
        return hold(tx, import, profile, uid, study, reason::UNSUPPORTED_CHARSET).await;
    }
    if study.patients.len() > 1 {
        return hold(tx, import, profile, uid, study, reason::UID_CONFLICT).await;
    }
    let (patient_id, patient) = study
        .patients
        .first_key_value()
        .ok_or_else(|| JobError::retry("the study scan holds no patient evidence"))?;
    if patient.conflicting() {
        return hold(tx, import, profile, uid, study, reason::IDENTITY_CONFLICT).await;
    }
    let current = orthanc.studies_under(uid).await.map_err(retry)?;
    if current.len() > 1 {
        return hold(tx, import, profile, uid, study, reason::UID_CONFLICT).await;
    }
    if let Some(id) = current.first() {
        let held = orthanc.study(id).await.map_err(retry)?;
        if held.patient_id != *patient_id || held.study_instance_uid != uid {
            return hold(tx, import, profile, uid, study, reason::UID_CONFLICT).await;
        }
    }
    let recorded = record.as_ref().filter(|r| r.orthanc_id.is_some());
    let mut already_held = None;
    if let (Some(r), [id]) = (recorded, current.as_slice())
        && r.orthanc_id.as_ref() == Some(id)
    {
        let series = orthanc.series_of(id).await.map_err(retry)?;
        if holds_all(&series, study) {
            already_held = Some((id.clone(), series));
        }
    }

    let mut kept = BTreeSet::new();
    let mut added = Vec::new();
    let (orthanc_id, series) = if let Some(held) = already_held {
        // A retry can find its instances in Orthanc before the clinical transaction committed.
        held
    } else {
        if !accepted {
            let name = patient.name().replace('^', " ");
            let identity = identity_match(
                Some(name.trim()),
                da(patient.birth_date()),
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
        let mut files = StudyFiles::open(&ctx.blobs, import).await?;
        let mut study_ids = BTreeSet::new();
        for &i in &study.entries {
            let (archive, staged) = files.stage_instance(i).await?;
            files = archive;
            let Some((file, size)) = staged else {
                tally.damaged += 1;
                continue;
            };
            tally.submitted_files += 1;
            match orthanc.store_file(file, size).await.map_err(retry)? {
                Stored::Kept { id, study: parent, new } => {
                    study_ids.insert(parent);
                    if new {
                        added.push(id.clone());
                    }
                    kept.insert(id);
                }
                Stored::Refused => tally.refused_files += 1,
            }
        }
        if kept.is_empty() {
            tx.commit().await?;
            return Ok(Outcome::Refused);
        }
        let held = orthanc.studies_under(uid).await.map_err(retry)?;
        let one = match held.as_slice() {
            [id] if study_ids.len() == 1 && study_ids.contains(id) => {
                let info = orthanc.study(id).await.map_err(retry)?;
                (info.patient_id == *patient_id && info.study_instance_uid == uid).then(|| id.clone())
            }
            _ => None,
        };
        let Some(id) = one else {
            // Commit the IDs before the first DELETE. Any writer entering through this UID's lock
            // will finish the cleanup before it can send instances of the study.
            sqlx::query(
                "insert into imaging_import_cleanup (study_instance_uid, import_id, instance_ids) \
                 values ($1, $2, $3)",
            )
            .bind(uid)
            .bind(import.id)
            .bind(&added)
            .execute(&mut *tx)
            .await?;
            tx.commit().await?;
            let mut tx = ctx.pool.begin().await?;
            lock_study(&mut tx, uid).await?;
            complete_cleanup(&mut tx, orthanc, import.id, uid).await?;
            return hold(tx, import, profile, uid, study, reason::UID_CONFLICT).await;
        };
        let series = orthanc.series_of(&id).await.map_err(retry)?;
        (id, series)
    };

    let date = recorded.and_then(|r| r.study_date).or_else(|| da(&study.study_date));
    let description = recorded
        .and_then(|r| r.description.as_deref())
        .or_else(|| Some(study.description.as_str()).filter(|d| !d.is_empty()));
    let fhir = study_fhir(uid, date, description, &series);
    let counts_changed =
        recorded.is_none_or(|r| r.fhir.as_ref() != Some(&fhir) || r.orthanc_id.as_ref() != Some(&orthanc_id));
    if counts_changed || !added.is_empty() {
        for stored in series.iter().filter(|s| study.series.contains_key(&s.uid)) {
            orthanc.warm_series(uid, &stored.uid).await.map_err(retry)?;
        }
    }
    let (report, reports) = if !study.pdfs.is_empty()
        && (counts_changed || !kept.is_empty() || recorded.is_some_and(|r| r.report_document_id.is_none()))
    {
        record_reports(ctx, &mut tx, import, profile, study).await?
    } else {
        (None, 0)
    };
    match recorded {
        None => record_study(&mut tx, import, uid, study, &orthanc_id, report, &series).await?,
        Some(r) if counts_changed || (r.report_document_id.is_none() && report.is_some()) => {
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
        Some(_) => {}
    }
    tx.commit().await?;
    if recorded.is_some() && added.is_empty() {
        return Ok(Outcome::Already { reports });
    }
    Ok(Outcome::Imported {
        instances: kept.len(),
        reports,
    })
}

/// Read the reports after verifying Orthanc's study and warming its series. Release each spool
/// before opening the next, and pass its PDF value through the streaming blob API.
async fn record_reports(
    ctx: &JobContext,
    conn: &mut PgConnection,
    import: &ImportRow,
    profile: &ProfileRow,
    study: &StudyScan,
) -> Result<(Option<Uuid>, usize), JobError> {
    let mut files = StudyFiles::open(&ctx.blobs, import).await?;
    let owner = BlobOwner::Profile(import.profile_id);
    let mut first = None;
    let mut reports = 0;
    for &i in &study.pdfs {
        let blobs = ctx.blobs.clone();
        let (archive, stored) = tokio::task::spawn_blocking(move || {
            let pdf = match files.stage(i)? {
                Some((file, _)) => match encapsulated_file(file, &files.dir) {
                    Ok(pdf) => pdf,
                    Err(e @ Part10Error::Staging(_)) => return Err(JobError::retry(e.to_string())),
                    Err(_) => None,
                },
                None => None,
            };
            let stored = pdf
                .map(|p| blobs.put_reader(owner, p))
                .transpose()
                .map_err(Error::from)?;
            Ok::<_, JobError>((files, stored))
        })
        .await
        .map_err(|e| JobError::retry(format!("storing a report stopped: {e}")))??;
        files = archive;
        if let Some((sha, size)) = stored {
            let id = record_report(conn, import, profile, study, &sha, size).await?;
            first.get_or_insert(id);
            reports += 1;
        }
    }
    Ok((first, reports))
}

/// An Encapsulated PDF of the study: stored for the profile and filed as an `imaging_report` document
/// (channel `imaging`), which the PDF importer then reads. Returns its document id.
async fn record_report(
    conn: &mut PgConnection,
    import: &ImportRow,
    profile: &ProfileRow,
    study: &StudyScan,
    sha: &str,
    size: u64,
) -> Result<Uuid, JobError> {
    let size = i64::try_from(size).unwrap_or(i64::MAX);
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
                sha,
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

/// One open item per study of an upload. A later run can change its reason.
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
    let open: Option<Uuid> = sqlx::query_scalar(
        "select id from review_item where kind = $1 and status = 'open' and ref_table = 'imaging_import' \
         and ref_id = $2 and payload->>'study_uid' = $3 order by created_at, id limit 1 for update",
    )
    .bind(REVIEW_KIND)
    .bind(import.id)
    .bind(uid)
    .fetch_optional(&mut *conn)
    .await?;
    let payload = json!({
        "import_id": import.id,
        "study_uid": uid,
        "reason": why,
        "patient_name": patient.name().replace('^', " ").trim(),
        "patient_birth_date": da(patient.birth_date()).map(|d| d.to_string()),
        "modality": study.modality(),
        "study_date": da(&study.study_date).map(|d| d.to_string()),
        "description": Some(study.description.as_str()).filter(|d| !d.is_empty()),
        "series": study.series.len(),
        "instances": study.entries.len(),
        "filename": import.filename,
    });
    let reasons = vec![why.to_string()];
    if let Some(id) = open {
        sqlx::query(
            "update review_item set payload = $2, reasons = $3 \
             where id = $1 and (payload is distinct from $2 or reasons is distinct from $3)",
        )
        .bind(id)
        .bind(payload)
        .bind(&reasons)
        .execute(&mut *conn)
        .await?;
        return Ok(());
    }
    create_item(
        &mut *conn,
        &ReviewItemIn {
            kind: REVIEW_KIND.into(),
            household_id: Some(profile.household_id),
            profile_id: Some(import.profile_id),
            payload,
            reasons,
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
