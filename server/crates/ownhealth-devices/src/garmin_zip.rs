//! Garmin account history from one Export Your Data archive. JSON members are matched by their
//! basenames and UploadedFiles ZIPs contain the FIT activity files. Reading uses archive indexes
//! and a private temporary file for each supported inner ZIP. Member names are never disk paths.
//! The stored wearables document contains only projected readings and their archive provenance.
//! A run with a `document_id` imports that stored document again (a retraction of another export, or
//! a move to another profile), as the zip itself is not kept.

use std::io::{self, BufRead, Read, Seek, Write};

use chrono::{DateTime, Duration, NaiveDate, NaiveDateTime, Utc};
use ownhealth_core::blobs::BlobOwner;
use ownhealth_core::documents::{GARMIN_READINGS_MIME, GarminMember, WEARABLES_DOC_TYPE};
use ownhealth_core::ingest::ImportSummary;
use ownhealth_core::jobs::{self, Job, JobContext, JobError};
use ownhealth_core::zip_directory::{self, DIRECTORY_LIMIT, GARMIN_MAX_FILES, GARMIN_MAX_NAME_BYTES};
use ownhealth_core::{Error, Result};
use ownhealth_ingest_structured::fhir::mark_imported;
use ownhealth_ingest_structured::jobs::document_of_type;
use ownhealth_ingest_structured::retract::{lock_export_of, lock_exports};
use ownhealth_ingest_structured::wearables::METRICS;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::PgConnection;
use uuid::Uuid;
use zip::ZipArchive;
use zip::result::ZipError;

use crate::store::{self, Conflict, Sample, Touched, Written};
use crate::{fit, strava_zip, takeout_fitbit};

pub const JOB: &str = "import.garmin_zip";
pub const PROVIDER: &str = "garmin_export";

const MAX_MEMBERS: usize = GARMIN_MAX_FILES;
const MAX_NAMES_BYTES: usize = GARMIN_MAX_NAME_BYTES;
pub(crate) const MAX_EXPANDED: u64 = 4 * 1024 * 1024 * 1024;
pub(crate) const MAX_JSON: u64 = 16 * 1024 * 1024;
const MAX_FIT_ZIP: u64 = 1024 * 1024 * 1024;
pub(crate) const MAX_READINGS: usize = 500_000;

const UNREADABLE: &str = "garmin_zip_unreadable";
const LIMIT: &str = "garmin_zip_limit";
/// The reason a file past the expanded-size or reading budget is listed with: the files before it
/// still import.
pub(crate) const OVER_BUDGET: &str = "limit";
/// What a member name shows instead of the account's email address.
const EMAIL: &str = "<email>";

/// The member count and the name bytes refuse the whole archive (they bound the work of listing it).
/// The expanded bytes and the readings are spent by the files the import reads: a file that does not
/// fit is listed, and the files before it still import.
pub(crate) struct Budget {
    limit: &'static str,
    members: usize,
    expanded: u64,
    names_bytes: usize,
    readings: usize,
}

impl Budget {
    pub(crate) fn new(limit: &'static str) -> Self {
        Self {
            limit,
            members: 0,
            expanded: 0,
            names_bytes: 0,
            readings: 0,
        }
    }

    pub(crate) fn remaining_expanded(&self) -> u64 {
        MAX_EXPANDED - self.expanded
    }

    pub(crate) fn add_members(&mut self, count: usize) -> Result<()> {
        self.members += count;
        if self.members > MAX_MEMBERS {
            return Err(Error::Upload(self.limit.into()));
        }
        Ok(())
    }

    pub(crate) fn add_name(&mut self, name_bytes: usize) -> Result<()> {
        self.names_bytes = self.names_bytes.saturating_add(name_bytes);
        if self.names_bytes > MAX_NAMES_BYTES {
            return Err(Error::Upload(self.limit.into()));
        }
        Ok(())
    }

    /// Spend `size` expanded bytes when they fit; false (nothing spent) when they do not.
    pub(crate) fn spend_expanded(&mut self, size: u64) -> bool {
        match self.expanded.checked_add(size).filter(|n| *n <= MAX_EXPANDED) {
            Some(n) => {
                self.expanded = n;
                true
            }
            None => false,
        }
    }

    /// Spend `count` readings when they fit; false (nothing spent) when they do not.
    fn spend_readings(&mut self, count: usize) -> bool {
        match self.readings.checked_add(count).filter(|n| *n <= MAX_READINGS) {
            Some(n) => {
                self.readings = n;
                true
            }
            None => false,
        }
    }
}

#[derive(Serialize)]
struct Omission {
    member: String,
    reason: &'static str,
}

struct Readings {
    member: String,
    fit: bool,
    samples: Vec<Sample>,
}

#[derive(Default)]
pub(crate) struct Report {
    files_imported: usize,
    not_imported: Vec<Omission>,
    pub(crate) gps_left_out: bool,
    readings: Vec<Readings>,
    /// Rows of a read file whose time could not be read or placed (Fitbit Takeout).
    pub(crate) rows_left_out: usize,
}

impl Report {
    /// The details of an archive whose files hold rows of readings (Fitbit Takeout).
    pub(crate) fn archive_details(&self) -> Value {
        json!({
            "files_imported": self.files_imported,
            "not_imported": self.not_imported,
            "rows_left_out": self.rows_left_out,
        })
    }

    pub(crate) fn add_member(&mut self, member: String, samples: Vec<Sample>) {
        self.files_imported += 1;
        self.readings.push(Readings {
            member,
            fit: false,
            samples,
        });
    }

    pub(crate) fn omit(&mut self, member: String, reason: &'static str) {
        self.not_imported.push(Omission { member, reason });
    }

    pub(crate) fn add(
        &mut self,
        budget: &mut Budget,
        member: String,
        fit: bool,
        samples: std::result::Result<Vec<Sample>, &'static str>,
    ) {
        match samples {
            Ok(samples) if samples.is_empty() => self.omit(member, "no_readings"),
            Ok(samples) if !budget.spend_readings(samples.len()) => self.omit(member, OVER_BUDGET),
            Ok(samples) => {
                self.files_imported += 1;
                self.readings.push(Readings { member, fit, samples });
            }
            Err(reason) => self.omit(member, reason),
        }
    }

    fn details(&self) -> Value {
        json!({
            "files_imported": self.files_imported,
            "gps_left_out": self.gps_left_out,
            "not_imported": self.not_imported,
        })
    }
}

enum MemberKind {
    Json(GarminMember),
    FitArchive,
    Fit,
}

impl MemberKind {
    fn detect(name: &str, inside_fit_zip: bool) -> Option<Self> {
        if inside_fit_zip {
            return name.to_ascii_lowercase().ends_with(".fit").then_some(Self::Fit);
        }
        Some(match GarminMember::from_name(name)? {
            GarminMember::FitZip => Self::FitArchive,
            kind => Self::Json(kind),
        })
    }

    fn cap(&self) -> u64 {
        match self {
            Self::Json(_) => MAX_JSON,
            Self::FitArchive => MAX_FIT_ZIP,
            Self::Fit => fit::MAX_BYTES,
        }
    }
}

pub(crate) fn unsafe_name(name: &str) -> bool {
    name.is_empty()
        || name.starts_with(['/', '\\'])
        || name.contains(':')
        || name.chars().any(char::is_control)
        || name.split(['/', '\\']).any(|part| part == "..")
}

/// Garmin names some files after the account's email address (`<email>_0_summarizedActivities.json`,
/// `<email>_<activity id>.fit`). A path part shaped like `name@domain.tld` up to the next `_` is
/// replaced, so the address is never stored or shown.
pub(crate) fn hide_email(name: &str) -> String {
    name.split_inclusive(['/', '\\'])
        .map(|part| {
            let Some(at) = part.find('@').filter(|at| *at > 0) else {
                return part.to_string();
            };
            let end = part[at..].find(['_', '/', '\\']).map_or(part.len(), |i| at + i);
            if part[at..end].contains('.') {
                format!("{EMAIL}{}", &part[end..])
            } else {
                part.to_string()
            }
        })
        .collect()
}

/// Read up to one byte beyond the member's declared size. A short, oversized or damaged member
/// returns false. Storage errors and output failures propagate so the job can retry.
pub(crate) fn read_member(reader: impl Read, size: u64, out: &mut impl Write) -> io::Result<bool> {
    let mut bounded = reader.take(size.saturating_add(1));
    let mut read = 0;
    let mut chunk = [0; 64 * 1024];
    loop {
        match bounded.read(&mut chunk) {
            Ok(0) => return Ok(read == size),
            Ok(n) => {
                read += n as u64;
                out.write_all(&chunk[..n])?;
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) if damaged_member(&e) => return Ok(false),
            Err(e) => return Err(e),
        }
    }
}

/// The ZIP reader is built with deflate only. A member in any other compression method fails on
/// its first read with `Unsupported`, so that kind marks the member as unreadable too.
fn damaged_member(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::InvalidData
            | io::ErrorKind::InvalidInput
            | io::ErrorKind::UnexpectedEof
            | io::ErrorKind::Unsupported
    )
}

/// The ZIP decoder uses these I/O error kinds for damaged data. Preserve the distinction when
/// storage reports one of the same kinds, such as an error opening a sealed blob block.
struct ArchiveReader<R>(R);

impl<R: Read> Read for ArchiveReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.0.read(buf).map_err(storage_error)
    }
}

impl<R: Seek> Seek for ArchiveReader<R> {
    fn seek(&mut self, pos: io::SeekFrom) -> io::Result<u64> {
        self.0.seek(pos).map_err(storage_error)
    }
}

fn storage_error(error: io::Error) -> io::Error {
    if damaged_member(&error) {
        io::Error::other(error)
    } else {
        error
    }
}

pub(crate) fn zip_error(error: ZipError, export: &ExportSettings) -> Error {
    match error {
        ZipError::Io(e) if !damaged_member(&e) => e.into(),
        ZipError::UnsupportedArchive(DIRECTORY_LIMIT) => Error::Upload(export.limit.into()),
        _ => Error::Upload(export.unreadable.into()),
    }
}

/// ZipArchive collapses duplicate directory entries. Refuse such an archive instead of silently
/// losing a member or letting a second entry with the same name shadow the first.
fn open(reader: impl Read + Seek, export: &ExportSettings) -> Result<ZipArchive<impl Read + Seek>> {
    let zip = zip_directory::open(ArchiveReader(reader)).map_err(|error| zip_error(error, export))?;
    if zip.has_duplicate_names() {
        return Err(Error::Upload(export.unreadable.into()));
    }
    Ok(zip)
}

/// Traverse one outer ZIP and at most one UploadedFiles ZIP below it. The shared budget counts the
/// files and the names of all entries across both levels. Expanded bytes include an
/// UploadedFiles part's own bytes and then its members' because both are unpacked.
fn scan<R: Read + Seek>(
    zip: &mut ZipArchive<R>,
    parent: Option<&str>,
    budget: &mut Budget,
    report: &mut Report,
) -> Result<()> {
    for index in 0..zip.len() {
        let header = zip.by_index_data(index).map_err(|error| zip_error(error, &GARMIN))?;
        let size = header.size();
        let decoded = header.name();
        let display = decoded
            .clone()
            .unwrap_or_else(|_| String::from_utf8_lossy(header.name_raw()));
        budget.add_name(display.len() + parent.map_or(0, |p| p.len() + 3))?;
        let mut member = parent.map_or_else(String::new, |p| format!("{p} / "));
        let shown: String = display
            .chars()
            .map(|c| if c.is_control() { '\u{fffd}' } else { c })
            .collect();
        member.push_str(&hide_email(&shown));
        let kind = MemberKind::detect(&display, parent.is_some());
        if !header.is_dir() && !matches!(kind, Some(MemberKind::FitArchive)) {
            budget.add_members(1)?;
        }
        if decoded.is_err() || unsafe_name(&display) || header.is_symlink() {
            report.omit(member, "unsafe_path");
            continue;
        }
        if header.is_dir() {
            continue;
        }
        let Some(kind) = kind else {
            let reason = if parent.is_some() && display.to_ascii_lowercase().ends_with(".zip") {
                "nesting_limit"
            } else {
                "unsupported"
            };
            report.omit(member, reason);
            continue;
        };
        if size > kind.cap() {
            report.omit(member, "member_too_large");
            continue;
        }
        if !budget.spend_expanded(size) {
            report.omit(member, OVER_BUDGET);
            continue;
        }
        let file = match zip.by_index(index).map_err(|error| zip_error(error, &GARMIN)) {
            Ok(file) => file,
            Err(Error::Upload(_)) => {
                report.omit(member, "invalid_member");
                continue;
            }
            Err(error) => return Err(error),
        };
        if matches!(kind, MemberKind::FitArchive) {
            let mut local = tempfile::tempfile()?;
            if !read_member(file, size, &mut local)? {
                report.omit(member, "invalid_member");
                continue;
            }
            local.rewind()?;
            match open(local, &GARMIN) {
                Ok(mut inner) => {
                    // the part is a container, not a file of readings: its FIT files are counted
                    let before = report.files_imported;
                    scan(&mut inner, Some(&member), budget, report)?;
                    if report.files_imported == before {
                        report.omit(member, "no_readings");
                    }
                }
                Err(Error::Upload(code)) if code == LIMIT => return Err(Error::Upload(code)),
                Err(Error::Upload(_)) => report.omit(member, "invalid_zip"),
                Err(e) => return Err(e),
            }
            continue;
        }
        let mut bytes = Vec::new();
        if !read_member(file, size, &mut bytes)? {
            report.omit(member, "invalid_member");
            continue;
        }
        let samples = match kind {
            MemberKind::Json(kind) => json_samples(kind, &bytes, MAX_READINGS - budget.readings),
            MemberKind::Fit => match std::panic::catch_unwind(|| fit::archive_activity(&bytes)) {
                Ok(Ok((samples, gps))) => {
                    report.gps_left_out |= gps;
                    Ok(samples)
                }
                _ => Err("invalid_fit"),
            },
            MemberKind::FitArchive => unreachable!(),
        };
        report.add(budget, member, matches!(kind, MemberKind::Fit), samples);
    }
    Ok(())
}

fn date(value: &Value) -> Option<DateTime<Utc>> {
    let text = value.as_str()?;
    let day = NaiveDate::parse_from_str(text, "%Y-%m-%d")
        .or_else(|_| NaiveDateTime::parse_from_str(text, "%Y-%m-%dT%H:%M:%S%.f").map(|t| t.date()))
        .ok()?;
    Some(crate::day_start(day))
}

fn instant(value: &Value) -> Option<DateTime<Utc>> {
    if let Some(n) = value.as_i64() {
        return DateTime::from_timestamp_millis(n);
    }
    let text = value.as_str()?;
    if let Ok(t) = DateTime::parse_from_rfc3339(text) {
        return Some(t.to_utc());
    }
    // Garmin's GMT fields also carry timestamps without a zone.
    ["%Y-%m-%d %H:%M:%S%.f", "%Y-%m-%dT%H:%M:%S%.f"]
        .iter()
        .find_map(|fmt| NaiveDateTime::parse_from_str(text, fmt).ok().map(|t| t.and_utc()))
}

fn offset(local: &Value, gmt: &Value) -> Option<i16> {
    let seconds = match local.as_str().and_then(|text| DateTime::parse_from_rfc3339(text).ok()) {
        Some(time) => i64::from(time.offset().local_minus_utc()),
        None => (instant(local)? - instant(gmt)?).num_seconds(),
    };
    (seconds % 60 == 0 && seconds.abs() <= 14 * 3600).then_some((seconds / 60) as i16)
}

fn number(row: &Value, keys: &[&str]) -> Option<f64> {
    keys.iter().find_map(|key| row[*key].as_f64())
}

fn valid_time(t: DateTime<Utc>, latest: DateTime<Utc>) -> bool {
    t.timestamp() >= 946_684_800 && t <= latest // 2000-01-01 UTC
}

fn row_samples(kind: GarminMember, row: &Value, latest: DateTime<Utc>, samples: &mut Vec<Sample>) {
    let day = || date(&row["calendarDate"]).or_else(|| date(&row["metaData"]["calendarDate"]));
    let weight = if row["weight"].is_object() { &row["weight"] } else { row };
    let (time, tz) = match kind {
        GarminMember::Daily => {
            let tz = offset(&row["wellnessStartTimeLocal"], &row["wellnessStartTimeGmt"]);
            (
                day().and_then(|d| d.checked_sub_signed(Duration::minutes(i64::from(tz.unwrap_or_default())))),
                tz,
            )
        }
        GarminMember::Hrv => (day(), None),
        GarminMember::Sleep => (
            instant(&row["sleepStartTimestampGMT"]),
            offset(&row["sleepEndTimestampLocal"], &row["sleepEndTimestampGMT"]),
        ),
        GarminMember::Weight => (
            instant(&weight["timestampGMT"])
                .or_else(|| instant(&row["timestampGMT"]))
                .or_else(|| row["timestamp"].as_i64().and_then(DateTime::from_timestamp_millis))
                .or_else(day),
            offset(&weight["timestampLocal"], &weight["timestampGMT"]),
        ),
        GarminMember::BloodPressure => (
            instant(&row["measurementTimestampGMT"])
                .or_else(|| instant(&row["timestampGMT"]))
                .or_else(|| row["timestamp"].as_i64().and_then(DateTime::from_timestamp_millis)),
            offset(&row["measurementTimestampLocal"], &row["measurementTimestampGMT"]),
        ),
        GarminMember::FitZip => (None, None),
    };
    let Some(t) = time.filter(|t| valid_time(*t, latest)) else {
        return;
    };
    let mut push = |metric, t_end, v: Option<f64>, low, high| {
        if let Some(v) = v.filter(|v| *v >= low && *v <= high) {
            samples.push(Sample {
                metric,
                t,
                t_end,
                v,
                tz,
                source: 0,
            });
        }
    };
    match kind {
        GarminMember::Daily => {
            push("steps", None, number(row, &["totalSteps"]), 0.0, 200_000.0);
            push(
                "hr_resting",
                None,
                number(row, &["currentDayRestingHeartRate"]),
                25.0,
                250.0,
            );
            push(
                "active_energy",
                None,
                number(row, &["activeKilocalories"]),
                0.0,
                20_000.0,
            );
            push("spo2", None, number(row, &["averageSpo2Value"]), 1.0, 100.0);
            push(
                "resp_rate",
                None,
                number(&row["respiration"], &["avgWakingRespirationValue"]).or_else(|| row["respiration"].as_f64()),
                1.0,
                120.0,
            );
        }
        GarminMember::Sleep => {
            let Some(end) =
                instant(&row["sleepEndTimestampGMT"]).filter(|e| *e > t && *e <= latest && *e - t <= Duration::days(2))
            else {
                return;
            };
            let in_bed = (end - t).num_seconds() as f64 / 60.0;
            let stages = ["deepSleepSeconds", "lightSleepSeconds", "remSleepSeconds"];
            let asleep = row["sleepTimeSeconds"].as_f64().or_else(|| {
                stages.iter().try_fold(0.0, |sum, stage| {
                    row[*stage].as_f64().filter(|n| *n >= 0.0).map(|n| sum + n)
                })
            });
            push("sleep_in_bed", Some(end), Some(in_bed), 0.0, 2880.0);
            push("sleep_asleep", Some(end), asleep.map(|n| n / 60.0), 0.0, in_bed);
        }
        GarminMember::Weight => {
            // A profile setting is different from an actual measurement. Garmin stores grams.
            if weight["sourceType"] != "USER_SETTING" {
                let grams = weight["weight"].as_f64();
                push("weight", None, grams.map(|g| g / 1000.0), 1.0, 500.0);
            }
        }
        GarminMember::BloodPressure => {
            if let (Some(sys), Some(dia)) = (
                number(row, &["systolic", "systolicPressure"]),
                number(row, &["diastolic", "diastolicPressure"]),
            ) && (30.0..=300.0).contains(&sys)
                && (15.0..sys).contains(&dia)
            {
                push("bp_systolic", None, Some(sys), 30.0, 300.0);
                push("bp_diastolic", None, Some(dia), 15.0, 300.0);
            }
        }
        GarminMember::Hrv => {
            // Garmin's overnight HRV uses RMSSD. Status and reference ranges are not readings.
            let from_metrics = || {
                row["metrics"].as_array()?.iter().find_map(|metric| {
                    let name = metric["type"].as_str()?;
                    if name.eq_ignore_ascii_case("hrv") || name.eq_ignore_ascii_case("hrv_rmssd") {
                        number(metric, &["value"])
                    } else {
                        None
                    }
                })
            };
            let value = from_metrics();
            push("hrv_rmssd", None, value, 1.0, 500.0);
        }
        GarminMember::FitZip => {}
    }
}

/// The readings of one JSON member; `limit` as soon as they pass `remaining` (the reading budget left),
/// so a huge file is never held in full.
fn json_samples(kind: GarminMember, data: &[u8], remaining: usize) -> std::result::Result<Vec<Sample>, &'static str> {
    let Ok(rows) = serde_json::from_slice::<Vec<Value>>(data.strip_prefix(b"\xef\xbb\xbf").unwrap_or(data)) else {
        return Err("invalid_json");
    };
    let latest = Utc::now() + Duration::days(1);
    let mut samples = Vec::new();
    for row in &rows {
        row_samples(kind, row, latest, &mut samples);
        if samples.len() > remaining {
            return Err(OVER_BUDGET);
        }
    }
    Ok(samples)
}

#[derive(Clone, Copy)]
pub(crate) struct ExportSettings {
    pub flag: &'static str,
    pub provider: &'static str,
    pub format: &'static str,
    pub mime: &'static str,
    pub unreadable: &'static str,
    pub limit: &'static str,
    pub label: &'static str,
    pub fit_label: Option<&'static str>,
}

const GARMIN: ExportSettings = ExportSettings {
    flag: "garmin",
    provider: PROVIDER,
    format: "ownhealth.garmin-readings/1",
    mime: GARMIN_READINGS_MIME,
    unreadable: UNREADABLE,
    limit: LIMIT,
    label: "Garmin Connect",
    fit_label: Some("Garmin FIT"),
};

/// One line of the stored document after its header; `member` indexes the header's `members`.
#[derive(Serialize, Deserialize)]
struct Reading<M> {
    member: usize,
    metric: M,
    start: DateTime<Utc>,
    end: Option<DateTime<Utc>>,
    offset_min: Option<i16>,
    value: f64,
}

#[derive(Serialize, Deserialize)]
struct StoredMember {
    name: String,
    fit: bool,
}

/// Read the readings back from a stored document (the JSON lines `prepare` wrote), grouped by member.
fn read_stored(reader: impl Read, export: &ExportSettings) -> Result<Vec<Readings>> {
    let bad = |what: &str| Error::Invalid(format!("not a stored {} readings file: {what}", export.label));
    let mut lines = io::BufReader::new(reader).lines();
    let header: Value = serde_json::from_str(&lines.next().ok_or_else(|| bad("empty"))??).map_err(|_| bad("header"))?;
    if header["format"] != export.format {
        return Err(bad("format"));
    }
    let members: Vec<StoredMember> = serde_json::from_value(header["members"].clone()).map_err(|_| bad("members"))?;
    let mut groups: Vec<Readings> = members
        .into_iter()
        .map(|m| Readings {
            member: m.name,
            fit: m.fit,
            samples: Vec::new(),
        })
        .collect();
    for line in lines {
        let line = line?;
        if line.is_empty() {
            continue;
        }
        let r: Reading<String> = serde_json::from_str(&line).map_err(|_| bad("reading"))?;
        let metric = METRICS
            .iter()
            .find(|m| m.0 == r.metric)
            .map(|m| m.0)
            .ok_or_else(|| bad("metric"))?;
        let group = groups.get_mut(r.member).ok_or_else(|| bad("member"))?;
        group.samples.push(Sample {
            metric,
            t: r.start,
            t_end: r.end,
            v: r.value,
            tz: r.offset_min,
            source: 0,
        });
    }
    Ok(groups)
}

struct Prepared {
    readings: Vec<Readings>,
    details: Value,
    kept_sha: Option<String>,
}

fn prepare(ctx: &JobContext, owner: BlobOwner, sha: &str, export: &ExportSettings) -> Result<Prepared> {
    let mut zip = open(ctx.blobs.open_seek(owner, sha)?, export)?;
    let mut report = Report::default();
    let mut budget = Budget::new(export.limit);
    let details = if export.flag == strava_zip::EXPORT.flag {
        let counts = strava_zip::scan(&mut zip, &mut budget, &mut report)?;
        let mut details = report.details();
        details["rows_without_file"] = json!(counts.rows_without_file);
        details["files_without_row"] = json!(counts.files_without_row);
        details
    } else if export.flag == takeout_fitbit::EXPORT.flag {
        // A Takeout export spends its own budget: its heart-rate CSV files are larger than a JSON file.
        takeout_fitbit::scan(&mut zip, &mut report)?;
        report.archive_details()
    } else {
        scan(&mut zip, None, &mut budget, &mut report)?;
        report.details()
    };
    if report.readings.is_empty() {
        return Ok(Prepared {
            readings: report.readings,
            details,
            kept_sha: None,
        });
    }
    let mut file = tempfile::tempfile()?;
    {
        let mut writer = io::BufWriter::new(&mut file);
        let members: Vec<_> = report
            .readings
            .iter()
            .map(|group| StoredMember {
                name: group.member.clone(),
                fit: group.fit,
            })
            .collect();
        serde_json::to_writer(
            &mut writer,
            &json!({"format": export.format, "archive_sha256": sha, "members": members}),
        )
        .map_err(io::Error::other)?;
        writer.write_all(b"\n")?;
        for (member, group) in report.readings.iter().enumerate() {
            for s in &group.samples {
                serde_json::to_writer(
                    &mut writer,
                    &Reading {
                        member,
                        metric: s.metric,
                        start: s.t,
                        end: s.t_end,
                        offset_min: s.tz,
                        value: s.v,
                    },
                )
                .map_err(io::Error::other)?;
                writer.write_all(b"\n")?;
            }
        }
        writer.flush()?;
    }
    file.rewind()?;
    let (kept, _) = ctx.blobs.put_reader(owner, file)?;
    Ok(Prepared {
        readings: report.readings,
        details,
        kept_sha: Some(kept),
    })
}

async fn write(
    conn: &mut PgConnection,
    profile: Uuid,
    doc: Uuid,
    readings: Vec<Readings>,
    details: &Value,
    source_id: Option<Uuid>,
    export: &ExportSettings,
) -> Result<ImportSummary> {
    let mut summary = ImportSummary::new(doc.to_string());
    let start = readings.iter().flat_map(|g| g.samples.iter().map(|s| s.t)).min();
    let mut primary = Vec::new();
    let mut fit = Vec::new();
    for group in readings {
        let source = if group.fit && export.fit_label.is_some() {
            &mut fit
        } else {
            &mut primary
        };
        source.extend(group.samples);
    }

    let mut touched = Touched::default();
    let mut written = Written::default();
    // Keep total daily energy and energy per activity in separate sources. The daily series uses
    // one source per day, so a day with both never adds the workout calories to the whole-day total.
    for (mut readings, label) in [(primary, export.label), (fit, export.fit_label.unwrap_or(export.label))] {
        if readings.is_empty() {
            continue;
        }
        let source = store::device_source(&mut *conn, profile, export.provider, Some(label), None, source_id).await?;
        for reading in &mut readings {
            reading.source = source;
        }
        written.add(store::insert(&mut *conn, profile, Some(doc), readings, Conflict::Keep, &mut touched).await?);
    }
    store::rebuild(&mut *conn, profile, touched).await?;
    sqlx::query(
        "update document set collected_at = $2, flags = flags || jsonb_build_object($4::text, $3::jsonb) where id = $1",
    )
    .bind(doc)
    .bind(start)
    .bind(details)
    .bind(export.flag)
    .execute(&mut *conn)
    .await?;
    summary.accepted = written.new;
    summary.skipped = written.duplicate();
    if summary.skipped > 0 {
        summary.reasons.insert("duplicate".into(), summary.skipped);
    }
    // the summary marks the document imported: a retraction of another export clears it to run again
    mark_imported(conn, doc, &summary, None).await?;
    Ok(summary)
}

/// Import a stored readings document again: after a retraction of another export of the profile
/// (the readings it held in this one's place come back), or after a move to another profile.
async fn import_document(
    ctx: &JobContext,
    job: &Job,
    source_id: Option<Uuid>,
    export: ExportSettings,
) -> jobs::HandlerResult {
    let doc = job.uuid("document_id")?;
    let mut tx = ctx.pool.begin().await?;
    lock_export_of(&mut tx, doc).await?;
    let row: Option<(Option<Uuid>, String, Value, String)> = sqlx::query_as(
        "select profile_id, encode(blob_sha256, 'hex'), flags, status from document where id = $1 and mime = $2 for update",
    )
    .bind(doc)
    .bind(export.mime)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((profile, sha, flags, status)) = row else {
        return Err(JobError::permanent(format!("the {} document is gone", export.label)));
    };
    let mut summary = ImportSummary::new(doc.to_string());
    let details = flags.get(export.flag).cloned().unwrap_or_else(|| json!({}));
    if let Some(queued_for) = job.str("profile_id").and_then(|p| Uuid::parse_str(p).ok())
        && profile != Some(queued_for)
    {
        // A later move queued its own run. This job's result belongs to the previous profile.
        summary.reasons.insert("moved".into(), 1);
        tx.commit().await?;
        return Ok(Some(summary.to_value()));
    }
    if status != "active" {
        // retracted while its run waited in the queue: it brings nothing back
        summary.reasons.insert("retracted".into(), 1);
    } else if let Some(done) = flags.get("import").filter(|d| d.is_object()) {
        // imported since this run was queued (a retried job): the stored summary
        let n = |k: &str| done.get(k).and_then(Value::as_i64).unwrap_or(0);
        (summary.accepted, summary.queued, summary.skipped) = (n("accepted"), n("queued"), n("skipped"));
        summary.reasons = done
            .get("reasons")
            .and_then(Value::as_object)
            .map(|m| m.iter().map(|(k, c)| (k.clone(), c.as_i64().unwrap_or(0))).collect())
            .unwrap_or_default();
    } else {
        let profile =
            profile.ok_or_else(|| JobError::permanent(format!("the {} document has no profile", export.label)))?;
        let blobs = ctx.blobs.clone();
        let readings = tokio::task::spawn_blocking(move || {
            read_stored(blobs.open_seek(BlobOwner::Profile(profile), &sha)?, &export)
        })
        .await
        .map_err(|e| JobError::from(Error::Other(e.into())))?
        .map_err(|e| match e {
            Error::Invalid(m) => JobError::permanent(m),
            other => JobError::from(other),
        })?;
        summary = write(&mut tx, profile, doc, readings, &details, source_id, &export).await?;
    }
    tx.commit().await?;
    let mut out = summary.to_value();
    out[export.flag] = details;
    Ok(Some(out))
}

pub(crate) async fn run(ctx: JobContext, job: Job, export: ExportSettings) -> jobs::HandlerResult {
    let channel = job
        .str("channel")
        .filter(|c| !c.is_empty())
        .unwrap_or("upload")
        .to_string();
    let source_id = job.str("source_id").and_then(|s| Uuid::parse_str(s).ok());
    if ownhealth_ingest_structured::py::truthy(job.payload.get("document_id")) {
        return import_document(&ctx, &job, source_id, export).await;
    }
    let profile = job.uuid("profile_id")?;
    let sha = job
        .str("blob_sha256")
        .ok_or_else(|| JobError::permanent(export.unreadable))?;

    // If the worker stops after committing its writes, reuse the answer even if the original
    // upload has already been removed. The job still goes through the normal lease completion.
    let previous: Option<Value> = sqlx::query_scalar("select result from job where id = $1")
        .bind(job.id)
        .fetch_one(&ctx.pool)
        .await?;
    if let Some(out) = previous {
        fit::forget_upload(&ctx, profile, sha, job.id).await;
        return Ok(Some(out));
    }
    let (worker_ctx, worker_sha) = (ctx.clone(), sha.to_owned());
    let prepared =
        tokio::task::spawn_blocking(move || prepare(&worker_ctx, BlobOwner::Profile(profile), &worker_sha, &export))
            .await
            .map_err(|e| JobError::from(Error::Other(e.into())))?;
    let Prepared {
        readings,
        details,
        kept_sha,
    } = match prepared {
        Ok(prepared) => prepared,
        Err(Error::Upload(code)) => {
            let mut tx = ctx.pool.begin().await?;
            lock_exports(&mut tx, profile, export.flag).await?;
            release_source_items(&mut tx, profile, sha, None).await?;
            tx.commit().await?;
            fit::forget_upload(&ctx, profile, sha, job.id).await;
            return Err(JobError::permanent(code));
        }
        Err(error) => return Err(error.into()),
    };
    let mut tx = ctx.pool.begin().await?;
    lock_exports(&mut tx, profile, export.flag).await?;
    let (mut out, document) = if let Some(kept) = kept_sha {
        let mut payload = job.payload.clone();
        payload["blob_sha256"] = json!(kept);
        let filename = job.str("filename").unwrap_or(export.flag);
        let stem = filename.rsplit_once('.').map_or(filename, |(stem, _)| stem);
        payload["filename"] = json!(format!("{stem}.jsonl"));
        let doc = document_of_type(&mut tx, &ctx.blobs, &payload, export.mime, &channel, WEARABLES_DOC_TYPE).await?;
        let summary = write(&mut tx, profile, doc, readings, &details, source_id, &export).await?;
        (summary.to_value(), Some(doc))
    } else {
        (json!({"accepted": 0, "skipped": 0, "reasons": {}}), None)
    };
    out[export.flag] = details;
    release_source_items(&mut tx, profile, sha, document).await?;
    sqlx::query("update job set result = $2 where id = $1")
        .bind(job.id)
        .bind(&out)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    fit::forget_upload(&ctx, profile, sha, job.id).await;
    Ok(Some(out))
}

async fn release_source_items(conn: &mut PgConnection, profile: Uuid, sha: &str, document: Option<Uuid>) -> Result<()> {
    // A queued inbox arrival owns the raw ZIP until the import and its result commit. Keep the hash
    // and link to the projected document for subsequent arrivals, then allow the raw bytes to go.
    sqlx::query(
        "update source_item set raw_released_at = now(), document_id = $3 \
         where profile_id = $1 and blob_sha256 = decode($2, 'hex') \
         and document_id is null and quarantine_id is null and raw_released_at is null",
    )
    .bind(profile)
    .bind(sha)
    .bind(document)
    .execute(conn)
    .await?;
    Ok(())
}

pub fn register() {
    jobs::register_handler(JOB, |ctx, job| Box::pin(run(ctx, job, GARMIN)));
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An end record alone whose central directory is one byte over the upload budget.
    fn end_record_over_the_directory_budget() -> Vec<u8> {
        let mut end = b"PK\x05\x06".to_vec();
        for field in [0u16, 0, 1, 1] {
            end.extend(field.to_le_bytes());
        }
        let size = u32::try_from(zip_directory::MAX_DIRECTORY_BYTES + 1).expect("budget fits a ZIP32 size");
        end.extend(size.to_le_bytes());
        end.extend(0u32.to_le_bytes());
        end.extend(0u16.to_le_bytes());
        end
    }

    #[test]
    fn an_export_whose_directory_is_over_the_budget_is_refused_as_too_large() {
        let opened = open(std::io::Cursor::new(end_record_over_the_directory_budget()), &GARMIN);
        assert!(matches!(opened, Err(Error::Upload(ref code)) if code == LIMIT));
    }

    #[test]
    fn an_uploaded_files_part_whose_directory_is_over_the_budget_refuses_the_export() -> anyhow::Result<()> {
        use std::io::Cursor;
        use zip::write::SimpleFileOptions;

        let mut outer = zip::ZipWriter::new(Cursor::new(Vec::new()));
        outer.start_file("UploadedFiles_1.zip", SimpleFileOptions::default())?;
        outer.write_all(&end_record_over_the_directory_budget())?;
        let mut archive = open(outer.finish()?, &GARMIN)?;
        let scanned = scan(&mut archive, None, &mut Budget::new(LIMIT), &mut Report::default());
        assert!(matches!(scanned, Err(Error::Upload(ref code)) if code == LIMIT));
        Ok(())
    }

    #[test]
    fn archive_limits_count_across_nested_archives() -> anyhow::Result<()> {
        let mut budget = Budget::new(LIMIT);
        budget.add_members(MAX_MEMBERS - 2)?;
        budget.add_members(2)?;
        assert!(matches!(budget.add_members(1), Err(Error::Upload(code)) if code == LIMIT));

        budget.add_name(MAX_NAMES_BYTES - 1)?;
        budget.add_name(1)?;
        assert!(matches!(budget.add_name(1), Err(Error::Upload(code)) if code == LIMIT));
        Ok(())
    }

    #[test]
    fn folders_and_uploaded_files_containers_do_not_spend_the_file_budget() -> anyhow::Result<()> {
        use std::io::Cursor;
        use zip::write::SimpleFileOptions;

        let mut inner = zip::ZipWriter::new(Cursor::new(Vec::new()));
        for index in 0..128 {
            inner.add_directory(format!("folder{index}/"), SimpleFileOptions::default())?;
        }
        inner.start_file("run.fit", SimpleFileOptions::default())?;
        let part = inner.finish()?.into_inner();

        let mut outer = zip::ZipWriter::new(Cursor::new(Vec::new()));
        for index in 0..128 {
            outer.add_directory(format!("folder{index}/"), SimpleFileOptions::default())?;
        }
        outer.start_file("UploadedFiles_1.zip", SimpleFileOptions::default())?;
        outer.write_all(&part)?;
        let mut archive = open(outer.finish()?, &GARMIN)?;
        let mut budget = Budget {
            members: MAX_MEMBERS - 1,
            ..Budget::new(LIMIT)
        };
        scan(&mut archive, None, &mut budget, &mut Report::default())?;
        assert_eq!(budget.members, MAX_MEMBERS);
        assert!(matches!(
            scan(&mut archive, None, &mut budget, &mut Report::default()),
            Err(Error::Upload(code)) if code == LIMIT
        ));
        Ok(())
    }

    #[test]
    fn a_storage_read_error_is_retryable() {
        let storage = FailedRead(|| io::Error::other("storage temporarily unavailable"));
        let error = read_member(storage, 10, &mut Vec::new()).expect_err("storage failure");
        assert_eq!(error.to_string(), "storage temporarily unavailable");
        assert!(!JobError::from(Error::Io(error)).permanent);

        let damaged = || io::Error::new(io::ErrorKind::InvalidData, "invalid bytes");
        assert!(!read_member(FailedRead(damaged), 10, &mut Vec::new()).expect("damaged member"));
        let storage = ArchiveReader(FailedRead(damaged));
        assert!(read_member(storage, 10, &mut Vec::new()).is_err());

        // The reader is built with deflate only and reports any other method on the first read.
        let method = || io::Error::new(io::ErrorKind::Unsupported, "compression method not supported");
        assert!(!read_member(FailedRead(method), 10, &mut Vec::new()).expect("member in another method"));
        let storage = ArchiveReader(FailedRead(method));
        assert!(read_member(storage, 10, &mut Vec::new()).is_err());
    }

    struct FailedRead<F>(F);

    impl<F: Fn() -> io::Error> Read for FailedRead<F> {
        fn read(&mut self, _buf: &mut [u8]) -> io::Result<usize> {
            Err((self.0)())
        }
    }

    #[test]
    fn explicit_local_offsets_are_kept_for_daily_sleep_and_pressure_readings() -> anyhow::Result<()> {
        for (kind, row) in [
            (
                GarminMember::Daily,
                json!({
                    "calendarDate": "2024-01-04", "totalSteps": 6200,
                    "wellnessStartTimeLocal": "2024-01-04T00:00:00+02:00",
                    "wellnessStartTimeGmt": "2024-01-03T22:00:00Z",
                }),
            ),
            (
                GarminMember::Sleep,
                json!({
                    "sleepStartTimestampGMT": "2024-01-03T22:00:00Z",
                    "sleepEndTimestampLocal": "2024-01-04T08:00:00+02:00",
                    "sleepEndTimestampGMT": "2024-01-04T06:00:00Z",
                    "sleepTimeSeconds": 25200,
                }),
            ),
            (
                GarminMember::BloodPressure,
                json!({
                    "measurementTimestampLocal": "2024-01-04T00:00:00+02:00",
                    "measurementTimestampGMT": "2024-01-03T22:00:00Z",
                    "systolic": 118, "diastolic": 75,
                }),
            ),
        ] {
            let mut samples = Vec::new();
            row_samples(kind, &row, Utc::now(), &mut samples);
            assert!(!samples.is_empty(), "{kind:?}");
            for sample in samples {
                assert_eq!(
                    (sample.t, sample.tz),
                    ("2024-01-03T22:00:00Z".parse()?, Some(120)),
                    "{kind:?}"
                );
            }
        }
        Ok(())
    }

    #[test]
    fn a_flat_weight_setting_is_not_a_measurement() {
        let flat = |source| {
            json!({
                "weight": 70000, "sourceType": source, "timestampGMT": "2024-01-04T08:00:00Z",
            })
        };
        let nested = |source| {
            json!({
                "weight": {"weight": 61200, "sourceType": source, "timestampGMT": "2024-01-04T08:00:00Z"},
            })
        };
        assert_eq!(rows(GarminMember::Weight, flat("MANUAL")), [("weight", 70.0)]);
        assert_eq!(rows(GarminMember::Weight, nested("SCALE")), [("weight", 61.2)]);
        assert!(rows(GarminMember::Weight, flat("USER_SETTING")).is_empty());
        assert!(rows(GarminMember::Weight, nested("USER_SETTING")).is_empty());
    }

    fn steps(n: usize) -> Vec<Sample> {
        let t = crate::day_start(NaiveDate::from_ymd_opt(2024, 1, 4).expect("date"));
        (0..n)
            .map(|i| Sample {
                metric: "steps",
                t: t + Duration::days(i as i64),
                t_end: None,
                v: 1.0,
                tz: None,
                source: 0,
            })
            .collect()
    }

    #[test]
    fn a_file_past_the_expanded_or_reading_budget_is_listed_and_the_files_before_it_import() {
        let mut budget = Budget::new(LIMIT);
        assert!(budget.spend_expanded(MAX_EXPANDED - 1));
        assert!(!budget.spend_expanded(2));
        assert!(!budget.spend_expanded(u64::MAX));
        assert!(budget.spend_expanded(1));
        assert!(!budget.spend_expanded(1));

        let mut report = Report::default();
        budget.readings = MAX_READINGS - 2;
        report.add(&mut budget, "a.json".into(), false, Ok(steps(3)));
        report.add(&mut budget, "b.json".into(), false, Ok(steps(2)));
        report.add(&mut budget, "c.json".into(), false, Ok(steps(1)));
        assert_eq!(report.files_imported, 1);
        assert_eq!(report.readings[0].member, "b.json");
        let left: Vec<_> = report
            .not_imported
            .iter()
            .map(|o| (o.member.as_str(), o.reason))
            .collect();
        assert_eq!(left, [("a.json", OVER_BUDGET), ("c.json", OVER_BUDGET)]);
    }

    #[test]
    fn a_json_file_stops_reading_at_the_reading_budget() {
        let data = br#"[{"calendarDate":"2024-01-04","totalSteps":1},{"calendarDate":"2024-01-05","totalSteps":2}]"#;
        assert_eq!(json_samples(GarminMember::Daily, data, 1).err(), Some(OVER_BUDGET));
        assert_eq!(json_samples(GarminMember::Daily, data, 2).map(|s| s.len()), Ok(2));
    }

    fn rows(kind: GarminMember, row: Value) -> Vec<(&'static str, f64)> {
        let mut samples = Vec::new();
        row_samples(kind, &row, Utc::now(), &mut samples);
        samples.iter().map(|s| (s.metric, s.v)).collect()
    }

    #[test]
    fn a_sleep_over_two_days_or_a_pressure_with_diastolic_at_or_over_systolic_is_no_reading() {
        let sleep = |end: &str| {
            json!({"sleepStartTimestampGMT": "2024-01-01T22:00:00Z", "sleepEndTimestampGMT": end,
                   "sleepTimeSeconds": 25200})
        };
        assert_eq!(
            rows(GarminMember::Sleep, sleep("2024-01-02T06:00:00Z")),
            [("sleep_in_bed", 480.0), ("sleep_asleep", 420.0)]
        );
        assert!(rows(GarminMember::Sleep, sleep("2024-01-04T06:00:00Z")).is_empty());

        let pressure = |sys: i64, dia: i64| json!({"measurementTimestampGMT": "2024-01-04T08:30:00Z", "systolic": sys, "diastolic": dia});
        assert_eq!(
            rows(GarminMember::BloodPressure, pressure(118, 75)),
            [("bp_systolic", 118.0), ("bp_diastolic", 75.0)]
        );
        assert!(rows(GarminMember::BloodPressure, pressure(90, 90)).is_empty());
        assert!(rows(GarminMember::BloodPressure, pressure(80, 95)).is_empty());
    }

    struct Counting<R> {
        inner: R,
        read: u64,
    }

    impl<R: Read> Read for Counting<R> {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            let n = self.inner.read(buf)?;
            self.read += n as u64;
            Ok(n)
        }
    }

    #[test]
    fn a_member_is_read_at_most_one_byte_past_its_declared_size() -> anyhow::Result<()> {
        // the header understates the member: 10 bytes declared, a megabyte behind it
        let mut source = Counting {
            inner: io::repeat(7).take(1 << 20),
            read: 0,
        };
        let mut out = Vec::new();
        assert!(!read_member(&mut source, 10, &mut out)?);
        assert_eq!(source.read, 11);
        assert_eq!(out.len(), 11);

        let mut exact = Vec::new();
        assert!(read_member(io::repeat(7).take(10), 10, &mut exact)?);
        assert!(!read_member(io::repeat(7).take(9), 10, &mut Vec::new())?);
        Ok(())
    }

    #[test]
    fn the_account_email_is_hidden_in_member_names() {
        for (name, shown) in [
            (
                "DI_CONNECT/DI-Connect-Fitness/lucia.rivera@example.org_0_summarizedActivities.json",
                "DI_CONNECT/DI-Connect-Fitness/<email>_0_summarizedActivities.json",
            ),
            ("lucia_rivera+x@example.org_123.fit", "<email>_123.fit"),
            ("Export lucia@example.org/UDSFile_1.json", "<email>/UDSFile_1.json"),
            ("@eaDir/UDSFile_1.json", "@eaDir/UDSFile_1.json"),
            ("notes@home/UDSFile_1.json", "notes@home/UDSFile_1.json"),
            ("DI_CONNECT/UDSFile_1.json", "DI_CONNECT/UDSFile_1.json"),
        ] {
            assert_eq!(hide_email(name), shown, "{name}");
        }
    }

    #[test]
    fn stored_readings_come_back_by_member() -> anyhow::Result<()> {
        let text = format!(
            "{}\n{}\n{}\n",
            json!({"format": GARMIN.format, "archive_sha256": "x",
                   "members": [{"name": "a.json", "fit": false}, {"name": "p.zip / b.fit", "fit": true}]}),
            json!({"member": 1, "metric": "hr", "start": "2024-01-04T08:00:00Z", "end": null, "offset_min": 60, "value": 120.0}),
            json!({"member": 0, "metric": "steps", "start": "2024-01-04T00:00:00Z", "end": null, "offset_min": null, "value": 6200.0}),
        );
        let groups = read_stored(text.as_bytes(), &GARMIN)?;
        let seen: Vec<_> = groups
            .iter()
            .map(|g| {
                (
                    g.member.as_str(),
                    g.fit,
                    g.samples.iter().map(|s| (s.metric, s.v, s.tz)).collect::<Vec<_>>(),
                )
            })
            .collect();
        assert_eq!(
            seen,
            [
                ("a.json", false, vec![("steps", 6200.0, None)]),
                ("p.zip / b.fit", true, vec![("hr", 120.0, Some(60))]),
            ]
        );
        let unknown = text.replace("\"hr\"", "\"blood_alcohol\"");
        assert!(matches!(
            read_stored(unknown.as_bytes(), &GARMIN),
            Err(Error::Invalid(_))
        ));
        let past = text.replace("\"member\":1", "\"member\":2");
        assert!(matches!(read_stored(past.as_bytes(), &GARMIN), Err(Error::Invalid(_))));
        Ok(())
    }
}
