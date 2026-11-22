//! LibreView and Dexcom Clarity CSV exports (lane D, slice 28): a CGM history from the vendor's own
//! export file. Job kind `import.cgm_csv`, payload as the other structured importers (`{profile_id,
//! household_id, blob_sha256, filename}`, or `{document_id}` for an import again); core sniffs the
//! header row (`documents::CGM_CSV_KINDS`) as `libreview_csv` or `dexcom_clarity_csv`. The file
//! becomes a `text/csv` document of the profile and owns the readings it brings.
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
//! time zone (`OWNHEALTH_TIMEZONE`, the zone the MCP day budget already uses) with Postgres's zone
//! rules, and each reading keeps the offset that applied (`tz_offset_min`). Without that setting, or
//! with a zone Postgres does not know, a time is read as UTC and keeps no offset: the rule the PDF and
//! FHIR importers apply to a time without an offset. On the night the clocks go back, the times of the
//! repeated hour are ambiguous: those readings are counted (`ambiguous_time`), not guessed. A time in the
//! hour the clocks skip in spring is not a local time (`nonexistent_time`). Two readings of one sensor at
//! one instant keep the first (`repeated_time`).
//!
//! Re-import: `Conflict::Keep`, the first file to bring a reading owns it. The same file again, or a
//! longer export that holds it, adds only readings that are not stored yet; the rest count as
//! `duplicate`. Retracting a file queues the profile's other CGM exports for import again
//! (`retract::retract_document_facts`), so the readings they share come back as theirs.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::Read as _;

use chrono::{DateTime, Duration, NaiveDate, NaiveDateTime, NaiveTime, Timelike, Utc};
use ownhealth_core::access::get_profile;
use ownhealth_core::blobs::{BlobOwner, BlobStore};
use ownhealth_core::documents::{self, CLARITY_TIME_SUFFIX, header_key};
use ownhealth_core::error::{Error, Result};
use ownhealth_core::ingest::ImportSummary;
use ownhealth_core::jobs::{self, Job, JobContext, JobError};
use ownhealth_ingest_structured::fhir::mark_imported;
use ownhealth_ingest_structured::jobs::document_for;
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
/// Readings one file may hold.
pub const MAX_READINGS: usize = 5_000_000;
/// The molar mass of D-glucose C6H12O6 in g/mol (data/terminology/molar/molar_mass.tsv, PubChem 5793).
pub const GLUCOSE_G_PER_MOL: f64 = 180.156;
/// A number outside this range (mg/dL) is not a glucose reading.
const PLAUSIBLE_MG_DL: (f64, f64) = (10.0, 1000.0);
/// Unknown column names kept in the result, each cut to this many characters.
const MAX_UNKNOWN: usize = 20;
const MAX_NAME: usize = 80;
/// Local times sent to Postgres at a time.
const ZONE_CHUNK: usize = 20_000;

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
                    let shown: String = cell
                        .trim()
                        .trim_start_matches('\u{feff}')
                        .chars()
                        .take(MAX_NAME)
                        .collect();
                    c.unknown.push(shown);
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
    let mut records = rdr.records();
    let header = records
        .next()
        .ok_or_else(|| not_cgm("no header row"))?
        .map_err(|e| not_cgm(&e.to_string()))?;
    let cols = columns(vendor, &header)?;
    let (time_col, kind_col) = (cols.time.unwrap_or(0), cols.kind.unwrap_or(0));

    let mut skipped: BTreeMap<&'static str, i64> = BTreeMap::new();
    let mut other_rows = 0i64;
    let mut device_ids: HashMap<(String, String), usize> = HashMap::new();
    let mut devices: Vec<(String, String)> = Vec::new();
    // Clarity's header block: source device id -> device name
    let mut names: HashMap<String, String> = HashMap::new();
    let mut pending: Vec<(usize, String, f64)> = Vec::new();
    for rec in records {
        let rec = rec.map_err(|e| not_cgm(&e.to_string()))?;
        let cell = |i: Option<usize>| i.and_then(|i| rec.get(i)).unwrap_or("");
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
                if cell(Some(time_col)).is_empty() {
                    // the header block: name, birth date, devices and alert settings, not events
                    if one_of(CLARITY_DEVICE_ROW, &kind.to_lowercase()) {
                        let (src, info) = (cell(cols.device), cell(cols.device_info));
                        if !src.is_empty() && !info.is_empty() {
                            names.entry(src.to_string()).or_insert_with(|| info.to_string());
                        }
                    }
                    continue;
                }
                if !kind.eq_ignore_ascii_case("egv") {
                    other_rows += 1;
                    continue;
                }
                let (col, unit, _) = cols.glucose[0];
                (col, unit)
            }
        };
        let mg = match glucose(cell(Some(col)), unit) {
            Glucose::Mg(v) => v,
            Glucose::Low => {
                bump(&mut skipped, "below_sensor_range");
                continue;
            }
            Glucose::High => {
                bump(&mut skipped, "above_sensor_range");
                continue;
            }
            Glucose::Bad => {
                bump(&mut skipped, "bad_value");
                continue;
            }
        };
        let key = (cell(cols.device).to_string(), cell(cols.serial).to_string());
        let device = *device_ids.entry(key.clone()).or_insert_with(|| {
            devices.push(key);
            devices.len() - 1
        });
        pending.push((device, cell(Some(time_col)).to_string(), mg));
        if pending.len() > MAX_READINGS {
            return Err(Error::Upload(format!(
                "the export holds more than {MAX_READINGS} readings: export a shorter period"
            )));
        }
    }
    let times: Vec<Option<NaiveDateTime>> = match vendor {
        Vendor::Clarity => pending.iter().map(|p| iso_time(&p.1)).collect(),
        Vendor::LibreView => libre_times(&pending.iter().map(|p| p.1.as_str()).collect::<Vec<_>>()),
    };
    let earliest = NaiveDate::from_ymd_opt(2000, 1, 1).unwrap_or_default();
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
/// export read the wrong way round spans months), and on a tie a 12-hour clock means the US order.
fn day_first(stamps: &[Stamp]) -> bool {
    let dm: Vec<&Stamp> = stamps.iter().filter(|s| !s.year_first).collect();
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
fn libre_times(raw: &[&str]) -> Vec<Option<NaiveDateTime>> {
    let stamps: Vec<Option<Stamp>> = raw.iter().map(|s| stamp(s)).collect();
    let known: Vec<Stamp> = stamps.iter().flatten().copied().collect();
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

/// The zone `name` when Postgres knows it, else none.
pub async fn known_zone(conn: &mut PgConnection, name: Option<&str>) -> Result<Option<String>> {
    let Some(name) = name.map(str::trim).filter(|z| !z.is_empty()) else {
        return Ok(None);
    };
    let known: bool = sqlx::query_scalar("select exists (select 1 from pg_timezone_names where name = $1)")
        .bind(name)
        .fetch_one(conn)
        .await?;
    Ok(known.then(|| name.to_string()))
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

/// Import a stored CGM export document into its profile, reading its local times in `zone`. Returns the
/// summary and the details the import result shows (`cgm_csv`).
pub async fn import_cgm_csv(
    conn: &mut PgConnection,
    blobs: &BlobStore,
    document_id: Uuid,
    source_id: Option<Uuid>,
    zone: Option<&str>,
) -> Result<(ImportSummary, Value)> {
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
    // imported before (the same file uploaded again): read again, and every reading counts as stored
    let again = flags.get("import").is_some_and(Value::is_object);
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
    let at = instants(&mut *conn, zone, &locals).await?;
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
    if w.duplicate() > 0 {
        reasons.insert("duplicate", w.duplicate());
    }
    let mut summary = ImportSummary::new(document_id.to_string());
    summary.accepted = w.new;
    summary.skipped = reasons.values().sum();
    summary.reasons = reasons.into_iter().map(|(k, n)| (k.to_string(), n)).collect();
    let details = json!({
        "vendor": provider,
        "unit": parsed.unit.label(),
        "readings": w.new,
        "duplicates": w.duplicate(),
        "devices": parsed.devices.iter().map(|d| d.name.clone()).collect::<Vec<_>>(),
        "unknown_columns": parsed.unknown_columns,
        "other_rows": parsed.other_rows,
        "timezone": zone,
        "again": again,
    });
    if !again {
        sqlx::query(
            "update document set flags = flags || jsonb_build_object('cgm_csv', cast($1 as jsonb)) where id = $2",
        )
        .bind(&details)
        .bind(document_id)
        .execute(&mut *conn)
        .await?;
        mark_imported(conn, document_id, &summary, None).await?;
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
    let mut tx = ctx.pool.begin().await?;
    let doc = document_for(&mut tx, &ctx.blobs, &job.payload, mime, &channel).await?;
    let zone = known_zone(&mut tx, std::env::var("OWNHEALTH_TIMEZONE").ok().as_deref()).await?;
    let (summary, details) = import_cgm_csv(&mut tx, &ctx.blobs, doc, source_id, zone.as_deref())
        .await
        .map_err(|e| match e {
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
}
