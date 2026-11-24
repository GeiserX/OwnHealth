//! A small reader of DICOM PS3.10 files: the few tags the importer needs before it sends a file to
//! Orthanc, and the PDF inside an Encapsulated PDF instance. Orthanc parses each file in full; this
//! reader only has to name the patient and the study, so the import can hold the right lock and
//! route a file to review before Orthanc ever sees it.
//!
//! A file is a 128-byte preamble, `DICM`, the file meta group (explicit VR little endian), then the
//! data set in the meta group's transfer syntax. It reads implicit VR little endian and explicit VR
//! little or big endian. Pixel compression (such as JPEG) leaves the data set explicit VR little
//! endian. The reader refuses deflated data sets before inflation. The reader
//! walks the top level of the data set and skips sequences, items and encapsulated pixel data by
//! their lengths and delimiters. Anything malformed is [`Part10Error::Corrupt`], never a panic.

use std::fs::File;
use std::io::{BufReader, Cursor, Read, Seek, SeekFrom, Take};

use dicom_encoding::text::{SpecificCharacterSet, TextCodec};

/// The DICOM magic after the preamble.
pub const MAGIC_AT: usize = 128;

/// SOP Class UIDs the importer treats apart.
pub const MEDIA_STORAGE_DIRECTORY: &str = "1.2.840.10008.1.3.10";
pub const ENCAPSULATED_PDF: &str = "1.2.840.10008.5.1.4.1.1.104.1";

const IMPLICIT_LE: &str = "1.2.840.10008.1.2";
const EXPLICIT_BE: &str = "1.2.840.10008.1.2.2";
const DEFLATED_LE: &str = "1.2.840.10008.1.2.1.99";

/// Sequences nest at most this deep.
const MAX_DEPTH: usize = 32;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Part10Error {
    /// No `DICM` after a 128-byte preamble: not a PS3.10 file (a viewer, autorun.inf, a README).
    NotDicom,
    /// It says it is DICOM but does not read as such.
    Corrupt(String),
    /// The file meta group declares a deflated data set.
    Deflated,
}

impl std::fmt::Display for Part10Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Part10Error::NotDicom => f.write_str("not a DICOM file"),
            Part10Error::Corrupt(why) => write!(f, "a damaged DICOM file: {why}"),
            Part10Error::Deflated => f.write_str("deflated DICOM transfer syntax is unsupported"),
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
    pub patient_id_issuer: String,
    /// Only present when PatientID could not be decoded. One character per original byte, so
    /// distinct unreadable IDs can still be distinguished.
    pub undecodable_patient_id: Option<String>,
    /// At least one nonempty patient name, ID or issuer could not be decoded.
    pub unsupported_charset: bool,
    /// As written, `^` between the name parts.
    pub patient_name: String,
    /// `YYYYMMDD`, as written.
    pub patient_birth_date: String,
    pub modality: String,
    /// `YYYYMMDD`, as written.
    pub study_date: String,
    /// In the declared repertoire, else as [`text`] reads it: it is not identity, so it never holds a
    /// study.
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

struct Walker<R> {
    input: R,
    pos: usize,
    end: usize,
    syntax: Syntax,
}

const ITEM: u32 = 0xFFFE_E000;
const ITEM_END: u32 = 0xFFFE_E00D;
const SEQUENCE_END: u32 = 0xFFFE_E0DD;
const UNDEFINED: u32 = 0xFFFF_FFFF;

/// PS3.5 §7.1.2 reserves the short layout for these VRs.
fn long_vr(vr: &[u8]) -> bool {
    !matches!(
        vr,
        b"AE"
            | b"AS"
            | b"AT"
            | b"CS"
            | b"DA"
            | b"DS"
            | b"DT"
            | b"FL"
            | b"FD"
            | b"IS"
            | b"LO"
            | b"LT"
            | b"PN"
            | b"SH"
            | b"SL"
            | b"SS"
            | b"ST"
            | b"TM"
            | b"UI"
            | b"UL"
            | b"US"
    )
}

impl<R: Read + Seek> Walker<R> {
    fn bytes_at(&mut self, at: usize, out: &mut [u8]) -> Result<(), Part10Error> {
        if at.checked_add(out.len()).is_none_or(|end| end > self.end) {
            return Err(corrupt("the file ends inside an element"));
        }
        self.input
            .seek(SeekFrom::Start(at as u64))
            .and_then(|_| self.input.read_exact(out))
            .map_err(|e| corrupt(format!("reading an element: {e}")))
    }

    fn u16_at(&mut self, at: usize) -> Result<u16, Part10Error> {
        let mut b = [0; 2];
        self.bytes_at(at, &mut b)?;
        Ok(match self.syntax {
            Syntax::ExplicitBe => u16::from_be_bytes(b),
            _ => u16::from_le_bytes(b),
        })
    }

    fn u32_at(&mut self, at: usize) -> Result<u32, Part10Error> {
        let mut b = [0; 4];
        self.bytes_at(at, &mut b)?;
        Ok(match self.syntax {
            Syntax::ExplicitBe => u32::from_be_bytes(b),
            _ => u32::from_le_bytes(b),
        })
    }

    fn tag_at(&mut self, at: usize) -> Result<u32, Part10Error> {
        Ok((u32::from(self.u16_at(at)?) << 16) | u32::from(self.u16_at(at + 2)?))
    }

    fn done(&self) -> bool {
        self.pos >= self.end
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
                let mut vr = [0; 2];
                self.bytes_at(self.pos + 4, &mut vr)?;
                if !vr.iter().all(u8::is_ascii_uppercase) {
                    return Err(corrupt(format!(
                        "no VR at element ({:04X},{:04X})",
                        tag >> 16,
                        tag & 0xFFFF
                    )));
                }
                if long_vr(&vr) {
                    let sq = &vr == b"SQ";
                    let len = self.u32_at(self.pos + 8)?;
                    self.pos += 12;
                    if &vr == b"UN" && len == UNDEFINED {
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
            .filter(|e| *e <= self.end)
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

/// The repertoire a Specific Character Set (0008,0005) value declares, when the reader decodes it:
/// the default, or a single-byte set named in its plain or its ISO 2022 form. Under ISO 2022 only the
/// first value applies until a value switches sets with ESC, and the reader decodes no value that does,
/// so the other values are left out. Multi-byte sets (ISO 2022 IR 87, 149, 58, 159) are not read.
fn repertoire(declared: &str) -> Option<SpecificCharacterSet> {
    let first = declared
        .split('\\')
        .next()
        .unwrap_or_default()
        .trim_matches([' ', '\0']);
    match first {
        "" | "ISO 2022 IR 6" => Some(SpecificCharacterSet::ISO_IR_6),
        code => match code.strip_prefix("ISO 2022 IR ") {
            Some(n @ ("13" | "100" | "101" | "109" | "110" | "126" | "127" | "138" | "144" | "148" | "166")) => {
                SpecificCharacterSet::from_code(&format!("ISO_IR {n}"))
            }
            Some(_) => None,
            None => SpecificCharacterSet::from_code(code),
        },
    }
}

/// The meta group: (transfer syntax, media storage SOP class, its instance UID, where the data set
/// starts).
fn meta(input: impl Read + Seek, end: usize) -> Result<(String, String, String, usize), Part10Error> {
    let mut w = Walker {
        input,
        pos: MAGIC_AT + 4,
        end,
        syntax: Syntax::ExplicitLe,
    };
    let declared_end = if w.tag_at(w.pos)? == 0x0002_0000 {
        let e = w.next(0)?;
        let value = e
            .value
            .filter(|r| r.len() == 4)
            .ok_or_else(|| corrupt("invalid file meta group length"))?;
        let len = w.u32_at(value.start)? as usize;
        let end = w
            .pos
            .checked_add(len)
            .filter(|end| *end <= w.end)
            .ok_or_else(|| corrupt("the file meta group runs past the end of the file"))?;
        w.end = end;
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
        if !matches!(e.tag, 0x0002_0002 | 0x0002_0003 | 0x0002_0010) {
            continue;
        }
        let r = e
            .value
            .filter(|r| r.len() <= 64)
            .ok_or_else(|| corrupt("invalid file meta UID"))?;
        let mut value = [0; 64];
        w.bytes_at(r.start, &mut value[..r.len()])?;
        let value = text(&value[..r.len()]);
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

fn syntax_for(ts: &str) -> Result<Syntax, Part10Error> {
    match ts {
        IMPLICIT_LE => Ok(Syntax::ImplicitLe),
        EXPLICIT_BE => Ok(Syntax::ExplicitBe),
        DEFLATED_LE => Err(Part10Error::Deflated),
        _ => Ok(Syntax::ExplicitLe),
    }
}

/// Calls `each(tag, value)` for each top-level element up to tag `last`, until it returns false.
fn walk(set: &[u8], syntax: Syntax, last: u32, mut each: impl FnMut(u32, &[u8]) -> bool) -> Result<(), Part10Error> {
    let mut w = Walker {
        input: Cursor::new(set),
        pos: 0,
        end: set.len(),
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
    Ok(())
}

/// The header of one PS3.10 file. It reads only as far as the study's tags (group 0020) unless the
/// file is an Encapsulated PDF, so a large image costs one pass over its first elements.
pub fn read_header(buf: &[u8]) -> Result<Header, Part10Error> {
    if !is_part10(buf) {
        return Err(Part10Error::NotDicom);
    }
    let (transfer_syntax, class, instance, start) = meta(Cursor::new(buf), buf.len())?;
    let syntax = syntax_for(&transfer_syntax)?;
    let mut h = Header {
        transfer_syntax,
        sop_class_uid: class,
        sop_instance_uid: instance,
        ..Header::default()
    };
    let mut charset = Some(SpecificCharacterSet::ISO_IR_6);
    walk(&buf[start..], syntax, 0x0020_000E, |tag, value| {
        let v = || text(value);
        let decode = |charset: &Option<SpecificCharacterSet>| {
            let charset = charset.as_ref()?;
            if value.contains(&0x1b) || (*charset == SpecificCharacterSet::ISO_IR_6 && !value.is_ascii()) {
                return None;
            }
            let decoded = charset.decode(value).ok()?;
            // These four tags are single-valued. The codec also escapes invalid bytes with a backslash.
            (!decoded.contains('\\')).then(|| decoded.trim_matches([' ', '\0']).to_string())
        };
        match tag {
            0x0008_0005 => {
                charset = std::str::from_utf8(value).ok().and_then(repertoire);
            }
            0x0008_0016 => h.sop_class_uid = v(),
            0x0008_0018 => h.sop_instance_uid = v(),
            0x0008_0020 => h.study_date = v(),
            0x0008_0060 => h.modality = v(),
            0x0008_1030 => h.study_description = decode(&charset).unwrap_or_else(v),
            0x0010_0010 | 0x0010_0020 | 0x0010_0021 => {
                if value.iter().all(|b| matches!(b, b' ' | 0)) {
                    return true;
                }
                if let Some(text) = decode(&charset) {
                    match tag {
                        0x0010_0010 => h.patient_name = text,
                        0x0010_0020 => h.patient_id = text,
                        _ => h.patient_id_issuer = text,
                    }
                } else {
                    h.unsupported_charset = true;
                    if tag == 0x0010_0020 {
                        let bytes: String = value.iter().copied().map(char::from).collect();
                        h.undecodable_patient_id = Some(bytes.trim_matches([' ', '\0']).to_string());
                    }
                }
            }
            0x0010_0030 => h.patient_birth_date = v(),
            0x0020_000D => h.study_instance_uid = v(),
            0x0020_000E => h.series_instance_uid = v(),
            _ => {}
        }
        tag != 0x0020_000E
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

const DOCUMENT: u32 = 0x0042_0011;
const DOCUMENT_LENGTH: u32 = 0x0042_0015;

/// Locate the encapsulated value and trim any DICOM padding (PS3.3 §C.24.2).
fn document_range(w: &mut Walker<impl Read + Seek>) -> Result<Option<std::ops::Range<usize>>, Part10Error> {
    let mut document = None;
    let mut length = None;
    while !w.done() {
        let tag = w.tag_at(w.pos)?;
        if tag > DOCUMENT_LENGTH {
            break;
        }
        let e = w.next(0)?;
        match e.tag {
            DOCUMENT => document = e.value,
            DOCUMENT_LENGTH => {
                let value = e
                    .value
                    .filter(|r| r.len() == 4)
                    .ok_or_else(|| corrupt("invalid Encapsulated Document Length"))?;
                length = Some(w.u32_at(value.start)? as usize);
                break;
            }
            _ => {}
        }
    }
    let Some(mut value) = document else {
        return Ok(None);
    };
    let n = value.len();
    if let Some(original) = length
        && (n % 2 != 0 || (original != n && original.checked_add(1) != Some(n)))
    {
        return Err(corrupt("Encapsulated Document Length does not match the document"));
    }
    if n > 0 && n % 2 == 0 && length != Some(n) {
        let mut last = [0];
        w.bytes_at(value.end - 1, &mut last)?;
        if last[0] == 0 {
            value.end -= 1;
        } else if length.is_some() {
            return Err(corrupt("invalid Encapsulated Document padding"));
        }
    }
    Ok(Some(value))
}

/// The document inside an Encapsulated PDF instance, without its DICOM padding.
pub fn encapsulated_document(buf: &[u8]) -> Result<Option<Vec<u8>>, Part10Error> {
    if !is_part10(buf) {
        return Err(Part10Error::NotDicom);
    }
    let (ts, _, _, start) = meta(Cursor::new(buf), buf.len())?;
    let syntax = syntax_for(&ts)?;
    let set = &buf[start..];
    let mut w = Walker {
        input: Cursor::new(set),
        pos: 0,
        end: set.len(),
        syntax,
    };
    Ok(document_range(&mut w)?.map(|value| set[value].to_vec()))
}

/// Open the PDF value of a staged Encapsulated PDF instance as a bounded reader.
pub(crate) fn encapsulated_file(file: File) -> Result<Option<Take<File>>, Part10Error> {
    let end = file.metadata().map_err(|e| corrupt(e.to_string()))?.len() as usize;
    let mut input = BufReader::new(file);
    let (ts, _, _, pos) = meta(&mut input, end)?;
    let syntax = syntax_for(&ts)?;
    let mut w = Walker {
        input,
        pos,
        end,
        syntax,
    };
    let Some(value) = document_range(&mut w)?.filter(|r| r.len() >= 5) else {
        return Ok(None);
    };
    let mut magic = [0; 5];
    w.bytes_at(value.start, &mut magic)?;
    if &magic != b"%PDF-" {
        return Ok(None);
    }
    let mut file = w.input.into_inner();
    file.seek(SeekFrom::Start(value.start as u64))
        .map_err(|e| corrupt(e.to_string()))?;
    Ok(Some(file.take(value.len() as u64)))
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
    use std::io::Write;

    /// A file the hard way, to test the reader on what the test generator (dicom-object) does not
    /// write: big endian and an undefined-length sequence before the patient.
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
    fn a_deflated_data_set_is_refused_from_its_meta_group() {
        let bytes = file(DEFLATED_LE, b"data that would fail to inflate");
        assert!(matches!(read_header(&bytes), Err(Part10Error::Deflated)));
        assert!(matches!(encapsulated_document(&bytes), Err(Part10Error::Deflated)));
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
    fn patient_name_uses_the_declared_cyrillic_repertoire() {
        let mut set = core(false);
        set.splice(0..0, explicit(false, 0x0008_0005, b"CS", b"ISO_IR 144"));
        let name = set.windows(11).position(|w| w == b"RIVERA^ALEX").expect("patient name");
        set[name..name + 11].copy_from_slice(b"\xc0\xd8\xd2\xd5\xe0\xd0^\xb0\xdb\xd5\xda");
        let h = read_header(&file("1.2.840.10008.1.2.1", &set)).expect("header");
        assert_eq!(h.patient_name, "Ривера^Алек");
    }

    #[test]
    fn a_description_outside_the_repertoire_keeps_its_text_and_holds_nothing() {
        let mut set = core(false);
        before_patient(&mut set, &explicit(false, 0x0008_1030, b"LO", b"TC T\xd3RAX"));
        let h = read_header(&file("1.2.840.10008.1.2.1", &set)).expect("header");
        assert!(!h.unsupported_charset);
        assert_eq!(h.study_description, "TC TÓRAX");
        assert_eq!(h.patient_name, "RIVERA^ALEX");
    }

    #[test]
    fn a_single_byte_iso_2022_repertoire_reads_as_its_base() {
        for code in [
            &b"ISO 2022 IR 100"[..],
            b"ISO 2022 IR 6\\ISO 2022 IR 100",
            b"\\ISO 2022 IR 144",
        ] {
            let mut set = core(false);
            set.splice(0..0, explicit(false, 0x0008_0005, b"CS", code));
            let name = set.windows(11).position(|w| w == b"RIVERA^ALEX").expect("patient name");
            // only the first value applies without an escape: a G1 byte of a later set is not read
            let latin = code == b"ISO 2022 IR 100";
            if latin {
                set[name + 7] = 0xc1;
            }
            let h = read_header(&file("1.2.840.10008.1.2.1", &set)).expect("header");
            assert!(!h.unsupported_charset, "{}", String::from_utf8_lossy(code));
            let want = if latin { "RIVERA^ÁLEX" } else { "RIVERA^ALEX" };
            assert_eq!(h.patient_name, want, "{}", String::from_utf8_lossy(code));
        }
    }

    #[test]
    fn an_unreadable_name_holds_and_keeps_any_readable_patient_id() {
        // a byte outside the default repertoire, and a set switch the reader does not follow
        for (code, byte) in [
            (&b""[..], 0xc1u8),
            (b"ISO 2022 IR 100", 0x1b),
            (b"ISO 2022 IR 6\\ISO 2022 IR 100", 0xc1),
            (b"ISO 2022 IR 87", b'A'),
        ] {
            let mut set = core(false);
            if !code.is_empty() {
                set.splice(0..0, explicit(false, 0x0008_0005, b"CS", code));
            }
            before_patient(&mut set, &explicit(false, 0x0008_1030, b"LO", b"TC TORAX"));
            let name = set.windows(11).position(|w| w == b"RIVERA^ALEX").expect("patient name");
            set[name + 7] = byte;
            let h = read_header(&file("1.2.840.10008.1.2.1", &set)).expect("header");
            assert!(h.unsupported_charset, "{}", String::from_utf8_lossy(code));
            assert_eq!(h.patient_name, "");
            if code == b"ISO 2022 IR 87" {
                assert_eq!(h.patient_id, "");
                assert_eq!(h.undecodable_patient_id.as_deref(), Some("LN-0001"));
            } else {
                assert_eq!(h.patient_id, "LN-0001");
                assert_eq!(h.undecodable_patient_id, None);
            }
            assert_eq!(h.study_description, "TC TORAX");
        }
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
    fn round5_unknown_explicit_vr_uses_a_32_bit_length() {
        let mut set = core(false);
        before_patient(&mut set, b"\x09\0\x10\x10ZZ\0\0\x04\0\0\0DATA");
        let h = read_header(&file("1.2.840.10008.1.2.1", &set)).expect("header after unknown VR");
        assert_eq!(h.patient_id, "LN-0001");
    }

    #[test]
    fn round5_partial_tag_after_series_uid_does_not_damage_the_header() {
        let mut set = core(false);
        let exact = file("1.2.840.10008.1.2.1", &set);
        let expected = read_header(&exact).expect("complete header");
        assert!(matches!(
            read_header(&exact[..exact.len() - 1]),
            Err(Part10Error::Corrupt(_))
        ));
        set.extend_from_slice(b"\x20\0");
        assert_eq!(read_header(&file("1.2.840.10008.1.2.1", &set)), Ok(expected));
    }

    #[test]
    fn round5_unsupported_charset_with_only_padding_identity_does_not_hold() {
        let mut set = core(false);
        for (value, len) in [(b"RIVERA^ALEX".as_slice(), 11), (b"LN-0001", 7)] {
            let start = set.windows(len).position(|w| w == value).expect("identity value");
            set[start..start + len].fill(b' ');
        }
        set.splice(0..0, explicit(false, 0x0008_0005, b"CS", b"ISO 2022 IR 87"));
        before_patient(&mut set, &explicit(false, 0x0008_1030, b"LO", b"LabNorte"));
        let h = read_header(&file("1.2.840.10008.1.2.1", &set)).expect("header");
        assert_eq!(h.study_description, "LabNorte");
        assert!(!h.unsupported_charset);
    }

    fn read_staged_pdf(bytes: &[u8]) -> Result<Option<Vec<u8>>, Part10Error> {
        let dir = tempfile::tempdir().expect("staging directory");
        let mut staged = tempfile::tempfile_in(dir.path()).expect("staged file");
        staged.write_all(bytes).expect("DICOM bytes");
        let Some(mut data) = encapsulated_file(staged)? else {
            return Ok(None);
        };
        let mut out = Vec::new();
        data.read_to_end(&mut out).expect("document");
        Ok(Some(out))
    }

    #[test]
    fn round5_declared_document_length_distinguishes_data_from_padding() {
        let value = b"%PDF-X!\0";
        for (original, want) in [(Some(8u32), &value[..]), (Some(7), &value[..7]), (None, &value[..7])] {
            let mut set = explicit(false, 0x0042_0011, b"OB", value);
            if let Some(n) = original {
                set.extend(explicit(false, 0x0042_0015, b"UL", &n.to_le_bytes()));
            }
            let bytes = file("1.2.840.10008.1.2.1", &set);
            assert_eq!(
                encapsulated_document(&bytes).expect("in-memory PDF").as_deref(),
                Some(want)
            );
            assert_eq!(read_staged_pdf(&bytes).expect("staged PDF").as_deref(), Some(want));
        }
        let mut set = explicit(false, 0x0042_0011, b"OB", value);
        set.extend(explicit(false, 0x0042_0015, b"UL", &6u32.to_le_bytes()));
        let bytes = file("1.2.840.10008.1.2.1", &set);
        assert!(matches!(encapsulated_document(&bytes), Err(Part10Error::Corrupt(_))));
        assert!(matches!(read_staged_pdf(&bytes), Err(Part10Error::Corrupt(_))));
    }

    #[test]
    fn text_takes_the_first_value_and_reads_latin_1() {
        assert_eq!(text(b"CT\\PT "), "CT");
        assert_eq!(text(b"ORTEGA^CARMEN\0"), "ORTEGA^CARMEN");
        assert_eq!(text(b"MU\xd1OZ^LUC\xcdA"), "MUÑOZ^LUCÍA");
        assert_eq!(text("MUÑOZ".as_bytes()), "MUÑOZ");
    }
}
