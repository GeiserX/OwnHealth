//! Fitbit and Google Health Takeout archives. The original ZIP and account profile do not become
//! documents. The retained JSON lines name each member that supplied readings.
//!
//! Times, as public exports show them (the fixtures README names the sources):
//! - `Global Export Data` JSON times carry no zone and are UTC;
//! - Google Data CSVs write UTC with `Z`; the legacy HRV detail CSVs write local time with no zone;
//! - a daily value (resting heart rate, a daily SpO2 or HRV summary) names its calendar day, even when
//!   written as midnight UTC.
//!
//! The zone in `Your Profile/Profile.csv` gives a UTC time its local offset and places a local time.
//! A row whose time cannot be read or placed (no zone, or a clock change) is left out and counted;
//! the rest of the archive still imports.

use std::io::{Read, Seek};

use chrono::{DateTime, Datelike, Duration, NaiveDate, NaiveDateTime, Offset, TimeZone, Utc};
use chrono_tz::Tz;
use ownhealth_core::blobs::BlobOwner;
use ownhealth_core::documents::{FITBIT_READINGS_MIME, fitbit_takeout_path};
use ownhealth_core::jobs::{self, JobContext};
use ownhealth_core::zip_directory::{GARMIN_MAX_FILES, GARMIN_MAX_NAME_BYTES};
use ownhealth_core::{Error, Result};
use serde_json::Value;
use zip::ZipArchive;

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

/// Each row with a value: its raw time (None when the row has none) and the value.
fn json_rows(bytes: &[u8], metric: Metric, mut push: impl FnMut(Option<&str>, f64) -> Result<()>) -> Result<()> {
    let rows: Vec<Value> = serde_json::from_slice(bytes).map_err(|_| refuse(UNREADABLE))?;
    for row in rows {
        if let Some(v) = metric.json_value(&row) {
            let time = row["dateTime"]
                .as_str()
                .or_else(|| row["timestamp"].as_str())
                .or_else(|| row["date"].as_str());
            push(time, v)?;
        }
    }
    Ok(())
}

fn csv_rows(bytes: &[u8], metric: Metric, mut push: impl FnMut(Option<&str>, f64) -> Result<()>) -> Result<()> {
    let mut reader = csv::ReaderBuilder::new().trim(csv::Trim::All).from_reader(bytes);
    let headers: Vec<_> = reader
        .headers()
        .map_err(|_| refuse(UNREADABLE))?
        .iter()
        .map(words)
        .collect();
    let column = |names: &[&str]| {
        names
            .iter()
            .find_map(|name| headers.iter().position(|header| header == name))
    };
    let time = column(&["timestamp", "datetime", "date time", "date"]).ok_or_else(|| refuse(UNREADABLE))?;
    let value = column(metric.csv_values()).ok_or_else(|| refuse(UNREADABLE))?;
    for record in reader.records() {
        let record = record.map_err(|_| refuse(UNREADABLE))?;
        if let Some(v) = record.get(value).and_then(|v| v.parse().ok()) {
            push(record.get(time), v)?;
        }
    }
    Ok(())
}

fn member_bytes<R: Read + Seek>(zip: &mut ZipArchive<R>, index: usize, expanded: &mut u64) -> Result<Vec<u8>> {
    let file = zip.by_index(index).map_err(|e| archive_error(e, UNREADABLE, LIMIT))?;
    let size = file.size();
    *expanded = expanded.saturating_add(size);
    if size > MAX_JSON || *expanded > MAX_EXPANDED {
        return Err(refuse(LIMIT));
    }
    let mut bytes = Vec::new();
    if !read_member(file, size, &mut bytes)? {
        return Err(refuse(UNREADABLE));
    }
    Ok(bytes)
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
        let Some((_, relative)) = fitbit_takeout_path(&name) else {
            continue;
        };
        if header.is_dir() {
            continue;
        }
        let shown = hide_email(
            &name
                .chars()
                .map(|c| if c.is_control() { '\u{fffd}' } else { c })
                .collect::<String>(),
        );
        if header.is_symlink() || unsafe_name(&name) {
            report.omit(shown, "unsafe_path");
            continue;
        }
        match member_kind(relative) {
            Some(MemberKind::Profile) => profiles.push(index),
            Some(MemberKind::Readings(member)) => members.push((index, shown, member)),
            None => report.omit(shown, "unsupported"),
        }
    }
    if members.is_empty() {
        return Err(refuse(UNKNOWN));
    }

    let mut expanded = 0;
    // Two profiles that name different zones place nothing.
    let mut zones = Vec::new();
    for index in profiles {
        let bytes = member_bytes(&mut zip, index, &mut expanded)?;
        zones.push(profile_zone(bytes.strip_prefix(b"\xef\xbb\xbf").unwrap_or(&bytes)));
    }
    let zone = zones
        .first()
        .copied()
        .flatten()
        .filter(|z| zones.iter().all(|o| *o == Some(*z)));
    let latest = Utc::now() + Duration::days(1);
    let mut count = 0;
    for (index, name, member) in members {
        let bytes = member_bytes(&mut zip, index, &mut expanded)?;
        let bytes = bytes.strip_prefix(b"\xef\xbb\xbf").unwrap_or(&bytes);
        let mut samples = Vec::new();
        let mut left_out = 0;
        let before = count;
        let push = |raw: Option<&str>, v| {
            let Some((t, offset)) = raw.and_then(|raw| when(raw, member, zone)) else {
                left_out += 1;
                return Ok(());
            };
            if let Some(sample) = member.metric.sample(t, offset, v, latest) {
                count += 1;
                if count > MAX_READINGS {
                    return Err(refuse(LIMIT));
                }
                samples.push(sample);
            }
            Ok(())
        };
        let read = if member.json {
            json_rows(bytes, member.metric, push)
        } else {
            csv_rows(bytes, member.metric, push)
        };
        match read {
            Ok(()) => {}
            // A file that is not the JSON or CSV it should be is listed; the others still import.
            Err(Error::Upload(code)) if code == UNREADABLE => {
                count = before;
                report.omit(name, if member.json { "invalid_json" } else { "invalid_csv" });
                continue;
            }
            Err(error) => return Err(error),
        }
        report.rows_left_out += left_out;
        if samples.is_empty() {
            report.omit(name, "no_readings");
        } else {
            report.add_member(name, samples);
        }
    }
    if count == 0 {
        return Err(refuse(NO_READINGS));
    }
    Ok(report)
}
