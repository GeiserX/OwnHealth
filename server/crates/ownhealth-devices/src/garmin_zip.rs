//! Garmin account history from one Export Your Data archive. JSON members are matched by their
//! basenames and UploadedFiles ZIPs contain the FIT activity files. Reading uses archive indexes
//! and a private temporary file for each supported inner ZIP. Member names are never disk paths.
//! The stored wearables document contains only projected readings and their archive provenance.

use std::io::{self, Read, Seek, Write};

use chrono::{DateTime, Duration, NaiveDate, NaiveDateTime, Utc};
use ownhealth_core::blobs::BlobOwner;
use ownhealth_core::documents::{GarminMember, WEARABLES_DOC_TYPE};
use ownhealth_core::ingest::ImportSummary;
use ownhealth_core::jobs::{self, Job, JobContext, JobError};
use ownhealth_core::{Error, Result};
use ownhealth_ingest_structured::jobs::document_of_type;
use serde::Serialize;
use serde_json::{Value, json};
use sqlx::PgConnection;
use uuid::Uuid;
use zip::ZipArchive;
use zip::result::ZipError;

use crate::fit;
use crate::store::{self, Conflict, Sample, Touched, Written};

pub const JOB: &str = "import.garmin_zip";
pub const PROVIDER: &str = "garmin_export";

const MAX_MEMBERS: usize = 50_000;
const MAX_NAMES_BYTES: usize = 4 * 1024 * 1024;
const MAX_EXPANDED: u64 = 4 * 1024 * 1024 * 1024;
const MAX_JSON: u64 = 16 * 1024 * 1024;
const MAX_FIT_ZIP: u64 = 1024 * 1024 * 1024;
const MAX_READINGS: usize = 500_000;

const UNREADABLE: &str = "garmin_zip_unreadable";
const LIMIT: &str = "garmin_zip_limit";

#[derive(Default)]
struct Budget {
    members: usize,
    expanded: u64,
    names_bytes: usize,
    readings: usize,
}

impl Budget {
    fn add_members(&mut self, count: usize) -> Result<()> {
        self.members += count;
        if self.members > MAX_MEMBERS {
            return Err(Error::Upload(LIMIT.into()));
        }
        Ok(())
    }

    fn add_member(&mut self, name_bytes: usize, expanded: u64) -> Result<()> {
        self.expanded = self.expanded.saturating_add(expanded);
        self.names_bytes = self.names_bytes.saturating_add(name_bytes);
        if self.expanded > MAX_EXPANDED || self.names_bytes > MAX_NAMES_BYTES {
            return Err(Error::Upload(LIMIT.into()));
        }
        Ok(())
    }

    fn add_readings(&mut self, count: usize) -> Result<()> {
        self.readings += count;
        if self.readings > MAX_READINGS {
            return Err(Error::Upload(LIMIT.into()));
        }
        Ok(())
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
struct Report {
    files_imported: usize,
    not_imported: Vec<Omission>,
    gps_left_out: bool,
    readings: Vec<Readings>,
}

impl Report {
    fn omit(&mut self, member: String, reason: &'static str) {
        self.not_imported.push(Omission { member, reason });
    }

    fn add(
        &mut self,
        budget: &mut Budget,
        member: String,
        fit: bool,
        samples: std::result::Result<Vec<Sample>, &'static str>,
    ) -> Result<()> {
        match samples {
            Ok(samples) if !samples.is_empty() => {
                budget.add_readings(samples.len())?;
                self.files_imported += 1;
                self.readings.push(Readings { member, fit, samples });
            }
            Ok(_) => self.omit(member, "no_readings"),
            Err(reason) => self.omit(member, reason),
        }
        Ok(())
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

fn unsafe_name(name: &str) -> bool {
    name.is_empty()
        || name.starts_with(['/', '\\'])
        || name.contains(':')
        || name.chars().any(char::is_control)
        || name.split(['/', '\\']).any(|part| part == "..")
}

fn zip_error(error: ZipError) -> Error {
    match error {
        ZipError::Io(e) if e.kind() != io::ErrorKind::UnexpectedEof => e.into(),
        _ => Error::Upload(UNREADABLE.into()),
    }
}

/// ZipArchive collapses duplicate directory entries. Refuse such an archive instead of silently
/// losing a member or letting a second entry with the same name shadow the first.
fn open(reader: impl Read + Seek) -> Result<ZipArchive<impl Read + Seek>> {
    let zip = ZipArchive::new(reader).map_err(zip_error)?;
    if zip.has_duplicate_names() {
        return Err(Error::Upload(UNREADABLE.into()));
    }
    Ok(zip)
}

/// Traverse one outer ZIP and at most one UploadedFiles ZIP below it. The shared budget counts
/// headers and declared expansion across both levels, including files the importer does not use.
fn scan<R: Read + Seek>(
    zip: &mut ZipArchive<R>,
    parent: Option<&str>,
    budget: &mut Budget,
    report: &mut Report,
) -> Result<()> {
    budget.add_members(zip.len())?;
    for index in 0..zip.len() {
        let header = zip.by_index_data(index).map_err(zip_error)?;
        let size = header.size();
        let decoded = header.name();
        let display = decoded
            .clone()
            .unwrap_or_else(|_| String::from_utf8_lossy(header.name_raw()));
        budget.add_member(display.len() + parent.map_or(0, |p| p.len() + 3), size)?;
        let mut member = parent.map_or_else(String::new, |p| format!("{p} / "));
        member.extend(display.chars().map(|c| if c.is_control() { '\u{fffd}' } else { c }));
        if decoded.is_err() || unsafe_name(&display) || header.is_symlink() {
            report.omit(member, "unsafe_path");
            continue;
        }
        if header.is_dir() {
            report.omit(member, "directory");
            continue;
        }
        let Some(kind) = MemberKind::detect(&display, parent.is_some()) else {
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
        let Ok(file) = zip.by_index(index) else {
            report.omit(member, "invalid_member");
            continue;
        };
        // Read at most one byte past the declared size, even if the header lies.
        let mut bounded = file.take(size + 1);
        if matches!(kind, MemberKind::FitArchive) {
            let mut local = tempfile::tempfile()?;
            let mut read = 0;
            // Separate read errors (a broken member) from disk write errors (retryable).
            let mut chunk = [0; 64 * 1024];
            let valid = loop {
                match bounded.read(&mut chunk) {
                    Ok(0) => break read == size,
                    Ok(n) => {
                        read += n as u64;
                        local.write_all(&chunk[..n])?;
                    }
                    Err(_) => break false,
                }
            };
            if valid {
                local.rewind()?;
                match open(local) {
                    Ok(mut inner) => {
                        let before = report.files_imported;
                        scan(&mut inner, Some(&member), budget, report)?;
                        if report.files_imported == before {
                            report.omit(member, "no_readings");
                        } else {
                            report.files_imported += 1;
                        }
                    }
                    Err(Error::Upload(_)) => report.omit(member, "invalid_zip"),
                    Err(e) => return Err(e),
                }
            } else {
                report.omit(member, "invalid_member");
            }
            continue;
        }
        let mut bytes = Vec::new();
        if bounded.read_to_end(&mut bytes).is_err() || bytes.len() as u64 != size {
            report.omit(member, "invalid_member");
            continue;
        }
        let samples = match kind {
            MemberKind::Json(kind) => json_samples(kind, &bytes, MAX_READINGS - budget.readings)?,
            MemberKind::Fit => match std::panic::catch_unwind(|| fit::archive_activity(&bytes)) {
                Ok(Ok((samples, gps))) => {
                    report.gps_left_out |= gps;
                    Ok(samples)
                }
                _ => Err("invalid_fit"),
            },
            MemberKind::FitArchive => unreachable!(),
        };
        report.add(budget, member, matches!(kind, MemberKind::Fit), samples)?;
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
    let seconds = (instant(local)? - instant(gmt)?).num_seconds();
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
    let weight = &row["weight"];
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
                let grams = weight["weight"].as_f64().or_else(|| weight.as_f64());
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

fn json_samples(
    kind: GarminMember,
    data: &[u8],
    remaining: usize,
) -> Result<std::result::Result<Vec<Sample>, &'static str>> {
    let Ok(rows) = serde_json::from_slice::<Vec<Value>>(data.strip_prefix(b"\xef\xbb\xbf").unwrap_or(data)) else {
        return Ok(Err("invalid_json"));
    };
    let latest = Utc::now() + Duration::days(1);
    let mut samples = Vec::new();
    for row in &rows {
        row_samples(kind, row, latest, &mut samples);
        if samples.len() > remaining {
            return Err(Error::Upload(LIMIT.into()));
        }
    }
    Ok(Ok(samples))
}

#[derive(Serialize)]
struct Reading {
    member: usize,
    metric: &'static str,
    start: DateTime<Utc>,
    end: Option<DateTime<Utc>>,
    offset_min: Option<i16>,
    value: f64,
}

fn prepare(ctx: &JobContext, owner: BlobOwner, sha: &str) -> Result<(Report, Option<String>)> {
    let mut zip = open(ctx.blobs.open_seek(owner, sha)?)?;
    let mut report = Report::default();
    scan(&mut zip, None, &mut Budget::default(), &mut report)?;
    if report.readings.is_empty() {
        return Ok((report, None));
    }
    let mut file = tempfile::tempfile()?;
    {
        let mut writer = io::BufWriter::new(&mut file);
        let members: Vec<_> = report.readings.iter().map(|group| &group.member).collect();
        serde_json::to_writer(
            &mut writer,
            &json!({"format": "ownhealth.garmin-readings/1", "archive_sha256": sha, "members": members}),
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
    Ok((report, Some(kept)))
}

async fn write(
    conn: &mut PgConnection,
    profile: Uuid,
    doc: Uuid,
    readings: Vec<Readings>,
    details: &Value,
) -> Result<ImportSummary> {
    let mut summary = ImportSummary::new(doc.to_string());
    let start = readings.iter().flat_map(|g| g.samples.iter().map(|s| s.t)).min();
    let mut daily = Vec::new();
    let mut fit = Vec::new();
    for group in readings {
        let source = if group.fit { &mut fit } else { &mut daily };
        source.extend(group.samples);
    }

    let mut touched = Touched::default();
    let mut written = Written::default();
    // Keep total daily energy and energy per activity in separate sources. The daily series uses
    // one source per day, so a day with both never adds the workout calories to the whole-day total.
    for (mut readings, label) in [(daily, "Garmin Connect"), (fit, "Garmin FIT")] {
        if readings.is_empty() {
            continue;
        }
        let source = store::device_source(&mut *conn, profile, PROVIDER, Some(label), None, None).await?;
        for reading in &mut readings {
            reading.source = source;
        }
        written.add(store::insert(&mut *conn, profile, Some(doc), readings, Conflict::Keep, &mut touched).await?);
    }
    store::rebuild(&mut *conn, profile, touched).await?;
    sqlx::query(
        "update document set collected_at = $2, flags = flags || jsonb_build_object('garmin', $3::jsonb) where id = $1",
    )
    .bind(doc)
    .bind(start)
    .bind(details)
    .execute(conn)
    .await?;
    summary.accepted = written.new;
    summary.skipped = written.duplicate();
    if summary.skipped > 0 {
        summary.reasons.insert("duplicate".into(), summary.skipped);
    }
    Ok(summary)
}

async fn run(ctx: JobContext, job: Job) -> jobs::HandlerResult {
    let profile = job.uuid("profile_id")?;
    let sha = job.str("blob_sha256").ok_or_else(|| JobError::permanent(UNREADABLE))?;

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
    let prepared = tokio::task::spawn_blocking(move || prepare(&worker_ctx, BlobOwner::Profile(profile), &worker_sha))
        .await
        .map_err(|e| JobError::from(Error::Other(e.into())))?;
    let (report, kept) = match prepared {
        Ok(prepared) => prepared,
        Err(Error::Upload(code)) => {
            fit::forget_upload(&ctx, profile, sha, job.id).await;
            return Err(JobError::permanent(code));
        }
        Err(error) => return Err(error.into()),
    };
    let details = report.details();
    let mut tx = ctx.pool.begin().await?;
    let mut out = if let Some(kept) = kept {
        let mut payload = job.payload.clone();
        payload["blob_sha256"] = json!(kept);
        let filename = job.str("filename").unwrap_or("garmin.zip");
        let stem = filename.rsplit_once('.').map_or(filename, |(stem, _)| stem);
        payload["filename"] = json!(format!("{stem}.jsonl"));
        let doc = document_of_type(
            &mut tx,
            &ctx.blobs,
            &payload,
            "application/x-ndjson",
            "upload",
            WEARABLES_DOC_TYPE,
        )
        .await?;
        write(&mut tx, profile, doc, report.readings, &details)
            .await?
            .to_value()
    } else {
        json!({"accepted": 0, "skipped": 0, "reasons": {}})
    };
    out["garmin"] = details;
    sqlx::query("update job set result = $2 where id = $1")
        .bind(job.id)
        .bind(&out)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    fit::forget_upload(&ctx, profile, sha, job.id).await;
    Ok(Some(out))
}

pub fn register() {
    jobs::register_handler(JOB, |ctx, job| Box::pin(run(ctx, job)));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn archive_limits_count_across_nested_archives() -> anyhow::Result<()> {
        let mut budget = Budget::default();
        budget.add_members(MAX_MEMBERS - 2)?;
        budget.add_members(2)?;
        assert!(matches!(budget.add_members(1), Err(Error::Upload(code)) if code == LIMIT));

        budget.add_member(1, MAX_EXPANDED - 1)?;
        budget.add_member(1, 1)?;
        assert!(matches!(budget.add_member(1, 1), Err(Error::Upload(code)) if code == LIMIT));
        Ok(())
    }
}
