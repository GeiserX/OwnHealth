//! Rule-based document classifier (`ingest/classify.py`, ingestion.md 5). No model.
//!
//! From the text lines of a document's first pages it guesses the document type, the language, the
//! patient block (name, birth date, sample id), the collection and report dates, and the report
//! status. Every guess is shown to the person, who confirms or corrects it; nothing here is a fact.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::LazyLock;

use chrono::{NaiveDate, NaiveDateTime, NaiveTime};
use regex::Regex;
use unicode_normalization::UnicodeNormalization;
use unicode_normalization::char::canonical_combining_class;

use crate::py::{az, count_bounded, has_bounded, strip_chars};

pub const DOC_TYPES: [&str; 6] = [
    "lab_report",
    "imaging_report",
    "discharge",
    "prescription",
    "genomics",
    "other",
];

const TYPE_WORDS: [(&str, &[&str]); 5] = [
    (
        "lab_report",
        &[
            "hemograma",
            "bioquimica",
            "laboratorio",
            "valores de referencia",
            "intervalo de referencia",
            "laboratory",
            "lab results",
            "reference range",
            "reference interval",
            "haematology",
            "hematology",
            "laborbefund",
            "referenzbereich",
            "normbereich",
            "blutbild",
            "klinische chemie",
            "laboratoire",
            "valeurs de reference",
            "valeurs usuelles",
            "biochimie",
            "hematologie",
            "analyses",
        ],
    ),
    (
        "imaging_report",
        &[
            "radiologia",
            "ecografia",
            "resonancia",
            "tomografia",
            "radiografia",
            "informe radiologico",
            "radiology",
            "ultrasound",
            "mri",
            "ct scan",
            "x-ray",
            "rontgen",
            "sonographie",
            "mrt",
            "radiologie",
            "echographie",
            "irm",
            "scanner",
            "impression:",
            "hallazgos",
            "findings",
        ],
    ),
    (
        "discharge",
        &[
            "informe de alta",
            "alta hospitalaria",
            "discharge summary",
            "discharge letter",
            "entlassungsbericht",
            "entlassbrief",
            "compte rendu d'hospitalisation",
            "lettre de sortie",
            "fecha de ingreso",
            "admission",
        ],
    ),
    (
        "prescription",
        &[
            "receta",
            "prescripcion",
            "posologia",
            "prescription",
            "rx",
            "dosage",
            "rezept",
            "verordnung",
            "ordonnance",
            "posologie",
        ],
    ),
    (
        "genomics",
        &[
            "genotipo",
            "genomica",
            "farmacogenetica",
            "genotype",
            "pharmacogenomic",
            "pharmacogenetic",
            "rsid",
            "vcf",
            "genetik",
            "genotyp",
            "genetique",
            "sequencage",
            "haplotype",
            "diplotype",
        ],
    ),
];

const LANG_WORDS: [(&str, &[&str]); 4] = [
    (
        "es",
        &[
            "de",
            "la",
            "el",
            "los",
            "las",
            "del",
            "y",
            "resultado",
            "paciente",
            "fecha",
            "unidades",
            "prueba",
            "valores",
            "referencia",
            "informe",
            "muestra",
        ],
    ),
    (
        "en",
        &[
            "the",
            "and",
            "of",
            "result",
            "results",
            "patient",
            "date",
            "units",
            "test",
            "reference",
            "range",
            "report",
            "sample",
            "collected",
        ],
    ),
    (
        "de",
        &[
            "der",
            "die",
            "das",
            "und",
            "ergebnis",
            "einheit",
            "datum",
            "untersuchung",
            "referenzbereich",
            "befund",
            "seite",
            "geb",
        ],
    ),
    (
        "fr",
        &[
            "le",
            "les",
            "des",
            "et",
            "du",
            "resultat",
            "unite",
            "valeurs",
            "analyse",
            "patient",
            "date",
            "prelevement",
            "naissance",
            "page",
        ],
    ),
];

const LABELS: [(&str, &[&str]); 8] = [
    (
        "name",
        &[
            "apellidos y nombre",
            "nombre y apellidos",
            "paciente",
            "nombre",
            "patient(in)",
            "patientin",
            "patient(e)",
            "patient",
            "name",
            "nom",
        ],
    ),
    // Spanish public health labs print the surnames apart from the given name; `classify` joins them
    ("surname", &["apellidos", "apellido", "primer apellido"]),
    ("surname2", &["segundo apellido"]),
    (
        "birth",
        &[
            "f. nacimiento",
            "f.nacimiento",
            "fecha de nacimiento",
            "fecha nacimiento",
            "f. nac.",
            "f. nac",
            "f.nac.",
            "f.nac",
            "fecha nac.",
            "fecha nac",
            "date of birth",
            "dob",
            "born",
            "geb.-datum",
            "geburtsdatum",
            "geb.",
            "ne(e) le",
            "nee le",
            "ne le",
            "date de naissance",
        ],
    ),
    (
        "sample",
        &[
            "n.o muestra",
            "no muestra",
            "n.o de muestra",
            "muestra",
            "sample id",
            "specimen id",
            "sample",
            "accession",
            "probennummer",
            "probe-nr.",
            "auftragsnr.",
            "auftrag",
            "no echantillon",
            "echantillon",
            "dossier",
            "informe",
            "report id",
            "report",
            "befund-nr.",
        ],
    ),
    (
        "collected",
        &[
            "fecha de toma",
            "fecha de extraccion",
            "fecha extraccion",
            "f. extraccion",
            "fecha toma",
            "collected",
            "collection date",
            "date collected",
            "entnahmedatum",
            "entnahme",
            "abnahme",
            "date de prelevement",
            "preleve le",
            "prelevement",
        ],
    ),
    (
        "reported",
        &[
            "fecha de informe",
            "fecha de emision",
            "fecha validacion",
            "fecha de validacion",
            "reported",
            "report date",
            "befunddatum",
            "ausgestellt",
            "edite le",
            "date d'edition",
            "date du compte rendu",
        ],
    ),
    // Never read: they end the value of the label before them, so a doctor's name or the cover type
    // printed after the patient's name on the same line is not taken as part of it.
    (
        "other",
        &[
            "medico responsable",
            "medico solicitante",
            "medico",
            "medica",
            "doctor",
            "doctora",
            "facultativo",
            "responsable",
            "solicitante",
            "peticionario",
            "physician",
            "servicio",
            "cobertura",
            "sexo",
            "edad",
            "nhc",
            "dni",
            "procedencia",
        ],
    ),
];

/// A name label right after one of these words names the doctor, not the patient (`Médico del paciente:`).
const STAFF: [&str; 12] = [
    "medico",
    "medica",
    "doctor",
    "doctora",
    "dr",
    "dra",
    "facultativo",
    "facultativa",
    "responsable",
    "solicitante",
    "peticionario",
    "physician",
];

/// Words that join a label's parts (`Nombre del médico`); a label whose value is only these is part of
/// a longer label, not a value.
const CONNECTORS: [&str; 10] = ["de", "del", "la", "el", "y", "e", "of", "the", "and", "du"];

/// The name-like kinds: the patient's name and surnames.
const NAME_KINDS: [&str; 3] = ["name", "surname", "surname2"];

const STATUS: [(&str, &[&str]); 2] = [
    (
        "amended",
        &[
            "rectificado",
            "informe corregido",
            "corregido",
            "amended",
            "corrected report",
            "korrigiert",
            "berichtigt",
            "rectifie",
            "compte rendu modifie",
        ],
    ),
    (
        "preliminary",
        &[
            "provisional",
            "preliminar",
            "preliminary",
            "vorlaufig",
            "provisoire",
            "partiel",
        ],
    ),
];

const MONTHS: &[(&str, u32)] = &[
    ("jan", 1),
    ("january", 1),
    ("enero", 1),
    ("ene", 1),
    ("januar", 1),
    ("janvier", 1),
    ("janv", 1),
    ("feb", 2),
    ("february", 2),
    ("febrero", 2),
    ("februar", 2),
    ("fevrier", 2),
    ("fevr", 2),
    ("mar", 3),
    ("march", 3),
    ("marzo", 3),
    ("marz", 3),
    ("mars", 3),
    ("apr", 4),
    ("april", 4),
    ("abril", 4),
    ("abr", 4),
    ("avril", 4),
    ("avr", 4),
    ("may", 5),
    ("mayo", 5),
    ("mai", 5),
    ("jun", 6),
    ("june", 6),
    ("junio", 6),
    ("juni", 6),
    ("juin", 6),
    ("jul", 7),
    ("july", 7),
    ("julio", 7),
    ("juli", 7),
    ("juillet", 7),
    ("juil", 7),
    ("aug", 8),
    ("august", 8),
    ("agosto", 8),
    ("ago", 8),
    ("aout", 8),
    ("sep", 9),
    ("sept", 9),
    ("september", 9),
    ("septiembre", 9),
    ("septembre", 9),
    ("oct", 10),
    ("october", 10),
    ("octubre", 10),
    ("oktober", 10),
    ("octobre", 10),
    ("okt", 10),
    ("nov", 11),
    ("november", 11),
    ("noviembre", 11),
    ("novembre", 11),
    ("dec", 12),
    ("december", 12),
    ("diciembre", 12),
    ("dic", 12),
    ("dezember", 12),
    ("decembre", 12),
    ("dez", 12),
];

fn month(name: &str) -> Option<u32> {
    MONTHS.iter().find(|(m, _)| *m == name).map(|(_, n)| *n)
}

fn rx(p: &str) -> Regex {
    Regex::new(p).unwrap_or_else(|e| panic!("bad built-in pattern {p}: {e}"))
}

static DATE_NUM: LazyLock<Regex> =
    LazyLock::new(|| rx(r"\b(\d{1,2})[./-](\d{1,2})[./-](\d{4})\b|\b(\d{4})-(\d{2})-(\d{2})\b"));
static DATE_WORD: LazyLock<Regex> =
    LazyLock::new(|| rx(r"\b(\d{1,2})\.?\s+([a-z]{3,10})\.?\s+(\d{4})\b|\b([a-z]{3,10})\.?\s+(\d{1,2}),\s*(\d{4})\b"));
static TIME: LazyLock<Regex> = LazyLock::new(|| rx(r"\b([01]?\d|2[0-3])[:h]([0-5]\d)\b"));
static AZ_RUN: LazyLock<Regex> = LazyLock::new(|| rx(r"[a-z]+"));
static TWO_LETTERS: LazyLock<Regex> = LazyLock::new(|| rx(r"[A-Za-zÀ-ÿ]{2}"));
static DIGIT: LazyLock<Regex> = LazyLock::new(|| rx(r"\d"));
/// A line that opens with a label of its own (`Médico: ...`), known to the table or not.
static LABELISH: LazyLock<Regex> = LazyLock::new(|| rx(r"^[^\d:]{1,40}:"));

/// Lowercase, strip accents, normalise the ordinal sign: for matching only.
pub fn fold(s: &str) -> String {
    let s = s.replace(['º', '°'], "o").replace('ª', "a");
    let stripped: String = s.nfkd().filter(|c| canonical_combining_class(*c) == 0).collect();
    stripped.to_lowercase()
}

fn num(m: &regex::Captures<'_>, i: usize) -> Option<i64> {
    m.get(i).and_then(|g| g.as_str().parse().ok())
}

fn ymd(y: Option<i64>, m: Option<i64>, d: Option<i64>) -> Option<NaiveDate> {
    NaiveDate::from_ymd_opt(
        i32::try_from(y?).ok()?,
        u32::try_from(m?).ok()?,
        u32::try_from(d?).ok()?,
    )
}

pub fn parse_date(s: &str) -> Option<NaiveDate> {
    let f = fold(s);
    if let Some(m) = DATE_NUM.captures(&f) {
        if m.get(4).is_some() {
            return ymd(num(&m, 4), num(&m, 5), num(&m, 6));
        }
        // day first: es, de, fr and UK English; US layouts print ISO or month names
        return ymd(num(&m, 3), num(&m, 2), num(&m, 1));
    }
    if let Some(m) = DATE_WORD.captures(&f) {
        if m.get(1).is_some() {
            let mon = month(m.get(2)?.as_str())?;
            return ymd(num(&m, 3), Some(mon as i64), num(&m, 1));
        }
        let mon = month(m.get(4)?.as_str())?;
        return ymd(num(&m, 6), Some(mon as i64), num(&m, 5));
    }
    None
}

pub fn parse_datetime(s: &str) -> Option<NaiveDateTime> {
    let d = parse_date(s)?;
    let rest = DATE_NUM.replace_all(&fold(s), " ").into_owned();
    let t = TIME
        .captures(&rest)
        .and_then(|m| NaiveTime::from_hms_opt(num(&m, 1)? as u32, num(&m, 2)? as u32, 0))
        .unwrap_or(NaiveTime::MIN);
    Some(d.and_time(t))
}

#[derive(Debug, Clone, PartialEq)]
pub struct Classification {
    pub doc_type: String,
    pub language: Option<String>,
    pub scores: BTreeMap<String, usize>,
    pub patient_name: Option<String>,
    pub birth_date: Option<NaiveDate>,
    pub sample_id: Option<String>,
    pub collected_at: Option<NaiveDateTime>,
    pub reported_at: Option<NaiveDateTime>,
    pub report_status: String,
}

impl Default for Classification {
    fn default() -> Self {
        Classification {
            doc_type: "other".into(),
            language: None,
            scores: BTreeMap::new(),
            patient_name: None,
            birth_date: None,
            sample_id: None,
            collected_at: None,
            reported_at: None,
            report_status: "final".into(),
        }
    }
}

/// Every label, longest first (Python's stable sort keeps the table order among equal lengths).
static ALL_LABELS: LazyLock<Vec<(&'static str, &'static str)>> = LazyLock::new(|| {
    let mut out: Vec<(&str, &str)> = LABELS
        .iter()
        .flat_map(|(k, labs)| labs.iter().map(move |l| (*k, *l)))
        .collect();
    out.sort_by_key(|(_, l)| std::cmp::Reverse(l.chars().count()));
    out
});

/// Character offset of a byte offset.
fn char_at(s: &str, byte: usize) -> usize {
    s[..byte].chars().count()
}

/// `Paciente: RIVERA, ALEX  F. nacimiento: 14/05/1979` -> {name: 'RIVERA, ALEX', birth: '14/05/1979'}.
/// The first non-empty value of each kind, in line order.
pub fn label_values(line: &str) -> Vec<(&'static str, String)> {
    let mut out: Vec<(&'static str, String)> = Vec::new();
    for (kind, value) in scan(line) {
        if !value.is_empty() && !out.iter().any(|(k, _)| *k == kind) {
            out.push((kind, value));
        }
    }
    out
}

/// The last word before a label, connectors skipped, is a doctor's title (`Médico del paciente:`).
fn after_staff_word(before: &str) -> bool {
    before
        .rsplit(|c: char| !c.is_ascii_lowercase())
        .filter(|w| !w.is_empty())
        .find(|w| !CONNECTORS.contains(w))
        .is_some_and(|w| STAFF.contains(&w))
}

/// Every label on the line with its value, empty values included, in line order.
fn scan(line: &str) -> Vec<(&'static str, String)> {
    let f = fold(line);
    // (start, end) in characters of the folded line, as Python indexed it
    let mut hits: Vec<(usize, usize, &str)> = Vec::new();
    let mut taken: Vec<(usize, usize)> = Vec::new();
    for (kind, lab) in ALL_LABELS.iter() {
        let mut from = 0usize;
        while let Some(rel) = f[from..].find(lab) {
            let i = from + rel;
            let j = i + lab.len();
            let (before, after) = crate::py::around(&f, i, j);
            if before.is_some_and(az) || after.is_some_and(az) {
                from = i + f[i..].chars().next().map_or(1, char::len_utf8);
                continue;
            }
            // \s*:? after the label
            let rest = &f[j..];
            let ws = rest.len() - rest.trim_start_matches(char::is_whitespace).len();
            let mut end = j + ws;
            if f[end..].starts_with(':') {
                end += 1;
            } else if let Some(r) = f[end..].strip_prefix('(')
                && let Some(close) = r.find(')')
                && close <= 24
                && !r[..close].contains([':', '('])
            {
                // a short note between the label and its colon: `Fecha de nacimiento (edad): ...`
                let after = &r[close + 1..];
                let ws2 = after.len() - after.trim_start_matches(char::is_whitespace).len();
                if after[ws2..].starts_with(':') {
                    end += 1 + close + 1 + ws2 + 1;
                }
            }
            from = end.max(i + 1);
            let (a, b) = (char_at(&f, i), char_at(&f, end));
            if taken.iter().any(|(x, y)| a < *y && *x < b) {
                continue;
            }
            // a label needs a colon after it, or must start the line, to count (avoids "Patient" in prose)
            if !f[i..end].contains(':') && a != 0 {
                continue;
            }
            taken.push((a, b));
            // still taken: a doctor's name label ends the value before it, it is just never read
            let kind = if NAME_KINDS.contains(kind) && after_staff_word(&f[..i]) {
                &"other"
            } else {
                kind
            };
            hits.push((a, b, kind));
        }
    }
    hits.sort();
    let chars: Vec<char> = line.chars().collect();
    let mut out: Vec<(&'static str, String)> = Vec::new();
    for (i, (_a, b, kind)) in hits.iter().enumerate() {
        let end = if i + 1 < hits.len() { hits[i + 1].0 } else { chars.len() };
        let (b, end) = ((*b).min(chars.len()), end.min(chars.len()));
        let value: String = if b < end {
            chars[b..end].iter().collect()
        } else {
            String::new()
        };
        let value = strip_chars(&value, " :|;,\t").to_string();
        // `Nombre del médico:` reads as the label `Nombre` with the value `del`: part of a longer label
        if !value.is_empty() && fold(&value).split_whitespace().all(|w| CONNECTORS.contains(&w)) {
            continue;
        }
        let kind: &'static str = LABELS.iter().find(|(k, _)| k == kind).map_or("name", |(k, _)| k);
        out.push((kind, value));
    }
    out
}

/// The value printed on the line after a label that stands alone at the end of its line: only a line
/// that holds no label itself, and only when it reads as that kind of value.
fn next_line_value(kind: &str, next: &str) -> Option<String> {
    let v = strip_chars(next, " :|;,\t");
    let f = fold(v);
    if v.is_empty() || !scan(next).is_empty() || LABELISH.is_match(&f) {
        return None;
    }
    let ok = match kind {
        "name" | "surname" | "surname2" => {
            // a section heading under the label is not a name; a full name has two words or more
            !DIGIT.is_match(v)
                && !TYPE_WORDS
                    .iter()
                    .any(|(_, ws)| ws.iter().any(|w| has_bounded(&f, w, az)))
                && (kind != "name" || name_tokens(v).len() >= 2)
        }
        "birth" => parse_date(v).is_some(),
        "collected" | "reported" => parse_datetime(v).is_some(),
        "sample" => v.split_whitespace().next().is_some_and(|t| DIGIT.is_match(t)),
        _ => false,
    };
    ok.then(|| v.to_string())
}

/// The page name: the given name, after the surnames printed under their own labels.
fn page_name(surnames: &[Option<String>; 2], given: Option<String>) -> Option<String> {
    let given = given?;
    let have = name_tokens(&given);
    let sur: Vec<&str> = surnames
        .iter()
        .flatten()
        .map(String::as_str)
        .filter(|s| !name_tokens(s).is_subset(&have))
        .collect();
    if sur.is_empty() {
        return Some(given);
    }
    Some(format!("{}, {given}", sur.join(" ")))
}

/// Guess type, language, patient block, dates and status from the first pages' text lines.
pub fn classify(lines: &[String]) -> Classification {
    let mut c = Classification::default();
    let folded: Vec<String> = lines.iter().map(|x| fold(x)).collect();
    let blob = folded.join("\n");
    let tokens: Vec<&str> = AZ_RUN.find_iter(&blob).map(|m| m.as_str()).collect();
    let lang_scores: Vec<(&str, usize)> = LANG_WORDS
        .iter()
        .map(|(lang, words)| (*lang, tokens.iter().filter(|t| words.contains(t)).count()))
        .collect();
    if lang_scores.iter().any(|(_, n)| *n > 0) {
        // Python's max keeps the first of equal scores
        let mut best = lang_scores[0];
        for s in &lang_scores[1..] {
            if s.1 > best.1 {
                best = *s;
            }
        }
        c.language = Some(best.0.to_string());
    }
    let mut best: Option<(&str, usize)> = None;
    for (t, words) in TYPE_WORDS.iter() {
        let n: usize = words.iter().map(|w| count_bounded(&blob, w, az)).sum();
        c.scores.insert(t.to_string(), n);
        if best.is_none_or(|(_, b)| n > b) {
            best = Some((t, n));
        }
    }
    if let Some((t, n)) = best
        && n > 0
    {
        c.doc_type = t.to_string();
    }
    let mut given: Option<String> = None;
    let mut surnames: [Option<String>; 2] = [None, None];
    for (n, line) in lines.iter().enumerate() {
        let mut found = label_values(line);
        // `Nombre:` alone at the end of its line, the value on the next one
        if let Some((kind, v)) = scan(line).last()
            && v.is_empty()
            && !found.iter().any(|(k, _)| k == kind)
            && let Some(v) = lines.get(n + 1).and_then(|next| next_line_value(kind, next))
        {
            found.push((kind, v));
        }
        for (kind, value) in found {
            let name_like = TWO_LETTERS.is_match(&value);
            match kind {
                "name" if given.is_none() && name_like => given = Some(value),
                "surname" if surnames[0].is_none() && name_like => surnames[0] = Some(value),
                "surname2" if surnames[1].is_none() && name_like => surnames[1] = Some(value),
                "birth" if c.birth_date.is_none() => c.birth_date = parse_date(&value),
                "sample" if c.sample_id.is_none() => {
                    let tok = value.split_whitespace().next().unwrap_or("");
                    if DIGIT.is_match(tok) {
                        c.sample_id = Some(tok.to_string());
                    }
                }
                "collected" if c.collected_at.is_none() => c.collected_at = parse_datetime(&value),
                "reported" if c.reported_at.is_none() => c.reported_at = parse_datetime(&value),
                _ => {}
            }
        }
    }
    c.patient_name = page_name(&surnames, given);
    for (status, words) in STATUS.iter() {
        if words
            .iter()
            .any(|w| crate::py::occurrences(&blob, w).any(|i| !crate::py::around(&blob, i, i).0.is_some_and(az)))
        {
            c.report_status = status.to_string();
            break;
        }
    }
    c
}

// ------------------------------------------------------------------ identity ----

pub fn name_tokens(name: &str) -> BTreeSet<String> {
    AZ_RUN
        .find_iter(&fold(name))
        .map(|m| m.as_str())
        .filter(|t| t.len() > 1)
        .map(String::from)
        .collect()
}

/// matched | mismatch | missing | hinted (name or birth date alone matched, the other absent).
pub fn identity_match(
    page_name: Option<&str>,
    page_birth: Option<NaiveDate>,
    profile_name: &str,
    profile_birth: Option<NaiveDate>,
) -> &'static str {
    let page_name = page_name.filter(|n| !n.is_empty());
    if page_name.is_none() && page_birth.is_none() {
        return "missing";
    }
    let name_ok = page_name.map(|n| {
        let (got, want) = (name_tokens(n), name_tokens(profile_name));
        !want.is_empty() && want.is_subset(&got)
    });
    let birth_ok = match (page_birth, profile_birth) {
        (Some(a), Some(b)) => Some(a == b),
        _ => None,
    };
    if name_ok == Some(false) || birth_ok == Some(false) {
        return "mismatch";
    }
    if name_ok == Some(true) && birth_ok == Some(true) {
        return "matched";
    }
    "hinted"
}

/// `has_bounded` with the `[a-z0-9]` neighbour rule the templates use.
pub fn has_word(folded: &str, word: &str) -> bool {
    has_bounded(folded, &fold(word), crate::py::az09)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(y: i32, m: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, day).expect("date")
    }

    #[test]
    fn test_parse_date() {
        for (s, want) in [
            ("14/05/1979", Some(d(1979, 5, 14))),
            ("14.05.1979", Some(d(1979, 5, 14))),
            ("1979-05-14", Some(d(1979, 5, 14))),
            ("14 May 1979", Some(d(1979, 5, 14))),
            ("May 14, 1979", Some(d(1979, 5, 14))),
            ("3 de marzo", None),
            ("31/02/2026", None),
        ] {
            assert_eq!(parse_date(s), want, "{s}");
        }
    }

    #[test]
    fn test_parse_datetime_keeps_the_time() {
        assert_eq!(
            parse_datetime("14/09/2026 08:15"),
            Some(d(2026, 9, 14).and_hms_opt(8, 15, 0).expect("time"))
        );
    }

    #[test]
    fn test_patient_block_dates_and_language_es() {
        let lines: Vec<String> = [
            "LabNorte",
            "Informe de resultados de laboratorio",
            "Paciente: RIVERA, ALEX      F. nacimiento: 14/05/1979",
            "Fecha de toma: 14/09/2026 08:15      Nº muestra: LN-1",
            "Fecha de informe: 14/09/2026 13:40      Página 1/3",
            "Prueba Resultado Unidades Valores de referencia",
            "HEMOGRAMA",
        ]
        .map(String::from)
        .to_vec();
        let c = classify(&lines);
        assert_eq!(
            (c.doc_type.as_str(), c.language.as_deref(), c.report_status.as_str()),
            ("lab_report", Some("es"), "final")
        );
        assert_eq!(c.patient_name.as_deref(), Some("RIVERA, ALEX"));
        assert_eq!(c.birth_date, Some(d(1979, 5, 14)));
        assert_eq!(c.collected_at, d(2026, 9, 14).and_hms_opt(8, 15, 0));
        assert_eq!(c.reported_at, d(2026, 9, 14).and_hms_opt(13, 40, 0));
        assert_eq!(c.sample_id.as_deref(), Some("LN-1"));
    }

    #[test]
    fn test_report_status() {
        for (line, status) in [
            ("Laborbefund - Vorläufiger Befund", "preliminary"),
            ("Laborbefund - KORRIGIERTER BEFUND", "amended"),
            ("Informe de resultados - INFORME RECTIFICADO", "amended"),
            ("Compte rendu provisoire", "preliminary"),
            ("Laboratory report", "final"),
        ] {
            assert_eq!(
                classify(&[line.into(), "Patient: X".into()]).report_status,
                status,
                "{line}"
            );
        }
    }

    #[test]
    fn test_non_lab_types() {
        let c = |l: &[&str]| classify(&l.iter().map(|s| s.to_string()).collect::<Vec<_>>()).doc_type;
        assert_eq!(
            c(&["Informe de alta hospitalaria", "Fecha de ingreso: 01/02/2026"]),
            "discharge"
        );
        assert_eq!(
            c(&[
                "Radiology report",
                "Findings: no acute change",
                "Impression: normal MRI"
            ]),
            "imaging_report"
        );
        assert_eq!(c(&["Hola", "nada que ver"]), "other");
    }

    #[test]
    fn test_identity_match() {
        let born = Some(d(2017, 6, 21));
        assert_eq!(
            identity_match(Some("RIVERA, LUCÍA"), born, "Lucía Rivera", born),
            "matched"
        );
        assert_eq!(
            identity_match(Some("Lucia Rivera"), born, "Lucía Rivera", born),
            "matched"
        );
        assert_eq!(
            identity_match(Some("RIVERA, ALEX"), Some(d(1979, 5, 14)), "Lucía Rivera", born),
            "mismatch"
        );
        assert_eq!(
            identity_match(Some("RIVERA, LUCIA"), Some(d(2017, 6, 22)), "Lucía Rivera", born),
            "mismatch"
        );
        assert_eq!(
            identity_match(Some("RIVERA, LUCIA"), None, "Lucía Rivera", born),
            "hinted"
        );
        assert_eq!(identity_match(None, None, "Lucía Rivera", born), "missing");
    }

    fn page(lines: &[&str]) -> Classification {
        classify(&lines.iter().map(|s| s.to_string()).collect::<Vec<_>>())
    }

    /// The patient block of a Spanish page against a two-surname profile born 1979-05-14.
    fn against_full_name(lines: &[&str]) -> &'static str {
        let c = page(lines);
        identity_match(
            c.patient_name.as_deref(),
            c.birth_date,
            "Alex Rivera Ortega",
            Some(d(1979, 5, 14)),
        )
    }

    #[test]
    fn surnames_and_given_name_combine_into_one_page_name() {
        // Spanish public health lab: one line, or one label per line
        for lines in [
            &["Apellidos: RIVERA ORTEGA      Nombre: ALEX"][..],
            &["Apellidos: RIVERA ORTEGA", "Nombre: ALEX"],
            &["Nombre: ALEX", "Apellidos: RIVERA ORTEGA"],
            &["Primer apellido: RIVERA   Segundo apellido: ORTEGA   Nombre: ALEX"],
        ] {
            let c = page(lines);
            assert_eq!(
                c.patient_name.as_deref().map(name_tokens),
                Some(name_tokens("Alex Rivera Ortega")),
                "{lines:?}"
            );
        }
        assert_eq!(
            page(&["Apellidos: RIVERA ORTEGA      Nombre: ALEX"])
                .patient_name
                .as_deref(),
            Some("RIVERA ORTEGA, ALEX")
        );
        // a combined label is one name, not a surname plus a connector
        for line in [
            "Apellidos y nombre: RIVERA ORTEGA, ALEX",
            "Nombre y apellidos: ALEX RIVERA ORTEGA",
        ] {
            assert_eq!(
                page(&[line]).patient_name.as_deref().map(name_tokens),
                Some(name_tokens("Alex Rivera Ortega")),
                "{line}"
            );
        }
        // the surnames alone are no page name
        assert_eq!(page(&["Apellidos: RIVERA ORTEGA"]).patient_name, None);
    }

    #[test]
    fn spanish_layouts_match_the_full_name_profile() {
        for lines in [
            &[
                "Apellidos: RIVERA ORTEGA      Nombre: ALEX",
                "Fecha de nacimiento: 14/05/1979",
            ][..],
            &["Apellidos: RIVERA ORTEGA", "Nombre: ALEX", "F. Nac: 14/05/1979"],
            // private hospital group: an age suffix on the label, after another label on the line
            &[
                "Paciente: RIVERA ORTEGA, ALEX",
                "Cobertura: PRIVADA      Fecha de nacimiento (edad): 14/05/1979 (47 años)",
            ],
            &["Paciente: RIVERA ORTEGA, ALEX    F.Nac: 14/05/1979"],
            &["Paciente: RIVERA ORTEGA, ALEX    Fecha Nac: 14/05/1979"],
            // label alone on its line, value on the next
            &["Nombre:", "ALEX RIVERA ORTEGA", "Fecha nacimiento:", "14/05/1979"],
            &["Paciente: RIVERA ORTEGA, ALEX      F. nacimiento:", "14/05/1979"],
        ] {
            assert_eq!(against_full_name(lines), "matched", "{lines:?}");
        }
    }

    #[test]
    fn birth_labels_with_an_age_suffix_or_no_dot() {
        for line in [
            "Cobertura: PRIVADA   Fecha de nacimiento (edad): 14/05/1979 (47 años)",
            "F. Nac: 14/05/1979",
            "F.Nac: 14/05/1979",
            "Fecha Nac: 14/05/1979",
            "Fecha nacimiento: 14/05/1979",
        ] {
            assert_eq!(page(&[line]).birth_date, Some(d(1979, 5, 14)), "{line}");
        }
    }

    #[test]
    fn the_matcher_stays_strict() {
        // another person's surnames and name
        assert_eq!(
            against_full_name(&["Apellidos: ORTEGA      Nombre: CARMEN", "F. Nac: 30/01/1955"]),
            "mismatch"
        );
        // the same given name with other surnames
        assert_eq!(
            against_full_name(&["Apellidos: RIVERA SANZ      Nombre: ALEX", "F. Nac: 14/05/1979"]),
            "mismatch"
        );
        // the right name, another birth date
        assert_eq!(
            against_full_name(&["Apellidos: RIVERA ORTEGA      Nombre: ALEX", "F. Nac: 15/05/1979"]),
            "mismatch"
        );
        assert_eq!(
            against_full_name(&[
                "Paciente: RIVERA ORTEGA, ALEX",
                "Cobertura: PRIVADA   Fecha de nacimiento (edad): 15/05/1979 (47 años)",
            ]),
            "mismatch"
        );
        // only the given name still is not the full-name profile
        assert_eq!(against_full_name(&["Nombre: ALEX", "F. Nac: 14/05/1979"]), "mismatch");
    }

    #[test]
    fn a_doctor_is_never_read_as_the_patient() {
        for lines in [
            &["Médico: ALEX RIVERA ORTEGA"][..],
            &["Responsable: ALEX RIVERA ORTEGA"],
            &["Responsable:", "ALEX RIVERA ORTEGA"],
            &["Nombre del médico: ALEX RIVERA ORTEGA"],
            &["Médico del paciente: ALEX RIVERA ORTEGA"],
            &["Doctor name: ALEX RIVERA ORTEGA"],
            &["Nombre:", "Médico: ALEX RIVERA ORTEGA"],
            &["Nombre:", "Responsable: ALEX RIVERA ORTEGA"],
            &["Nombre:", "Médico solicitante: ALEX RIVERA ORTEGA"],
        ] {
            assert_eq!(page(lines).patient_name, None, "{lines:?}");
        }
        // the doctor's surnames are not the patient's
        assert_eq!(
            page(&["Apellidos del médico: RIVERA ORTEGA   Nombre: ALEX"])
                .patient_name
                .as_deref(),
            Some("ALEX")
        );
        // a doctor's label after the patient's ends the patient's name
        assert_eq!(
            page(&["Paciente: RIVERA ORTEGA, ALEX   Médico: DR. SANZ GIL"])
                .patient_name
                .as_deref(),
            Some("RIVERA ORTEGA, ALEX")
        );
    }

    #[test]
    fn a_next_line_value_is_read_only_when_it_is_not_a_label() {
        assert_eq!(page(&["Nombre:", "F. Nac: 14/05/1979"]).patient_name, None);
        assert_eq!(page(&["Fecha nacimiento:", "Nombre: ALEX"]).birth_date, None);
        // a section heading under a lone label is not a name: one word, or a document-type word
        assert_eq!(page(&["Paciente:", "ORINA"]).patient_name, None);
        assert_eq!(page(&["Paciente:", "Bioquímica general"]).patient_name, None);
        // nor an address line
        assert_eq!(page(&["Paciente:", "Calle Mayor 12"]).patient_name, None);
        assert_eq!(page(&["Fecha nacimiento:", "RIVERA ORTEGA"]).birth_date, None);
        // a lone surname on the next line is read
        assert_eq!(
            page(&["Apellidos:", "RIVERA ORTEGA", "Nombre: ALEX"])
                .patient_name
                .as_deref(),
            Some("RIVERA ORTEGA, ALEX")
        );
    }

    #[test]
    fn fold_strips_accents_and_ordinals() {
        assert_eq!(fold("Nº Análisis"), "no analisis");
    }
}
