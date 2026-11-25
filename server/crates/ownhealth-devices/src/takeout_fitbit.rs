//! Fitbit and Google Health Takeout archives. The original ZIP and account profile do not become
//! documents. The retained JSON lines name each member that supplied readings.
//!
//! Times, as public exports show them (the fixtures README names the sources):
//! - `Global Export Data` JSON times carry no zone and are UTC;
//! - Google Data CSVs write UTC with `Z`; the legacy HRV detail CSVs write local time with no zone;
//! - a daily value (resting heart rate, a daily SpO2 or HRV summary) names its calendar day, even when
//!   written as midnight UTC.
//!
//! Each product folder's `Your Profile/Profile.csv` gives a UTC time its local offset and places a
//! local time. A folder that has no profile can use a zone shared by the other usable profiles.
//! A row whose time cannot be read or placed (no zone, or a clock change) is left out and counted;
//! the rest of the archive still imports.

use std::collections::BTreeMap;
use std::collections::btree_map::Entry;
use std::io::Read;

use chrono::{DateTime, Datelike, Duration, NaiveDate, NaiveDateTime, Offset, TimeZone, Utc};
use chrono_tz::Tz;
use ownhealth_core::blobs::BlobOwner;
use ownhealth_core::documents::{FITBIT_READINGS_MIME, fitbit_takeout_path};
use ownhealth_core::jobs::{self, JobContext};
use ownhealth_core::zip_directory::{GARMIN_MAX_FILES, GARMIN_MAX_NAME_BYTES};
use ownhealth_core::{Error, Result};
use serde_json::Value;
use zip::read::ZipFile;

use crate::garmin_zip::{
    self, ArchiveImport, MAX_EXPANDED, MAX_JSON, MAX_READINGS, Report, archive_error, hide_email, open_archive,
    read_member, unsafe_name,
};
use crate::store::Sample;

pub const JOB: &str = "import.fitbit_takeout";
pub const PROVIDER: &str = "fitbit_takeout";

const UNKNOWN: &str = "fitbit_takeout_unknown_layout";
const UNREADABLE: &str = "fitbit_takeout_unreadable";
const LIMIT: &str = "fitbit_takeout_limit";
const NO_READINGS: &str = "fitbit_takeout_no_readings";
const INVALID_CSV: &str = "invalid_csv";
const INVALID_JSON: &str = "invalid_json";

// A month of five-second heart rate can exceed MAX_JSON. The byte and input row caps
// also bound work for archives whose rows collapse to very few output readings.
const MAX_HEART_RATE_CSV: u64 = 64 * 1024 * 1024;
const MAX_INPUT_ROWS: usize = 12_000_000;

const IMPORT: ArchiveImport = ArchiveImport {
    flag: PROVIDER,
    format: "ownhealth.fitbit-readings/1",
    mime: FITBIT_READINGS_MIME,
    provider: PROVIDER,
    label: "Fitbit",
    unreadable: UNREADABLE,
    scan: scan_archive,
    details: Report::archive_details,
};

pub fn register() {
    jobs::register_handler(JOB, |ctx, job| Box::pin(garmin_zip::run(ctx, job, &IMPORT)));
}

fn refuse(code: &str) -> Error {
    Error::Upload(code.into())
}

#[derive(Clone, Copy)]
enum Metric {
    HeartRate,
    Resting,
    Rmssd,
    Spo2,
}

impl Metric {
    fn sample(self, t: DateTime<Utc>, tz: i16, v: f64, latest: DateTime<Utc>) -> Option<Sample> {
        let (metric, bounds) = match self {
            Self::HeartRate => ("hr", 25.0..=250.0),
            Self::Resting => ("hr_resting", 25.0..=250.0),
            Self::Rmssd => ("hrv_rmssd", 1.0..=500.0),
            Self::Spo2 => ("spo2", 1.0..=100.0),
        };
        (t <= latest && t.date_naive().year() >= 2000 && bounds.contains(&v)).then_some(Sample {
            metric,
            t,
            t_end: None,
            v,
            tz: Some(tz),
            source: 0,
        })
    }

    fn json_value(self, row: &Value) -> Option<f64> {
        let value = &row["value"];
        let fields: &[&str] = match self {
            Self::HeartRate => &["bpm"],
            Self::Resting => &["value", "restingHeartRate", "bpm"],
            Self::Rmssd => &["rmssd", "dailyRmssd"],
            Self::Spo2 => &["avg", "average_value", "spo2"],
        };
        fields
            .iter()
            .find_map(|key| numeric(&value[key]).or_else(|| numeric(&row[key])))
            .or_else(|| numeric(value))
    }

    fn csv_values(self) -> &'static [&'static str] {
        match self {
            Self::HeartRate => &["beats per minute", "beats per minute bpm", "bpm", "heart rate"],
            Self::Resting => &[
                "beats per minute",
                "bpm",
                "resting heart rate",
                "resting heart rate bpm",
            ],
            Self::Rmssd => &[
                "rmssd",
                "rmssd ms",
                "root mean square of successive differences milliseconds",
                "average heart rate variability milliseconds",
            ],
            Self::Spo2 => &[
                "average oxygen saturation percentage",
                "oxygen saturation percentage",
                "oxygen saturation percent",
                "oxygen saturation",
                "average oxygen saturation percent",
                "average value",
                "spo2",
                "spo2 percent",
            ],
        }
    }
}

/// A file of readings: its metric, whether it is Global Export Data JSON, and whether each row is one
/// day's value.
#[derive(Clone, Copy)]
struct Member {
    metric: Metric,
    json: bool,
    daily: bool,
}

enum MemberKind {
    Profile,
    Readings(Member),
}

fn readings(metric: Metric, json: bool, daily: bool) -> Option<MemberKind> {
    Some(MemberKind::Readings(Member {
        metric,
        json,
        daily: daily || matches!(metric, Metric::Resting),
    }))
}

/// Turn the export's separators and optional unit punctuation into spaces. We match complete
/// column names, so fields such as SDNN, minimum SpO2 and confidence never become readings.
fn words(value: &str) -> String {
    value
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|part| !part.is_empty())
        .map(str::to_ascii_lowercase)
        .collect::<Vec<_>>()
        .join(" ")
}

fn named(stem: &str, label: &str) -> bool {
    stem == label
        || stem
            .strip_prefix(label)
            .and_then(|rest| rest.strip_prefix(' '))
            .is_some_and(|rest| rest.starts_with(|c: char| c.is_ascii_digit()))
}

fn member_kind(path: &str) -> Option<MemberKind> {
    let (parent, base) = path.rsplit_once('/').unwrap_or(("", path));
    let (stem, extension) = base.rsplit_once('.')?;
    let stem = words(stem);
    let extension = extension.to_ascii_lowercase();
    if stem == "profile" && extension == "csv" && words(parent) == "your profile" {
        return Some(MemberKind::Profile);
    }
    if extension == "json" && parent.eq_ignore_ascii_case("Global Export Data") {
        for (label, metric) in [
            ("heart rate", Metric::HeartRate),
            ("resting heart rate", Metric::Resting),
            ("heart rate variability", Metric::Rmssd),
            ("hrv", Metric::Rmssd),
            ("spo2", Metric::Spo2),
            ("oxygen saturation", Metric::Spo2),
        ] {
            if named(&stem, label) {
                return readings(metric, true, false);
            }
        }
    }
    if extension != "csv" {
        return None;
    }
    let (folder, series) = parent.split_once('/').unwrap_or((parent, ""));
    if ["google data", "physical activity googledata"].contains(&words(folder).as_str()) {
        let date_filename = stem.starts_with(|c: char| c.is_ascii_digit());
        for (label, metric) in [
            ("heart rate", Metric::HeartRate),
            ("daily resting heart rate", Metric::Resting),
            ("resting heart rate", Metric::Resting),
            ("heart rate variability", Metric::Rmssd),
            ("daily heart rate variability", Metric::Rmssd),
            ("oxygen saturation", Metric::Spo2),
            ("daily oxygen saturation", Metric::Spo2),
            ("spo2", Metric::Spo2),
        ] {
            if (series.is_empty() && named(&stem, label))
                || (words(series) == label && (date_filename || named(&stem, label)))
            {
                return readings(metric, false, label.starts_with("daily"));
            }
        }
    }
    // Older exports also ship separate Fitbit CSVs alongside their Global Export Data JSON.
    if words(folder) == "heart rate variability" && series.is_empty() {
        for label in [
            "heart rate variability details",
            "heart rate variability summary",
            "daily heart rate variability summary",
        ] {
            if named(&stem, label) {
                return readings(Metric::Rmssd, false, label != "heart rate variability details");
            }
        }
    }
    // The real folder is `Oxygen Saturation (SpO2)`; `Minute SpO2` files are not read (the daily
    // file already holds each night's average).
    if [
        "oxygen saturation spo2",
        "oxygen saturation",
        "spo2",
        "global export data",
    ]
    .contains(&words(folder).as_str())
        && series.is_empty()
        && ["daily spo2", "daily oxygen saturation"]
            .iter()
            .any(|label| named(&stem, label))
    {
        return readings(Metric::Spo2, false, true);
    }
    None
}

fn numeric(value: &Value) -> Option<f64> {
    value.as_f64().or_else(|| value.as_str()?.trim().parse().ok())
}

/// The `timezone` column of the account profile; None when it is missing or not a zone name.
fn profile_zone(bytes: &[u8]) -> Option<Tz> {
    let mut reader = csv::ReaderBuilder::new().trim(csv::Trim::All).from_reader(bytes);
    let column = reader.headers().ok()?.iter().position(|h| words(h) == "timezone")?;
    let record = reader.records().next()?.ok()?;
    record.get(column)?.parse().ok()
}

/// A UTC instant with the offset the profile zone had then (0 without a zone).
fn utc(t: DateTime<Utc>, zone: Option<Tz>) -> (DateTime<Utc>, i16) {
    let offset = zone.map_or(0, |z| {
        z.offset_from_utc_datetime(&t.naive_utc()).fix().local_minus_utc()
    });
    (t, (offset / 60) as i16)
}

/// When a row's reading was taken: UTC and the local offset in minutes. None when the time cannot be
/// read, or is a local time with no zone or in a clock change.
fn when(raw: &str, member: Member, zone: Option<Tz>) -> Option<(DateTime<Utc>, i16)> {
    let raw = raw.trim();
    let written = DateTime::parse_from_rfc3339(raw).ok();
    let naive = || {
        ["%m/%d/%y %H:%M:%S%.f", "%Y-%m-%dT%H:%M:%S%.f", "%Y-%m-%d %H:%M:%S%.f"]
            .iter()
            .find_map(|format| NaiveDateTime::parse_from_str(raw, format).ok())
    };
    let date = NaiveDate::parse_from_str(raw, "%Y-%m-%d").ok();
    if member.daily || date.is_some() {
        // A day's value keeps its calendar day: midnight UTC, the day rollup's own convention.
        let day = date
            .or_else(|| written.map(|t| t.naive_local().date()))
            .or_else(|| naive().map(|t| t.date()))?;
        return Some((crate::day_start(day), 0));
    }
    if let Some(t) = written {
        let offset = t.offset().local_minus_utc();
        return Some(if offset == 0 {
            utc(t.to_utc(), zone)
        } else {
            (t.to_utc(), (offset / 60) as i16)
        });
    }
    let local = naive()?;
    if member.json {
        return Some(utc(local.and_utc(), zone));
    }
    let t = zone?.from_local_datetime(&local).single()?;
    Some((t.to_utc(), (t.offset().fix().local_minus_utc() / 60) as i16))
}

/// Visit each row so the input budget counts rows even when they have no usable value.
fn json_rows(
    bytes: &[u8],
    metric: Metric,
    mut push: impl FnMut(Option<&str>, Option<f64>) -> Result<()>,
) -> Result<()> {
    let rows: Vec<Value> = serde_json::from_slice(bytes).map_err(|_| refuse(INVALID_JSON))?;
    for row in rows {
        let time = row["dateTime"]
            .as_str()
            .or_else(|| row["timestamp"].as_str())
            .or_else(|| row["date"].as_str());
        push(time, metric.json_value(&row))?;
    }
    Ok(())
}

fn csv_error(error: csv::Error) -> Error {
    match error.into_kind() {
        csv::ErrorKind::Io(error) => archive_error(error.into(), UNREADABLE, LIMIT),
        _ => refuse(INVALID_CSV),
    }
}

fn csv_rows(
    file: impl Read,
    size: u64,
    metric: Metric,
    mut push: impl FnMut(Option<&str>, Option<f64>) -> Result<()>,
) -> Result<()> {
    // The CSV reader buffers its input and handles the UTF-8 BOM and quoted newlines.
    let mut reader = csv::ReaderBuilder::new()
        .trim(csv::Trim::All)
        .from_reader(file.take(size + 1));
    let headers: Vec<_> = reader.headers().map_err(csv_error)?.iter().map(words).collect();
    let column = |names: &[&str]| {
        names
            .iter()
            .find_map(|name| headers.iter().position(|header| header == name))
    };
    let time = column(&["timestamp", "datetime", "date time", "date"]).ok_or_else(|| refuse(INVALID_CSV))?;
    let value = column(metric.csv_values()).ok_or_else(|| refuse(INVALID_CSV))?;
    let mut record = csv::StringRecord::new();
    while reader.read_record(&mut record).map_err(csv_error)? {
        push(record.get(time), record.get(value).and_then(|v| v.parse().ok()))?;
    }
    if reader.into_inner().limit() != 1 {
        return Err(refuse(UNREADABLE));
    }
    Ok(())
}

#[derive(Default)]
struct HeartRateMinute {
    // Keep the original instants until all members have contributed. The mean weights
    // each instant once even when two exports supply overlapping parts of the minute.
    values: BTreeMap<DateTime<Utc>, f64>,
    first: Option<(DateTime<Utc>, Option<i16>, usize)>,
    sum: f64,
}

impl HeartRateMinute {
    fn add_value(&mut self, time: DateTime<Utc>, bpm: f64) {
        if let Entry::Vacant(slot) = self.values.entry(time) {
            slot.insert(bpm);
            self.sum += bpm;
        }
    }

    fn keep_first(&mut self, first: (DateTime<Utc>, Option<i16>, usize)) {
        if self.first.is_none_or(|current| first.0 < current.0) {
            self.first = Some(first);
        }
    }

    fn add_sample(&mut self, sample: Sample, file: usize) {
        self.keep_first((sample.t, sample.tz, file));
        self.add_value(sample.t, sample.v);
    }

    fn merge(&mut self, incoming: Self) {
        if let Some(first) = incoming.first {
            self.keep_first(first);
        }
        for (t, bpm) in incoming.values {
            self.add_value(t, bpm);
        }
    }

    fn finish(self, minute: i64) -> (usize, Sample) {
        let (_, tz, file) = self.first.expect("a minute has at least one reading");
        (
            file,
            Sample {
                metric: "hr",
                t: DateTime::from_timestamp(minute * 60, 0).expect("minute of a valid reading"),
                t_end: None,
                v: (self.sum / self.values.len() as f64).round(),
                tz,
                source: 0,
            },
        )
    }
}

fn spend_bytes(size: u64, limit: u64, expanded: &mut u64) -> Result<()> {
    *expanded = expanded.saturating_add(size);
    if size > limit || *expanded > MAX_EXPANDED {
        return Err(refuse(LIMIT));
    }
    Ok(())
}

fn member_bytes(file: impl Read, size: u64) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    if !read_member(file, size, &mut bytes)? {
        return Err(refuse(UNREADABLE));
    }
    Ok(bytes)
}

fn read_zone<R: Read>(file: ZipFile<'_, R>, expanded: &mut u64) -> Result<Option<Tz>> {
    let size = file.size();
    if size > MAX_JSON {
        return Ok(None);
    }
    spend_bytes(size, MAX_JSON, expanded)?;
    let bytes = member_bytes(file, size)?;
    Ok(profile_zone(bytes.strip_prefix(b"\xef\xbb\xbf").unwrap_or(&bytes)))
}

struct ReadingFile {
    index: usize,
    path: String,
    root: String,
    kind: Member,
}

fn scan_archive(ctx: &JobContext, owner: BlobOwner, sha: &str) -> Result<Report> {
    let mut zip = open_archive(ctx.blobs.open_seek(owner, sha)?, UNREADABLE, LIMIT)?;
    let mut report = Report::default();
    let mut profiles = Vec::new();
    let mut members = Vec::new();
    let mut names = 0;
    let mut files = 0;
    for index in 0..zip.len() {
        let header = zip
            .by_index_data(index)
            .map_err(|e| archive_error(e, UNREADABLE, LIMIT))?;
        names += header.name_raw().len();
        files += usize::from(!header.is_dir());
        if names > GARMIN_MAX_NAME_BYTES || files > GARMIN_MAX_FILES {
            return Err(refuse(LIMIT));
        }
        let Ok(name) = header.name() else { continue };
        // Files outside the Fitbit and Google Health folders belong to other Takeout products.
        let Some((root, relative)) = fitbit_takeout_path(&name) else {
            continue;
        };
        if header.is_dir() {
            continue;
        }
        let shown = || {
            hide_email(
                &name
                    .chars()
                    .map(|c| if c.is_control() { '\u{fffd}' } else { c })
                    .collect::<String>(),
            )
        };
        if header.is_symlink() || unsafe_name(&name) {
            report.omit(shown(), "unsafe_path");
            continue;
        }
        match member_kind(relative) {
            Some(MemberKind::Profile) => profiles.push((index, root.to_ascii_lowercase())),
            Some(MemberKind::Readings(kind)) => members.push(ReadingFile {
                index,
                root: root.to_ascii_lowercase(),
                path: name.into_owned(),
                kind,
            }),
            None => report.omit(shown(), "unsupported"),
        }
    }
    if members.is_empty() {
        return Err(refuse(UNKNOWN));
    }

    let mut expanded = 0;
    let mut zones = BTreeMap::new();
    for (index, root) in profiles {
        let zone = zip
            .by_index(index)
            .map_err(|e| archive_error(e, UNREADABLE, LIMIT))
            .and_then(|file| read_zone(file, &mut expanded));
        let zone = match zone {
            Ok(zone) => zone,
            Err(Error::Upload(code)) if code == UNREADABLE => None,
            Err(error) => return Err(error),
        };
        // A malformed profile or conflicting profiles within this folder leave its zone unknown.
        zones
            .entry(root)
            .and_modify(|current| {
                if *current != zone {
                    *current = None;
                }
            })
            .or_insert(zone);
    }
    let mut available = zones.values().flatten();
    let shared_zone = available.next().copied().filter(|first| available.all(|z| z == first));

    // The first path in lexicographic order wins when heart-rate values at the same instant
    // disagree. Duplicate ZIP paths are refused by open_archive.
    members.sort_unstable_by(|first, second| first.path.cmp(&second.path));
    let latest = Utc::now() + Duration::days(1);
    let mut input_rows = 0;
    let mut imported = Vec::new();
    let mut minutes = BTreeMap::<i64, HeartRateMinute>::new();
    let mut non_heart_rate = BTreeMap::new();
    for ReadingFile {
        index,
        path,
        root,
        kind,
    } in members
    {
        let name = hide_email(&path);
        let zone = zones.get(&root).copied().unwrap_or(shared_zone);
        let heart_rate = matches!(kind.metric, Metric::HeartRate);
        let file = zip.by_index(index).map_err(|e| archive_error(e, UNREADABLE, LIMIT))?;
        let size = file.size();
        let limit = if heart_rate && !kind.json {
            MAX_HEART_RATE_CSV
        } else {
            MAX_JSON
        };
        spend_bytes(size, limit, &mut expanded)?;

        // Stage one member at a time so an invalid CSV cannot contribute a partial minute.
        let mut member_minutes = BTreeMap::<i64, HeartRateMinute>::new();
        let mut member_points = BTreeMap::new();
        let mut left_out = 0;
        let mut over_limit = false;
        let member = imported.len();
        let push = |raw: Option<&str>, value: Option<f64>| {
            input_rows += 1;
            if input_rows > MAX_INPUT_ROWS {
                return Err(refuse(LIMIT));
            }
            let Some(v) = value.filter(|_| !over_limit) else {
                return Ok(());
            };
            let Some((t, offset)) = raw.and_then(|raw| when(raw, kind, zone)) else {
                left_out += 1;
                return Ok(());
            };
            if let Some(sample) = kind.metric.sample(t, offset, v, latest) {
                if heart_rate {
                    let minute = t.timestamp().div_euclid(60);
                    member_minutes.entry(minute).or_default().add_sample(sample, member);
                } else {
                    member_points.insert(t, sample);
                }
                over_limit = member_minutes.len() + member_points.len() > MAX_READINGS;
            }
            Ok(())
        };
        let read = if kind.json {
            let bytes = member_bytes(file, size)?;
            json_rows(bytes.strip_prefix(b"\xef\xbb\xbf").unwrap_or(&bytes), kind.metric, push)
        } else {
            csv_rows(file, size, kind.metric, push)
        };
        let invalid = if kind.json { INVALID_JSON } else { INVALID_CSV };
        match read {
            Ok(()) => {}
            Err(Error::Upload(code)) if code == invalid => {
                report.omit(name, invalid);
                continue;
            }
            Err(error) => return Err(error),
        }
        if over_limit {
            return Err(refuse(LIMIT));
        }
        report.rows_left_out += left_out;
        if member_minutes.is_empty() && member_points.is_empty() {
            report.omit(name, "no_readings");
            continue;
        }
        imported.push((name, Vec::new()));
        for (minute, incoming) in member_minutes {
            match minutes.entry(minute) {
                Entry::Vacant(slot) => {
                    slot.insert(incoming);
                }
                Entry::Occupied(mut slot) => slot.get_mut().merge(incoming),
            }
        }
        for (t, sample) in member_points {
            // Keep the existing last-ZIP-member precedence for non-heart-rate metrics.
            match non_heart_rate.entry((sample.metric, t)) {
                Entry::Vacant(slot) => {
                    slot.insert((index, member, sample));
                }
                Entry::Occupied(mut slot) if index > slot.get().0 => {
                    slot.insert((index, member, sample));
                }
                Entry::Occupied(_) => {}
            }
        }
        if minutes.len() + non_heart_rate.len() > MAX_READINGS {
            return Err(refuse(LIMIT));
        }
    }
    if imported.is_empty() {
        return Err(refuse(NO_READINGS));
    }
    for (minute, readings) in minutes {
        let (member, sample) = readings.finish(minute);
        imported[member].1.push(sample);
    }
    for (_, member, sample) in non_heart_rate.into_values() {
        imported[member].1.push(sample);
    }
    for (name, samples) in imported {
        report.add_member(name, samples);
    }
    Ok(report)
}
