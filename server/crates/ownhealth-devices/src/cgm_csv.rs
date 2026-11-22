//! LibreView and Dexcom Clarity CSV exports (lane D, slice 28): a CGM history from the vendor's own
//! export file. Job kind `import.cgm_csv`, payload as the other structured importers (`{profile_id,
//! household_id, blob_sha256, filename}`, or `{document_id}` for an import again); core sniffs the
//! header row (`documents::CGM_CSV_KINDS`) as `libreview_csv` or `dexcom_clarity_csv`. The file
//! becomes a `text/csv` document of the profile (`doc_type` [`WEARABLES_DOC_TYPE`], so only a member
//! who may read wearables lists, opens or downloads it) and owns the readings it brings.
//!
//! Columns are found by their header text, never by their position, in English, German and Spanish:
//! - LibreView: `Device Timestamp`, `Record Type`, `Historic Glucose` (record type 0) and `Scan Glucose`
//!   (record type 1), with `Device` and `Serial Number` naming the sensor. Rows of other record types
//!   (strip glucose, ketones, insulin, food, notes) are not readings: they are counted as other rows.
//! - Clarity: the timestamp (any header ending in `...Thh:mm:ss)`), `Event Type` and `Glucose Value`.
//!   Only `EGV` rows are readings; `Source Device ID` names the device, and the header block's device
//!   row gives its name. The header block's alerts carry thresholds, not readings.
//!
//! A published column that is not read (insulin, carbohydrates, notes) is known and left out. A header
//! these tables do not know is reported in the import result (`unknown_columns`), never dropped
//! silently.
//!
//! Units: the glucose column's header says mg/dL or mmol/L. Readings are stored in mg/dL
//! (`cgm::METRIC`), as Nightscout's are. mmol/L is multiplied by 180.156 / 10 (the molar mass of
//! D-glucose, as in the molar table) and rounded to a whole mg/dL. Sensors measure whole mg/dL and
//! exports print mmol/L with one decimal, so the stored value gives the printed mmol/L back, and a
//! printed 10.0 mmol/L stays in range (180, not 180.156). `Low` and `High` (`Niedrig`, `Hoch`, `Bajo`,
//! `Alto`) mark a reading outside the sensor's range, with no number: they are counted as not imported
//! (`below_sensor_range`, `above_sensor_range`).
//!
//! Times: the exports print local wall-clock times without an offset. They are read in the install's
//! time zone (`OWNHEALTH_TIMEZONE`, the zone the MCP day budget already uses) when it is set, else in
//! the zone the browser sent with the upload (`timezone` in the job payload), with Postgres's zone
//! rules, and each reading keeps the offset that applied (`tz_offset_min`). An old zone name that the
//! tz database links to a current one (`Asia/Calcutta`, which Chromium still reports) reads as the
//! current one (`data/tz/backward`), since the Debian Postgres image ships only current names. A name
//! Postgres knows keeps that name, so the line that says which zone read the file names the zone the
//! person is in (`Europe/Oslo`, which the tz database links to `Europe/Berlin`). Two names the tz
//! database links are one zone: the same file uploaded again under the other name replaces nothing,
//! and its result names the zone the file keeps. A zone
//! Postgres does not know by either name is no zone; it never refuses an upload. With neither zone, a
//! time is read as UTC and keeps no offset: the rule the PDF and FHIR importers apply to a time without
//! an offset. The zone used is a property of the file
//! (`flags.cgm_csv.timezone`): an import again after a retraction reads the file in that zone, and the
//! same file uploaded again in another zone has its own readings replaced, so one copy stays, at the
//! new instants. On the night the clocks go back, the times of the repeated hour are ambiguous: those
//! readings are counted (`ambiguous_time`), not guessed. A time in the hour the clocks skip in spring is
//! not a local time (`nonexistent_time`). Two readings of one sensor at one instant keep the first
//! (`repeated_time`).
//!
//! A file cut short (a download that stopped) ends without a line break inside its last row: when that
//! row stops at or before the cell this importer reads, it is counted (`cut_row`), not read, so a value
//! cut from 131 to 13 is never stored. A last row that holds every cell this importer reads stays a
//! reading, line break or not.
//!
//! Each sensor of the file becomes a `wearable_source`, whose id is a counter of the whole install: a
//! file that names more than [`MAX_DEVICES`] sensors is refused before anything is written. Device names
//! and serials are cut to 80 characters.
//!
//! Re-import: `Conflict::Keep`, the first file to bring a reading owns it. The same file again, or a
//! longer export that holds it, adds only readings that are not stored yet; the rest count as
//! `duplicate`. Retracting a file queues the profile's other CGM exports for import again
//! (`retract::retract_document_facts`), so the readings they share come back as theirs. A file whose
//! readings are replaced because it was uploaded in another zone does the same: the instants it left
//! may hold readings of another export. A retraction or a move of a CGM export takes the lock its
//! imports hold (`retract::lock_export_of`) before anything else, so it waits for a running import and
//! then removes what it wrote.
//!
//! Low and High rows count toward [`MAX_READINGS`], and only dates from 2000 on decide the order of day
//! and month.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::io::Read as _;

use chrono::{DateTime, Duration, NaiveDate, NaiveDateTime, NaiveTime, Timelike, Utc};
use ownhealth_core::access::get_profile;
use ownhealth_core::blobs::{BlobOwner, BlobStore};
use ownhealth_core::documents::{self, CLARITY_TIME_SUFFIX, WEARABLES_DOC_TYPE, header_key};
use ownhealth_core::error::{Error, Result};
use ownhealth_core::ingest::ImportSummary;
use ownhealth_core::jobs::{self, Job, JobContext, JobError};
use ownhealth_ingest_structured::fhir::mark_imported;
use ownhealth_ingest_structured::jobs::document_of_type;
use serde_json::{Value, json};
use sqlx::PgConnection;
use uuid::Uuid;

use crate::cgm;
use crate::store::{self, Conflict, Sample};

pub const JOB: &str = "import.cgm_csv";
/// `wearable_source.provider` of each vendor.
pub const LIBREVIEW: &str = "libreview";
pub const CLARITY: &str = "dexcom_clarity";
/// The largest export read: years of 5-minute readings fit many times over.
pub const MAX_BYTES: u64 = 256 * 1024 * 1024;
/// Readings one file may hold, its Low and High rows counted with them.
pub const MAX_READINGS: usize = 5_000_000;
/// The molar mass of D-glucose C6H12O6 in g/mol (data/terminology/molar/molar_mass.tsv, PubChem 5793).
pub const GLUCOSE_G_PER_MOL: f64 = 180.156;
/// A number outside this range (mg/dL) is not a glucose reading.
const PLAUSIBLE_MG_DL: (f64, f64) = (10.0, 1000.0);
/// Sensors one file may name. Each one takes a `wearable_source` id, a smallint counter shared by the
/// whole install that a rolled-back import does not give back.
pub const MAX_DEVICES: usize = 32;
/// Unknown column names kept in the result; these, device names and serials are cut to `MAX_NAME`
/// characters.
const MAX_UNKNOWN: usize = 20;
const MAX_NAME: usize = 80;
/// Local times sent to Postgres at a time.
const ZONE_CHUNK: usize = 20_000;
/// A reading dated before this year is a bad time, and its date says nothing of the file's day order.
const EARLIEST_YEAR: i32 = 2000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Vendor {
    LibreView,
    Clarity,
}

impl Vendor {
    pub fn provider(self) -> &'static str {
        match self {
            Vendor::LibreView => LIBREVIEW,
            Vendor::Clarity => CLARITY,
        }
    }

    fn of_kind(kind: &str) -> Option<Vendor> {
        match kind {
            "libreview_csv" => Some(Vendor::LibreView),
            "dexcom_clarity_csv" => Some(Vendor::Clarity),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unit {
    MgDl,
    MmolL,
}

impl Unit {
    pub fn label(self) -> &'static str {
        match self {
            Unit::MgDl => "mg/dL",
            Unit::MmolL => "mmol/L",
        }
    }

    /// The unit a header ends in (`... mg/dL`, `... (mmol/L)`), with the name before it.
    fn of_header(key: &str) -> Option<(Unit, &str)> {
        let (body, paren) = match key.strip_suffix(')') {
            Some(b) => (b, true),
            None => (key, false),
        };
        let (unit, rest) = match body.strip_suffix("mg/dl") {
            Some(r) => (Unit::MgDl, r),
            None => (Unit::MmolL, body.strip_suffix("mmol/l")?),
        };
        let rest = if paren { rest.strip_suffix('(')? } else { rest };
        Some((unit, rest.trim_end()))
    }
}

/// mmol/L to mg/dL with the molar mass of D-glucose, to the whole mg/dL a sensor measures.
pub fn mg_dl_from_mmol(mmol: f64) -> f64 {
    (mmol * GLUCOSE_G_PER_MOL / 10.0).round()
}

// The header names, as `documents::header_key` writes them: English, German, Spanish.
const LIBRE_DEVICE: &[&str] = &["device", "gerät", "dispositivo"];
const LIBRE_SERIAL: &[&str] = &["serial number", "seriennummer", "número de serie", "número de serial"];
const LIBRE_HISTORIC: &[&str] = &[
    "historic glucose",
    "glukosewert-verlauf",
    "glukose verlauf",
    "historial de glucosa",
];
const LIBRE_SCAN: &[&str] = &["scan glucose", "glukose-scan", "escaneo de glucosa"];
/// Published LibreView columns that hold no CGM reading. Those with a unit are named without it.
const LIBRE_OTHER: &[&str] = &[
    "non-numeric rapid-acting insulin",
    "rapid-acting insulin (units)",
    "non-numeric food",
    "carbohydrates (grams)",
    "carbohydrates (servings)",
    "non-numeric long-acting insulin",
    "long-acting insulin value (units)",
    "long-acting insulin (units)",
    "notes",
    "strip glucose",
    "ketone",
    "historic ketone",
    "scan ketone",
    "meal insulin (units)",
    "correction insulin (units)",
    "user change insulin (units)",
    "nicht numerisches schnellwirkendes insulin",
    "schnellwirkendes insulin (einheiten)",
    "nicht numerische nahrungsdaten",
    "kohlenhydrate (gramm)",
    "kohlenhydrate (portionen)",
    "nicht numerisches depotinsulin",
    "depotinsulin (einheiten)",
    "notizen",
    "glukose-teststreifen",
    "keton",
    "mahlzeiteninsulin (einheiten)",
    "korrekturinsulin (einheiten)",
    "insulinänderung durch anwender (einheiten)",
    "insulina de acción rápida no numérica",
    "insulina de acción rápida (unidades)",
    "alimento no numérico",
    "carbohidratos (gramos)",
    "carbohidratos (porciones)",
    "insulina de acción prolongada no numérica",
    "insulina de acción prolongada (unidades)",
    "notas",
    "glucosa de la tira",
    "cetona",
    "insulina para comida (unidades)",
    "insulina de corrección (unidades)",
    "cambio de insulina del usuario (unidades)",
];
const CLARITY_SOURCE: &[&str] = &["source device id", "quellengerät-id", "id del dispositivo de origen"];
const CLARITY_DEVICE_INFO: &[&str] = &["device info", "geräteinfo", "información del dispositivo"];
const CLARITY_GLUCOSE: &[&str] = &["glucose value", "glukosewert", "valor de glucosa"];
/// Published Clarity columns that hold no reading of an EGV row.
const CLARITY_OTHER: &[&str] = &[
    "index",
    "índice",
    "event subtype",
    "ereignis-unterart",
    "subtipo de evento",
    "patient info",
    "patienten-info",
    "información del paciente",
    "insulin value (u)",
    "insulinwert (e)",
    "valor de insulina (u)",
    "carb value (grams)",
    "kohlenh.-wert (gramm)",
    "valor de carbohidratos (gramos)",
    "duration (hh:mm:ss)",
    "dauer (hh:mm:ss)",
    "duración (hh:mm:ss)",
    "transmitter time (long integer)",
    "transmitterzeit (lange ganzzahl)",
    "hora del transmisor (número entero largo)",
    "transmitter id",
    "transmitter-id",
    "id del transmisor",
];
/// The start of Clarity's rate-of-change column (`... (mg/dL/min)`).
const CLARITY_RATE: &[&str] = &[
    "glucose rate of change",
    "glukose-änderungsgeschwindigkeit",
    "tasa de cambio de glucosa",
];
/// The event type of a header-block row that names a device (`Device`, `Gerät`, `Dispositivo`).
const CLARITY_DEVICE_ROW: &[&str] = &["device", "gerät", "dispositivo"];
/// A value that marks a reading under or over the sensor's range.
const LOW_WORDS: &[&str] = &["low", "lo", "niedrig", "bajo", "baja"];
const HIGH_WORDS: &[&str] = &["high", "hi", "hoch", "alto", "alta"];

/// One sensor of the export.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Device {
    pub name: String,
    pub external_id: Option<String>,
}

/// A reading before its instant is known: the export's local time and the value in mg/dL.
#[derive(Debug, Clone, PartialEq)]
pub struct Reading {
    /// Index into `Parsed::devices`.
    pub device: usize,
    pub local: NaiveDateTime,
    pub mg_dl: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Parsed {
    pub vendor: Vendor,
    /// The unit of the (first) glucose column.
    pub unit: Unit,
    pub devices: Vec<Device>,
    pub readings: Vec<Reading>,
    /// Reading rows not imported, by reason.
    pub skipped: BTreeMap<&'static str, i64>,
    /// Header cells the tables do not know, as the file writes them.
    pub unknown_columns: Vec<String>,
    /// Rows that are not glucose readings (insulin, food, notes, strip glucose, calibrations).
    pub other_rows: i64,
}

fn not_cgm(why: &str) -> Error {
    Error::Upload(format!("not a readable LibreView or Dexcom Clarity export: {why}"))
}

fn one_of(names: &[&str], key: &str) -> bool {
    names.contains(&key)
}

/// A name from the file, cut to `MAX_NAME` characters.
fn cut(s: &str) -> String {
    s.chars().take(MAX_NAME).collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Role {
    Time,
    Kind,
    Device,
    Serial,
    DeviceInfo,
    /// A glucose column: its unit, and the LibreView record type it holds (Clarity: 0).
    Glucose(Unit, u8),
    Known,
}

fn role(vendor: Vendor, key: &str) -> Option<Role> {
    let (time, types) = documents::CGM_CSV_KINDS
        .iter()
        .find(|(kind, ..)| Vendor::of_kind(kind) == Some(vendor))
        .map(|(_, t, k)| (*t, *k))?;
    let unit = Unit::of_header(key);
    match vendor {
        Vendor::LibreView => Some(if one_of(time, key) {
            Role::Time
        } else if one_of(types, key) {
            Role::Kind
        } else if one_of(LIBRE_DEVICE, key) {
            Role::Device
        } else if one_of(LIBRE_SERIAL, key) {
            Role::Serial
        } else if let Some((u, _)) = unit.filter(|(_, n)| one_of(LIBRE_HISTORIC, n)) {
            Role::Glucose(u, 0)
        } else if let Some((u, _)) = unit.filter(|(_, n)| one_of(LIBRE_SCAN, n)) {
            Role::Glucose(u, 1)
        } else if one_of(LIBRE_OTHER, key) || unit.is_some_and(|(_, n)| one_of(LIBRE_OTHER, n)) {
            Role::Known
        } else {
            return None;
        }),
        Vendor::Clarity => Some(if key.ends_with(CLARITY_TIME_SUFFIX) {
            Role::Time
        } else if one_of(types, key) {
            Role::Kind
        } else if one_of(CLARITY_SOURCE, key) {
            Role::Device
        } else if one_of(CLARITY_DEVICE_INFO, key) {
            Role::DeviceInfo
        } else if let Some((u, _)) = unit.filter(|(_, n)| one_of(CLARITY_GLUCOSE, n)) {
            Role::Glucose(u, 0)
        } else if one_of(CLARITY_OTHER, key) || CLARITY_RATE.iter().any(|n| key.starts_with(n)) {
            Role::Known
        } else {
            return None;
        }),
    }
}

#[derive(Debug, Default)]
struct Columns {
    time: Option<usize>,
    kind: Option<usize>,
    device: Option<usize>,
    serial: Option<usize>,
    device_info: Option<usize>,
    glucose: Vec<(usize, Unit, u8)>,
    unknown: Vec<String>,
}

fn columns(vendor: Vendor, header: &csv::StringRecord) -> Result<Columns> {
    let mut c = Columns::default();
    for (i, cell) in header.iter().enumerate() {
        let key = header_key(cell);
        if key.is_empty() {
            continue;
        }
        match role(vendor, &key) {
            Some(Role::Time) => c.time = c.time.or(Some(i)),
            Some(Role::Kind) => c.kind = c.kind.or(Some(i)),
            Some(Role::Device) => c.device = c.device.or(Some(i)),
            Some(Role::Serial) => c.serial = c.serial.or(Some(i)),
            Some(Role::DeviceInfo) => c.device_info = c.device_info.or(Some(i)),
            Some(Role::Glucose(u, t)) if !c.glucose.iter().any(|g| g.2 == t) => c.glucose.push((i, u, t)),
            Some(Role::Glucose(..)) | None => {
                if c.unknown.len() < MAX_UNKNOWN {
                    c.unknown.push(cut(cell.trim().trim_start_matches('\u{feff}')));
                }
            }
            Some(Role::Known) => {}
        }
    }
    if c.time.is_none() {
        return Err(not_cgm("no timestamp column"));
    }
    if c.kind.is_none() {
        return Err(not_cgm("no record or event type column"));
    }
    if c.glucose.is_empty() {
        return Err(not_cgm("no glucose column in mg/dL or mmol/L"));
    }
    Ok(c)
}

enum Glucose {
    Mg(f64),
    Low,
    High,
    Bad,
}

fn glucose(cell: &str, unit: Unit) -> Glucose {
    let c = cell.trim();
    let key = c.to_lowercase();
    if one_of(LOW_WORDS, &key) {
        return Glucose::Low;
    }
    if one_of(HIGH_WORDS, &key) {
        return Glucose::High;
    }
    // a decimal comma ("5,6") in a German or Spanish export
    let n: Option<f64> = if c.contains(',') && !c.contains('.') {
        c.replace(',', ".").parse().ok()
    } else {
        c.parse().ok()
    };
    let Some(n) = n.filter(|n| n.is_finite()) else {
        return Glucose::Bad;
    };
    let mg = match unit {
        Unit::MgDl => n,
        Unit::MmolL => mg_dl_from_mmol(n),
    };
    if (PLAUSIBLE_MG_DL.0..=PLAUSIBLE_MG_DL.1).contains(&mg) {
        Glucose::Mg(mg)
    } else {
        Glucose::Bad
    }
}

fn bump(m: &mut BTreeMap<&'static str, i64>, reason: &'static str) {
    *m.entry(reason).or_default() += 1;
}

/// Read an export. Pure and blocking: call it from `spawn_blocking`.
pub fn parse(bytes: &[u8]) -> Result<Parsed> {
    parse_capped(bytes, MAX_READINGS)
}

/// [`parse`], refusing a file of more than `cap` readings.
fn parse_capped(bytes: &[u8], cap: usize) -> Result<Parsed> {
    let text = documents::decode_text(bytes);
    let mut offset = 0usize;
    let mut found = None;
    for line in text.split_inclusive('\n').take(documents::CGM_CSV_HEADER_LINES) {
        if let Some(kind) = documents::cgm_csv_kind(line.trim_end_matches(['\r', '\n'])) {
            found = Some((kind, line));
            break;
        }
        offset += line.len();
    }
    let (kind, header_line) = found.ok_or_else(|| not_cgm("no LibreView or Dexcom Clarity header row"))?;
    let vendor = Vendor::of_kind(kind).ok_or_else(|| not_cgm("no LibreView or Dexcom Clarity header row"))?;
    let mut rdr = csv::ReaderBuilder::new()
        .delimiter(documents::csv_delimiter(header_line))
        .has_headers(false)
        .flexible(true)
        .trim(csv::Trim::All)
        .from_reader(text[offset..].as_bytes());
    let mut records = rdr.records().peekable();
    let header = records
        .next()
        .ok_or_else(|| not_cgm("no header row"))?
        .map_err(|e| not_cgm(&e.to_string()))?;
    let cols = columns(vendor, &header)?;
    let (time_col, kind_col) = (cols.time.unwrap_or(0), cols.kind.unwrap_or(0));
    // a download that stopped ends inside its last row, with no line break after it
    let whole = text.ends_with(['\n', '\r']);

    let mut skipped: BTreeMap<&'static str, i64> = BTreeMap::new();
    let mut other_rows = 0i64;
    let mut device_ids: HashMap<(String, String), usize> = HashMap::new();
    let mut devices: Vec<(String, String)> = Vec::new();
    // Clarity's header block: source device id -> device name
    let mut names: HashMap<String, String> = HashMap::new();
    let mut pending: Vec<(usize, String, f64)> = Vec::new();
    // Low and High rows: no reading, but they count toward the cap, and a LibreView row's date still
    // tells the order of day and month (Clarity's ISO times need no such hint)
    let mut out_of_range = 0usize;
    let mut hints: Vec<Stamp> = Vec::new();
    let too_many = || {
        Error::Upload(format!(
            "the export holds more than {cap} readings: export a shorter period"
        ))
    };
    while let Some(rec) = records.next() {
        let rec = rec.map_err(|e| not_cgm(&e.to_string()))?;
        let cell = |i: Option<usize>| i.and_then(|i| rec.get(i)).unwrap_or("");
        // the cut last row: shorter than the header, and it stops at or before the cell `i`
        let cut_at = |i: usize| !whole && rec.len() < header.len() && i + 1 >= rec.len();
        let last = records.peek().is_none();
        if last && cut_at(time_col.max(kind_col)) {
            bump(&mut skipped, "cut_row");
            continue;
        }
        let kind = cell(Some(kind_col));
        let (col, unit) = match vendor {
            Vendor::LibreView => {
                let Ok(t) = kind.parse::<u8>() else {
                    if rec.iter().any(|c| !c.is_empty()) {
                        other_rows += 1;
                    }
                    continue;
                };
                let Some((col, unit, _)) = cols.glucose.iter().find(|g| g.2 == t) else {
                    other_rows += 1;
                    continue;
                };
                (*col, *unit)
            }
            Vendor::Clarity => {
                let egv = kind.eq_ignore_ascii_case("egv");
                // an EGV row without a time is still a reading, counted below as a bad time
                if !egv && cell(Some(time_col)).is_empty() {
                    // the header block: name, birth date, devices and alert settings, not events
                    if one_of(CLARITY_DEVICE_ROW, &kind.to_lowercase()) {
                        let (src, info) = (cell(cols.device), cell(cols.device_info));
                        if !src.is_empty() && !info.is_empty() {
                            names.entry(cut(src)).or_insert_with(|| cut(info));
                        }
                    }
                    continue;
                }
                if !egv {
                    other_rows += 1;
                    continue;
                }
                let (col, unit, _) = cols.glucose[0];
                (col, unit)
            }
        };
        if last && cut_at(col) {
            bump(&mut skipped, "cut_row");
            continue;
        }
        let outside = match glucose(cell(Some(col)), unit) {
            Glucose::Mg(v) => Ok(v),
            Glucose::Low => Err("below_sensor_range"),
            Glucose::High => Err("above_sensor_range"),
            Glucose::Bad => {
                bump(&mut skipped, "bad_value");
                continue;
            }
        };
        let mg = match outside {
            Ok(v) => v,
            Err(reason) => {
                bump(&mut skipped, reason);
                out_of_range += 1;
                if pending.len() + out_of_range > cap {
                    return Err(too_many());
                }
                if vendor == Vendor::LibreView
                    && let Some(s) = stamp(cell(Some(time_col)))
                {
                    hints.push(s);
                }
                continue;
            }
        };
        let key = (cut(cell(cols.device)), cut(cell(cols.serial)));
        let device = match device_ids.get(&key) {
            Some(d) => *d,
            None => {
                if devices.len() == MAX_DEVICES {
                    return Err(Error::Upload(format!(
                        "the export names more than {MAX_DEVICES} sensors: export one person's sensors, or a shorter period"
                    )));
                }
                devices.push(key.clone());
                device_ids.insert(key, devices.len() - 1);
                devices.len() - 1
            }
        };
        pending.push((device, cell(Some(time_col)).to_string(), mg));
        if pending.len() + out_of_range > cap {
            return Err(too_many());
        }
    }
    let times: Vec<Option<NaiveDateTime>> = match vendor {
        Vendor::Clarity => pending.iter().map(|p| iso_time(&p.1)).collect(),
        Vendor::LibreView => libre_times_with(&pending.iter().map(|p| p.1.as_str()).collect::<Vec<_>>(), &hints),
    };
    let earliest = NaiveDate::from_ymd_opt(EARLIEST_YEAR, 1, 1).unwrap_or_default();
    let mut readings = Vec::with_capacity(pending.len());
    for ((device, _, mg_dl), local) in pending.into_iter().zip(times) {
        match local.filter(|l| l.date() >= earliest) {
            Some(local) => readings.push(Reading { device, local, mg_dl }),
            None => bump(&mut skipped, "bad_time"),
        }
    }
    let devices = devices
        .into_iter()
        .map(|(device, serial)| match vendor {
            Vendor::LibreView => Device {
                name: if device.is_empty() {
                    "FreeStyle Libre".into()
                } else {
                    device
                },
                external_id: (!serial.is_empty()).then_some(serial),
            },
            Vendor::Clarity => Device {
                name: names.get(&device).cloned().unwrap_or_else(|| {
                    if device.is_empty() {
                        "Dexcom".into()
                    } else {
                        device.clone()
                    }
                }),
                external_id: (!device.is_empty()).then_some(device),
            },
        })
        .collect();
    Ok(Parsed {
        vendor,
        unit: cols.glucose[0].1,
        devices,
        readings,
        skipped,
        unknown_columns: cols.unknown,
        other_rows,
    })
}

/// Clarity's `YYYY-MM-DDThh:mm:ss` (a space instead of the T, or no seconds, also read).
fn iso_time(s: &str) -> Option<NaiveDateTime> {
    [
        "%Y-%m-%dT%H:%M:%S",
        "%Y-%m-%d %H:%M:%S",
        "%Y-%m-%dT%H:%M:%S%.f",
        "%Y-%m-%d %H:%M:%S%.f",
        "%Y-%m-%dT%H:%M",
        "%Y-%m-%d %H:%M",
    ]
    .iter()
    .find_map(|f| NaiveDateTime::parse_from_str(s.trim(), f).ok())
}

/// A LibreView `Device Timestamp` before the order of day and month is known.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Stamp {
    /// The first two numbers of the date (day and month in some order), or month and day after a year.
    a: u32,
    b: u32,
    y: i32,
    year_first: bool,
    time: NaiveTime,
    twelve_hour: bool,
}

fn stamp(s: &str) -> Option<Stamp> {
    let s = s.trim();
    let (date, clock) = s.split_once([' ', 'T'])?;
    let parts: Vec<&str> = date.split(['-', '/', '.']).collect();
    let [p0, p1, p2] = parts.as_slice() else {
        return None;
    };
    let digits = |p: &str| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit());
    if ![p0, p1, p2].iter().all(|p| digits(p)) {
        return None;
    }
    let year_first = p0.len() == 4;
    let (a, b, y) = if year_first { (p1, p2, p0) } else { (p0, p1, p2) };
    if y.len() != 4 {
        return None;
    }
    let clock = clock.trim();
    let upper = clock.to_uppercase();
    let (clock, pm) = match (upper.strip_suffix("AM"), upper.strip_suffix("PM")) {
        (Some(c), _) => (c.trim().to_string(), Some(false)),
        (_, Some(c)) => (c.trim().to_string(), Some(true)),
        _ => (clock.to_string(), None),
    };
    let mut time = ["%H:%M:%S", "%H:%M"]
        .iter()
        .find_map(|f| NaiveTime::parse_from_str(&clock, f).ok())?;
    if let Some(pm) = pm {
        let h = time.hour();
        if !(1..=12).contains(&h) {
            return None;
        }
        let h24 = match (h, pm) {
            (12, false) => 0,
            (12, true) => 12,
            (h, false) => h,
            (h, true) => h + 12,
        };
        time = time.with_hour(h24)?;
    }
    Some(Stamp {
        a: a.parse().ok()?,
        b: b.parse().ok()?,
        y: y.parse().ok()?,
        year_first,
        time,
        twelve_hour: pm.is_some(),
    })
}

fn date_of(s: &Stamp, day_first: bool) -> Option<NaiveDate> {
    let (m, d) = if s.year_first || !day_first {
        (s.a, s.b)
    } else {
        (s.b, s.a)
    };
    NaiveDate::from_ymd_opt(s.y, m, d)
}

/// Whether the file writes the day before the month. A first number above 12 says day first, a second
/// above 12 month first. When no number says it, the order whose dates span fewer days wins (a 14-day
/// export read the wrong way round spans months), and on a tie a 12-hour clock means the US order. Only
/// stamps that are a date in at least one order, from 2000 on (the earliest reading kept), count: one
/// impossible date (`99/01`) or one dated 1999 decides nothing, and stays a bad time.
fn day_first(stamps: &[Stamp]) -> bool {
    let dm: Vec<&Stamp> = stamps
        .iter()
        .filter(|s| {
            !s.year_first && s.y >= EARLIEST_YEAR && (date_of(s, true).is_some() || date_of(s, false).is_some())
        })
        .collect();
    if dm.iter().any(|s| s.a > 12) {
        return true;
    }
    if dm.iter().any(|s| s.b > 12) {
        return false;
    }
    let span = |df: bool| -> Option<i64> {
        let dates: Vec<NaiveDate> = dm.iter().map(|s| date_of(s, df)).collect::<Option<_>>()?;
        let lo = dates.iter().min()?;
        let hi = dates.iter().max()?;
        Some((*hi - *lo).num_days())
    };
    match (span(true), span(false)) {
        (Some(d), Some(m)) if d != m => d < m,
        (Some(_), None) => true,
        (None, Some(_)) => false,
        _ => !dm.iter().any(|s| s.twelve_hour),
    }
}

/// LibreView timestamps: `DD-MM-YYYY HH:MM`, `MM-DD-YYYY HH:MM`, `MM-DD-YYYY hh:mm AM`, with `-`, `/` or
/// `.`, the order of day and month decided once for the whole file.
#[cfg(test)]
fn libre_times(raw: &[&str]) -> Vec<Option<NaiveDateTime>> {
    libre_times_with(raw, &[])
}

/// [`libre_times`], with the times of the file's other rows (`hints`, its Low and High rows) also
/// deciding the order of day and month.
fn libre_times_with(raw: &[&str], hints: &[Stamp]) -> Vec<Option<NaiveDateTime>> {
    let stamps: Vec<Option<Stamp>> = raw.iter().map(|s| stamp(s)).collect();
    let known: Vec<Stamp> = stamps.iter().flatten().chain(hints).copied().collect();
    let df = day_first(&known);
    stamps
        .iter()
        .map(|s| s.and_then(|s| Some(date_of(&s, df)?.and_time(s.time))))
        .collect()
}

/// A local time read in a zone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Instant {
    /// The instant, and the zone's UTC offset in minutes then (none without a zone).
    At(DateTime<Utc>, Option<i16>),
    /// A time of the hour that repeats when the clocks go back.
    Ambiguous,
    /// A time of the hour the clocks skip.
    Missing,
}

/// The tz database's `backward` table (release 2026f, public domain): `Link TARGET NAME` lines from an
/// old or merged zone name to its current one.
const TZ_BACKWARD: &str = include_str!("../../../../data/tz/backward");

/// The current name of an old zone name (`Asia/Calcutta` gives `Asia/Kolkata`), when the tz database
/// links it. Browsers still report some old names, and the Debian Postgres image knows only the
/// current ones.
pub fn zone_link_target(name: &str) -> Option<&'static str> {
    TZ_BACKWARD.lines().find_map(|line| {
        let line = line.split('#').next().unwrap_or_default();
        match line.split_whitespace().collect::<Vec<_>>()[..] {
            ["Link", target, link] if link == name => Some(target),
            _ => None,
        }
    })
}

/// The zone `name` as Postgres knows it: the name itself when Postgres knows it, so a person reads back
/// the zone their browser or the server named (`Europe/Oslo` stays `Europe/Oslo`, though the tz
/// database links it to `Europe/Berlin`); else the current name of an old one ([`zone_link_target`])
/// when Postgres knows that. Else none, as for a name over 64 characters.
pub async fn known_zone(conn: &mut PgConnection, name: Option<&str>) -> Result<Option<String>> {
    let Some(name) = name.map(str::trim).filter(|z| !z.is_empty() && z.len() <= 64) else {
        return Ok(None);
    };
    let names: Vec<&str> = [name].into_iter().chain(zone_link_target(name)).collect();
    let known: Option<String> = sqlx::query_scalar(
        "select n from unnest(cast($1 as text[])) with ordinality as x(n, i) \
         where exists (select 1 from pg_timezone_names where name = x.n) order by x.i limit 1",
    )
    .bind(&names)
    .fetch_optional(conn)
    .await?;
    Ok(known)
}

/// Read local times in `zone` with Postgres's zone rules; without one, as UTC.
pub async fn instants(conn: &mut PgConnection, zone: Option<&str>, locals: &[NaiveDateTime]) -> Result<Vec<Instant>> {
    let Some(zone) = zone else {
        return Ok(locals.iter().map(|l| Instant::At(l.and_utc(), None)).collect());
    };
    // `d` is how far the offset moves within 6 hours of the time: a time is ambiguous when the instant
    // `d` before or after it shows the same wall clock, and missing when its own instant does not
    let sql = "select y.u, cast(round(extract(epoch from (y.u at time zone $2) - (y.u at time zone 'UTC')) / 60) as integer), \
         (y.u at time zone $2) <> x.l, \
         y.d <> interval '0' and ((y.u - y.d) at time zone $2 = x.l or (y.u + y.d) at time zone $2 = x.l) \
         from unnest(cast($1 as timestamp[])) with ordinality as x(l, i) \
         cross join lateral (select x.l at time zone $2 as u) w \
         cross join lateral (select w.u, \
           (((w.u - interval '6 hours') at time zone $2) - ((w.u - interval '6 hours') at time zone 'UTC')) \
           - (((w.u + interval '6 hours') at time zone $2) - ((w.u + interval '6 hours') at time zone 'UTC')) as d) y \
         order by x.i";
    let mut out = Vec::with_capacity(locals.len());
    for chunk in locals.chunks(ZONE_CHUNK) {
        let rows: Vec<(DateTime<Utc>, i32, bool, bool)> =
            sqlx::query_as(sql).bind(chunk).bind(zone).fetch_all(&mut *conn).await?;
        for (u, off, missing, twice) in rows {
            out.push(if missing {
                Instant::Missing
            } else if twice {
                Instant::Ambiguous
            } else {
                Instant::At(u, i16::try_from(off).ok())
            });
        }
    }
    Ok(out)
}

/// The zone a run reads the file's times in.
#[derive(Debug, Clone, Copy)]
enum Read<'a> {
    /// An upload, in this zone. A file read before in another zone has its own readings replaced; with
    /// no zone at all, a file read before keeps its zone.
    Upload(Option<&'a str>),
    /// An import again of a stored document (a retraction elsewhere queued it): the zone the file was
    /// first read in, or this one for a file never read.
    Again(Option<&'a str>),
}

/// The zone an upload's times are read in: the server's (`OWNHEALTH_TIMEZONE`) when it is set, else the
/// browser's, sent with the upload. Both are checked with [`known_zone`] first.
pub fn zone_choice<'a>(server: Option<&'a str>, upload: Option<&'a str>) -> Option<&'a str> {
    server.or(upload)
}

/// The server's zone (`OWNHEALTH_TIMEZONE`, passed in as `server`) and the upload's, each as Postgres
/// knows it ([`known_zone`]): an old name Postgres lacks reads as its current one, an unknown one is none.
pub async fn known_zones(
    conn: &mut PgConnection,
    server: Option<&str>,
    upload: Option<&str>,
) -> Result<(Option<String>, Option<String>)> {
    Ok((known_zone(&mut *conn, server).await?, known_zone(conn, upload).await?))
}

/// Import an uploaded CGM export document into its profile, reading its local times in `zone`. Returns
/// the summary and the details the import result shows (`cgm_csv`). The same file read before in
/// another zone has its own readings replaced (`replaced`, `replaced_timezone`).
pub async fn import_cgm_csv(
    conn: &mut PgConnection,
    blobs: &BlobStore,
    document_id: Uuid,
    source_id: Option<Uuid>,
    zone: Option<&str>,
) -> Result<(ImportSummary, Value)> {
    import_in(conn, blobs, document_id, source_id, Read::Upload(zone)).await
}

/// Import a stored CGM export again (a retraction of another export queued it), in the zone its file
/// was first read in (`flags.cgm_csv.timezone`); `fallback` for a file never read.
pub async fn import_cgm_csv_again(
    conn: &mut PgConnection,
    blobs: &BlobStore,
    document_id: Uuid,
    source_id: Option<Uuid>,
    fallback: Option<&str>,
) -> Result<(ImportSummary, Value)> {
    import_in(conn, blobs, document_id, source_id, Read::Again(fallback)).await
}

/// Delete the readings a document owns and rebuild the rollups they fed. Returns how many went.
async fn delete_own(conn: &mut PgConnection, profile_id: Uuid, document_id: Uuid) -> Result<i64> {
    let gone: Vec<(String, i16, DateTime<Utc>)> =
        sqlx::query_as("delete from wearable_sample where document_id = $1 returning metric, source_id, t")
            .bind(document_id)
            .fetch_all(&mut *conn)
            .await?;
    let (Some(lo), Some(hi)) = (gone.iter().map(|g| g.2).min(), gone.iter().map(|g| g.2).max()) else {
        return Ok(0);
    };
    let metrics: Vec<String> = gone
        .iter()
        .map(|g| g.0.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let sources: Vec<i16> = gone.iter().map(|g| g.1).collect::<BTreeSet<_>>().into_iter().collect();
    ownhealth_ingest_structured::wearables::recompute_rollups_between(
        &mut *conn,
        profile_id,
        Some(&metrics),
        Some(&sources),
        Some((lo, hi)),
    )
    .await?;
    Ok(gone.len() as i64)
}

async fn import_in(
    conn: &mut PgConnection,
    blobs: &BlobStore,
    document_id: Uuid,
    source_id: Option<Uuid>,
    read: Read<'_>,
) -> Result<(ImportSummary, Value)> {
    // one CGM import of a profile at a time: a run that replaces its readings and queues the others
    // again never interleaves with another's run (retract::lock_exports)
    let owner: Option<Uuid> = sqlx::query_scalar("select profile_id from document where id = $1")
        .bind(document_id)
        .fetch_one(&mut *conn)
        .await?;
    if let Some(owner) = owner {
        ownhealth_ingest_structured::retract::lock_exports(&mut *conn, owner, "cgm_csv").await?;
    }
    let (profile_id, sha, flags, status): (Option<Uuid>, String, Value, String) =
        sqlx::query_as("select profile_id, encode(blob_sha256, 'hex'), flags, status from document where id = $1")
            .bind(document_id)
            .fetch_one(&mut *conn)
            .await?;
    if status != "active" {
        // retracted while its import (again) waited in the queue: it brings nothing back
        let mut s = ImportSummary::new(document_id.to_string());
        s.reasons.insert("retracted".into(), 1);
        return Ok((s, Value::Null));
    }
    // the zone the file was read in before, if it was read: Some(None) is UTC
    let before: Option<Option<String>> = flags
        .get("cgm_csv")
        .filter(|d| d.is_object())
        .map(|d| d.get("timezone").and_then(Value::as_str).map(str::to_string));
    // two names the tz database links (an old name and its current one, or a zone merged into another)
    // are one zone: the same rules since 1970
    let same = |z: &str| zone_link_target(z).unwrap_or(z).to_string();
    // `named` is the zone the result names: the file's own, when an upload sends another name of it
    let (zone, replace, named): (Option<String>, bool, Option<String>) = match (read, &before) {
        (Read::Upload(Some(z)), Some(b)) => {
            let replace = b.as_deref().map(same) != Some(same(z));
            // read in the name Postgres knows now, whichever name the file was read under before
            (
                Some(z.to_string()),
                replace,
                if replace { Some(z.to_string()) } else { b.clone() },
            )
        }
        (Read::Upload(None) | Read::Again(_), Some(b)) => (b.clone(), false, b.clone()),
        (Read::Upload(z) | Read::Again(z), None) => (z.map(str::to_string), false, z.map(str::to_string)),
    };
    // imported before (the same file uploaded again): read again, and every reading counts as stored
    let again = !replace && flags.get("import").is_some_and(Value::is_object);
    let profile_id = profile_id.ok_or_else(|| Error::Invalid("the document has no profile".into()))?;
    let profile = get_profile(&mut *conn, profile_id)
        .await?
        .ok_or_else(|| Error::NotFound(format!("unknown profile {profile_id}")))?;
    let mut bytes = Vec::new();
    blobs
        .open_seek(BlobOwner::Profile(profile.id), &sha)?
        .take(MAX_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_BYTES {
        return Err(Error::Upload(format!(
            "the export is larger than {} MB: export a shorter period",
            MAX_BYTES / 1024 / 1024
        )));
    }
    let parsed = tokio::task::spawn_blocking(move || parse(&bytes))
        .await
        .map_err(|e| Error::Other(anyhow::anyhow!("CGM export read task failed: {e}")))??;
    // read in another zone before: its readings leave, and the file brings them at the new instants
    let replaced = if replace {
        delete_own(&mut *conn, profile.id, document_id).await?
    } else {
        0
    };
    let provider = parsed.vendor.provider();
    let mut sources = Vec::with_capacity(parsed.devices.len());
    for d in &parsed.devices {
        sources.push(
            store::device_source(
                &mut *conn,
                profile.id,
                provider,
                Some(&d.name),
                d.external_id.as_deref(),
                source_id,
            )
            .await?,
        );
    }
    let locals: Vec<NaiveDateTime> = parsed.readings.iter().map(|r| r.local).collect();
    let at = instants(&mut *conn, zone.as_deref(), &locals).await?;
    let mut reasons = parsed.skipped.clone();
    let latest = Utc::now() + Duration::days(1);
    let mut seen: HashSet<(i16, DateTime<Utc>)> = HashSet::new();
    let mut samples = Vec::with_capacity(parsed.readings.len());
    for (r, i) in parsed.readings.iter().zip(at) {
        let source = sources[r.device];
        match i {
            Instant::Ambiguous => bump(&mut reasons, "ambiguous_time"),
            Instant::Missing => bump(&mut reasons, "nonexistent_time"),
            Instant::At(t, _) if t > latest => bump(&mut reasons, "bad_time"),
            Instant::At(t, tz) => {
                if !seen.insert((source, t)) {
                    bump(&mut reasons, "repeated_time");
                    continue;
                }
                samples.push(Sample {
                    metric: cgm::METRIC,
                    t,
                    t_end: None,
                    v: r.mg_dl,
                    tz,
                    source,
                });
            }
        }
    }
    let w = store::write(&mut *conn, profile.id, Some(document_id), samples, Conflict::Keep).await?;
    if replaced > 0 {
        // the instants this file left may hold readings another export brings, which this file kept out
        // until now: the profile's other CGM exports are read again, each in its own zone, as after a
        // retraction
        ownhealth_ingest_structured::retract::reimport_kind(&mut *conn, document_id, "libreview_csv", "cgm_csv")
            .await?;
    }
    if w.duplicate() > 0 {
        reasons.insert("duplicate", w.duplicate());
    }
    let mut summary = ImportSummary::new(document_id.to_string());
    summary.accepted = w.new;
    summary.skipped = reasons.values().sum();
    summary.reasons = reasons.into_iter().map(|(k, n)| (k.to_string(), n)).collect();
    // the readings this file holds now, for the document screen: its own, not those another file owns
    let held: i64 = sqlx::query_scalar("select count(*) from wearable_sample where document_id = $1")
        .bind(document_id)
        .fetch_one(&mut *conn)
        .await?;
    let mut details = json!({
        "vendor": provider,
        "unit": parsed.unit.label(),
        "readings": w.new,
        "duplicates": w.duplicate(),
        "held": held,
        "devices": parsed.devices.iter().map(|d| d.name.clone()).collect::<Vec<_>>(),
        "unknown_columns": parsed.unknown_columns,
        "other_rows": parsed.other_rows,
        "timezone": named,
        "again": again,
    });
    if replace {
        details["replaced"] = json!(replaced);
        details["replaced_timezone"] = json!(before.flatten());
    }
    if !again {
        sqlx::query(
            "update document set flags = flags || jsonb_build_object('cgm_csv', cast($1 as jsonb)) where id = $2",
        )
        .bind(&details)
        .bind(document_id)
        .execute(&mut *conn)
        .await?;
        mark_imported(conn, document_id, &summary, None).await?;
    } else {
        // an import again keeps the first summary, but the count of readings the file holds moves: it
        // gains those a retracted export held before it
        sqlx::query(
            "update document set flags = jsonb_set(flags, '{cgm_csv,held}', to_jsonb(cast($1 as bigint))) \
             where id = $2 and jsonb_typeof(flags->'cgm_csv') = 'object'",
        )
        .bind(held)
        .bind(document_id)
        .execute(&mut *conn)
        .await?;
    }
    Ok((summary, details))
}

async fn run(ctx: JobContext, job: Job) -> jobs::HandlerResult {
    let channel = job
        .str("channel")
        .filter(|c| !c.is_empty())
        .unwrap_or("upload")
        .to_string();
    let source_id = job.str("source_id").and_then(|s| Uuid::parse_str(s).ok());
    let mime = documents::mime("libreview_csv").unwrap_or("text/csv");
    // a retraction queues `{document_id}`; an upload carries the blob and the browser's zone
    let stored = job.str("document_id").is_some_and(|d| !d.is_empty());
    let mut tx = ctx.pool.begin().await?;
    let doc = document_of_type(&mut tx, &ctx.blobs, &job.payload, mime, &channel, WEARABLES_DOC_TYPE).await?;
    let server = std::env::var("OWNHEALTH_TIMEZONE").ok();
    let (server, browser) = known_zones(&mut tx, server.as_deref(), job.str("timezone")).await?;
    let done = if stored {
        import_cgm_csv_again(&mut tx, &ctx.blobs, doc, source_id, server.as_deref()).await
    } else {
        let zone = zone_choice(server.as_deref(), browser.as_deref());
        import_cgm_csv(&mut tx, &ctx.blobs, doc, source_id, zone).await
    };
    let (summary, details) = done.map_err(|e| match e {
        Error::Upload(m) | Error::Invalid(m) => JobError::permanent(m),
        other => JobError::from(other),
    })?;
    tx.commit().await?;
    let mut out = summary.to_value();
    out["cgm_csv"] = details;
    Ok(Some(out))
}

pub fn register() {
    jobs::register_handler(JOB, |ctx, job| Box::pin(run(ctx, job)));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dt(y: i32, m: u32, d: u32, h: u32, mi: u32) -> Option<NaiveDateTime> {
        NaiveDate::from_ymd_opt(y, m, d)?.and_hms_opt(h, mi, 0)
    }

    #[test]
    fn the_molar_mass_is_the_molar_tables() {
        let m = ownhealth_normalize::tables::molar()
            .get("glucose")
            .map(|m| m.g_per_mol_text.clone());
        assert_eq!(m.as_deref(), Some("180.156"));
        assert_eq!(GLUCOSE_G_PER_MOL.to_string(), "180.156");
    }

    #[test]
    fn mmol_to_mg_keeps_the_clinical_edges_and_gives_the_printed_value_back() {
        // 3.9 and 10.0 mmol/L are the edges of 70-180 mg/dL: they stay inside, 3.8 and 10.1 outside
        assert_eq!(mg_dl_from_mmol(3.8), 68.0);
        assert_eq!(mg_dl_from_mmol(3.9), 70.0);
        assert_eq!(mg_dl_from_mmol(10.0), 180.0);
        assert_eq!(mg_dl_from_mmol(10.1), 182.0);
        // the other way: every one-decimal mmol/L a sensor prints (2.2 to 27.8) comes back from the
        // stored mg/dL
        for d in 22..=278 {
            let mmol = f64::from(d) / 10.0;
            let back = (mg_dl_from_mmol(mmol) * 10.0 / GLUCOSE_G_PER_MOL * 10.0).round() / 10.0;
            assert_eq!(back, mmol, "{mmol}");
        }
    }

    #[test]
    fn values_low_high_and_decimal_commas() {
        let mg = |c: &str, u: Unit| match glucose(c, u) {
            Glucose::Mg(v) => Some(v),
            _ => None,
        };
        assert_eq!(mg("98", Unit::MgDl), Some(98.0));
        assert_eq!(mg("5,6", Unit::MmolL), Some(101.0));
        assert_eq!(mg(" 5.6 ", Unit::MmolL), Some(101.0));
        for low in ["Low", "LO", "Niedrig", "Bajo"] {
            assert!(matches!(glucose(low, Unit::MgDl), Glucose::Low), "{low}");
        }
        for high in ["High", "HI", "Hoch", "Alto"] {
            assert!(matches!(glucose(high, Unit::MmolL), Glucose::High), "{high}");
        }
        for bad in ["", "n/a", "5.000,1", "0", "1e9", "NaN", "inf"] {
            assert!(matches!(glucose(bad, Unit::MgDl), Glucose::Bad), "{bad}");
        }
    }

    #[test]
    fn units_come_from_the_header_end() {
        assert_eq!(
            Unit::of_header("historic glucose mg/dl"),
            Some((Unit::MgDl, "historic glucose"))
        );
        assert_eq!(
            Unit::of_header("glukosewert (mmol/l)"),
            Some((Unit::MmolL, "glukosewert"))
        );
        assert_eq!(Unit::of_header("glucose rate of change (mg/dl/min)"), None);
        assert_eq!(Unit::of_header("notes"), None);
    }

    #[test]
    fn libreview_dates_are_read_in_one_order_for_the_whole_file() {
        // a day above 12 first: day first
        assert_eq!(
            libre_times(&["14-11-2026 06:03", "01-12-2026 00:00"]),
            vec![dt(2026, 11, 14, 6, 3), dt(2026, 12, 1, 0, 0)]
        );
        // a day above 12 second, with a 12-hour clock: the US order
        assert_eq!(
            libre_times(&["11-14-2026 12:03 AM", "11-14-2026 12:10 PM", "11-14-2026 01:33 PM"]),
            vec![
                dt(2026, 11, 14, 0, 3),
                dt(2026, 11, 14, 12, 10),
                dt(2026, 11, 14, 13, 33)
            ]
        );
        // no number above 12: the order that spans fewer days (1-2 November, not 11 January to 11 February)
        assert_eq!(
            libre_times(&["01-11-2026 23:00", "02-11-2026 00:15"]),
            vec![dt(2026, 11, 1, 23, 0), dt(2026, 11, 2, 0, 15)]
        );
        assert_eq!(
            libre_times(&["11-01-2026 23:00", "11-02-2026 00:15"]),
            vec![dt(2026, 11, 1, 23, 0), dt(2026, 11, 2, 0, 15)]
        );
        // one date only: a 12-hour clock says month first, else day first
        assert_eq!(libre_times(&["05-06-2026 08:00 AM"]), vec![dt(2026, 5, 6, 8, 0)]);
        assert_eq!(libre_times(&["05-06-2026 08:00"]), vec![dt(2026, 6, 5, 8, 0)]);
        // slashes, dots, seconds and year first read too; what is not a date is none
        assert_eq!(
            libre_times(&["2026/11/14 06:03:30"]),
            vec![NaiveDate::from_ymd_opt(2026, 11, 14).and_then(|d| d.and_hms_opt(6, 3, 30))]
        );
        assert_eq!(
            libre_times(&[
                "14.11.2026 06:03",
                "x",
                "14-11-26 06:03",
                "31-02-2026 06:03",
                "14-11-2026 13:00 PM"
            ]),
            vec![dt(2026, 11, 14, 6, 3), None, None, None, None]
        );
    }

    #[test]
    fn an_impossible_date_does_not_decide_the_order_of_the_file() {
        // 99 is a day in no month and a month in no year: the US dates around it decide the order, and
        // it stays a bad time
        assert_eq!(
            libre_times(&["99/01/2026 08:00 AM", "11/05/2026 08:00 AM", "11/14/2026 08:00 AM"]),
            vec![None, dt(2026, 11, 5, 8, 0), dt(2026, 11, 14, 8, 0)]
        );
        // and the other way: 40 does not say month first, so the shorter span reads 5 and 6 November
        assert_eq!(
            libre_times(&["01/40/2026 08:00", "05/11/2026 08:00", "06/11/2026 08:00"]),
            vec![None, dt(2026, 11, 5, 8, 0), dt(2026, 11, 6, 8, 0)]
        );
    }

    #[test]
    fn low_and_high_rows_also_decide_the_order_of_the_file() -> Result<()> {
        // every reading is on 11-12, which reads either way; the Low row on 11-13 says month first
        let text = "Device,Serial Number,Device Timestamp,Record Type,Historic Glucose mg/dL\n\
            FreeStyle Libre 3,SN1,11-12-2026 10:00,0,112\nFreeStyle Libre 3,SN1,11-12-2026 10:15,0,118\n\
            FreeStyle Libre 3,SN1,11-13-2026 03:00,0,Low\n";
        let p = parse(text.as_bytes())?;
        assert_eq!(
            p.readings.iter().map(|r| Some(r.local)).collect::<Vec<_>>(),
            vec![dt(2026, 11, 12, 10, 0), dt(2026, 11, 12, 10, 15)]
        );
        assert_eq!(p.skipped, BTreeMap::from([("below_sensor_range", 1)]));
        Ok(())
    }

    #[test]
    fn a_date_before_2000_does_not_decide_the_order_of_the_file() -> Result<()> {
        // US readings on 11/12, which read either way; a Low row dated 13/01/1999 would say day first
        let head = "Device,Serial Number,Device Timestamp,Record Type,Historic Glucose mg/dL\n\
            FreeStyle Libre 3,SN1,11-12-2026 10:00 AM,0,112\nFreeStyle Libre 3,SN1,11-12-2026 10:15 AM,0,118\n";
        let nov_12 = vec![dt(2026, 11, 12, 10, 0), dt(2026, 11, 12, 10, 15)];
        let p = parse(format!("{head}FreeStyle Libre 3,SN1,13-01-1999 03:00 AM,0,Low\n").as_bytes())?;
        assert_eq!(p.readings.iter().map(|r| Some(r.local)).collect::<Vec<_>>(), nov_12);
        assert_eq!(p.skipped, BTreeMap::from([("below_sensor_range", 1)]));
        // a reading dated 1999 decides nothing either, and is a bad time
        let p = parse(format!("{head}FreeStyle Libre 3,SN1,13-01-1999 03:00 AM,0,99\n").as_bytes())?;
        assert_eq!(p.readings.iter().map(|r| Some(r.local)).collect::<Vec<_>>(), nov_12);
        assert_eq!(p.skipped, BTreeMap::from([("bad_time", 1)]));
        Ok(())
    }

    #[test]
    fn low_and_high_rows_count_toward_the_reading_cap() -> Result<()> {
        let libre = "Device,Serial Number,Device Timestamp,Record Type,Historic Glucose mg/dL\n\
            FreeStyle Libre 3,SN1,14-11-2026 10:00,0,112\nFreeStyle Libre 3,SN1,14-11-2026 10:15,0,Low\n\
            FreeStyle Libre 3,SN1,14-11-2026 10:30,0,High\n";
        let clarity = format!(
            "{CLARITY_HEAD}1,2026-11-14T08:00:00,EGV,,,,iOS G7,101\n2,2026-11-14T08:05:00,EGV,,,,iOS G7,Low\n\
             3,2026-11-14T08:10:00,EGV,,,,iOS G7,High\n"
        );
        for text in [libre, clarity.as_str()] {
            let refused = parse_capped(text.as_bytes(), 2).map(|p| p.readings.len());
            assert!(
                matches!(&refused, Err(Error::Upload(m)) if m.contains("more than 2 readings")),
                "{refused:?}"
            );
            assert_eq!(parse_capped(text.as_bytes(), 3)?.readings.len(), 1);
        }
        Ok(())
    }

    #[test]
    fn a_separator_inside_a_quoted_header_does_not_change_the_delimiter() -> Result<()> {
        let p = parse(
            "Index,Timestamp (YYYY-MM-DDThh:mm:ss),Event Type,Glucose Value (mg/dL),\"Notes;;;;;\"\n1,2026-11-14T08:00:00,EGV,98,x\n"
                .as_bytes(),
        )?;
        assert_eq!(p.readings.len(), 1);
        assert_eq!(p.unknown_columns, vec!["Notes;;;;;".to_string()]);
        Ok(())
    }

    #[test]
    fn a_clarity_reading_without_a_time_is_a_bad_time() -> Result<()> {
        // an EGV row with an empty timestamp is a reading whose time is unreadable; the untimed device
        // and alert rows of the header block stay out of every count
        let text = format!(
            "{CLARITY_HEAD}1,,Device,,,Dexcom G7,iOS G7,,,,,,,\n2,,Alert,High,,,iOS G7,250,,,,,,\n\
             3,,EGV,,,,iOS G7,100,,,,,9000000,8M4TQZ\n4,2026-11-14T08:05:00,EGV,,,,iOS G7,104,,,,,9000300,8M4TQZ\n"
        );
        let p = parse(text.as_bytes())?;
        assert_eq!(p.readings.len(), 1);
        assert_eq!(p.skipped, BTreeMap::from([("bad_time", 1)]));
        assert_eq!(p.other_rows, 0);
        assert_eq!(p.devices[0].name, "Dexcom G7");
        Ok(())
    }

    #[test]
    fn the_server_zone_wins_over_the_browser_zone() {
        assert_eq!(
            zone_choice(Some("Europe/Madrid"), Some("America/New_York")),
            Some("Europe/Madrid")
        );
        assert_eq!(zone_choice(None, Some("America/New_York")), Some("America/New_York"));
        assert_eq!(zone_choice(None, None), None);
    }

    #[test]
    fn an_unknown_header_is_kept_as_written() -> Result<()> {
        let p = parse("Index,Timestamp (YYYY-MM-DDThh:mm:ss),Event Type,Glucose Value (mg/dL),Notiz des Arztes\n1,2026-11-14T08:00:00,EGV,98,x\n".as_bytes())?;
        assert_eq!(p.unknown_columns, vec!["Notiz des Arztes".to_string()]);
        assert_eq!(p.readings.len(), 1);
        assert_eq!(
            p.devices,
            vec![Device {
                name: "Dexcom".into(),
                external_id: None
            }]
        );
        Ok(())
    }

    /// The Clarity header and a short EGV row that ends at its glucose cell (8 of the 14 cells).
    const CLARITY_HEAD: &str = "Index,Timestamp (YYYY-MM-DDThh:mm:ss),Event Type,Event Subtype,Patient Info,Device Info,\
        Source Device ID,Glucose Value (mg/dL),Insulin Value (u),Carb Value (grams),Duration (hh:mm:ss),\
        Glucose Rate of Change (mg/dL/min),Transmitter Time (Long Integer),Transmitter ID\n";

    #[test]
    fn a_short_last_row_that_ends_with_a_line_break_is_a_reading() -> Result<()> {
        // shorter than the header and ending at its value, but the line break says nothing was cut
        let text =
            format!("{CLARITY_HEAD}1,2026-11-14T08:00:00,EGV,,,,iOS G7,101\n2,2026-11-14T08:05:00,EGV,,,,iOS G7,104\n");
        let p = parse(text.as_bytes())?;
        assert_eq!(p.readings.len(), 2, "{:?}", p.skipped);
        assert!(p.skipped.is_empty(), "{:?}", p.skipped);
        Ok(())
    }

    #[test]
    fn a_short_row_before_the_last_one_is_a_reading_without_a_final_line_break() -> Result<()> {
        // the file has no final line break, but only its last row can be the one a download cut
        let text = format!(
            "{CLARITY_HEAD}1,2026-11-14T08:00:00,EGV,,,,iOS G7,101\n2,2026-11-14T08:05:00,EGV,,,,iOS G7,104,,,,,9000300,8M4TQZ"
        );
        let p = parse(text.as_bytes())?;
        assert_eq!(p.readings.len(), 2, "{:?}", p.skipped);
        assert!(p.skipped.is_empty(), "{:?}", p.skipped);
        Ok(())
    }
}
