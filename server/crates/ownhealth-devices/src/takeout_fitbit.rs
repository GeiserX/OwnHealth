//! Fitbit and Google Health Takeout archives. The original ZIP and account profile do not become
//! documents. The retained JSON lines name each member that supplied readings.

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
const TIMEZONE: &str = "fitbit_takeout_timezone_required";
const LOCAL_TIME: &str = "fitbit_takeout_local_time";
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
    details: Report::file_count,
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

enum MemberKind {
    Profile,
    Json(Metric),
    Csv(Metric),
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
    if stem == "profile" && extension == "json" {
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
                return Some(MemberKind::Json(metric));
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
                return Some(MemberKind::Csv(metric));
            }
        }
    }
    // Older exports also ship separate Fitbit CSVs alongside their Global Export Data JSON.
    if folder.eq_ignore_ascii_case("Heart Rate Variability") && series.is_empty() {
        for label in [
            "heart rate variability details",
            "heart rate variability summary",
            "daily heart rate variability summary",
        ] {
            if named(&stem, label) {
                return Some(MemberKind::Csv(Metric::Rmssd));
            }
        }
    }
    if ["Oxygen Saturation", "SpO2", "Global Export Data"]
        .iter()
        .any(|known| folder.eq_ignore_ascii_case(known))
        && series.is_empty()
        && ["daily spo2", "daily oxygen saturation"]
            .iter()
            .any(|label| named(&stem, label))
    {
        return Some(MemberKind::Csv(Metric::Spo2));
    }
    None
}

fn numeric(value: &Value) -> Option<f64> {
    value.as_f64().or_else(|| value.as_str()?.trim().parse().ok())
}

fn profile_zone(bytes: &[u8]) -> Result<Tz> {
    let value: Value = serde_json::from_slice(bytes).map_err(|_| refuse(TIMEZONE))?;
    let row = value.get("profile").unwrap_or(&value);
    row.get("timezone")
        .or_else(|| row.get("timeZone"))
        .and_then(Value::as_str)
        .and_then(|zone| zone.trim().parse().ok())
        .ok_or_else(|| refuse(TIMEZONE))
}

fn timestamp(raw: &str, zone: Option<Tz>, legacy_json: bool) -> Result<(DateTime<Utc>, i16)> {
    let raw = raw.trim();
    if let Ok(t) = DateTime::parse_from_rfc3339(raw) {
        return Ok((t.to_utc(), (t.offset().local_minus_utc() / 60) as i16));
    }
    let day = NaiveDate::parse_from_str(raw, "%Y-%m-%d").ok();
    if !legacy_json && let Some(day) = day {
        // Google CSV daily summaries give a calendar day. Preserve its rollup day at UTC midnight.
        return Ok((day.and_hms_opt(0, 0, 0).expect("midnight").and_utc(), 0));
    }
    let local = ["%m/%d/%y %H:%M:%S%.f", "%Y-%m-%dT%H:%M:%S%.f", "%Y-%m-%d %H:%M:%S%.f"]
        .iter()
        .find_map(|format| NaiveDateTime::parse_from_str(raw, format).ok())
        .or_else(|| day.and_then(|d| d.and_hms_opt(0, 0, 0)))
        .ok_or_else(|| refuse(UNREADABLE))?;
    let t = zone
        .ok_or_else(|| refuse(TIMEZONE))?
        .from_local_datetime(&local)
        .single()
        .ok_or_else(|| refuse(LOCAL_TIME))?;
    Ok((t.to_utc(), (t.offset().fix().local_minus_utc() / 60) as i16))
}

fn json_rows(bytes: &[u8], metric: Metric, mut push: impl FnMut(&str, f64) -> Result<()>) -> Result<()> {
    let rows: Vec<Value> = serde_json::from_slice(bytes).map_err(|_| refuse(UNREADABLE))?;
    for row in rows {
        if let Some(v) = metric.json_value(&row) {
            let time = row["dateTime"]
                .as_str()
                .or_else(|| row["timestamp"].as_str())
                .or_else(|| row["date"].as_str())
                .ok_or_else(|| refuse(UNREADABLE))?;
            push(time, v)?;
        }
    }
    Ok(())
}

fn csv_rows(bytes: &[u8], metric: Metric, mut push: impl FnMut(&str, f64) -> Result<()>) -> Result<()> {
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
            push(record.get(time).ok_or_else(|| refuse(UNREADABLE))?, v)?;
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
    let mut root = None;
    let mut profile = None;
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
        let Some((folder, relative)) = fitbit_takeout_path(&name) else {
            continue;
        };
        if header.is_dir() || header.is_symlink() || unsafe_name(&name) {
            continue;
        }
        let Some(kind) = member_kind(relative) else { continue };
        if root.get_or_insert_with(|| folder.to_string()) != folder {
            return Err(refuse(UNKNOWN));
        }
        match kind {
            MemberKind::Profile if profile.replace(index).is_some() => return Err(refuse(TIMEZONE)),
            MemberKind::Profile => {}
            _ => members.push((index, hide_email(&name), kind)),
        }
    }
    if members.is_empty() {
        return Err(refuse(UNKNOWN));
    }

    let mut expanded = 0;
    let zone = profile
        .map(|index| {
            member_bytes(&mut zip, index, &mut expanded)
                .and_then(|bytes| profile_zone(bytes.strip_prefix(b"\xef\xbb\xbf").unwrap_or(&bytes)))
        })
        .transpose();
    // CSVs with explicit offsets and calendar-only dates do not need a profile.
    let zone = match zone {
        Ok(zone) => zone,
        Err(Error::Upload(code)) if code == TIMEZONE => None,
        Err(error) => return Err(error),
    };
    let latest = Utc::now() + Duration::days(1);
    let mut report = Report::default();
    let mut count = 0;
    for (index, name, kind) in members {
        let (metric, legacy) = match kind {
            MemberKind::Json(metric) => (metric, true),
            MemberKind::Csv(metric) => (metric, false),
            MemberKind::Profile => unreachable!("only readings members are collected"),
        };
        let bytes = member_bytes(&mut zip, index, &mut expanded)?;
        let bytes = bytes.strip_prefix(b"\xef\xbb\xbf").unwrap_or(&bytes);
        let mut samples = Vec::new();
        let push = |raw: &str, v| {
            let (t, offset) = timestamp(raw, zone, legacy)?;
            if let Some(sample) = metric.sample(t, offset, v, latest) {
                count += 1;
                if count > MAX_READINGS {
                    return Err(refuse(LIMIT));
                }
                samples.push(sample);
            }
            Ok(())
        };
        if legacy {
            json_rows(bytes, metric, push)?;
        } else {
            csv_rows(bytes, metric, push)?;
        }
        if !samples.is_empty() {
            report.add_member(name, samples);
        }
    }
    if count == 0 {
        return Err(refuse(NO_READINGS));
    }
    Ok(report)
}
