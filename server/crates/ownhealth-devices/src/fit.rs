//! FIT activity files (lane D, v1.0 slice 29): a workout recorded by a watch, a bike computer or an app,
//! as the `.fit` file the device writes or that file gzipped (`.fit.gz`, as Strava and Garmin bulk
//! exports hand them out). Job kind `import.fit`, payload as the other structured importers
//! (`{profile_id, household_id, blob_sha256, filename}`, or `{document_id}` for an import again); core
//! sniffs the `.FIT` header (`documents::sniff`) as `fit` or `fit_gz`. The file becomes a wearables
//! document of the profile (`doc_type` [`WEARABLES_DOC_TYPE`], so only a member who may read wearables
//! sees it) and owns the samples it brings, so retracting it removes them.
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
//! Position is never kept: latitude, longitude and every other position field (and an NMEA sentence,
//! which carries one as text) are read past and never copied into a sample, a flag, the job result, a
//! log line or a review item. The result says only whether the file held any (`gps_left_out`), so the
//! person knows the route was left out.
//!
//! The stored file holds no position either. An upload is read once; the document then keeps a copy
//! of it ([`strip`]) whose position values are the FIT invalid value (the file's own layout, header and
//! messages, with its checksums written again), gzip unpacked, and the uploaded bytes are deleted
//! from the blob store. A refused upload is deleted too. Either way the bytes stay only while another
//! row still holds them (a document, an inbox item, a held mail, or an import of them that is still
//! queued or running); of two imports of one upload, the last to end deletes it. A developer field is
//! blanked when its description names a position or says it stands for a profile position field
//! ([`PROFILE_POSITIONS`]), even when the description comes after its values.
//!
//! Times: FIT timestamps are UTC (seconds since 1989-12-31 00:00 UTC), so no time zone is read or
//! guessed and samples keep no offset. A time before 2000 (a device whose clock was never set writes
//! times near 1990) or more than a day ahead, an activity that ends more than a day ahead or lasts more
//! than [`MAX_SPAN_DAYS`] days, is counted as `bad_time`.
//!
//! Limits: a file (or a gzipped file once unpacked) over [`MAX_BYTES`] is refused, so a gzip bomb never
//! fills memory. A file brings at most [`MAX_MINUTES`] minutes of heart rate (7 days) and
//! [`MAX_ACTIVITIES`] activities; what is past them is counted as `over_limit`. One file makes at most
//! one wearable source: the device its first `file_id` names (a chained file with more `file_id`
//! messages adds none), so a crafted file cannot use up the install-wide `wearable_source` id sequence.
//!
//! A damaged file is refused with a reason: cut short (shorter than its header says), a checksum that
//! does not match, or data the parser cannot read. A FIT file that brings no heart rate, distance or
//! energy (a course, a settings file, a monitoring file, or one whose every value was left out) is
//! refused as holding no activity.
//!
//! Re-import: `Conflict::Keep`, the first file to bring a sample owns it. The same file again, or the
//! same activity as `.fit` and as `.fit.gz` (one stored copy, so one document), adds nothing; its
//! samples count as `duplicate`, and so does a second activity of one file with the same start.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io::Read as _;

use chrono::{DateTime, Duration, TimeZone, Utc};
use fitparser::de::{FitObject, FitStreamProcessor};
use fitparser::profile::MesgNum;
use fitparser::{ErrorKind, Value as FitValue};
use ownhealth_core::access::get_profile;
use ownhealth_core::blobs::{BlobOwner, BlobStore};
use ownhealth_core::documents::WEARABLES_DOC_TYPE;
use ownhealth_core::error::{Error, Result};
use ownhealth_core::ingest::ImportSummary;
use ownhealth_core::jobs::{self, Job, JobContext, JobError};
use ownhealth_ingest_structured::jobs::document_of_type;
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
/// The most heart rate minutes one file brings: 7 days.
pub const MAX_MINUTES: usize = 7 * 24 * 60;
/// The most activities (sessions) one file brings.
pub const MAX_ACTIVITIES: usize = 1_000;
/// The longest activity kept, in days.
pub const MAX_SPAN_DAYS: i64 = 7;
/// The sports whose distance is walking and running distance (`distance`).
pub const DISTANCE_SPORTS: [&str; 3] = ["running", "walking", "hiking"];
/// The mime of a stored FIT document: always the plain file, never gzip.
const FIT_MIME: &str = "application/vnd.ant.fit";
/// The earliest sample time kept: 2000-01-01 UTC.
const EARLIEST: i64 = 946_684_800;
/// Plausible values: heart rate per minute, a distance in km, an energy in kcal.
const HR: (f64, f64) = (25.0, 250.0);
const KM: (f64, f64) = (0.0, 1000.0);
const KCAL: (f64, f64) = (0.0, 20_000.0);
/// A device name read from the file is cut to this many characters.
const MAX_NAME: usize = 80;
/// `field_description` (a developer field's name and base type) and `nmea_sentence` (a GPS receiver's
/// sentence, positions as text): global message numbers of the FIT profile.
const FIELD_DESCRIPTION: u16 = 206;
const NMEA_SENTENCE: u16 = 177;
const NMEA_TEXT: u8 = 1;
/// The fields of `field_description` that name the profile field a developer field stands for.
const NATIVE_MESG: u8 = 14;
const NATIVE_FIELD: u8 = 15;
/// Every position field of the FIT profile (fitparser 0.12) as (global message number, field number):
/// record, lap, session, split, segment lap and point, course point, climb, jump, GPS metadata and
/// weather. A developer field that declares itself one of these (`field_description` fields 14 and 15)
/// is a position whatever it is named.
pub const PROFILE_POSITIONS: [(u16, u8); 46] = [
    (18, 3),
    (18, 4),
    (18, 29),
    (18, 30),
    (18, 31),
    (18, 32),
    (18, 38),
    (18, 39),
    (19, 3),
    (19, 4),
    (19, 5),
    (19, 6),
    (19, 27),
    (19, 28),
    (19, 29),
    (19, 30),
    (20, 0),
    (20, 1),
    (32, 2),
    (32, 3),
    (128, 10),
    (128, 11),
    (142, 3),
    (142, 4),
    (142, 5),
    (142, 6),
    (142, 25),
    (142, 26),
    (142, 27),
    (142, 28),
    (150, 1),
    (150, 2),
    (160, 1),
    (160, 2),
    (285, 5),
    (285, 6),
    (312, 17),
    (312, 18),
    (312, 19),
    (312, 20),
    (312, 21),
    (312, 22),
    (312, 23),
    (312, 24),
    (317, 0),
    (317, 1),
];

/// Whether the profile field `field` of message `mesg` holds a position (an NMEA sentence holds one as
/// text).
fn native_position(mesg: u16, field: u8) -> bool {
    PROFILE_POSITIONS.contains(&(mesg, field)) || (mesg, field) == (NMEA_SENTENCE, NMEA_TEXT)
}

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
    /// The file gives an end that cannot be (an elapsed time past what a date can hold).
    pub bad_end: bool,
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
    /// Readings and activities past [`MAX_MINUTES`] and [`MAX_ACTIVITIES`].
    pub over_limit: i64,
    /// The file held at least one position (never its value).
    pub positions: bool,
    /// (global message number, field number) of the profile fields that held a position: what
    /// [`strip`] blanks in the stored copy.
    pub position_fields: BTreeSet<(u16, u8)>,
}

fn unreadable(why: &str) -> Error {
    Error::Upload(format!("not a readable FIT file: {why}"))
}

/// A gzipped FIT file unpacked, at most [`MAX_BYTES`]; any other bytes as they are. Bytes over
/// [`MAX_BYTES`] are refused as too large before anything is unpacked.
pub fn unpack(bytes: Vec<u8>) -> Result<Vec<u8>> {
    if bytes.len() as u64 > MAX_BYTES {
        return Err(too_big());
    }
    if !bytes.starts_with(&[0x1f, 0x8b]) {
        return Ok(bytes);
    }
    unpack_gzip(bytes.as_slice())
}

/// A gzip stream unpacked: the read stops at [`MAX_BYTES`] + 1 unpacked bytes, so a gzip bomb never
/// fills memory, and more than [`MAX_BYTES`] is refused as too large.
pub fn unpack_gzip(r: impl std::io::Read) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    flate2::read::MultiGzDecoder::new(r)
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

fn no_activity() -> Error {
    Error::Upload("the FIT file holds no activity: no heart rate, distance or energy".into())
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

/// A position field, by its name: the profile's `position_lat`, `start_position_long`, `nec_lat`,
/// `observed_location_long`, ...; a developer field (named by whoever wrote the app) also when its name
/// says position, location, GPS, latitude or longitude in any form.
fn is_position(name: &str, developer: bool) -> bool {
    let n = name.to_ascii_lowercase();
    let geo = ["_lat", "_long", "_lon", "_lng"].iter().any(|s| n.ends_with(s))
        || ["lat", "lon", "lng", "long"].contains(&n.as_str())
        || n.contains("latitude")
        || n.contains("longitude");
    geo || (developer && (n.contains("position") || n.contains("location") || n.contains("gps")))
}

/// Whether fitparser read `f` as a developer field (its `developer_data_index` is set). The crate
/// keeps that index private and only shows it through its serde form.
fn is_developer(f: &fitparser::FitDataField) -> bool {
    serde_json::to_value(f)
        .ok()
        .is_some_and(|v| v.get("developer_data_index").is_some_and(|i| !i.is_null()))
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
    let global = kind.as_u16();
    for f in fields {
        // an empty string is what a blanked text field reads as: it holds nothing
        if matches!(f.value(), FitValue::String(s) if s.is_empty()) {
            continue;
        }
        let key = (global, f.number());
        if global == NMEA_SENTENCE && f.number() == NMEA_TEXT {
            out.positions = true;
            out.position_fields.insert(key);
            continue;
        }
        if !is_position(f.name(), true) {
            continue;
        }
        let profile_name = is_position(f.name(), false);
        if profile_name && out.position_fields.contains(&key) {
            continue;
        }
        let developer = is_developer(f);
        if profile_name || developer {
            out.positions = true;
            if !developer {
                out.position_fields.insert(key);
            }
        }
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
                if out.hr.len() >= MAX_MINUTES && !out.hr.contains_key(&minute) {
                    out.over_limit += 1;
                    return;
                }
                let e = out.hr.entry(minute).or_insert((0.0, 0));
                e.0 += hr;
                e.1 += 1;
            }
        }
        MesgNum::Session => {
            if out.sessions.len() >= MAX_ACTIVITIES {
                out.over_limit += 1;
                return;
            }
            let start = get("start_time").and_then(timestamp);
            let elapsed = get("total_elapsed_time").and_then(number).filter(|s| *s > 0.0);
            let (end, bad_end) = match (start, elapsed) {
                (Some(s), Some(secs)) => {
                    // a float elapsed time can be anything: an end no date holds is a bad time, not a panic
                    let end = Duration::try_milliseconds((secs * 1000.0).round() as i64)
                        .and_then(|d| s.checked_add_signed(d));
                    (end, end.is_none())
                }
                _ => (
                    get("timestamp")
                        .and_then(timestamp)
                        .filter(|e| start.is_some_and(|s| *e > s)),
                    false,
                ),
            };
            out.sessions.push(Session {
                start,
                end,
                bad_end,
                sport: get("sport").and_then(text),
                distance_m: get("total_distance").and_then(number),
                kcal: get("total_calories").and_then(number),
            });
        }
        _ => {}
    }
}

/// The FIT base type `base` (its low 5 bits): the size of one value and its invalid value.
fn invalid_value(base: u8) -> (usize, u64) {
    match base & 0x1F {
        1 => (1, 0x7F),
        7 | 10 => (1, 0),
        3 => (2, 0x7FFF),
        4 => (2, 0xFFFF),
        11 => (2, 0),
        5 => (4, 0x7FFF_FFFF),
        6 | 8 => (4, 0xFFFF_FFFF),
        12 => (4, 0),
        14 => (8, 0x7FFF_FFFF_FFFF_FFFF),
        9 | 15 => (8, u64::MAX),
        16 => (8, 0),
        // enum, uint8, byte, and any base type the protocol does not name
        _ => (1, 0xFF),
    }
}

/// Write the invalid value of `base` over every value of a field.
fn blank(field: &mut [u8], base: u8, big_endian: bool) {
    let (size, v) = invalid_value(base);
    let bytes = if big_endian {
        v.to_be_bytes()[8 - size..].to_vec()
    } else {
        v.to_le_bytes()[..size].to_vec()
    };
    for c in field.chunks_mut(size) {
        if c.len() == size {
            c.copy_from_slice(&bytes);
        } else {
            c.fill(0xFF);
        }
    }
}

/// One local message definition: its global number, byte order, fields as (number, size, base type)
/// and developer fields as (number, size, developer data index).
struct Definition {
    global: u16,
    big_endian: bool,
    fields: Vec<(u8, usize, u8)>,
    developer: Vec<(u8, usize, u8)>,
}

/// What a `field_description` says of one developer field: its base type, whether its name says
/// position, and the profile field it stands for (`native_mesg_num`, `native_field_num`).
struct Described {
    base: u8,
    named: bool,
    natives: Vec<(Option<u16>, u8)>,
}

impl Described {
    /// Whether this developer field, in a message of number `global`, holds a position: by its name,
    /// or by the profile field it stands for (in the message it names, else in its own message).
    fn position(&self, global: u16) -> bool {
        self.named
            || self
                .natives
                .iter()
                .any(|(mesg, field)| native_position(mesg.unwrap_or(global), *field))
    }
}

fn layout() -> Error {
    unreadable("its messages do not fit its header")
}

/// The FIT file `bytes` (unpacked, read by [`read_fit`] first) with every position blanked: each
/// profile field in `fields` ([`Read::position_fields`]) and each developer field that holds a
/// position get the invalid value of their base type, in every message. A developer field holds a
/// position when its `field_description` names one, or says the field stands for a profile position
/// field (its fields 14 and 15), wherever in the file that description comes: each file of the chain is
/// read twice, once for its descriptions and once to blank. Nothing else changes but the checksum at
/// the end of each file in the chain, which is written again; a 14-byte header's own checksum covers
/// only the header, which stays as it was. Also says whether a developer field held a position, which
/// the parser cannot tell for a field described after its values or named neutrally.
pub fn strip(bytes: &[u8], fields: &BTreeSet<(u16, u8)>) -> Result<(Vec<u8>, bool)> {
    let mut out = bytes.to_vec();
    let mut developer_positions = false;
    let mut at = 0usize;
    while at < out.len() {
        if out.len() < at + 12 {
            return Err(layout());
        }
        let header = usize::from(out[at]);
        let data = u32::from_le_bytes([out[at + 4], out[at + 5], out[at + 6], out[at + 7]]) as usize;
        let start = at + header;
        let end = start.checked_add(data).ok_or_else(layout)?;
        if end + 2 > out.len() {
            return Err(layout());
        }
        // (developer data index, field number) -> what its descriptions in this file say
        let mut described: HashMap<(u8, u8), Described> = HashMap::new();
        for blanking in [false, true] {
            developer_positions |= walk_messages(&mut out[..end], start, fields, &mut described, blanking)?;
        }
        let crc = crc16(&out[at..end]);
        out[end..end + 2].copy_from_slice(&crc.to_le_bytes());
        at = end + 2;
    }
    Ok((out, developer_positions))
}

/// Read the messages of one file of a chain (`out[start..]`, its data up to `out.len()`). The first
/// pass (`blanking` false) only gathers every `field_description` into `described`; the second blanks
/// the profile fields in `profile` and every developer field that holds a position, and says whether
/// there was one.
fn walk_messages(
    out: &mut [u8],
    start: usize,
    profile: &BTreeSet<(u16, u8)>,
    described: &mut HashMap<(u8, u8), Described>,
    blanking: bool,
) -> Result<bool> {
    let end = out.len();
    let mut defs: HashMap<u8, Definition> = HashMap::new();
    let mut developer_positions = false;
    let mut p = start;
    while p < end {
        let head = out[p];
        p += 1;
        if head & 0xC0 == 0x40 {
            // a definition message
            let local = head & 0x0F;
            if p + 5 > end {
                return Err(layout());
            }
            let big_endian = out[p + 1] == 1;
            let g = [out[p + 2], out[p + 3]];
            let global = if big_endian {
                u16::from_be_bytes(g)
            } else {
                u16::from_le_bytes(g)
            };
            let n = usize::from(out[p + 4]);
            p += 5;
            if p + 3 * n > end {
                return Err(layout());
            }
            let fields: Vec<(u8, usize, u8)> = (0..n)
                .map(|i| (out[p + 3 * i], usize::from(out[p + 3 * i + 1]), out[p + 3 * i + 2]))
                .collect();
            p += 3 * n;
            let mut developer = Vec::new();
            if head & 0x20 != 0 {
                if p >= end {
                    return Err(layout());
                }
                let m = usize::from(out[p]);
                p += 1;
                if p + 3 * m > end {
                    return Err(layout());
                }
                developer = (0..m)
                    .map(|i| (out[p + 3 * i], usize::from(out[p + 3 * i + 1]), out[p + 3 * i + 2]))
                    .collect();
                p += 3 * m;
            }
            defs.insert(
                local,
                Definition {
                    global,
                    big_endian,
                    fields,
                    developer,
                },
            );
            continue;
        }
        // a data message: a compressed timestamp header names its local type in bits 5 and 6
        let local = if head & 0x80 != 0 {
            (head >> 5) & 0x03
        } else {
            head & 0x0F
        };
        let def = defs.get(&local).ok_or_else(layout)?;
        let mut description: [Option<u8>; 3] = [None; 3];
        let (mut name, mut native_mesg, mut native_field) = (None, None, None);
        for &(num, size, base) in &def.fields {
            if p + size > end {
                return Err(layout());
            }
            let value = &mut out[p..p + size];
            if def.global == FIELD_DESCRIPTION && !blanking {
                match num {
                    0..=2 if size >= 1 => description[usize::from(num)] = Some(value[0]),
                    3 => {
                        let s = value.split(|b| *b == 0).next().unwrap_or_default();
                        name = Some(String::from_utf8_lossy(s).into_owned());
                    }
                    NATIVE_MESG if size >= 2 => {
                        let b = [value[0], value[1]];
                        let m = if def.big_endian {
                            u16::from_be_bytes(b)
                        } else {
                            u16::from_le_bytes(b)
                        };
                        native_mesg = (m != u16::MAX).then_some(m);
                    }
                    NATIVE_FIELD if size >= 1 => native_field = (value[0] != u8::MAX).then_some(value[0]),
                    _ => {}
                }
            }
            if blanking && profile.contains(&(def.global, num)) {
                blank(value, base, def.big_endian);
            }
            p += size;
        }
        for &(num, size, index) in &def.developer {
            if p + size > end {
                return Err(layout());
            }
            if blanking
                && let Some(d) = described.get(&(index, num))
                && d.position(def.global)
            {
                blank(&mut out[p..p + size], d.base, def.big_endian);
                developer_positions = true;
            }
            p += size;
        }
        if let [Some(index), Some(num), Some(base)] = description {
            // one field described twice in a file: a position if either description says so
            let d = described.entry((index, num)).or_insert(Described {
                base,
                named: false,
                natives: Vec::new(),
            });
            d.named |= name.as_deref().is_some_and(|n| is_position(n, true));
            d.natives.extend(native_field.map(|f| (native_mesg, f)));
        }
    }
    Ok(developer_positions)
}

/// The FIT protocol's CRC-16 of `bytes`.
fn crc16(bytes: &[u8]) -> u16 {
    const TABLE: [u16; 16] = [
        0x0000, 0xCC01, 0xD801, 0x1400, 0xF001, 0x3C00, 0x2800, 0xE401, 0xA001, 0x6C00, 0x7800, 0xB401, 0x5000, 0x9C01,
        0x8801, 0x4400,
    ];
    let mut crc = 0u16;
    for b in bytes {
        for nibble in [b & 0x0F, b >> 4] {
            let t = TABLE[usize::from(crc & 0x0F)];
            crc = ((crc >> 4) & 0x0FFF) ^ t ^ TABLE[usize::from(nibble)];
        }
    }
    crc
}

/// The samples of a read file (source still 0: the caller sets it), and the reasons the rest were left
/// out.
pub fn samples(read: &Read, latest: DateTime<Utc>) -> (Vec<Sample>, BTreeMap<&'static str, i64>) {
    let mut reasons: BTreeMap<&'static str, i64> = BTreeMap::new();
    let mut bump = |k: &'static str, n: i64| {
        if n > 0 {
            *reasons.entry(k).or_default() += n;
        }
    };
    bump("bad_time", read.bad_time);
    bump("bad_value", read.bad_hr);
    bump("over_limit", read.over_limit);
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
            source: 0,
        });
    }
    let mut starts = BTreeSet::new();
    for s in &read.sessions {
        let has_distance = s.distance_m.is_some_and(|d| d > 0.0);
        let has_energy = s.kcal.is_some_and(|k| k > 0.0);
        let values = i64::from(has_distance) + i64::from(has_energy);
        if values == 0 {
            continue;
        }
        let Some(start) = s.start.filter(|t| in_time(*t, latest)) else {
            bump("bad_time", values);
            continue;
        };
        let end = s.end.filter(|e| *e > start);
        let too_long = end.is_some_and(|e| e > latest || e - start > Duration::days(MAX_SPAN_DAYS));
        if s.bad_end || too_long {
            bump("bad_time", values);
            continue;
        }
        // two activities of one file that start together: the first one that brings a sample counts
        if starts.contains(&start) {
            bump("duplicate", values);
            continue;
        }
        let before = out.len();
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
                    source: 0,
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
                    source: 0,
                });
            } else {
                bump("bad_value", 1);
            }
        }
        // an activity that brought nothing (a ride's distance, values out of range) keeps no start
        if out.len() > before {
            starts.insert(start);
        }
    }
    (out, reasons)
}

/// A read file's samples, refused when there are none: a course, a settings file, or a file whose
/// every value was left out.
fn activity(read: &Read) -> Result<(Vec<Sample>, BTreeMap<&'static str, i64>)> {
    let (all, reasons) = samples(read, Utc::now() + Duration::days(1));
    if all.is_empty() {
        return Err(no_activity());
    }
    Ok((all, reasons))
}

/// Import a stored FIT document into its profile again (payload `{document_id}`). Returns the summary
/// and the details the upload panel shows (`fit` in the job result): counts per metric, the device,
/// and whether positions were left out.
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
    let mut bytes = Vec::new();
    blobs
        .open_seek(BlobOwner::Profile(profile_id), &sha)?
        .take(MAX_BYTES + 1)
        .read_to_end(&mut bytes)?;
    let read = blocking(move || unpack(bytes).and_then(|b| read_fit(&b))).await?;
    let (all, reasons) = activity(&read)?;
    import_read(conn, profile_id, document_id, &read, all, reasons, source_id).await
}

/// Run a FIT read off the async threads; a parser panic is a file it could not read.
async fn blocking<T: Send + 'static>(f: impl FnOnce() -> Result<T> + Send + 'static) -> Result<T> {
    tokio::task::spawn_blocking(f).await.map_err(|e| {
        if e.is_panic() {
            unreadable("the parser could not read it")
        } else {
            Error::Other(anyhow::anyhow!("FIT read task failed: {e}"))
        }
    })?
}

/// Write a read file's samples for its document, and keep on the document what the file holds
/// (`flags.fit`) and when its first activity started (`collected_at`).
async fn import_read(
    conn: &mut PgConnection,
    profile_id: Uuid,
    document_id: Uuid,
    read: &Read,
    all: Vec<Sample>,
    mut reasons: BTreeMap<&'static str, i64>,
    source_id: Option<Uuid>,
) -> Result<(ImportSummary, Value)> {
    let profile = get_profile(&mut *conn, profile_id)
        .await?
        .ok_or_else(|| Error::NotFound(format!("unknown profile {profile_id}")))?;
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
    let mut held: BTreeMap<&'static str, i64> = BTreeMap::new();
    let mut by_metric: BTreeMap<&'static str, Vec<Sample>> = BTreeMap::new();
    for mut s in all {
        s.source = source;
        *held.entry(s.metric).or_default() += 1;
        by_metric.entry(s.metric).or_default().push(s);
    }
    // the document's day: its first activity's start, else its first heart rate minute
    let first = |hr: bool| {
        by_metric
            .iter()
            .filter(|(m, _)| (**m == "hr") == hr)
            .flat_map(|(_, v)| v.iter().map(|s| s.t))
            .min()
    };
    let started = first(false).or_else(|| first(true));
    let mut touched = store::Touched::default();
    let mut new: BTreeMap<&'static str, i64> = BTreeMap::new();
    let mut duplicate = 0i64;
    for metric in ["hr", "distance", "active_energy"] {
        let batch = by_metric.remove(metric).unwrap_or_default();
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
        *reasons.entry("duplicate").or_default() += duplicate;
    }
    let count = |m: &str| held.get(m).copied().unwrap_or_default();
    let holds = json!({
        "activities": read.sessions.len(),
        "heart_rate_minutes": count("hr"),
        "distances": count("distance"),
        "energy": count("active_energy"),
        "device": device.name,
    });
    // an import again reads the stored copy, which holds no position: what the upload said stays. A
    // document of the same bytes stored before under another type (a photo's name) holds wearables now.
    let gps_left_out: bool = sqlx::query_scalar(
        "update document set doc_type = $5, collected_at = coalesce($2, collected_at), \
         flags = flags || jsonb_build_object('fit', \
         $3::jsonb || jsonb_build_object('gps_left_out', coalesce((flags->'fit'->>'gps_left_out')::boolean, false) or $4)) \
         where id = $1 returning (flags->'fit'->>'gps_left_out')::boolean",
    )
    .bind(document_id)
    .bind(started)
    .bind(&holds)
    .bind(read.positions)
    .bind(WEARABLES_DOC_TYPE)
    .fetch_one(&mut *conn)
    .await?;
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
        "gps_left_out": gps_left_out,
    });
    Ok((summary, details))
}

/// A refusal of the file is permanent; anything else (the database, the disk) may heal on a retry.
fn job_error(e: Error) -> JobError {
    match e {
        Error::Upload(m) | Error::Invalid(m) => JobError::permanent(m),
        other => JobError::from(other),
    }
}

/// A new upload: read it once, store the copy with no position as the document's file, import it,
/// and delete the uploaded bytes.
async fn import_upload(ctx: &JobContext, job: &Job, channel: &str, source_id: Option<Uuid>) -> jobs::HandlerResult {
    let payload = &job.payload;
    let filename = payload.get("filename").and_then(Value::as_str).unwrap_or_default();
    let (Some(profile_id), Some(sha)) = (
        job.str("profile_id").and_then(|s| Uuid::parse_str(s).ok()),
        job.str("blob_sha256").filter(|s| !s.is_empty()),
    ) else {
        return Err(JobError::permanent("bad payload: 'profile_id' and 'blob_sha256'"));
    };
    let owner = BlobOwner::Profile(profile_id);
    if !ctx.blobs.exists(owner, sha) {
        return Err(JobError::permanent(format!(
            "{filename}: the uploaded file is not in the blob store"
        )));
    }
    let mut raw = Vec::new();
    ctx.blobs
        .open_seek(owner, sha)
        .map_err(|e| JobError::from(Error::from(e)))?
        .take(MAX_BYTES + 1)
        .read_to_end(&mut raw)
        .map_err(|e| JobError::from(Error::from(e)))?;
    let gzipped = raw.starts_with(&[0x1f, 0x8b]);
    let (read, kept) = blocking(move || {
        let bytes = unpack(raw)?;
        let mut read = read_fit(&bytes)?;
        let (kept, developer_positions) = strip(&bytes, &read.position_fields)?;
        // a developer field described after its values, or named neutrally, is a position only strip sees
        read.positions |= developer_positions;
        Ok((read, kept))
    })
    .await
    .map_err(job_error)?;
    let (all, reasons) = activity(&read).map_err(job_error)?;
    // the write syncs the file to disk: off the async threads too; a disk error may heal on a retry
    let blobs = ctx.blobs.clone();
    let kept_sha = tokio::task::spawn_blocking(move || blobs.put_bytes(&kept))
        .await
        .map_err(|e| JobError::from(Error::Other(anyhow::anyhow!("FIT write task failed: {e}"))))?
        .map_err(|e| JobError::from(Error::from(e)))?;
    let mut stored = payload.clone();
    stored["blob_sha256"] = json!(kept_sha);
    // the stored copy is the plain file: a .fit.gz upload keeps its name without .gz
    if gzipped && filename.to_lowercase().ends_with(".gz") {
        stored["filename"] = json!(filename[..filename.len() - 3]);
    }
    let mut tx = ctx.pool.begin().await?;
    let doc = document_of_type(&mut tx, &ctx.blobs, &stored, FIT_MIME, channel, WEARABLES_DOC_TYPE).await?;
    let (summary, details) = import_read(&mut tx, profile_id, doc, &read, all, reasons, source_id)
        .await
        .map_err(job_error)?;
    tx.commit().await?;
    if kept_sha != sha {
        forget_upload(ctx, sha, job.id).await;
    }
    let mut out = summary.to_value();
    out["fit"] = details;
    Ok(Some(out))
}

/// Delete uploaded FIT bytes from the blob store unless another row still holds them: a document, an
/// inbox item, a held mail, or another import of them that is still queued or running and has not let
/// them go. A job lets them go first (`upload_released` in its payload: it has stored its copy or been
/// refused for good, so it never reads them again), then looks. Of two imports that end together, the
/// one that looks last sees the other let go, so the last one deletes the upload, although a worker
/// marks a job done only after its handler returns.
async fn forget_upload(ctx: &JobContext, sha: &str, job_id: i64) {
    let done: Result<()> = async {
        let mut conn = ctx.pool.acquire().await?;
        sqlx::query("update job set payload = payload || '{\"upload_released\": true}'::jsonb where id = $1")
            .bind(job_id)
            .execute(&mut *conn)
            .await?;
        let held: bool = sqlx::query_scalar(
            "select exists (select 1 from document where blob_sha256 = decode($1, 'hex')) \
             or exists (select 1 from source_item where blob_sha256 = decode($1, 'hex')) \
             or exists (select 1 from quarantine_item where raw_blob = decode($1, 'hex')) \
             or exists (select 1 from job where id <> $2 and state in ('queued', 'running') \
                and payload->>'blob_sha256' = $1 and payload->'upload_released' is null)",
        )
        .bind(sha)
        .bind(job_id)
        .fetch_one(&mut *conn)
        .await?;
        if !held {
            ctx.blobs.delete(sha)?;
        }
        Ok(())
    }
    .await;
    if let Err(e) = done {
        tracing::warn!(job = job_id, error = %e, "could not delete an uploaded FIT file");
    }
}

async fn run(ctx: JobContext, job: Job) -> jobs::HandlerResult {
    let channel = job
        .str("channel")
        .filter(|c| !c.is_empty())
        .unwrap_or("upload")
        .to_string();
    let source_id = job.str("source_id").and_then(|s| Uuid::parse_str(s).ok());
    if !ownhealth_ingest_structured::py::truthy(job.payload.get("document_id")) {
        let out = import_upload(&ctx, &job, &channel, source_id).await;
        // a refused upload keeps nothing: it may hold a route
        if let (Err(e), Some(sha)) = (&out, job.str("blob_sha256"))
            && e.permanent
        {
            forget_upload(&ctx, sha, job.id).await;
        }
        return out;
    }
    let mut tx = ctx.pool.begin().await?;
    let doc = document_of_type(
        &mut tx,
        &ctx.blobs,
        &job.payload,
        FIT_MIME,
        &channel,
        WEARABLES_DOC_TYPE,
    )
    .await?;
    let (summary, details) = import_fit(&mut tx, &ctx.blobs, doc, source_id)
        .await
        .map_err(job_error)?;
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
            "observed_location_lat",
        ] {
            assert!(is_position(n, false), "{n}");
        }
        // a cycling power position or a GPS accuracy is not where someone was
        for n in [
            "heart_rate",
            "timestamp",
            "total_distance",
            "sport",
            "total_calories",
            "avg_power_position",
            "gps_accuracy",
        ] {
            assert!(!is_position(n, false), "{n}");
        }
        // a developer names its own fields
        for n in ["Latitude", "lon", "GPS fix", "Position", "location"] {
            assert!(is_position(n, true), "{n}");
        }
        assert!(!is_position("Power", true));
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

    #[test]
    fn a_blanked_field_holds_the_invalid_value_of_its_type() {
        let mut f = [1u8, 2, 3, 4];
        blank(&mut f, 0x85, false);
        assert_eq!(i32::from_le_bytes(f), 0x7FFF_FFFF);
        blank(&mut f, 0x85, true);
        assert_eq!(i32::from_be_bytes(f), 0x7FFF_FFFF);
        let mut s = *b"$GPGGA,1";
        blank(&mut s, 0x07, false);
        assert_eq!(s, [0u8; 8]);
        // the protocol's check value: CRC-16 of "123456789"
        assert_eq!(crc16(b"123456789"), 0xBB3D);
    }
}
