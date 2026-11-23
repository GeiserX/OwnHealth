//! A small reader of DICOM PS3.10 files: the few tags the importer needs before it sends a file to
//! Orthanc, and the PDF inside an Encapsulated PDF instance. Orthanc parses each file in full; this
//! reader only has to name the patient and the study, so the import can hold the right lock and
//! route a file to review before Orthanc ever sees it.
//!
//! A file is a 128-byte preamble, `DICM`, the file meta group (explicit VR little endian), then the
//! data set in the meta group's transfer syntax: implicit VR little endian, explicit VR little or big
//! endian, or deflated explicit VR little endian. Every other transfer syntax (JPEG and the like)
//! encodes only the pixel data differently, and its data set is explicit VR little endian. The reader
//! walks the top level of the data set and skips sequences, items and encapsulated pixel data by
//! their lengths and delimiters. Anything malformed is [`Part10Error::Corrupt`], never a panic.

use std::io::Read;

/// The DICOM magic after the preamble.
pub const MAGIC_AT: usize = 128;

/// SOP Class UIDs the importer treats apart.
pub const MEDIA_STORAGE_DIRECTORY: &str = "1.2.840.10008.1.3.10";
pub const ENCAPSULATED_PDF: &str = "1.2.840.10008.5.1.4.1.1.104.1";

const IMPLICIT_LE: &str = "1.2.840.10008.1.2";
const EXPLICIT_BE: &str = "1.2.840.10008.1.2.2";
const DEFLATED_LE: &str = "1.2.840.10008.1.2.1.99";

/// How far a deflated data set is inflated to read a header: the importer's tags sit in its first
/// elements, so a set that holds none of them in this much is damaged. A few hundred bytes of input
/// can inflate to gigabytes; the cap keeps that out of memory.
pub const MAX_INFLATED_HEADER: u64 = 16 << 20;
/// How far a deflated data set is inflated to find an encapsulated document (a PDF report).
pub const MAX_INFLATED_DOCUMENT: u64 = 128 << 20;
/// Sequences nest at most this deep.
const MAX_DEPTH: usize = 32;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Part10Error {
    /// No `DICM` after a 128-byte preamble: not a PS3.10 file (a viewer, autorun.inf, a README).
    NotDicom,
    /// It says it is DICOM but does not read as such.
    Corrupt(String),
}

impl std::fmt::Display for Part10Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Part10Error::NotDicom => f.write_str("not a DICOM file"),
            Part10Error::Corrupt(why) => write!(f, "a damaged DICOM file: {why}"),
        }
    }
}

impl std::error::Error for Part10Error {}

fn corrupt(why: impl Into<String>) -> Part10Error {
    Part10Error::Corrupt(why.into())
}

/// Whether `head` starts a PS3.10 file.
pub fn is_part10(head: &[u8]) -> bool {
    head.len() >= MAGIC_AT + 4 && &head[MAGIC_AT..MAGIC_AT + 4] == b"DICM"
}

/// What the importer reads from one file. Text values are trimmed, and a multi-valued one keeps its
/// first value.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Header {
    pub transfer_syntax: String,
    pub sop_class_uid: String,
    pub sop_instance_uid: String,
    pub study_instance_uid: String,
    pub series_instance_uid: String,
    pub patient_id: String,
    /// As written, `^` between the name parts.
    pub patient_name: String,
    /// `YYYYMMDD`, as written.
    pub patient_birth_date: String,
    pub modality: String,
    /// `YYYYMMDD`, as written.
    pub study_date: String,
    pub study_description: String,
}

impl Header {
    pub fn is_directory(&self) -> bool {
        self.sop_class_uid == MEDIA_STORAGE_DIRECTORY
    }

    pub fn is_pdf(&self) -> bool {
        self.sop_class_uid == ENCAPSULATED_PDF
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Syntax {
    ExplicitLe,
    ExplicitBe,
    ImplicitLe,
}

/// One element: its tag, VR (two spaces for implicit VR), and where its value is in the buffer
/// (None: undefined length, a sequence or encapsulated data the walker skipped).
struct Element {
    tag: u32,
    value: Option<std::ops::Range<usize>>,
}

struct Walker<'a> {
    buf: &'a [u8],
    pos: usize,
    syntax: Syntax,
}

const ITEM: u32 = 0xFFFE_E000;
const ITEM_END: u32 = 0xFFFE_E00D;
const SEQUENCE_END: u32 = 0xFFFE_E0DD;
const UNDEFINED: u32 = 0xFFFF_FFFF;

/// VRs with a 2-byte reserved field and a 4-byte length in explicit VR.
fn long_vr(vr: &[u8]) -> bool {
    matches!(
        vr,
        b"OB" | b"OD" | b"OF" | b"OL" | b"OV" | b"OW" | b"SQ" | b"UC" | b"UN" | b"UR" | b"UT" | b"SV" | b"UV"
    )
}

impl<'a> Walker<'a> {
    fn u16_at(&self, at: usize) -> Result<u16, Part10Error> {
        let b = self
            .buf
            .get(at..at + 2)
            .ok_or_else(|| corrupt("the file ends inside an element"))?;
        Ok(match self.syntax {
            Syntax::ExplicitBe => u16::from_be_bytes([b[0], b[1]]),
            _ => u16::from_le_bytes([b[0], b[1]]),
        })
    }

    fn u32_at(&self, at: usize) -> Result<u32, Part10Error> {
        let b = self
            .buf
            .get(at..at + 4)
            .ok_or_else(|| corrupt("the file ends inside an element"))?;
        Ok(match self.syntax {
            Syntax::ExplicitBe => u32::from_be_bytes([b[0], b[1], b[2], b[3]]),
            _ => u32::from_le_bytes([b[0], b[1], b[2], b[3]]),
        })
    }

    fn tag_at(&self, at: usize) -> Result<u32, Part10Error> {
        Ok((u32::from(self.u16_at(at)?) << 16) | u32::from(self.u16_at(at + 2)?))
    }

    fn done(&self) -> bool {
        self.pos >= self.buf.len()
    }

    /// The next element of the current level, its value skipped over.
    fn next(&mut self, depth: usize) -> Result<Element, Part10Error> {
        let tag = self.tag_at(self.pos)?;
        if matches!(tag, ITEM | ITEM_END | SEQUENCE_END) {
            // a delimiter (no VR in any syntax)
            let len = self.u32_at(self.pos + 4)?;
            self.pos += 8;
            return self.with_value(tag, len, false, depth);
        }
        let (len, sequence) = match self.syntax {
            Syntax::ImplicitLe => {
                let len = self.u32_at(self.pos + 4)?;
                self.pos += 8;
                // implicit VR: an undefined length is a sequence (or UN read as one)
                (len, len == UNDEFINED)
            }
            _ => {
                let vr = self
                    .buf
                    .get(self.pos + 4..self.pos + 6)
                    .ok_or_else(|| corrupt("the file ends inside an element"))?;
                if !vr.iter().all(u8::is_ascii_uppercase) {
                    return Err(corrupt(format!(
                        "no VR at element ({:04X},{:04X})",
                        tag >> 16,
                        tag & 0xFFFF
                    )));
                }
                if long_vr(vr) {
                    let sq = vr == b"SQ";
                    let len = self.u32_at(self.pos + 8)?;
                    self.pos += 12;
                    if vr == b"UN" && len == UNDEFINED {
                        let enclosing = std::mem::replace(&mut self.syntax, Syntax::ImplicitLe);
                        let element = self.with_value(tag, len, true, depth);
                        self.syntax = enclosing;
                        return element;
                    }
                    // an undefined-length OB, OW or UN is encapsulated pixel data or a UN sequence:
                    // both are items up to a sequence delimiter
                    (len, sq || len == UNDEFINED)
                } else {
                    let len = u32::from(self.u16_at(self.pos + 6)?);
                    self.pos += 8;
                    (len, false)
                }
            }
        };
        self.with_value(tag, len, sequence, depth)
    }

    fn with_value(&mut self, tag: u32, len: u32, sequence: bool, depth: usize) -> Result<Element, Part10Error> {
        if len == UNDEFINED {
            if tag == ITEM || sequence {
                self.skip_until(if tag == ITEM { ITEM_END } else { SEQUENCE_END }, depth + 1)?;
                return Ok(Element { tag, value: None });
            }
            return Err(corrupt(format!(
                "undefined length on element ({:04X},{:04X})",
                tag >> 16,
                tag & 0xFFFF
            )));
        }
        let start = self.pos;
        let end = start
            .checked_add(len as usize)
            .filter(|e| *e <= self.buf.len())
            .ok_or_else(|| corrupt("an element runs past the end of the file"))?;
        self.pos = end;
        Ok(Element {
            tag,
            value: Some(start..end),
        })
    }

    /// Skip elements and items until `end` (an item or sequence delimiter).
    fn skip_until(&mut self, end: u32, depth: usize) -> Result<(), Part10Error> {
        if depth > MAX_DEPTH {
            return Err(corrupt("sequences nest too deep"));
        }
        loop {
            if self.done() {
                return Err(corrupt("a sequence has no end"));
            }
            let e = self.next(depth)?;
            if e.tag == end {
                return Ok(());
            }
        }
    }
}

/// The text of a string value: up to the first backslash (the first value), without padding. UTF-8
/// when it reads as UTF-8, else ISO 8859-1 (the default repertoire's superset that CDs use most).
fn text(raw: &[u8]) -> String {
    let first = raw.split(|b| *b == b'\\').next().unwrap_or_default();
    let s = match std::str::from_utf8(first) {
        Ok(s) => s.to_string(),
        Err(_) => first.iter().map(|b| char::from(*b)).collect(),
    };
    s.trim_matches(|c: char| c == ' ' || c == '\0').to_string()
}

/// The meta group: (transfer syntax, media storage SOP class, its instance UID, where the data set
/// starts).
fn meta(buf: &[u8]) -> Result<(String, String, String, usize), Part10Error> {
    let mut w = Walker {
        buf,
        pos: MAGIC_AT + 4,
        syntax: Syntax::ExplicitLe,
    };
    let declared_end = if w.tag_at(w.pos)? == 0x0002_0000 {
        let e = w.next(0)?;
        let value = e
            .value
            .and_then(|r| buf[r].try_into().ok())
            .ok_or_else(|| corrupt("invalid file meta group length"))?;
        let end = w
            .pos
            .checked_add(u32::from_le_bytes(value) as usize)
            .filter(|end| *end <= buf.len())
            .ok_or_else(|| corrupt("the file meta group runs past the end of the file"))?;
        w.buf = &buf[..end];
        Some(end)
    } else {
        None
    };
    let (mut ts, mut class, mut instance) = (String::new(), String::new(), String::new());
    while !w.done() {
        if w.tag_at(w.pos)? >> 16 != 0x0002 {
            if declared_end.is_some() {
                return Err(corrupt("the file meta group length includes the data set"));
            }
            break;
        }
        let e = w.next(0)?;
        let value = e.value.map(|r| text(&buf[r])).unwrap_or_default();
        match e.tag {
            0x0002_0002 => class = value,
            0x0002_0003 => instance = value,
            0x0002_0010 => ts = value,
            _ => {}
        }
    }
    if ts.is_empty() {
        return Err(corrupt("the file meta group names no transfer syntax"));
    }
    Ok((ts, class, instance, w.pos))
}

/// The data set's bytes and syntax: inflated, as far as `cap` bytes, when the transfer syntax is
/// deflated. A set cut at the cap reads as far as it goes: the walk stops before the cut when what it
/// looks for comes first, and finds the set damaged otherwise.
fn data_set<'a>(
    buf: &'a [u8],
    ts: &str,
    start: usize,
    cap: u64,
    inflated: &'a mut Vec<u8>,
) -> Result<(&'a [u8], Syntax), Part10Error> {
    let syntax = match ts {
        IMPLICIT_LE => Syntax::ImplicitLe,
        EXPLICIT_BE => Syntax::ExplicitBe,
        _ => Syntax::ExplicitLe,
    };
    if ts == DEFLATED_LE {
        flate2::read::DeflateDecoder::new(&buf[start..])
            .take(cap)
            .read_to_end(inflated)
            .map_err(|e| corrupt(format!("the deflated data set does not inflate: {e}")))?;
        return Ok((inflated.as_slice(), syntax));
    }
    Ok((&buf[start..], syntax))
}

/// Calls `each(tag, value)` for every top-level element of the data set up to tag `last`, until it
/// returns false. The walk looks at the next tag before it reads the element, so an element past
/// `last` (pixel data, or a set cut at the inflate cap) is never read.
fn walk(buf: &[u8], last: u32, cap: u64, mut each: impl FnMut(u32, &[u8]) -> bool) -> Result<String, Part10Error> {
    if !is_part10(buf) {
        return Err(Part10Error::NotDicom);
    }
    let (ts, _, _, start) = meta(buf)?;
    let mut inflated = Vec::new();
    let (set, syntax) = data_set(buf, &ts, start, cap, &mut inflated)?;
    let mut w = Walker {
        buf: set,
        pos: 0,
        syntax,
    };
    while !w.done() {
        let tag = w.tag_at(w.pos)?;
        if tag > last && !matches!(tag, ITEM | ITEM_END | SEQUENCE_END) {
            break;
        }
        let e = w.next(0)?;
        if matches!(e.tag, ITEM | ITEM_END | SEQUENCE_END) {
            return Err(corrupt("a delimiter outside a sequence"));
        }
        let value = e.value.map(|r| &set[r]).unwrap_or_default();
        if !each(e.tag, value) {
            break;
        }
    }
    Ok(ts)
}

/// The header of one PS3.10 file. It reads only as far as the study's tags (group 0020) unless the
/// file is an Encapsulated PDF, so a large image costs one pass over its first elements.
pub fn read_header(buf: &[u8]) -> Result<Header, Part10Error> {
    if !is_part10(buf) {
        return Err(Part10Error::NotDicom);
    }
    let (_, class, instance, _) = meta(buf)?;
    let mut h = Header {
        sop_class_uid: class,
        sop_instance_uid: instance,
        ..Header::default()
    };
    h.transfer_syntax = walk(buf, 0x0020_000E, MAX_INFLATED_HEADER, |tag, value| {
        let v = || text(value);
        match tag {
            0x0008_0016 => h.sop_class_uid = v(),
            0x0008_0018 => h.sop_instance_uid = v(),
            0x0008_0020 => h.study_date = v(),
            0x0008_0060 => h.modality = v(),
            0x0008_1030 => h.study_description = v(),
            0x0010_0010 => h.patient_name = v(),
            0x0010_0020 => h.patient_id = v(),
            0x0010_0030 => h.patient_birth_date = v(),
            0x0020_000D => h.study_instance_uid = v(),
            0x0020_000E => h.series_instance_uid = v(),
            _ => {}
        }
        // everything the importer needs sits at or before (0020,000E)
        true
    })?;
    if h.is_directory() {
        return Ok(h);
    }
    if !is_uid(&h.study_instance_uid) {
        return Err(corrupt("no valid StudyInstanceUID"));
    }
    if !is_uid(&h.series_instance_uid) || !is_uid(&h.sop_instance_uid) {
        return Err(corrupt("no valid SeriesInstanceUID or SOPInstanceUID"));
    }
    Ok(h)
}

/// The document inside an Encapsulated PDF instance (0042,0011), without the padding byte DICOM may
/// add to make its length even. None when the file holds no such element.
pub fn encapsulated_document(buf: &[u8]) -> Result<Option<Vec<u8>>, Part10Error> {
    let mut doc = None;
    walk(buf, 0x0042_0011, MAX_INFLATED_DOCUMENT, |tag, value| {
        if tag == 0x0042_0011 {
            doc = Some(value.to_vec());
            return false;
        }
        true
    })?;
    Ok(doc.map(|mut d| {
        // a PDF ends at its last %%EOF; DICOM pads an odd length with one NUL
        if d.len() % 2 == 0 && d.last() == Some(&0) {
            d.pop();
        }
        d
    }))
}

/// A DICOM UID: digits in dot-separated parts, at most 64 characters.
pub fn is_uid(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.split('.')
            .all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A file the hard way, to test the reader on what the test generator (dicom-object) does not
    /// write: big endian, a deflated set, and an undefined-length sequence before the patient.
    fn file(ts: &str, set: &[u8]) -> Vec<u8> {
        let mut out = vec![0u8; 128];
        out.extend_from_slice(b"DICM");
        let mut ts_value = ts.as_bytes().to_vec();
        if ts_value.len() % 2 == 1 {
            ts_value.push(0);
        }
        out.extend_from_slice(&[0x02, 0x00, 0x10, 0x00]);
        out.extend_from_slice(b"UI");
        out.extend_from_slice(&u16::try_from(ts_value.len()).unwrap_or(0).to_le_bytes());
        out.extend_from_slice(&ts_value);
        out.extend_from_slice(set);
        out
    }

    fn explicit(be: bool, tag: u32, vr: &[u8; 2], value: &[u8]) -> Vec<u8> {
        let u16b = |v: u16| if be { v.to_be_bytes() } else { v.to_le_bytes() };
        let u32b = |v: u32| if be { v.to_be_bytes() } else { v.to_le_bytes() };
        let mut v = value.to_vec();
        if v.len() % 2 == 1 {
            v.push(b' ');
        }
        let mut out = Vec::new();
        out.extend_from_slice(&u16b((tag >> 16) as u16));
        out.extend_from_slice(&u16b((tag & 0xFFFF) as u16));
        out.extend_from_slice(vr);
        if long_vr(vr) {
            out.extend_from_slice(&[0, 0]);
            out.extend_from_slice(&u32b(u32::try_from(v.len()).unwrap_or(0)));
        } else {
            out.extend_from_slice(&u16b(u16::try_from(v.len()).unwrap_or(0)));
        }
        out.extend_from_slice(&v);
        out
    }

    fn core(be: bool) -> Vec<u8> {
        let mut s = Vec::new();
        for (tag, vr, value) in [
            (0x0008_0016u32, b"UI", "1.2.840.10008.5.1.4.1.1.2"),
            (0x0008_0018, b"UI", "1.2.826.0.1.3680043.10.999.7.1.1"),
            (0x0008_0020, b"DA", "20260914"),
            (0x0008_0060, b"CS", "CT"),
        ] {
            s.extend(explicit(be, tag, vr, value.as_bytes()));
        }
        // (0008,1110) Referenced Study Sequence, undefined length, one undefined-length item
        let u16b = |v: u16| if be { v.to_be_bytes() } else { v.to_le_bytes() };
        let u32b = |v: u32| if be { v.to_be_bytes() } else { v.to_le_bytes() };
        s.extend_from_slice(&u16b(0x0008));
        s.extend_from_slice(&u16b(0x1110));
        s.extend_from_slice(b"SQ\0\0");
        s.extend_from_slice(&u32b(UNDEFINED));
        s.extend_from_slice(&u16b(0xFFFE));
        s.extend_from_slice(&u16b(0xE000));
        s.extend_from_slice(&u32b(UNDEFINED));
        s.extend(explicit(be, 0x0008_1155, b"UI", b"1.2.3"));
        s.extend_from_slice(&u16b(0xFFFE));
        s.extend_from_slice(&u16b(0xE00D));
        s.extend_from_slice(&u32b(0));
        s.extend_from_slice(&u16b(0xFFFE));
        s.extend_from_slice(&u16b(0xE0DD));
        s.extend_from_slice(&u32b(0));
        for (tag, vr, value) in [
            (0x0010_0010u32, b"PN", "RIVERA^ALEX"),
            (0x0010_0020, b"LO", "LN-0001"),
            (0x0010_0030, b"DA", "19790514"),
            (0x0020_000D, b"UI", "1.2.826.0.1.3680043.10.999.7"),
            (0x0020_000E, b"UI", "1.2.826.0.1.3680043.10.999.7.1"),
        ] {
            s.extend(explicit(be, tag, vr, value.as_bytes()));
        }
        s
    }

    #[test]
    fn it_reads_big_endian_and_skips_an_undefined_length_sequence() {
        let h = read_header(&file(EXPLICIT_BE, &core(true))).expect("header");
        assert_eq!(h.patient_name, "RIVERA^ALEX");
        assert_eq!(h.patient_id, "LN-0001");
        assert_eq!(h.patient_birth_date, "19790514");
        assert_eq!(h.study_instance_uid, "1.2.826.0.1.3680043.10.999.7");
        assert_eq!(h.series_instance_uid, "1.2.826.0.1.3680043.10.999.7.1");
        assert_eq!(h.modality, "CT");
        assert_eq!(h.study_date, "20260914");
    }

    #[test]
    fn it_reads_a_deflated_data_set() {
        let mut deflated = Vec::new();
        {
            use std::io::Write;
            let mut z = flate2::write::DeflateEncoder::new(&mut deflated, flate2::Compression::default());
            z.write_all(&core(false)).expect("deflate");
            z.finish().expect("deflate");
        }
        let h = read_header(&file(DEFLATED_LE, &deflated)).expect("header");
        assert_eq!(h.study_instance_uid, "1.2.826.0.1.3680043.10.999.7");
        assert_eq!(h.patient_id, "LN-0001");
    }

    fn deflate(set: &[u8]) -> Vec<u8> {
        use std::io::Write;
        let mut out = Vec::new();
        let mut z = flate2::write::DeflateEncoder::new(&mut out, flate2::Compression::default());
        z.write_all(set).expect("deflate");
        z.finish().expect("deflate");
        out
    }

    /// A private element of `n` zero bytes in group 0009, which sorts before the patient and study.
    fn private_zeros(n: usize) -> Vec<u8> {
        let mut s = explicit(false, 0x0009_0010, b"LO", b"SYNTHETIC");
        s.extend(explicit(false, 0x0009_1010, b"OB", &vec![0u8; n]));
        s
    }

    #[test]
    fn a_deflated_set_is_inflated_only_as_far_as_the_header_needs() {
        // a header behind 20 MiB of zeros (about 20 KiB deflated) is not read: damaged
        let mut bomb = core(false);
        let patient = [0x10, 0x00, 0x10, 0x00, b'P', b'N'];
        let at = bomb.windows(6).position(|w| w == patient).expect("patient name");
        let rest = bomb.split_off(at);
        bomb.extend(private_zeros(20 << 20));
        bomb.extend(rest);
        let deflated = deflate(&bomb);
        assert!(deflated.len() < 1 << 20, "{}", deflated.len());
        assert!(
            matches!(read_header(&file(DEFLATED_LE, &deflated)), Err(Part10Error::Corrupt(_))),
            "a header past the inflate cap is damaged"
        );
        // the header first and 40 MiB of pixel data after it: the header reads, the rest is not
        // inflated past the cap
        let mut image = core(false);
        image.extend(explicit(false, 0x7FE0_0010, b"OB", &vec![0u8; 40 << 20]));
        let h = read_header(&file(DEFLATED_LE, &deflate(&image))).expect("header");
        assert_eq!(h.series_instance_uid, "1.2.826.0.1.3680043.10.999.7.1");
    }

    fn before_patient(set: &mut Vec<u8>, element: &[u8]) {
        let patient = [0x10, 0, 0x10, 0, b'P', b'N'];
        let at = set.windows(6).position(|w| w == patient).expect("patient name");
        set.splice(at..at, element.iter().copied());
    }

    #[test]
    fn an_undefined_un_contains_implicit_vr_and_restores_explicit_vr() {
        let mut set = core(false);
        let un = [
            9, 0, 0x10, 0x10, b'U', b'N', 0, 0, 255, 255, 255, 255, 0xfe, 0xff, 0, 0xe0, 255, 255, 255, 255, 8, 0,
            0x55, 0x11, 6, 0, 0, 0, b'1', b'.', b'2', b'.', b'3', 0, 0xfe, 0xff, 0x0d, 0xe0, 0, 0, 0, 0, 0xfe, 0xff,
            0xdd, 0xe0, 0, 0, 0, 0,
        ];
        before_patient(&mut set, &un);
        let h = read_header(&file("1.2.840.10008.1.2.1", &set)).expect("header");
        assert_eq!(h.patient_name, "RIVERA^ALEX");
    }

    #[test]
    fn the_meta_group_length_delimits_a_deflated_set_starting_with_0002() {
        let mut deflated = vec![2, 0, 0, 0, 255, 255];
        deflated.extend(deflate(&core(false)));
        let mut dicom = file(DEFLATED_LE, &deflated);
        let start = MAGIC_AT + 4;
        let length = u32::try_from(dicom.len() - start - deflated.len()).expect("meta length");
        dicom.splice(start..start, explicit(false, 0x0002_0000, b"UL", &length.to_le_bytes()));
        let h = read_header(&dicom).expect("header");
        assert_eq!(h.patient_id, "LN-0001");
    }

    #[test]
    fn damaged_files_are_corrupt_and_others_are_not_dicom() {
        let good = file("1.2.840.10008.1.2.1", &core(false));
        assert!(read_header(&good).is_ok());
        assert_eq!(read_header(b"MZ\x90\0 viewer.exe"), Err(Part10Error::NotDicom));
        assert_eq!(
            read_header(b"[autorun]\r\nopen=viewer.exe\r\n"),
            Err(Part10Error::NotDicom)
        );
        // every cut of the file before its last tag is damaged, never a panic or a good header
        let last = good.len() - "1.2.826.0.1.3680043.10.999.7.1".len() - 8;
        for cut in MAGIC_AT + 4..last {
            assert!(
                matches!(read_header(&good[..cut]), Err(Part10Error::Corrupt(_))),
                "cut at {cut}"
            );
        }
        // a length that runs past the end
        let mut bad = good.clone();
        let at = MAGIC_AT + 4 + 8 + 20 + 6; // the first data set element's 2-byte length
        bad[at] = 0xFF;
        bad[at + 1] = 0x7F;
        assert!(matches!(read_header(&bad), Err(Part10Error::Corrupt(_))));
    }

    #[test]
    fn text_takes_the_first_value_and_reads_latin_1() {
        assert_eq!(text(b"CT\\PT "), "CT");
        assert_eq!(text(b"ORTEGA^CARMEN\0"), "ORTEGA^CARMEN");
        assert_eq!(text(b"MU\xd1OZ^LUC\xcdA"), "MUÑOZ^LUCÍA");
        assert_eq!(text("MUÑOZ".as_bytes()), "MUÑOZ");
    }
}
