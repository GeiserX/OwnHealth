//! Strava account exports join activities.csv to the files in activities/. The original archive,
//! CSV text, device metadata and routes are discarded after projection to wearable readings.

use std::collections::{BTreeMap, HashSet};
use std::io::{Read, Seek};

use chrono::{DateTime, Duration, Utc};
use ownhealth_core::documents::{STRAVA_READINGS_MIME, strava_manifest};
use ownhealth_core::jobs;
use ownhealth_core::zip_directory::GARMIN_MAX_FILES;
use ownhealth_core::{Error, Result};
use quick_xml::NsReader;
use quick_xml::events::{BytesStart, Event};
use quick_xml::name::ResolveResult;
use zip::ZipArchive;

use crate::fit;
use crate::garmin_zip::{self, Budget, ExportSettings, MAX_JSON, OVER_BUDGET, Report, read_member, unsafe_name};
use crate::store::Sample;

pub const JOB: &str = "import.strava_zip";
pub const PROVIDER: &str = "strava_export";
pub(crate) const EXPORT: ExportSettings = ExportSettings {
    flag: "strava",
    provider: PROVIDER,
    format: "ownhealth.strava-readings/1",
    mime: STRAVA_READINGS_MIME,
    unreadable: "strava_zip_unreadable",
    limit: "strava_zip_limit",
    label: "Strava",
    fit_label: None,
};

pub fn register() {
    jobs::register_handler(JOB, |ctx, job| Box::pin(garmin_zip::run(ctx, job, EXPORT)));
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Format {
    Fit,
    Gpx,
    Tcx,
}

impl Format {
    fn detect(name: &str) -> Option<(Self, bool)> {
        let name = name.to_ascii_lowercase();
        let plain = name.strip_suffix(".gz").unwrap_or(&name);
        let format = match plain.rsplit_once('.')?.1 {
            "fit" => Self::Fit,
            "gpx" => Self::Gpx,
            "tcx" => Self::Tcx,
            _ => return None,
        };
        Some((format, plain.len() != name.len()))
    }
}

#[derive(Default)]
pub(crate) struct JoinCounts {
    pub rows_without_file: usize,
    pub files_without_row: usize,
}

fn activity_path(name: &str) -> bool {
    name.strip_prefix("activities/").is_some_and(|rest| !rest.is_empty())
        && !name.ends_with('/')
        && !unsafe_name(name)
        && !name.contains('\\')
}

fn unreadable() -> Error {
    Error::Upload(EXPORT.unreadable.into())
}

fn zip_error(error: zip::result::ZipError) -> Error {
    garmin_zip::zip_error(error, &EXPORT)
}

fn member_bytes<R: Read + Seek>(
    zip: &mut ZipArchive<R>,
    index: usize,
    budget: &mut Budget,
    cap: u64,
) -> Result<std::result::Result<Vec<u8>, &'static str>> {
    let file = match zip.by_index(index).map_err(zip_error) {
        Ok(file) => file,
        Err(Error::Upload(_)) => return Ok(Err("invalid_member")),
        Err(error) => return Err(error),
    };
    let size = file.size();
    if size > cap {
        return Ok(Err("member_too_large"));
    }
    if !budget.spend_expanded(size) {
        return Ok(Err(OVER_BUDGET));
    }
    let mut bytes = Vec::new();
    if !read_member(file, size, &mut bytes)? {
        return Ok(Err("invalid_member"));
    }
    Ok(Ok(bytes))
}

fn unpack(bytes: Vec<u8>, budget: &mut Budget) -> std::result::Result<Vec<u8>, &'static str> {
    let remaining = budget.remaining_expanded();
    let limit = remaining.min(fit::MAX_BYTES);
    let mut out = Vec::new();
    let result = flate2::read::MultiGzDecoder::new(bytes.as_slice())
        .take(limit + 1)
        .read_to_end(&mut out);
    let used = out.len() as u64;
    // Charge the output even if the decoder later finds a damaged stream or exceeds the limit.
    budget.spend_expanded(used.min(remaining));
    if used > remaining {
        return Err(OVER_BUDGET);
    }
    if used > limit {
        return Err("member_too_large");
    }
    result.map_err(|_| "invalid_gzip")?;
    Ok(out)
}

fn csv_path(cell: &[u8]) -> Option<&str> {
    std::str::from_utf8(cell)
        .ok()
        .map(str::trim)
        .filter(|value| activity_path(value))
}

/// Count matches against the ZIP as well as other activity paths. A row's name or description
/// might happen to start with activities/, so an exact archive match is stronger evidence.
fn join_manifest(csv: &[u8], files: &BTreeMap<String, usize>) -> Result<JoinCounts> {
    let reader = || csv::ReaderBuilder::new().flexible(true).from_reader(csv);
    let mut columns = BTreeMap::<usize, (usize, usize)>::new();
    let mut rows = 0;
    for record in reader().byte_records() {
        let record = record.map_err(|_| unreadable())?;
        rows += 1;
        if rows > GARMIN_MAX_FILES {
            return Err(Error::Upload(EXPORT.limit.into()));
        }
        for (column, cell) in record.iter().enumerate() {
            if let Some(name) = csv_path(cell) {
                let (matched, names) = columns.entry(column).or_default();
                *matched += usize::from(files.contains_key(name));
                *names += 1;
            }
        }
    }

    let mut counts = JoinCounts {
        rows_without_file: rows,
        files_without_row: files.len(),
    };
    let Some((&column, _)) = columns.iter().rev().max_by_key(|(_, score)| *score) else {
        return Ok(counts);
    };
    let mut joined = HashSet::new();
    for record in reader().byte_records() {
        let record = record.map_err(|_| unreadable())?;
        if let Some(name) = record.get(column).and_then(csv_path)
            && let Some((stored_name, _)) = files.get_key_value(name)
        {
            counts.rows_without_file -= 1;
            joined.insert(stored_name);
        }
    }
    counts.files_without_row -= joined.len();
    Ok(counts)
}

pub(crate) fn scan<R: Read + Seek>(
    zip: &mut ZipArchive<R>,
    budget: &mut Budget,
    report: &mut Report,
) -> Result<JoinCounts> {
    let manifest = strava_manifest(zip).ok_or_else(unreadable)?;
    let root = manifest.strip_suffix("activities.csv").expect("Strava manifest suffix");
    let mut csv = None;
    let mut files = BTreeMap::new();
    for index in 0..zip.len() {
        let header = zip.by_index_data(index).map_err(zip_error)?;
        budget.add_name(header.name_raw().len())?;
        if header.is_dir() {
            continue;
        }
        budget.add_members(1)?;
        let Ok(name) = header.name() else {
            continue;
        };
        if name == manifest {
            if unsafe_name(&name) || header.is_symlink() {
                return Err(unreadable());
            }
            csv = Some(index);
        } else if let Some(name) = name.strip_prefix(root)
            && name.starts_with("activities/")
        {
            if activity_path(name) && !header.is_symlink() {
                files.insert(name.to_owned(), index);
            } else {
                report.omit(format!("ZIP #{}", index + 1), "unsafe_path");
            }
        }
    }

    let csv = member_bytes(zip, csv.ok_or_else(unreadable)?, budget, MAX_JSON)?.map_err(|reason| {
        if matches!(reason, "member_too_large" | OVER_BUDGET) {
            Error::Upload(EXPORT.limit.into())
        } else {
            unreadable()
        }
    })?;
    let counts = join_manifest(&csv, &files)?;
    for (name, index) in files {
        let member = name
            .strip_prefix("activities/")
            .and_then(|base| base.split_once('.'))
            .filter(|(id, _)| !id.is_empty() && id.bytes().all(|byte| byte.is_ascii_digit()))
            .filter(|(_, ext)| {
                let ext = ext.to_ascii_lowercase();
                matches!(ext.as_str(), "fit" | "fit.gz" | "gpx" | "gpx.gz" | "tcx" | "tcx.gz")
            })
            .map_or_else(|| format!("ZIP #{}", index + 1), |_| name.clone());
        let Some((format, gzipped)) = Format::detect(&name) else {
            report.omit(member, "unsupported");
            continue;
        };
        let bytes = member_bytes(zip, index, budget, fit::MAX_BYTES)?;
        let result = bytes
            .and_then(|bytes| if gzipped { unpack(bytes, budget) } else { Ok(bytes) })
            .and_then(|bytes| project(format, &bytes));
        let samples = result.map(|(samples, gps)| {
            report.gps_left_out |= gps;
            samples
        });
        report.add(budget, member, format == Format::Fit, samples);
    }
    Ok(counts)
}

fn project(format: Format, bytes: &[u8]) -> std::result::Result<(Vec<Sample>, bool), &'static str> {
    if format == Format::Fit {
        return match std::panic::catch_unwind(|| fit::archive_activity(bytes)) {
            Ok(Ok(projection)) => Ok(projection),
            _ => Err("invalid_fit"),
        };
    }
    xml_readings(format, bytes).ok_or(match format {
        Format::Gpx => "invalid_gpx",
        _ => "invalid_tcx",
    })
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tag {
    Gpx,
    Track,
    Segment,
    Point,
    Type,
    Extensions,
    PointExtension,
    Hr,
    Time,
    Tcx,
    Activities,
    Activity,
    Lap,
    TcxTrack,
    TcxPoint,
    Position,
    Bpm,
    Value,
    Distance,
    Calories,
    TotalTime,
    Other,
}

fn tag(format: Format, namespace: ResolveResult<'_>, local: &str) -> Tag {
    let ns = match namespace {
        ResolveResult::Bound(ns) => ns.into_inner(),
        ResolveResult::Unbound => "",
        ResolveResult::Unknown(_) => return Tag::Other,
    };
    match (format, ns, local) {
        (
            Format::Gpx,
            "http://www.garmin.com/xmlschemas/TrackPointExtension/v1"
            | "http://www.garmin.com/xmlschemas/TrackPointExtension/v2",
            "TrackPointExtension",
        ) => Tag::PointExtension,
        (
            Format::Gpx,
            "http://www.garmin.com/xmlschemas/TrackPointExtension/v1"
            | "http://www.garmin.com/xmlschemas/TrackPointExtension/v2",
            "hr",
        ) => Tag::Hr,
        (Format::Gpx, "" | "http://www.topografix.com/GPX/1/1" | "http://www.topografix.com/GPX/1/0", name) => {
            match name {
                "gpx" => Tag::Gpx,
                "trk" => Tag::Track,
                "trkseg" => Tag::Segment,
                "trkpt" => Tag::Point,
                "type" => Tag::Type,
                "extensions" => Tag::Extensions,
                "time" => Tag::Time,
                _ => Tag::Other,
            }
        }
        (
            Format::Tcx,
            ""
            | "http://www.garmin.com/xmlschemas/TrainingCenterDatabase/v2"
            | "http://www.garmin.com/xmlschemas/TrainingCenterDatabase/v1",
            name,
        ) => match name {
            "TrainingCenterDatabase" => Tag::Tcx,
            "Activities" => Tag::Activities,
            "Activity" => Tag::Activity,
            "Lap" => Tag::Lap,
            "Track" => Tag::TcxTrack,
            "Trackpoint" => Tag::TcxPoint,
            "Position" => Tag::Position,
            "Time" => Tag::Time,
            "HeartRateBpm" => Tag::Bpm,
            "Value" => Tag::Value,
            "DistanceMeters" => Tag::Distance,
            "Calories" => Tag::Calories,
            "TotalTimeSeconds" => Tag::TotalTime,
            _ => Tag::Other,
        },
        _ => Tag::Other,
    }
}

#[derive(Clone, Copy)]
struct Coordinates {
    lat: f64,
    lon: f64,
}

impl Coordinates {
    fn from_point(start: &BytesStart<'_>) -> Option<Self> {
        let lat = attribute(start, "lat")?.parse::<f64>().ok()?;
        let lon = attribute(start, "lon")?.parse::<f64>().ok()?;
        ((-90.0..=90.0).contains(&lat) && (-180.0..=180.0).contains(&lon)).then_some(Self {
            lat: lat.to_radians(),
            lon: lon.to_radians(),
        })
    }

    fn metres_to(self, next: Self) -> f64 {
        let a = ((next.lat - self.lat) / 2.0).sin().powi(2)
            + self.lat.cos() * next.lat.cos() * ((next.lon - self.lon) / 2.0).sin().powi(2);
        2.0 * 6_371_000.0 * a.clamp(0.0, 1.0).sqrt().asin()
    }
}

fn attribute<'a>(start: &'a BytesStart<'_>, name: &str) -> Option<&'a str> {
    let attr = start.try_get_attribute(name).ok()??;
    match attr.value {
        std::borrow::Cow::Borrowed(value) => Some(value),
        std::borrow::Cow::Owned(_) => None,
    }
}

fn utc(text: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(text).ok().map(|t| t.to_utc())
}

fn sport(text: &str) -> Option<String> {
    let kind = match text.to_ascii_lowercase().as_str() {
        "run" | "running" | "trailrun" => "running",
        "walk" | "walking" => "walking",
        "hike" | "hiking" => "hiking",
        _ => return None,
    };
    Some(kind.into())
}

/// Strava's own GPX writes its activity type numbers: 9 run, 10 walk, 4 hike (1 is a ride).
fn gpx_sport(text: &str) -> Option<String> {
    sport(match text {
        "9" => "running",
        "10" => "walking",
        "4" => "hiking",
        other => other,
    })
}

#[derive(Default)]
struct Point {
    time: Option<DateTime<Utc>>,
    hr: Option<f64>,
    coords: Option<Coordinates>,
}

#[derive(Default)]
struct Lap {
    start: Option<DateTime<Utc>>,
    duration: Option<f64>,
    metres: Option<f64>,
    kcal: Option<f64>,
}

struct XmlReadings {
    out: fit::Read,
    latest: DateTime<Utc>,
    activity: fit::Session,
    lap: Lap,
    point: Point,
    previous: Option<(DateTime<Utc>, Coordinates)>,
}

impl XmlReadings {
    fn start(&mut self, path: &[Tag], element: &BytesStart<'_>) {
        use Tag::*;
        match path {
            [Gpx, Track] | [Tcx, Activities, Activity] => {
                self.activity = fit::Session::default();
                self.activity.sport = attribute(element, "Sport").and_then(sport);
            }
            [Gpx, Track, Segment] => self.previous = None,
            [Gpx, Track, Segment, Point] => {
                self.out.positions |= element.try_get_attribute("lat").ok().flatten().is_some()
                    || element.try_get_attribute("lon").ok().flatten().is_some();
                self.point = self::Point {
                    coords: Coordinates::from_point(element),
                    ..Default::default()
                };
            }
            [Tcx, Activities, Activity, Lap] => {
                self.lap = self::Lap {
                    start: attribute(element, "StartTime").and_then(utc),
                    ..Default::default()
                };
            }
            [Tcx, Activities, Activity, Lap, TcxTrack, TcxPoint] => self.point = self::Point::default(),
            [Tcx, Activities, Activity, Lap, TcxTrack, TcxPoint, Position] => self.out.positions = true,
            _ => {}
        }
    }

    fn text(&mut self, path: &[Tag], value: &str) {
        use Tag::*;
        let value = value.trim();
        let positive = || value.parse::<f64>().ok().filter(|v| v.is_finite() && *v >= 0.0);
        match path {
            [Gpx, Track, Type] => self.activity.sport = gpx_sport(value),
            [Gpx, Track, Segment, Point, Time] | [Tcx, Activities, Activity, Lap, TcxTrack, TcxPoint, Time] => {
                self.point.time = utc(value)
            }
            [Gpx, Track, Segment, Point, Extensions, PointExtension, Hr]
            | [Tcx, Activities, Activity, Lap, TcxTrack, TcxPoint, Bpm, Value] => self.point.hr = positive(),
            [Tcx, Activities, Activity, Lap, Distance] => self.lap.metres = positive(),
            [Tcx, Activities, Activity, Lap, Calories] => self.lap.kcal = positive(),
            [Tcx, Activities, Activity, Lap, TotalTime] => self.lap.duration = positive(),
            _ => {}
        }
    }

    fn point(&mut self) {
        let time = self.point.time.filter(|t| fit::in_time(*t, self.latest));
        let next = time.zip(self.point.coords);
        if let (Some((before, from)), Some((after, to))) = (self.previous, next)
            && before < after
        {
            *self.activity.distance_m.get_or_insert(0.0) += from.metres_to(to);
        }
        self.previous = next;
        if let Some(time) = time {
            self.include_time(time);
            if let Some(hr) = self.point.hr {
                fit::archive_heart_rate(&mut self.out, time, hr, self.latest);
            }
        }
    }

    fn include_time(&mut self, time: DateTime<Utc>) {
        self.activity.start = Some(self.activity.start.map_or(time, |start| start.min(time)));
        self.activity.end = Some(self.activity.end.map_or(time, |end| end.max(time)));
    }

    fn end(&mut self, path: &[Tag]) {
        use Tag::*;
        match path {
            [Gpx, Track, Segment, Point] | [Tcx, Activities, Activity, Lap, TcxTrack, TcxPoint] => self.point(),
            [Tcx, Activities, Activity, Lap] => {
                if let Some(start) = self.lap.start {
                    self.include_time(start);
                    if let Some(seconds) = self.lap.duration {
                        let end = Duration::try_milliseconds((seconds * 1000.0).round() as i64)
                            .and_then(|duration| start.checked_add_signed(duration));
                        if let Some(end) = end {
                            self.include_time(end);
                        } else {
                            self.activity.bad_end = true;
                        }
                    }
                }
                if let Some(metres) = self.lap.metres {
                    *self.activity.distance_m.get_or_insert(0.0) += metres;
                }
                if let Some(kcal) = self.lap.kcal {
                    *self.activity.kcal.get_or_insert(0.0) += kcal;
                }
            }
            [Gpx, Track] | [Tcx, Activities, Activity] if self.out.sessions.len() < fit::MAX_ACTIVITIES => {
                self.out.sessions.push(std::mem::take(&mut self.activity));
            }
            _ => {}
        }
    }
}

fn xml_readings(format: Format, bytes: &[u8]) -> Option<(Vec<Sample>, bool)> {
    let root = if format == Format::Gpx { Tag::Gpx } else { Tag::Tcx };
    let mut xml = NsReader::from_reader(bytes);
    let mut state = XmlReadings {
        out: fit::Read::default(),
        latest: Utc::now() + Duration::days(1),
        activity: fit::Session::default(),
        lap: Lap::default(),
        point: Point::default(),
        previous: None,
    };
    let mut path = Vec::new();
    let mut seen_root = false;
    loop {
        let (namespace, event) = xml.read_resolved_event().ok()?;
        match event {
            Event::Start(ref node) | Event::Empty(ref node) => {
                let name = tag(format, namespace, node.local_name().as_ref());
                if path.is_empty() {
                    if seen_root || name != root {
                        return None;
                    }
                    seen_root = true;
                }
                if path.len() >= 64 {
                    return None;
                }
                path.push(name);
                state.start(&path, node);
                if matches!(event, Event::Empty(_)) {
                    state.end(&path);
                    path.pop();
                }
            }
            Event::End(_) => {
                state.end(&path);
                path.pop()?;
            }
            Event::Text(text) if !path.is_empty() && text.len() <= 64 => {
                state.text(&path, &text.xml10_content());
            }
            Event::DocType(_) => return None,
            Event::Eof => break,
            _ => {}
        }
    }
    if !seen_root || !path.is_empty() {
        return None;
    }
    Some((fit::samples(&state.out, state.latest).0, state.out.positions))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn gzip_output_spends_the_archive_budget_even_when_it_does_not_fit() -> anyhow::Result<()> {
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(b"one more reading")?;
        let bytes = encoder.finish()?;
        let mut budget = Budget::new(EXPORT.limit);
        assert!(budget.spend_expanded(budget.remaining_expanded() - 3));
        assert_eq!(unpack(bytes, &mut budget), Err(OVER_BUDGET));
        assert_eq!(budget.remaining_expanded(), 0);
        Ok(())
    }

    #[test]
    fn an_unrecognized_namespace_or_xml_outside_the_activity_brings_no_readings() {
        let source = br#"<gpx xmlns="http://www.topografix.com/GPX/1/1" xmlns:fake="urn:other">
          <trk><trkseg><trkpt lat="5" lon="6"><time>2024-01-08T08:00:01Z</time>
          <extensions><fake:TrackPointExtension><fake:hr>120</fake:hr></fake:TrackPointExtension></extensions>
          </trkpt></trkseg></trk></gpx>"#;
        let (samples, gps) = xml_readings(Format::Gpx, source).expect("well-formed GPX");
        assert!(samples.is_empty());
        assert!(gps);
        assert!(xml_readings(Format::Gpx, b"<!DOCTYPE gpx><gpx/>").is_none());
        assert!(xml_readings(Format::Gpx, b"<gpx><trk>").is_none());
    }

    fn gpx_distance(kind: &str) -> Vec<(&'static str, f64)> {
        let source = format!(
            r#"<gpx xmlns="http://www.topografix.com/GPX/1/1"><trk><type>{kind}</type><trkseg>
            <trkpt lat="40.0" lon="-3.0"><time>2024-01-08T08:00:00Z</time></trkpt>
            <trkpt lat="40.01" lon="-3.0"><time>2024-01-08T08:05:00Z</time></trkpt>
            </trkseg></trk></gpx>"#
        );
        let (samples, _) = xml_readings(Format::Gpx, source.as_bytes()).expect("well-formed GPX");
        samples.iter().map(|s| (s.metric, s.v)).collect()
    }

    #[test]
    fn strava_activity_type_numbers_in_a_gpx_name_the_sport() {
        // Strava's own GPX writes 9 for a run, 10 for a walk, 4 for a hike and 1 for a ride.
        for kind in ["running", "9", "10", "4"] {
            assert_eq!(gpx_distance(kind), [("distance", 1.112)], "type {kind}");
        }
        assert!(gpx_distance("1").is_empty(), "a ride brings no walking or running distance");
    }

    #[test]
    fn a_tcx_position_reports_a_route() {
        let tcx = |position: &str| {
            format!(
                r#"<TrainingCenterDatabase xmlns="http://www.garmin.com/xmlschemas/TrainingCenterDatabase/v2">
                <Activities><Activity Sport="Running"><Lap StartTime="2024-01-09T08:00:00Z">
                <Track><Trackpoint><Time>2024-01-09T08:00:15Z</Time>{position}
                <HeartRateBpm><Value>147</Value></HeartRateBpm></Trackpoint></Track>
                </Lap></Activity></Activities></TrainingCenterDatabase>"#
            )
        };
        let gps = |source: String| xml_readings(Format::Tcx, source.as_bytes()).expect("well-formed TCX").1;
        assert!(gps(tcx(
            "<Position><LatitudeDegrees>40.1</LatitudeDegrees><LongitudeDegrees>-3.1</LongitudeDegrees></Position>"
        )));
        assert!(!gps(tcx("")));
    }

    #[test]
    fn the_filename_column_is_the_one_whose_paths_are_in_the_archive() -> anyhow::Result<()> {
        // A free-text column holds more activity paths than the Filename column, but none of them
        // is in the archive.
        let files = BTreeMap::from([("activities/1.gpx".to_owned(), 1), ("activities/2.gpx".to_owned(), 2)]);
        let csv = b"Description,Filename\n\
            activities/a.gpx,activities/1.gpx\n\
            activities/b.gpx,activities/2.gpx\n\
            activities/c.gpx,\n";
        let counts = join_manifest(csv, &files)?;
        assert_eq!((counts.rows_without_file, counts.files_without_row), (1, 0));
        Ok(())
    }
}
