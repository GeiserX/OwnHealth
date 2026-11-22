//! FIT activity files (lane D, v1.0 slice 29): a workout recorded by a watch, a bike computer or an app,
//! as the `.fit` file the device writes or that file gzipped (`.fit.gz`, as Strava and Garmin bulk
//! exports hand them out). Job kind `import.fit`, payload as the other structured importers
//! (`{profile_id, household_id, blob_sha256, filename}`, or `{document_id}` for an import again); core
//! sniffs the `.FIT` header (`documents::sniff`) as `fit` or `fit_gz`. The file becomes a document of the
//! profile and owns the samples it brings, so retracting it removes them.
//!
//! The file is read with `fitparser` (MIT), one message at a time, and only three things are kept:
//! - heart rate (`record.heart_rate`): one sample per minute, the mean of the readings in that minute,
//!   rounded to a whole beat per minute (`hr`);
//! - distance (`session.total_distance`): one sample per activity, from its start to its end, in km
//!   (`distance`). That metric is walking and running distance, so only a running, walking or hiking
//!   session gives one; the distance of another sport (cycling, swimming, ...) is counted as
//!   `other_sport_distance` and not stored;
//! - energy (`session.total_calories`): one sample per activity, from its start to its end, in kcal
//!   (`active_energy`): the calories the device counted for the activity.
//!
//! Position is never kept: latitude, longitude and every other position or GPS field are read past and
//! never copied into a sample, a flag, the job result, a log line or a review item. The result says only
//! whether the file held any (`gps_left_out`), so the person knows the route was left out.
//!
//! Times: FIT timestamps are UTC (seconds since 1989-12-31 00:00 UTC), so no time zone is read or
//! guessed and samples keep no offset. A time before 2000 (a device whose clock was never set writes
//! times near 1990) or more than a day ahead is counted as `bad_time`.
//!
//! Limits: a file (or a gzipped file once unpacked) over [`MAX_BYTES`] is refused, so a gzip bomb never
//! fills memory. One file makes at most one wearable source: the device its first `file_id` names
//! (a chained file with more `file_id` messages adds none), so a crafted file cannot use up the
//! install-wide `wearable_source` id sequence.
//!
//! A damaged file is refused with a reason: cut short (shorter than its header says), a checksum that
//! does not match, or data the parser cannot read. A FIT file with no heart rate, distance or energy
//! (a course, a settings file, a monitoring file) is refused as holding no activity.
//!
//! Re-import: `Conflict::Keep`, the first file to bring a sample owns it. The same file again, or the
//! same activity as `.fit` and as `.fit.gz`, adds nothing; its samples count as `duplicate`.

use std::collections::BTreeMap;
use std::io::Read as _;

use chrono::{DateTime, Duration, TimeZone, Utc};
use fitparser::de::{FitObject, FitStreamProcessor};
use fitparser::profile::MesgNum;
use fitparser::{ErrorKind, Value as FitValue};
use ownhealth_core::access::get_profile;
use ownhealth_core::blobs::{BlobOwner, BlobStore};
use ownhealth_core::documents;
use ownhealth_core::error::{Error, Result};
use ownhealth_core::ingest::ImportSummary;
use ownhealth_core::jobs::{self, Job, JobContext, JobError};
use ownhealth_ingest_structured::jobs::document_for;
use serde_json::{Value, json};
use sqlx::PgConnection;
use uuid::Uuid;

use crate::store::{self, Conflict, Sample};

pub const JOB: &str = "import.fit";
/// `wearable_source.provider` of a device whose activities came in as FIT files.
pub const PROVIDER: &str = "fit";
/// The largest FIT file read, and the most a `.fit.gz` may unpack to: a day of one-second records
/// is a few MB.
pub const MAX_BYTES: u64 = 32 * 1024 * 1024;
/// The sports whose distance is walking and running distance (`distance`).
pub const DISTANCE_SPORTS: [&str; 3] = ["running", "walking", "hiking"];
/// The earliest sample time kept: 2000-01-01 UTC.
const EARLIEST: i64 = 946_684_800;
/// Plausible values: heart rate per minute, a distance in km, an energy in kcal.
const HR: (f64, f64) = (25.0, 250.0);
const KM: (f64, f64) = (0.0, 1000.0);
const KCAL: (f64, f64) = (0.0, 20_000.0);
/// A device name read from the file is cut to this many characters.
const MAX_NAME: usize = 80;

/// The device that made the file (its first `file_id`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Device {
    pub name: String,
    pub serial: Option<String>,
}

/// One activity (a `session`; a multisport activity has one per sport).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Session {
    pub start: Option<DateTime<Utc>>,
    pub end: Option<DateTime<Utc>>,
    pub sport: Option<String>,
    pub distance_m: Option<f64>,
    pub kcal: Option<f64>,
}

/// What a FIT file holds that we keep, and nothing else.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Read {
    pub device: Option<Device>,
    /// Minute start (unix seconds) -> (sum, readings) of the heart rate readings in that minute.
    pub hr: BTreeMap<i64, (f64, u32)>,
    pub sessions: Vec<Session>,
    /// Heart rate readings outside the plausible range.
    pub bad_hr: i64,
    /// Readings whose time is before 2000 or more than a day ahead.
    pub bad_time: i64,
    /// The file held at least one position or GPS field (never its value).
    pub positions: bool,
}

fn unreadable(why: &str) -> Error {
    Error::Upload(format!("not a readable FIT file: {why}"))
}

/// A gzipped FIT file unpacked, at most [`MAX_BYTES`]; any other bytes as they are.
pub fn unpack(bytes: Vec<u8>) -> Result<Vec<u8>> {
    if !bytes.starts_with(&[0x1f, 0x8b]) {
        return Ok(bytes);
    }
    let mut out = Vec::new();
    flate2::read::MultiGzDecoder::new(bytes.as_slice())
        .take(MAX_BYTES + 1)
        .read_to_end(&mut out)
        .map_err(|_| unreadable("the gzip data is damaged"))?;
    if out.len() as u64 > MAX_BYTES {
        return Err(too_big());
    }
    Ok(out)
}

fn too_big() -> Error {
    Error::Upload(format!(
        "the FIT file is larger than {} MB once unpacked: this is not one activity",
        MAX_BYTES / 1024 / 1024
    ))
}

fn timestamp(v: &FitValue) -> Option<DateTime<Utc>> {
    match v {
        FitValue::Timestamp(t) => Some(t.with_timezone(&Utc)),
        _ => None,
    }
}

fn number(v: &FitValue) -> Option<f64> {
    match v {
        FitValue::Timestamp(_) | FitValue::String(_) | FitValue::Array(_) | FitValue::Invalid => None,
        other => TryInto::<f64>::try_into(other.clone()).ok().filter(|n| n.is_finite()),
    }
}

fn text(v: &FitValue) -> Option<String> {
    match v {
        FitValue::String(s) => Some(s.clone()),
        other => number(other).map(|n| format!("{n}")),
    }
}

/// A position or GPS field, by its profile name (`position_lat`, `start_position_long`, `nec_lat`,
/// `gps_accuracy`, ...). Only its presence is noted.
fn is_position(name: &str) -> bool {
    name.ends_with("_lat") || name.ends_with("_long") || name.contains("position") || name.starts_with("gps")
}

/// A name from the file: printable characters only, trimmed, at most [`MAX_NAME`] characters.
fn clean(s: &str) -> String {
    let s: String = s.chars().filter(|c| !c.is_control()).collect();
    s.trim().chars().take(MAX_NAME).collect()
}

/// `wahoo_fitness` -> `Wahoo fitness`.
fn pretty(s: &str) -> String {
    let s = s.replace('_', " ");
    let mut c = s.chars();
    c.next()
        .map(|f| f.to_uppercase().chain(c).collect())
        .unwrap_or_default()
}

fn device(fields: &[fitparser::FitDataField]) -> Device {
    let get = |n: &str| fields.iter().find(|f| f.name() == n).map(|f| f.value());
    let product_name = get("product_name").and_then(text).map(|s| clean(&s));
    let manufacturer = get("manufacturer").and_then(text).map(|s| pretty(&clean(&s)));
    let product = ["garmin_product", "favero_product", "product"]
        .iter()
        .find_map(|n| get(n).and_then(text))
        .map(|s| clean(&s));
    let name = match (product_name.filter(|n| !n.is_empty()), manufacturer, product) {
        (Some(n), ..) => n,
        (None, Some(m), Some(p)) => clean(&format!("{m} {p}")),
        (None, Some(m), None) => m,
        (None, None, Some(p)) => p,
        (None, None, None) => String::new(),
    };
    Device {
        name: if name.is_empty() { "FIT device".into() } else { name },
        serial: get("serial_number").and_then(number).map(|n| format!("{n}")),
    }
}

fn in_time(t: DateTime<Utc>, latest: DateTime<Utc>) -> bool {
    t.timestamp() >= EARLIEST && t <= latest
}

/// Read a FIT file (unpacked). Blocking: call it from `spawn_blocking`.
pub fn read_fit(bytes: &[u8]) -> Result<Read> {
    if bytes.len() as u64 > MAX_BYTES {
        return Err(too_big());
    }
    // the header says how long the data is: a shorter file was cut short
    if bytes.len() < 12 || &bytes[8..12] != b".FIT" || !matches!(bytes[0], 12 | 14) {
        return Err(unreadable("it has no FIT header"));
    }
    let header = u64::from(bytes[0]);
    let data = u64::from(u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]));
    if (bytes.len() as u64) < header + data + 2 {
        return Err(unreadable("it is cut short"));
    }
    let latest = Utc::now() + Duration::days(1);
    let mut out = Read::default();
    let mut p = FitStreamProcessor::new();
    let mut rest = bytes;
    let mut closed = false;
    while !rest.is_empty() {
        let (next, obj) = p.deserialize_next(rest).map_err(|e| fit_error(&e))?;
        rest = next;
        match obj {
            FitObject::Header(_) => closed = false,
            FitObject::Crc(_) => {
                p.reset();
                closed = true;
            }
            FitObject::DefinitionMessage(_) => {}
            FitObject::DataMessage(m) => {
                let rec = p.decode_message(m).map_err(|e| fit_error(&e))?;
                take(&mut out, rec.kind(), rec.fields(), latest);
            }
        }
    }
    if !closed {
        return Err(unreadable("it is cut short"));
    }
    Ok(out)
}

fn fit_error(e: &ErrorKind) -> Error {
    match e {
        ErrorKind::InvalidCrc(..) => unreadable("its checksum does not match, so it is damaged"),
        ErrorKind::UnexpectedEof(_) => unreadable("it is cut short"),
        other => unreadable(&format!("it is damaged ({other})")),
    }
}

fn take(out: &mut Read, kind: MesgNum, fields: &[fitparser::FitDataField], latest: DateTime<Utc>) {
    if fields.iter().any(|f| is_position(f.name())) {
        out.positions = true;
    }
    let get = |n: &str| fields.iter().find(|f| f.name() == n).map(|f| f.value());
    match kind {
        MesgNum::FileId if out.device.is_none() => out.device = Some(device(fields)),
        MesgNum::Record => {
            let Some(hr) = get("heart_rate").and_then(number) else {
                return;
            };
            let Some(t) = get("timestamp").and_then(timestamp) else {
                out.bad_time += 1;
                return;
            };
            if !in_time(t, latest) {
                out.bad_time += 1;
            } else if !(HR.0..=HR.1).contains(&hr) {
                out.bad_hr += 1;
            } else {
                let minute = t.timestamp().div_euclid(60) * 60;
                let e = out.hr.entry(minute).or_insert((0.0, 0));
                e.0 += hr;
                e.1 += 1;
            }
        }
        MesgNum::Session => {
            let start = get("start_time").and_then(timestamp);
            let elapsed = get("total_elapsed_time").and_then(number).filter(|s| *s > 0.0);
            let end = match (start, elapsed) {
                (Some(s), Some(secs)) => Some(s + Duration::milliseconds((secs * 1000.0).round() as i64)),
                _ => get("timestamp")
                    .and_then(timestamp)
                    .filter(|e| start.is_some_and(|s| *e > s)),
            };
            out.sessions.push(Session {
                start,
                end,
                sport: get("sport").and_then(text),
                distance_m: get("total_distance").and_then(number),
                kcal: get("total_calories").and_then(number),
            });
        }
        _ => {}
    }
}

/// The samples of a read file for one wearable source, and the reasons the rest were left out.
pub fn samples(read: &Read, source: i16, latest: DateTime<Utc>) -> (Vec<Sample>, BTreeMap<&'static str, i64>) {
    let mut reasons: BTreeMap<&'static str, i64> = BTreeMap::new();
    let mut bump = |k: &'static str, n: i64| {
        if n > 0 {
            *reasons.entry(k).or_default() += n;
        }
    };
    bump("bad_time", read.bad_time);
    bump("bad_value", read.bad_hr);
    let mut out = Vec::new();
    for (minute, (sum, n)) in &read.hr {
        let Some(t) = Utc.timestamp_opt(*minute, 0).single() else {
            continue;
        };
        out.push(Sample {
            metric: "hr",
            t,
            t_end: None,
            v: (sum / f64::from(*n)).round(),
            tz: None,
            source,
        });
    }
    for s in &read.sessions {
        let has_distance = s.distance_m.is_some_and(|d| d > 0.0);
        let has_energy = s.kcal.is_some_and(|k| k > 0.0);
        if !has_distance && !has_energy {
            continue;
        }
        let Some(start) = s.start.filter(|t| in_time(*t, latest)) else {
            bump("bad_time", i64::from(has_distance) + i64::from(has_energy));
            continue;
        };
        let end = s.end.filter(|e| *e > start);
        if has_distance {
            let km = s.distance_m.unwrap_or_default() / 1000.0;
            let walking = s.sport.as_deref().is_some_and(|sp| DISTANCE_SPORTS.contains(&sp));
            if !walking {
                bump("other_sport_distance", 1);
            } else if km > KM.0 && km <= KM.1 {
                out.push(Sample {
                    metric: "distance",
                    t: start,
                    t_end: end,
                    v: (km * 1000.0).round() / 1000.0,
                    tz: None,
                    source,
                });
            } else {
                bump("bad_value", 1);
            }
        }
        if has_energy {
            let kcal = s.kcal.unwrap_or_default();
            if kcal > KCAL.0 && kcal <= KCAL.1 {
                out.push(Sample {
                    metric: "active_energy",
                    t: start,
                    t_end: end,
                    v: kcal.round(),
                    tz: None,
                    source,
                });
            } else {
                bump("bad_value", 1);
            }
        }
    }
    (out, reasons)
}

/// Import a stored FIT document into its profile. Returns the summary and the details the upload panel
/// shows (`fit` in the job result): counts per metric, the device, and whether positions were left out.
pub async fn import_fit(
    conn: &mut PgConnection,
    blobs: &BlobStore,
    document_id: Uuid,
    source_id: Option<Uuid>,
) -> Result<(ImportSummary, Value)> {
    let (profile_id, sha, status): (Option<Uuid>, String, String) =
        sqlx::query_as("select profile_id, encode(blob_sha256, 'hex'), status from document where id = $1")
            .bind(document_id)
            .fetch_one(&mut *conn)
            .await?;
    if status != "active" {
        // retracted while its import waited in the queue: it brings nothing back
        let mut s = ImportSummary::new(document_id.to_string());
        s.reasons.insert("retracted".into(), 1);
        return Ok((s, Value::Null));
    }
    let profile_id = profile_id.ok_or_else(|| Error::Invalid("the document has no profile".into()))?;
    let profile = get_profile(&mut *conn, profile_id)
        .await?
        .ok_or_else(|| Error::NotFound(format!("unknown profile {profile_id}")))?;
    let mut bytes = Vec::new();
    blobs
        .open_seek(BlobOwner::Profile(profile.id), &sha)?
        .take(MAX_BYTES + 1)
        .read_to_end(&mut bytes)?;
    let read = tokio::task::spawn_blocking(move || unpack(bytes).and_then(|b| read_fit(&b)))
        .await
        .map_err(|e| {
            if e.is_panic() {
                unreadable("the parser could not read it")
            } else {
                Error::Other(anyhow::anyhow!("FIT read task failed: {e}"))
            }
        })??;
    let has_activity = !read.hr.is_empty()
        || read
            .sessions
            .iter()
            .any(|s| s.distance_m.is_some_and(|d| d > 0.0) || s.kcal.is_some_and(|k| k > 0.0));
    if !has_activity {
        return Err(Error::Upload(
            "the FIT file holds no activity: no heart rate, distance or energy".into(),
        ));
    }
    let device = read.device.clone().unwrap_or_else(|| Device {
        name: "FIT device".into(),
        serial: None,
    });
    let source = store::device_source(
        &mut *conn,
        profile.id,
        PROVIDER,
        Some(&device.name),
        device.serial.as_deref(),
        source_id,
    )
    .await?;
    let (all, mut reasons) = samples(&read, source, Utc::now() + Duration::days(1));
    let mut touched = store::Touched::default();
    let mut new: BTreeMap<&'static str, i64> = BTreeMap::new();
    let mut duplicate = 0i64;
    for metric in ["hr", "distance", "active_energy"] {
        let batch: Vec<Sample> = all.iter().filter(|s| s.metric == metric).cloned().collect();
        let w = store::insert(
            &mut *conn,
            profile.id,
            Some(document_id),
            batch,
            Conflict::Keep,
            &mut touched,
        )
        .await?;
        new.insert(metric, w.new);
        duplicate += w.duplicate();
    }
    store::rebuild(&mut *conn, profile.id, touched).await?;
    if duplicate > 0 {
        reasons.insert("duplicate", duplicate);
    }
    let mut summary = ImportSummary::new(document_id.to_string());
    summary.accepted = new.values().sum();
    summary.skipped = reasons.values().sum();
    summary.reasons = reasons.into_iter().map(|(k, n)| (k.to_string(), n)).collect();
    let details = json!({
        "activities": read.sessions.len(),
        "heart_rate_minutes": new["hr"],
        "distances": new["distance"],
        "energy": new["active_energy"],
        "device": device.name,
        "gps_left_out": read.positions,
    });
    Ok((summary, details))
}

/// The upload kind of a stored blob, for the document's mime: `fit_gz` when it is gzipped.
fn stored_kind(blobs: &BlobStore, payload: &Value) -> &'static str {
    let profile = payload
        .get("profile_id")
        .and_then(Value::as_str)
        .and_then(|s| Uuid::parse_str(s).ok());
    let sha = payload.get("blob_sha256").and_then(Value::as_str);
    let mut head = [0u8; 2];
    let gz = match (profile, sha) {
        (Some(p), Some(s)) => blobs
            .open_seek(BlobOwner::Profile(p), s)
            .is_ok_and(|mut r| r.read_exact(&mut head).is_ok() && head == [0x1f, 0x8b]),
        _ => false,
    };
    if gz { "fit_gz" } else { "fit" }
}

async fn run(ctx: JobContext, job: Job) -> jobs::HandlerResult {
    let channel = job
        .str("channel")
        .filter(|c| !c.is_empty())
        .unwrap_or("upload")
        .to_string();
    let source_id = job.str("source_id").and_then(|s| Uuid::parse_str(s).ok());
    let mime = documents::mime(stored_kind(&ctx.blobs, &job.payload)).unwrap_or("application/octet-stream");
    let mut tx = ctx.pool.begin().await?;
    let doc = document_for(&mut tx, &ctx.blobs, &job.payload, mime, &channel).await?;
    let (summary, details) = import_fit(&mut tx, &ctx.blobs, doc, source_id)
        .await
        .map_err(|e| match e {
            Error::Upload(m) | Error::Invalid(m) => JobError::permanent(m),
            other => JobError::from(other),
        })?;
    tx.commit().await?;
    let mut out = summary.to_value();
    out["fit"] = details;
    Ok(Some(out))
}

pub fn register() {
    jobs::register_handler(JOB, |ctx, job| Box::pin(run(ctx, job)));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn position_fields_are_named_by_their_profile_names() {
        for n in [
            "position_lat",
            "position_long",
            "start_position_lat",
            "end_position_long",
            "nec_lat",
            "swc_long",
            "gps_accuracy",
        ] {
            assert!(is_position(n), "{n}");
        }
        for n in ["heart_rate", "timestamp", "total_distance", "sport", "total_calories"] {
            assert!(!is_position(n), "{n}");
        }
    }

    #[test]
    fn a_file_shorter_than_its_header_says_is_cut_short() {
        let mut b = vec![14u8, 0x20, 0x6c, 0x08, 100, 0, 0, 0];
        b.extend_from_slice(b".FIT");
        b.extend_from_slice(&[0, 0]);
        b.extend(std::iter::repeat_n(0u8, 50));
        let e = read_fit(&b).err().map(|e| e.to_string()).unwrap_or_default();
        assert!(e.contains("not a readable FIT file: it is cut short"), "{e}");
        let e = read_fit(b"hello, world!")
            .err()
            .map(|e| e.to_string())
            .unwrap_or_default();
        assert!(e.contains("no FIT header"), "{e}");
    }

    #[test]
    fn device_names_are_cleaned_and_cut() {
        assert_eq!(pretty("wahoo_fitness"), "Wahoo fitness");
        assert_eq!(clean(" a\u{7}b "), "ab");
        assert_eq!(clean(&"x".repeat(200)).len(), MAX_NAME);
    }
}
