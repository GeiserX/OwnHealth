//! GEDCOM import, structure only (v1.0 slice 24).
//!
//! A family tree file (GEDCOM 5.5.1 or 7.0) is read in memory, once, and never stored: no blob, no job
//! payload, no log line. The parser keeps only the structure a family history needs: each INDI record's
//! sex, birth year, death and age at death, and the FAMC/FAMS links; each FAM record's HUSB, WIFE and
//! CHIL. Every other value (NAME, NOTE, PLAC, sources, media, extension tags) is skipped as its line is
//! read and never copied. A refusal names a line number and a reason, never a value.
//!
//! The person picks themselves by position (the n-th INDI record of the file, `self`). Each relative
//! that `data/family/relationships.yaml` has a code for becomes a `family_proposal` review item with
//! the relationship, sex, birth year and death age only; accepting it calls [`family::add`], and
//! dismissing it creates nothing.
//!
//! Hostile input stays bounded: the route reads at most [`MAX_BYTES`]; a line longer than
//! [`MAX_LINE`], more than [`MAX_PEOPLE`] people or [`MAX_FAMILIES`] families, a skipped level or a tree
//! that makes someone their own ancestor is refused. Nothing here recurses: the ancestor check is
//! Kahn's algorithm over people and families, and the relatives are a fixed walk of at most three
//! links, so a deep tree costs time in proportion to its size and no stack.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use chrono::{Datelike, Utc};
use futures::future::BoxFuture;
use serde_json::{Value, json};
use sqlx::PgConnection;
use uuid::Uuid;

use crate::error::{Error, Result};
use crate::family::{self, MemberIn};
use crate::provenance::{SrcIn, SrcRef};
use crate::review::{self, ReviewItem, ReviewItemIn};

/// The review kind a relative from a tree waits as.
pub const KIND: &str = "family_proposal";
/// The source label of a relative accepted from a tree (a format name, the same in every language).
pub const SRC_LABEL: &str = "GEDCOM";
/// The largest file the route reads.
pub const MAX_BYTES: usize = 10 * 1024 * 1024;
/// The longest line: GEDCOM 5.5.1 allows 255 characters; 7.0 sets no limit but splits text with CONT.
pub const MAX_LINE: usize = 32 * 1024;
pub const MAX_PEOPLE: usize = 50_000;
pub const MAX_FAMILIES: usize = 50_000;
/// The most relatives one import proposes; the rest are counted in `left_out`.
pub const MAX_PROPOSALS: usize = 200;
/// The parents of the person whose side of the family (grandparents, aunts and uncles, cousins) is
/// walked. A person has two birth parents; a file may name a few more when parentage is uncertain.
const MAX_SIDES: usize = 8;

// ----------------------------------------------------------------- refusals ----

/// Why a file was refused. The message names a line number at most, never a value of the file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Refusal {
    pub code: &'static str,
    pub line: Option<usize>,
}

impl Refusal {
    fn at(code: &'static str, line: usize) -> Refusal {
        Refusal { code, line: Some(line) }
    }

    fn whole(code: &'static str) -> Refusal {
        Refusal { code, line: None }
    }

    /// The English text; the UI words each code in the person's language.
    pub fn message(&self) -> String {
        let n = self.line.unwrap_or(0);
        match self.code {
            "not_gedcom" => "This file is not a GEDCOM family tree: it does not start with a GEDCOM header.".into(),
            "too_large" => "A family tree file is at most 10 MiB.".into(),
            "line_too_long" => format!("Line {n} is longer than 32 KiB, which no GEDCOM file needs."),
            "bad_line" => format!("Line {n} is not a GEDCOM line."),
            "level_jump" => format!("Line {n} skips a level of the GEDCOM structure."),
            "too_many_people" => "The file holds more than 50,000 people.".into(),
            "too_many_families" => "The file holds more than 50,000 families.".into(),
            "cycle" => "The file makes someone their own ancestor.".into(),
            "no_people" => "The file holds no people.".into(),
            _ => "The file could not be read as a GEDCOM family tree.".into(),
        }
    }
}

/// Every refusal text, for the wording lint (`tests/wording_all.rs`).
pub fn phrases() -> Value {
    let codes = [
        "not_gedcom",
        "too_large",
        "line_too_long",
        "bad_line",
        "level_jump",
        "too_many_people",
        "too_many_families",
        "cycle",
        "no_people",
        "other",
    ];
    json!(codes.iter().map(|c| Refusal::at(c, 7).message()).collect::<Vec<_>>())
}

// --------------------------------------------------------------------- tree ----

/// One INDI record: what the file says about the person's structure, nothing else.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Person {
    /// `female` | `male` | `unknown`
    pub sex: &'static str,
    pub born_year: Option<i32>,
    pub deceased: bool,
    pub death_age: Option<i32>,
}

/// A parsed file: people in file order (their position is the index the person picks by) and the
/// birth links between them.
#[derive(Debug, Clone, Default)]
pub struct Tree {
    /// `5.5.1`, `5.5`, `5.5.5` or `7.0` when the header says so.
    pub version: Option<&'static str>,
    pub people: Vec<Person>,
    /// Per family: its parents and its birth children, sorted.
    parents: Vec<Vec<usize>>,
    children: Vec<Vec<usize>>,
    /// Per person: the families they are a birth child of, and the ones they are a parent in.
    famc: Vec<Vec<usize>>,
    fams: Vec<Vec<usize>>,
}

#[derive(Default)]
struct RawPerson {
    person: Person,
    birth_seen: bool,
    death_year: Option<i32>,
    /// (family xref, birth link?)
    famc: Vec<(Vec<u8>, bool)>,
    fams: Vec<Vec<u8>>,
}

/// GEDCOM gives a family one HUSB and one WIFE: a later one is ignored, so no family has more than two
/// parents and the preview stays in proportion to the file.
#[derive(Default)]
struct RawFamily {
    husb: Option<Vec<u8>>,
    wife: Option<Vec<u8>>,
    children: Vec<Vec<u8>>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Record {
    Head,
    Person(usize),
    Family(usize),
    Other,
    End,
}

/// One line of the file: level, optional xref, tag, optional value. Borrowed from the buffer.
struct Line<'a> {
    level: u32,
    xref: Option<&'a [u8]>,
    tag: &'a [u8],
    value: Option<&'a [u8]>,
}

fn parse_line(raw: &[u8]) -> Option<Line<'_>> {
    // GEDCOM 5.5 allowed leading white space; 7.0 does not, and nothing is lost by allowing it
    let start = raw.iter().position(|b| *b != b' ' && *b != b'\t')?;
    let s = raw.get(start..)?;
    let digits = s.iter().take_while(|b| b.is_ascii_digit()).count();
    if digits == 0 || digits > 2 {
        return None;
    }
    let level = std::str::from_utf8(s.get(..digits)?).ok()?.parse::<u32>().ok()?;
    let mut rest = s.get(digits..)?;
    rest = rest.strip_prefix(b" ")?;
    let xref = if rest.first() == Some(&b'@') {
        let end = rest.iter().position(|b| *b == b' ')?;
        let x = rest.get(..end)?;
        if x.len() < 3 || x.last() != Some(&b'@') {
            return None;
        }
        rest = rest.get(end + 1..)?;
        Some(x)
    } else {
        None
    };
    let tag_len = rest
        .iter()
        .take_while(|b| b.is_ascii_alphanumeric() || **b == b'_')
        .count();
    if tag_len == 0 {
        return None;
    }
    let tag = rest.get(..tag_len)?;
    let value = match rest.get(tag_len..)? {
        [] => None,
        [b' ', v @ ..] => Some(v),
        _ => return None,
    };
    Some(Line {
        level,
        xref,
        tag,
        value,
    })
}

/// `@X1@` as a pointer value; `@VOID@` (GEDCOM 7's "no record") is none.
fn pointer(value: Option<&[u8]>) -> Option<Vec<u8>> {
    let v = value?.trim_ascii();
    if v.len() < 3 || v.first() != Some(&b'@') || v.last() != Some(&b'@') || v == b"@VOID@" {
        return None;
    }
    Some(v.to_vec())
}

fn sex(value: Option<&[u8]>) -> &'static str {
    match value.map(<[u8]>::trim_ascii).and_then(|v| v.first()) {
        Some(b'M' | b'm') => "male",
        Some(b'F' | b'f') => "female",
        _ => "unknown",
    }
}

/// The year of a GEDCOM date, when it names one in the Gregorian or Julian calendar: `14 MAR 1985`,
/// `ABT 1955`, `BET 1927 AND 1929` (the first), `@#DJULIAN@ 1750/51`. None for a bound (`BEF`, `AFT`),
/// a period (`FROM`, `TO`), another calendar, a date before the common era or a phrase alone.
pub fn date_year(value: &[u8]) -> Option<i32> {
    let s = std::str::from_utf8(value).ok()?;
    let mut year: Option<&str> = None;
    for (i, tok) in s.split_ascii_whitespace().enumerate() {
        let t = tok.to_ascii_uppercase();
        match t.as_str() {
            "BEF" | "AFT" | "FROM" | "TO" if i == 0 => return None,
            "ABT" | "CAL" | "EST" | "BET" | "INT" if i == 0 => {}
            "AND" => break,
            "GREGORIAN" | "JULIAN" | "@#DGREGORIAN@" | "@#DJULIAN@" => {}
            "BCE" | "B.C." | "BC" => return None,
            _ if t.starts_with('(') => break,
            _ if t.starts_with('@') || t.starts_with('_') || t == "HEBREW" || t == "FRENCH_R" => return None,
            _ => {
                let head = t.split('/').next().unwrap_or("");
                if !head.is_empty() && head.len() <= 4 && head.bytes().all(|b| b.is_ascii_digit()) {
                    year = Some(tok.split('/').next().unwrap_or(""));
                }
            }
        }
    }
    year?.parse().ok()
}

/// Whole years of a GEDCOM age: `64y`, `64y 3m`, `8d` (0), `INFANT`, `STILLBORN` (0), a bare `64`.
/// None for a bound (`< 1y`, `> 70y`), `CHILD` or anything else.
pub fn age_years(value: &[u8]) -> Option<i32> {
    let s = std::str::from_utf8(value).ok()?.trim();
    if s.starts_with('<') || s.starts_with('>') {
        return None;
    }
    match s.to_ascii_uppercase().as_str() {
        "INFANT" | "STILLBORN" => return Some(0),
        "CHILD" => return None,
        _ => {}
    }
    let mut years = None;
    let mut part = false;
    for tok in s.split_ascii_whitespace() {
        let (num, unit) = tok.split_at(tok.find(|c: char| !c.is_ascii_digit()).unwrap_or(tok.len()));
        if num.is_empty() || num.len() > 3 {
            return None;
        }
        match unit {
            "y" | "Y" | "" => years = Some(num.parse::<i32>().ok()?),
            "m" | "M" | "w" | "W" | "d" | "D" => part = true,
            _ => return None,
        }
    }
    years.or(part.then_some(0))
}

/// Decode UTF-16 (with a byte order mark, or a `0 HEAD` that shows one) to UTF-8; anything else is read
/// as bytes, since only ASCII tags, levels and pointers matter.
fn utf8(bytes: &[u8]) -> std::borrow::Cow<'_, [u8]> {
    let le = match bytes {
        [0xFF, 0xFE, ..] | [b'0', 0, ..] => Some(true),
        [0xFE, 0xFF, ..] | [0, b'0', ..] => Some(false),
        _ => None,
    };
    let Some(le) = le else {
        return std::borrow::Cow::Borrowed(bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(bytes));
    };
    let units = bytes.as_chunks::<2>().0.iter().map(|&pair| {
        if le {
            u16::from_le_bytes(pair)
        } else {
            u16::from_be_bytes(pair)
        }
    });
    let text: String = char::decode_utf16(units)
        .map(|c| c.unwrap_or(char::REPLACEMENT_CHARACTER))
        .collect();
    let text = text.strip_prefix('\u{FEFF}').unwrap_or(&text).to_string();
    std::borrow::Cow::Owned(text.into_bytes())
}

/// Parse a file in memory. Nothing of it is kept but the structure in [`Tree`].
pub fn parse(bytes: &[u8]) -> std::result::Result<Tree, Refusal> {
    if bytes.len() > MAX_BYTES {
        return Err(Refusal::whole("too_large"));
    }
    let data = utf8(bytes);
    let mut people: Vec<RawPerson> = Vec::new();
    let mut families: Vec<RawFamily> = Vec::new();
    let mut person_ids: HashMap<Vec<u8>, usize> = HashMap::new();
    let mut family_ids: HashMap<Vec<u8>, usize> = HashMap::new();
    let mut version: Option<&'static str> = None;
    let mut record = Record::Other;
    let mut prev_level: Option<u32> = None;
    // the level-1 tag the current line sits under
    let mut l1: &[u8] = b"";

    let mut n = 0usize;
    let mut rest: &[u8] = &data;
    while !rest.is_empty() && record != Record::End {
        let end = rest
            .iter()
            .position(|b| *b == b'\n' || *b == b'\r')
            .unwrap_or(rest.len());
        let raw = rest.get(..end).unwrap_or_default();
        let mut next = end;
        if rest.get(end) == Some(&b'\r') && rest.get(end + 1) == Some(&b'\n') {
            next += 1;
        }
        rest = rest.get(next + 1..).unwrap_or_default();
        n += 1;
        if raw.len() > MAX_LINE {
            return Err(Refusal::at("line_too_long", n));
        }
        if raw.iter().all(|b| *b == b' ' || *b == b'\t') {
            continue;
        }
        let Some(line) = parse_line(raw) else {
            return Err(if prev_level.is_none() {
                Refusal::whole("not_gedcom")
            } else {
                Refusal::at("bad_line", n)
            });
        };
        match prev_level {
            None if line.level != 0 || line.tag != b"HEAD" => return Err(Refusal::whole("not_gedcom")),
            Some(p) if line.level > p + 1 => return Err(Refusal::at("level_jump", n)),
            _ => {}
        }
        prev_level = Some(line.level);
        match line.level {
            0 => {
                l1 = b"";
                record = match line.tag {
                    b"HEAD" => Record::Head,
                    b"TRLR" => Record::End,
                    b"INDI" => {
                        if people.len() >= MAX_PEOPLE {
                            return Err(Refusal::whole("too_many_people"));
                        }
                        let i = people.len();
                        people.push(RawPerson::default());
                        if let Some(x) = line.xref {
                            person_ids.entry(x.to_vec()).or_insert(i);
                        }
                        Record::Person(i)
                    }
                    b"FAM" => {
                        if families.len() >= MAX_FAMILIES {
                            return Err(Refusal::whole("too_many_families"));
                        }
                        let i = families.len();
                        families.push(RawFamily::default());
                        if let Some(x) = line.xref {
                            family_ids.entry(x.to_vec()).or_insert(i);
                        }
                        Record::Family(i)
                    }
                    _ => Record::Other,
                };
            }
            1 => {
                l1 = line.tag;
                match record {
                    Record::Person(i) => {
                        let Some(p) = people.get_mut(i) else { continue };
                        match line.tag {
                            b"SEX" => p.person.sex = sex(line.value),
                            b"DEAT" => p.person.deceased = true,
                            b"FAMC" => {
                                if let Some(f) = pointer(line.value) {
                                    p.famc.push((f, true));
                                } else {
                                    // a FAMC to nowhere: its PEDI below must not mark another link
                                    l1 = b"";
                                }
                            }
                            b"FAMS" => p.fams.extend(pointer(line.value)),
                            _ => {}
                        }
                    }
                    Record::Family(i) => {
                        let Some(f) = families.get_mut(i) else { continue };
                        match line.tag {
                            b"HUSB" if f.husb.is_none() => f.husb = pointer(line.value),
                            b"WIFE" if f.wife.is_none() => f.wife = pointer(line.value),
                            b"CHIL" => f.children.extend(pointer(line.value)),
                            _ => {}
                        }
                    }
                    _ => {}
                }
            }
            2 => match record {
                Record::Person(i) => {
                    let Some(p) = people.get_mut(i) else { continue };
                    match (l1, line.tag) {
                        (b"BIRT", b"DATE") if !p.birth_seen => {
                            p.birth_seen = true;
                            p.person.born_year = line.value.and_then(date_year);
                        }
                        (b"DEAT", b"DATE") if p.death_year.is_none() => {
                            p.death_year = line.value.and_then(date_year);
                        }
                        (b"DEAT", b"AGE") if p.person.death_age.is_none() => {
                            p.person.death_age = line.value.and_then(age_years);
                        }
                        (b"FAMC", b"PEDI") => {
                            let birth = line
                                .value
                                .map(<[u8]>::trim_ascii)
                                .is_none_or(|v| v.eq_ignore_ascii_case(b"BIRTH"));
                            if let Some(link) = p.famc.last_mut() {
                                link.1 = birth;
                            }
                        }
                        _ => {}
                    }
                }
                Record::Head if l1 == b"GEDC" && line.tag == b"VERS" => {
                    version = line.value.and_then(gedcom_version);
                }
                _ => {}
            },
            _ => {}
        }
    }
    if people.is_empty() {
        return Err(Refusal::whole("no_people"));
    }
    resolve(version, people, families, &person_ids, &family_ids)
}

fn gedcom_version(v: &[u8]) -> Option<&'static str> {
    match v.trim_ascii() {
        b"5.5.1" => Some("5.5.1"),
        b"5.5" => Some("5.5"),
        b"5.5.5" => Some("5.5.5"),
        v if v.starts_with(b"7.0") => Some("7.0"),
        _ => None,
    }
}

fn resolve(
    version: Option<&'static str>,
    raw_people: Vec<RawPerson>,
    raw_families: Vec<RawFamily>,
    person_ids: &HashMap<Vec<u8>, usize>,
    family_ids: &HashMap<Vec<u8>, usize>,
) -> std::result::Result<Tree, Refusal> {
    let nf = raw_families.len();
    let mut parents: Vec<BTreeSet<usize>> = vec![BTreeSet::new(); nf];
    let mut children: Vec<BTreeSet<usize>> = vec![BTreeSet::new(); nf];
    // a child link the person's own record marks as not by birth (adopted, foster, sealing), unless
    // another FAMC to the same family says birth
    let mut not_birth: BTreeSet<(usize, usize)> = BTreeSet::new();
    let mut birth: BTreeSet<(usize, usize)> = BTreeSet::new();
    for (f, fam) in raw_families.iter().enumerate() {
        for x in fam.husb.iter().chain(&fam.wife) {
            if let (Some(&p), Some(set)) = (person_ids.get(x), parents.get_mut(f)) {
                set.insert(p);
            }
        }
        for x in &fam.children {
            if let (Some(&c), Some(set)) = (person_ids.get(x), children.get_mut(f)) {
                set.insert(c);
            }
        }
    }
    let mut people = Vec::with_capacity(raw_people.len());
    for (i, raw) in raw_people.into_iter().enumerate() {
        // a FAMS fills a family's parents only up to two, after the family's own HUSB and WIFE
        for x in &raw.fams {
            if let Some(&f) = family_ids.get(x)
                && let Some(set) = parents.get_mut(f)
                && set.len() < 2
            {
                set.insert(i);
            }
        }
        for (x, by_birth) in &raw.famc {
            if let Some(&f) = family_ids.get(x) {
                if *by_birth {
                    birth.insert((i, f));
                    if let Some(set) = children.get_mut(f) {
                        set.insert(i);
                    }
                } else {
                    not_birth.insert((i, f));
                }
            }
        }
        let mut person = raw.person;
        if person.sex.is_empty() {
            person.sex = "unknown";
        }
        if person.death_age.is_none()
            && let (Some(b), Some(d)) = (person.born_year, raw.death_year)
            && (0..=130).contains(&(d - b))
        {
            person.death_age = Some(d - b);
        }
        people.push(person);
    }
    for (i, f) in &not_birth {
        if !birth.contains(&(*i, *f))
            && let Some(set) = children.get_mut(*f)
        {
            set.remove(i);
        }
    }
    let np = people.len();
    let mut famc = vec![Vec::new(); np];
    let mut fams = vec![Vec::new(); np];
    for f in 0..nf {
        for &p in parents.get(f).into_iter().flatten() {
            if let Some(v) = fams.get_mut(p) {
                v.push(f);
            }
        }
        for &c in children.get(f).into_iter().flatten() {
            if let Some(v) = famc.get_mut(c) {
                v.push(f);
            }
        }
    }
    let tree = Tree {
        version,
        people,
        parents: parents.into_iter().map(|s| s.into_iter().collect()).collect(),
        children: children.into_iter().map(|s| s.into_iter().collect()).collect(),
        famc,
        fams,
    };
    if tree.has_cycle() {
        return Err(Refusal::whole("cycle"));
    }
    Ok(tree)
}

impl Tree {
    /// Kahn's algorithm over people (0..n) and families (n..n+m): a parent points to their family, a
    /// family to its children. Anything left over sits on a cycle.
    fn has_cycle(&self) -> bool {
        let np = self.people.len();
        let nf = self.parents.len();
        let mut indegree = vec![0usize; np + nf];
        for f in 0..nf {
            if let Some(d) = indegree.get_mut(np + f) {
                *d = self.parents.get(f).map_or(0, Vec::len);
            }
            for &c in self.children.get(f).into_iter().flatten() {
                if let Some(d) = indegree.get_mut(c) {
                    *d += 1;
                }
            }
        }
        let mut queue: Vec<usize> = (0..np + nf).filter(|&v| indegree.get(v) == Some(&0)).collect();
        let mut done = 0usize;
        while let Some(v) = queue.pop() {
            done += 1;
            let next: &[usize] = if v < np {
                self.fams.get(v).map_or(&[], Vec::as_slice)
            } else {
                self.children.get(v - np).map_or(&[], Vec::as_slice)
            };
            for &w in next {
                let w = if v < np { np + w } else { w };
                if let Some(d) = indegree.get_mut(w) {
                    *d = d.saturating_sub(1);
                    if *d == 0 {
                        queue.push(w);
                    }
                }
            }
        }
        done < np + nf
    }

    fn of(lists: &[Vec<usize>], i: usize) -> &[usize] {
        let list: &[usize] = lists.get(i).map_or(&[], Vec::as_slice);
        #[cfg(test)]
        tests::VISITED.with(|v| v.set(v.get().saturating_add(list.len())));
        list
    }

    pub fn parents_of(&self, i: usize) -> BTreeSet<usize> {
        Self::of(&self.famc, i)
            .iter()
            .flat_map(|&f| Self::of(&self.parents, f).iter().copied())
            .filter(|&p| p != i)
            .collect()
    }

    pub fn children_of(&self, i: usize) -> BTreeSet<usize> {
        Self::of(&self.fams, i)
            .iter()
            .flat_map(|&f| Self::of(&self.children, f).iter().copied())
            .filter(|&c| c != i)
            .collect()
    }

    pub fn partners_of(&self, i: usize) -> BTreeSet<usize> {
        Self::of(&self.fams, i)
            .iter()
            .flat_map(|&f| Self::of(&self.parents, f).iter().copied())
            .filter(|&p| p != i)
            .collect()
    }

    /// Children of the same family as `i`.
    fn full_siblings(&self, i: usize) -> BTreeSet<usize> {
        Self::of(&self.famc, i)
            .iter()
            .flat_map(|&f| Self::of(&self.children, f).iter().copied())
            .filter(|&c| c != i)
            .collect()
    }

    /// Children of another family of one of `i`'s parents.
    fn half_siblings(&self, i: usize) -> BTreeSet<usize> {
        // a set: a person who is the child of thousands of families must not scan them per parent
        let own: BTreeSet<usize> = Self::of(&self.famc, i).iter().copied().collect();
        let full = self.full_siblings(i);
        self.parents_of(i)
            .iter()
            .flat_map(|&p| Self::of(&self.fams, p).iter().copied())
            .filter(|f| !own.contains(f))
            .flat_map(|f| Self::of(&self.children, f).iter().copied())
            .filter(|c| *c != i && !full.contains(c))
            .collect()
    }

    fn parent_siblings(&self, sides: &[(usize, bool)]) -> Vec<(usize, bool)> {
        let mut siblings = Vec::new();
        let mut seen = BTreeSet::new();
        // Keep the first occurrence so a relative on both sides keeps the same relationship code.
        for &(p, paternal) in sides {
            for &sibling in self.full_siblings(p).union(&self.half_siblings(p)) {
                let pair = (sibling, paternal);
                if seen.insert(pair) {
                    siblings.push(pair);
                }
            }
        }
        siblings
    }

    fn sex(&self, i: usize) -> &'static str {
        self.people.get(i).map_or("unknown", |p| p.sex)
    }

    /// Each relative of `me` that the relationship catalog has a code for, closest first, each person
    /// once: parents, partners, children, siblings, half-siblings, grandparents, grandchildren, aunts
    /// and uncles, nephews and nieces, cousins and parents-in-law. A relative whose code depends on a
    /// sex the file does not give (a grandparent of unknown sex) is left out.
    pub fn relatives(&self, me: usize) -> Vec<(usize, &'static str)> {
        let mut out: Vec<(usize, &'static str)> = Vec::new();
        let mut seen: BTreeSet<usize> = BTreeSet::from([me]);
        let mut add = |who: usize, code: Option<&'static str>| {
            if let Some(code) = code
                && seen.insert(who)
            {
                out.push((who, code));
            }
        };
        let by_sex =
            |who: usize, male: &'static str, female: &'static str, unknown: Option<&'static str>| match self.sex(who) {
                "male" => Some(male),
                "female" => Some(female),
                _ => unknown,
            };
        let parents = self.parents_of(me);
        for &p in &parents {
            add(p, by_sex(p, "FTH", "MTH", Some("PRN")));
        }
        let partners = self.partners_of(me);
        for &s in &partners {
            add(s, Some("SPS"));
        }
        let children = self.children_of(me);
        for &c in &children {
            add(c, by_sex(c, "SON", "DAU", Some("CHILD")));
        }
        let full = self.full_siblings(me);
        for &s in &full {
            add(s, by_sex(s, "BRO", "SIS", Some("SIB")));
        }
        let half = self.half_siblings(me);
        for &s in &half {
            add(s, by_sex(s, "HBRO", "HSIS", Some("HSIB")));
        }
        // the side of the family: through the father (paternal) or the mother (maternal). Each side walks
        // that parent's parents and siblings, so only the first MAX_SIDES parents get one: a file that
        // gives a person thousands of parents would otherwise walk every family once per parent. Their
        // relatives past the cap count as people with no relationship we record.
        let sides: Vec<(usize, bool)> = parents
            .iter()
            .filter_map(|&p| match self.sex(p) {
                "male" => Some((p, true)),
                "female" => Some((p, false)),
                _ => None,
            })
            .take(MAX_SIDES)
            .collect();
        for &(p, paternal) in &sides {
            for g in self.parents_of(p) {
                let code = if paternal {
                    by_sex(g, "PGRFTH", "PGRMTH", None)
                } else {
                    by_sex(g, "MGRFTH", "MGRMTH", None)
                };
                add(g, code);
            }
        }
        for &c in &children {
            for g in self.children_of(c) {
                add(g, by_sex(g, "GRNDSON", "GRNDDAU", None));
            }
        }
        let aunts = self.parent_siblings(&sides);
        for &(a, paternal) in &aunts {
            let code = if paternal {
                by_sex(a, "PUNCLE", "PAUNT", None)
            } else {
                by_sex(a, "MUNCLE", "MAUNT", None)
            };
            add(a, code);
        }
        for s in full.union(&half) {
            for c in self.children_of(*s) {
                add(c, by_sex(c, "NEPHEW", "NIECE", None));
            }
        }
        for (a, paternal) in aunts {
            for c in self.children_of(a) {
                add(c, Some(if paternal { "PCOUSN" } else { "MCOUSN" }));
            }
        }
        for &s in &partners {
            for p in self.parents_of(s) {
                add(p, by_sex(p, "FTHINLAW", "MTHINLAW", None));
            }
        }
        out
    }

    /// The picker's view of the file: per person, by position, the sex, birth year, death and links to
    /// other positions. No name: the browser reads names from the person's own copy of the file.
    pub fn people_out(&self) -> Vec<Value> {
        (0..self.people.len())
            .map(|i| {
                let p = self.people.get(i).cloned().unwrap_or_default();
                json!({
                    "i": i,
                    "sex": if p.sex.is_empty() { "unknown" } else { p.sex },
                    "born_year": p.born_year,
                    "deceased": p.deceased,
                    "death_age": p.death_age,
                    "parents": self.parents_of(i),
                    "partners": self.partners_of(i),
                    "children": self.children_of(i),
                })
            })
            .collect()
    }
}

// --------------------------------------------------------------- proposals ----

/// What an import did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Outcome {
    /// The new review items, closest relative first.
    pub proposed: Vec<Uuid>,
    /// Relatives already proposed and open, or already recorded with the same relationship, sex and
    /// birth year.
    pub already: usize,
    /// Relatives past [`MAX_PROPOSALS`].
    pub left_out: usize,
    /// People of the file the catalog has no relationship for (in-laws of relatives, distant kin), and
    /// relatives that could not be recorded.
    pub outside: usize,
}

/// A person born more than this many years ago has died, even when the file records no death. Trees
/// often leave the death out for old ancestors. Gramps uses the same age for "probably alive".
const PROBABLY_DEAD_AFTER: i32 = 110;

/// The member a relative of the file would be, with values `family::MemberIn` accepts.
fn member(tree: &Tree, profile_id: Uuid, who: usize, code: &str) -> MemberIn {
    let p = tree.people.get(who).cloned().unwrap_or_default();
    let this_year = Utc::now().year();
    let mut m = MemberIn::new(profile_id, code);
    m.sex_at_birth = Some(if p.sex.is_empty() { "unknown" } else { p.sex }.to_string());
    m.born_year = p.born_year.filter(|y| (1800..=this_year).contains(y));
    m.deceased = p.deceased || p.born_year.is_some_and(|y| this_year - y > PROBABLY_DEAD_AFTER);
    m.death_age = p.death_age.filter(|a| (0..=130).contains(a)).filter(|_| p.deceased);
    m
}

type Key = (String, Option<String>, Option<i32>, bool, Option<i32>);

fn key_of(m: &MemberIn) -> Key {
    (
        m.relationship.clone(),
        m.sex_at_birth.clone(),
        m.born_year,
        m.deceased,
        m.death_age,
    )
}

/// The payload of a proposal: the relationship and its names, and the structure fields. Nothing else
/// of the file.
fn payload(m: &MemberIn, version: Option<&str>) -> Result<Value> {
    let cat = family::catalog()?;
    let rel = cat
        .relationship(&m.relationship)
        .ok_or_else(|| Error::Invalid(format!("relationship: unknown code {:?}", m.relationship)))?;
    let label: serde_json::Map<String, Value> = rel.names().iter().map(|(l, t)| ((*l).to_string(), json!(t))).collect();
    Ok(json!({
        "source": "gedcom",
        "version": version,
        "relationship": m.relationship,
        "label": label,
        "sex_at_birth": m.sex_at_birth,
        "born_year": m.born_year,
        "deceased": m.deceased,
        "death_age": m.death_age,
    }))
}

/// Queue the relatives of `me` as `family_proposal` items on the profile. The caller has checked access.
/// `recorded` is the profile's relatives as the caller's family list shows them, with a birth year: a
/// sensitive row the list leaves out, or a linked row whose birth year it masks, is not in it. A value
/// the list hides from the caller must not change what the import answers.
pub async fn propose(
    conn: &mut PgConnection,
    household_id: Uuid,
    profile_id: Uuid,
    tree: &Tree,
    me: usize,
    recorded: &[family::Member],
) -> Result<Outcome> {
    if me >= tree.people.len() {
        return Err(Error::Invalid("self: no person at that position in the file".into()));
    }
    // two imports of one profile at once would both see no open proposal and queue the same relatives
    sqlx::query("select pg_advisory_xact_lock(hashtextextended($1, 24))")
        .bind(format!("gedcom:{profile_id}"))
        .execute(&mut *conn)
        .await?;
    // each open proposal and each recorded relative stands for one person: two brothers with the same
    // key are two people, so these count, and a match uses one up
    let mut known: BTreeMap<Key, usize> = BTreeMap::new();
    let open: Vec<Value> =
        sqlx::query_scalar("select payload from review_item where kind = $1 and profile_id = $2 and status = 'open'")
            .bind(KIND)
            .bind(profile_id)
            .fetch_all(&mut *conn)
            .await?;
    for p in open {
        if let Ok(m) = member_from(profile_id, &p) {
            *known.entry(key_of(&m)).or_default() += 1;
        }
    }
    // a recorded relative with the same relationship, sex and birth year is the same person
    let mut seen: BTreeMap<(String, Option<String>, Option<i32>), usize> = BTreeMap::new();
    for m in recorded {
        *seen
            .entry((m.relationship.clone(), m.sex_at_birth.clone(), m.born_year))
            .or_default() += 1;
    }
    let relatives = tree.relatives(me);
    let mut out = Outcome {
        outside: tree.people.len().saturating_sub(1 + relatives.len()),
        ..Outcome::default()
    };
    for (who, code) in relatives {
        let m = member(tree, profile_id, who, code);
        let Ok(m) = m.normalized() else {
            out.outside += 1;
            continue;
        };
        let k = key_of(&m);
        if take(&mut known, &k) || take(&mut seen, &(k.0.clone(), k.1.clone(), k.2)) {
            out.already += 1;
            continue;
        }
        if out.proposed.len() >= MAX_PROPOSALS {
            out.left_out += 1;
            continue;
        }
        let id = review::create_item(
            &mut *conn,
            &ReviewItemIn {
                kind: KIND.into(),
                household_id: Some(household_id),
                profile_id: Some(profile_id),
                payload: payload(&m, tree.version)?,
                ..ReviewItemIn::default()
            },
        )
        .await?;
        // one import's items share the transaction's now(): give each its own time, so the queue
        // (ordered by created_at) shows them closest relative first, as they were made
        sqlx::query("update review_item set created_at = clock_timestamp() where id = $1")
            .bind(id)
            .execute(&mut *conn)
            .await?;
        out.proposed.push(id);
    }
    Ok(out)
}

/// Use up one of the rows counted under `k`; false when none is left.
fn take<K: Ord>(counts: &mut BTreeMap<K, usize>, k: &K) -> bool {
    match counts.get_mut(k) {
        Some(n) if *n > 0 => {
            *n -= 1;
            true
        }
        _ => false,
    }
}

/// The member a proposal's payload describes.
fn member_from(profile_id: Uuid, p: &Value) -> Result<MemberIn> {
    let bad = || Error::Review("this family proposal is not readable".into());
    let relationship = p.get("relationship").and_then(Value::as_str).ok_or_else(bad)?;
    let mut m = MemberIn::new(profile_id, relationship);
    m.sex_at_birth = p.get("sex_at_birth").and_then(Value::as_str).map(str::to_string);
    m.born_year = p
        .get("born_year")
        .and_then(Value::as_i64)
        .and_then(|y| i32::try_from(y).ok());
    m.deceased = p.get("deceased").and_then(Value::as_bool).unwrap_or(false);
    m.death_age = p
        .get("death_age")
        .and_then(Value::as_i64)
        .and_then(|a| i32::try_from(a).ok());
    m.normalized()
}

/// Accept a `family_proposal`: one new relative, entered by the person who accepts, with the source
/// label `GEDCOM`. A proposal is accepted as it is; a correction is a new version afterwards.
pub fn accept_proposal_boxed<'c>(
    conn: &'c mut PgConnection,
    item: &'c ReviewItem,
    user_id: Uuid,
    edits: Option<&'c Value>,
) -> BoxFuture<'c, Result<Vec<Uuid>>> {
    Box::pin(async move {
        if edits.is_some() {
            return Err(Error::Review(
                "a family proposal is accepted as it is; the relative can be corrected after".into(),
            ));
        }
        let profile_id = item
            .profile_id
            .ok_or_else(|| Error::Review("this family proposal has no profile".into()))?;
        let m = member_from(profile_id, &item.payload)?;
        let src = SrcRef::New(SrcIn {
            profile_id: Some(profile_id),
            label: Some(SRC_LABEL.into()),
            ..SrcIn::manual(Some(user_id))
        });
        let done = family::add(conn, &m, &src, "person", None, None).await?;
        Ok(vec![done.id])
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const F551: &[u8] = include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../../fixtures/gedcom/rivera-551.ged"
    ));
    const F70: &[u8] = include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../../fixtures/gedcom/rivera-70.ged"
    ));

    thread_local! {
        /// Family members the relationship walk has read on this thread, so a test can bound the work
        /// a hostile file costs without timing it.
        pub(super) static VISITED: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    }

    fn codes(t: &Tree, me: usize) -> Vec<&'static str> {
        t.relatives(me).into_iter().map(|(_, c)| c).collect()
    }

    #[test]
    fn dates_give_a_year_only_when_they_name_one() {
        for (v, y) in [
            ("14 MAR 1985", Some(1985)),
            ("ABT 1955", Some(1955)),
            ("BET 1927 AND 1929", Some(1927)),
            ("@#DGREGORIAN@ 1930", Some(1930)),
            ("JULIAN 1750/51", Some(1750)),
            ("@#DJULIAN@ 3 FEB 1750/51", Some(1750)),
            ("INT 1900 (about then)", Some(1900)),
            ("MAR 1901", Some(1901)),
            ("BEF 1900", None),
            ("AFT 1900", None),
            ("FROM 1700 TO 1800", None),
            ("HEBREW 5780", None),
            ("@#DFRENCH R@ 1 VEND 12", None),
            ("BET FRENCH_R 2 _JOUR 8 AND _CALENDRIER 4 COMP 8", None),
            ("_CALENDAR 8 _MONTH 190 _EPOCH", None),
            ("44 BCE", None),
            ("(sometime in spring)", None),
            ("", None),
        ] {
            assert_eq!(date_year(v.as_bytes()), y, "{v}");
        }
        assert_eq!(date_year(b"\xff\xfe 1900"), None);
    }

    #[test]
    fn ages_give_whole_years() {
        for (v, a) in [
            ("64y", Some(64)),
            ("64y 3m", Some(64)),
            ("8d", Some(0)),
            ("3m 2w", Some(0)),
            ("64", Some(64)),
            ("INFANT", Some(0)),
            ("STILLBORN", Some(0)),
            ("CHILD", None),
            ("> 70y", None),
            ("< 1y", None),
            ("old", None),
            ("123456y", None),
        ] {
            assert_eq!(age_years(v.as_bytes()), a, "{v}");
        }
    }

    #[test]
    fn the_rivera_551_tree_places_each_relative_once() {
        let t = parse(F551).expect("parses");
        assert_eq!(t.version, Some("5.5.1"));
        assert_eq!(t.people.len(), 19);
        let alex = 0;
        assert_eq!(
            codes(&t, alex),
            [
                "FTH", "MTH", "SPS", "DAU", "SIS", "HBRO", "PGRFTH", "PGRMTH", "MGRMTH", "MGRFTH", "PUNCLE", "NEPHEW",
                "PCOUSN", "FTHINLAW"
            ]
        );
        // Tomás: ABT 1955, 64y at death; Jorge: 1925 to 1990 gives 65; Elena: BET 1927 AND 1929
        let father = &t.people[1];
        assert_eq!(
            (father.sex, father.born_year, father.deceased, father.death_age),
            ("male", Some(1955), true, Some(64))
        );
        assert_eq!(t.people[3].death_age, Some(65));
        assert_eq!(t.people[4].born_year, Some(1927));
        // Pablo, adopted (PEDI adopted), is not a sibling by birth
        assert!(!t.relatives(alex).iter().any(|(who, _)| *who == 8));
    }

    #[test]
    fn parent_siblings_stay_unique_per_side_and_keep_relationship_precedence() {
        use std::fmt::Write as _;

        let mut file = String::from("0 HEAD\n1 GEDC\n2 VERS 7.0\n");
        for (i, sex) in ["U", "M", "M", "F", "F", "U"].into_iter().enumerate() {
            writeln!(file, "0 @I{i}@ INDI\n1 SEX {sex}").expect("write a synthetic record");
        }
        file.push_str(concat!(
            "0 @F0@ FAM\n1 CHIL @I1@\n1 CHIL @I2@\n1 CHIL @I3@\n1 CHIL @I4@\n",
            "0 @F1@ FAM\n1 HUSB @I1@\n1 WIFE @I3@\n1 CHIL @I0@\n",
            "0 @F2@ FAM\n1 HUSB @I2@\n1 CHIL @I0@\n",
            "0 @F3@ FAM\n1 WIFE @I4@\n1 CHIL @I5@\n0 TRLR\n",
        ));
        let tree = parse(file.as_bytes()).expect("small family tree parses");
        let relatives = tree.parent_siblings(&[(1, true), (2, true), (3, false)]);
        assert_eq!(
            relatives,
            [
                (2, true),
                (3, true),
                (4, true),
                (1, true),
                (1, false),
                (2, false),
                (4, false)
            ]
        );
        assert_eq!(
            tree.relatives(0),
            [(1, "FTH"), (2, "FTH"), (3, "MTH"), (4, "PAUNT"), (5, "PCOUSN")]
        );
    }

    #[test]
    fn the_gedcom_7_maximal70_shape_parses() {
        let t = parse(F70).expect("parses");
        assert_eq!(t.version, Some("7.0"));
        // @I1@ @I2@ @I3@ @I4@ @XI1@ and one INDI record without an xref
        assert_eq!(t.people.len(), 6);
        let sexes: Vec<&str> = t.people.iter().map(|p| p.sex).collect();
        assert_eq!(sexes, ["male", "female", "unknown", "unknown", "unknown", "unknown"]);
        // @I1@: born 1 JAN 2000, died 28 MAR 2022; FAMC @F2@ twice (ADOPTED and BIRTH) counts as birth
        assert_eq!(t.people[0].born_year, Some(2000));
        assert_eq!(t.people[0].death_age, Some(22));
        assert_eq!(codes(&t, 3), ["FTH", "MTH"]);
        assert_eq!(codes(&t, 0), ["SPS", "CHILD"]);
        // the xref-less INDI: a French Republican birth and an extension calendar death give no year
        assert_eq!(t.people[5].born_year, None);
        assert!(t.people[5].deceased);
    }

    #[test]
    fn line_endings_bom_and_utf16_read_the_same() {
        let crlf: Vec<u8> = F551
            .iter()
            .flat_map(|b| if *b == b'\n' { vec![b'\r', b'\n'] } else { vec![*b] })
            .collect();
        let cr: Vec<u8> = F551.iter().map(|b| if *b == b'\n' { b'\r' } else { *b }).collect();
        let text = std::str::from_utf8(F551).expect("utf-8");
        let mut le = vec![0xFF, 0xFE];
        le.extend(text.encode_utf16().flat_map(u16::to_le_bytes));
        let mut be = vec![0xFE, 0xFF];
        be.extend(text.encode_utf16().flat_map(u16::to_be_bytes));
        let want = codes(&parse(F551).expect("lf"), 0);
        for (what, bytes) in [("crlf", crlf), ("cr", cr), ("utf16le", le), ("utf16be", be)] {
            assert_eq!(codes(&parse(&bytes).expect(what), 0), want, "{what}");
        }
    }

    #[test]
    fn hostile_files_are_refused_with_a_line_number_and_no_value() {
        let secret = "Vexlorn";
        let long = format!(
            "0 HEAD\n1 NOTE {}\n0 TRLR\n",
            secret.repeat(MAX_LINE / secret.len() + 1)
        );
        let cases: Vec<(&str, Vec<u8>)> = vec![
            ("not_gedcom", b"%PDF-1.7 Vexlorn".to_vec()),
            ("not_gedcom", b"1 HEAD\n".to_vec()),
            ("line_too_long", long.into_bytes()),
            ("bad_line", b"0 HEAD\nVexlorn was here\n".to_vec()),
            ("bad_line", b"0 HEAD\n1 NA\xffME Vexlorn\n".to_vec()),
            ("bad_line", b"0 HEAD\n0 @I1 INDI\n".to_vec()),
            ("level_jump", b"0 HEAD\n0 @I1@ INDI\n3 NAME Vexlorn\n".to_vec()),
            ("no_people", b"0 HEAD\n1 NOTE Vexlorn\n0 TRLR\n".to_vec()),
            (
                "cycle",
                b"0 HEAD\n0 @I1@ INDI\n1 NAME Vexlorn\n1 FAMC @F1@\n1 FAMS @F1@\n0 @F1@ FAM\n0 TRLR\n".to_vec(),
            ),
            (
                "cycle",
                b"0 HEAD\n0 @A@ INDI\n0 @B@ INDI\n0 @F1@ FAM\n1 HUSB @A@\n1 CHIL @B@\n0 @F2@ FAM\n1 HUSB @B@\n1 CHIL @A@\n"
                    .to_vec(),
            ),
            ("too_large", vec![b' '; MAX_BYTES + 1]),
        ];
        for (code, bytes) in cases {
            let r = parse(&bytes).expect_err(code);
            assert_eq!(r.code, code);
            assert!(!r.message().contains(secret), "{}", r.message());
        }
        let mut many = b"0 HEAD\n".to_vec();
        for _ in 0..=MAX_PEOPLE {
            many.extend_from_slice(b"0 INDI\n");
        }
        assert_eq!(parse(&many).map(|_| ()).expect_err("people").code, "too_many_people");
    }

    #[test]
    fn a_deep_tree_parses_without_recursion() {
        // 40,000 generations, each the only child of the one before
        let gens = 40_000;
        let mut f = String::from("0 HEAD\n");
        for i in 0..gens {
            f.push_str(&format!("0 @I{i}@ INDI\n1 SEX M\n"));
            if i > 0 {
                f.push_str(&format!("1 FAMC @F{}@\n", i - 1));
            }
            f.push_str(&format!("1 FAMS @F{i}@\n0 @F{i}@ FAM\n1 HUSB @I{i}@\n"));
        }
        let t = parse(f.as_bytes()).expect("deep");
        assert_eq!(t.people.len(), gens);
        assert_eq!(codes(&t, gens / 2), ["FTH", "SON", "PGRFTH", "GRNDSON"]);
    }

    #[test]
    fn the_first_birth_date_counts_and_a_famc_to_nowhere_marks_no_link() {
        // two births: the first one is the birth year
        let t = parse(b"0 HEAD\n0 @A@ INDI\n1 BIRT\n2 DATE 1950\n1 BIRT\n2 DATE 1960\n0 TRLR\n").expect("parses");
        assert_eq!(t.people[0].born_year, Some(1950));
        // a FAMC with no pointer after a birth FAMC: its PEDI adopted must not mark the birth link
        let t = parse(
            b"0 HEAD\n0 @C@ INDI\n1 FAMC @F@\n1 FAMC\n2 PEDI adopted\n0 @P@ INDI\n1 SEX M\n\
              0 @F@ FAM\n1 HUSB @P@\n1 CHIL @C@\n0 TRLR\n",
        )
        .expect("parses");
        assert_eq!(codes(&t, 0), ["FTH"]);
    }

    #[test]
    fn a_family_keeps_one_husband_and_one_wife_so_the_preview_stays_linear() {
        // one family that 2,000 people name as their own: only the first two are its parents
        let n = 2000;
        let mut f = String::from("0 HEAD\n");
        for i in 0..n {
            f.push_str(&format!("0 @I{i}@ INDI\n1 FAMS @F@\n"));
        }
        f.push_str("0 @F@ FAM\n0 TRLR\n");
        let t = parse(f.as_bytes()).expect("parses");
        let size = serde_json::to_vec(&t.people_out()).map_or(0, |v| v.len());
        assert!(size < n * 200, "{size} bytes for {n} people");
        assert_eq!(t.partners_of(0), BTreeSet::from([1]));
        assert!(t.partners_of(2).is_empty());
        // the family's own record wins: its first HUSB and first WIFE, then no one else
        let t = parse(
            b"0 HEAD\n0 @A@ INDI\n0 @B@ INDI\n0 @C@ INDI\n0 @D@ INDI\n0 @E@ INDI\n1 FAMS @F@\n\
              0 @F@ FAM\n1 HUSB @A@\n1 HUSB @B@\n1 WIFE @C@\n1 WIFE @D@\n0 TRLR\n",
        )
        .expect("parses");
        assert_eq!(t.partners_of(0), BTreeSet::from([2]));
        assert!(t.partners_of(1).is_empty() && t.partners_of(4).is_empty());
    }

    #[test]
    fn someone_born_more_than_110_years_ago_is_proposed_as_deceased() {
        let year = Utc::now().year();
        let f = format!(
            "0 HEAD\n0 @A@ INDI\n1 BIRT\n2 DATE {}\n0 @B@ INDI\n1 BIRT\n2 DATE {}\n0 @C@ INDI\n1 BIRT\n2 DATE 1750\n0 TRLR\n",
            year - 111,
            year - 110
        );
        let t = parse(f.as_bytes()).expect("parses");
        let old = member(&t, Uuid::nil(), 0, "PGRFTH");
        assert!(old.deceased);
        assert_eq!(old.death_age, None);
        assert!(!member(&t, Uuid::nil(), 1, "PGRFTH").deceased);
        // a year before 1800 is not kept, but still says the person has died
        let older = member(&t, Uuid::nil(), 2, "PGRFTH");
        assert_eq!((older.born_year, older.deceased), (None, true));
    }

    #[test]
    fn a_person_named_the_child_of_hundreds_of_couples_costs_a_bounded_walk() {
        use std::fmt::Write as _;

        // a hostile file: X is the birth child of 500 couples, each with its own father and mother, and
        // all 1,000 parents are birth children of one shared family G. Walking the aunts and uncles of
        // every parent reads the 1,000 children of G once per parent: millions of reads, not a few
        // thousand, and 20,000 couples in a 10 MiB file would be billions inside the import transaction.
        let n = 500;
        let mut file = String::from("0 HEAD\n0 @X@ INDI\n");
        for k in 0..n {
            writeln!(file, "0 @H{k}@ INDI\n1 SEX M\n0 @W{k}@ INDI\n1 SEX F").expect("write a parent");
        }
        for k in 0..n {
            writeln!(file, "0 @F{k}@ FAM\n1 HUSB @H{k}@\n1 WIFE @W{k}@\n1 CHIL @X@").expect("write a couple");
        }
        file.push_str("0 @G@ FAM\n");
        for k in 0..n {
            writeln!(file, "1 CHIL @H{k}@\n1 CHIL @W{k}@").expect("write a child of G");
        }
        file.push_str("0 TRLR\n");
        let tree = parse(file.as_bytes()).expect("the hostile shape is a valid file");
        // the file holds 2,500 links: 1,000 parents, 500 child links of X and 1,000 of G
        let links = 5 * n;
        VISITED.with(|v| v.set(0));
        let relatives = tree.relatives(0);
        let visited = VISITED.with(std::cell::Cell::get);
        assert!(visited < 20 * links, "{visited} family members read for {links} links");
        // every parent is still a relative; their siblings are all parents already
        assert_eq!(relatives.len(), 2 * n);
        assert!(relatives.iter().all(|(_, code)| matches!(*code, "FTH" | "MTH")));
    }

    #[test]
    fn mutated_files_never_panic() {
        // a fixed-seed walk of byte flips, cuts and splices over both fixtures: each must parse or be
        // refused, never panic
        let mut seed: u64 = 0x5eed_0024;
        let mut next = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        for base in [F551, F70] {
            for _ in 0..1500 {
                let mut b = base.to_vec();
                let len = b.len().max(1) as u64;
                match next() % 4 {
                    0 => {
                        for _ in 0..8 {
                            let i = (next() % len) as usize;
                            if let Some(x) = b.get_mut(i) {
                                *x = (next() & 0xFF) as u8;
                            }
                        }
                    }
                    1 => b.truncate((next() % len) as usize),
                    2 => {
                        let i = (next() % len) as usize;
                        let j = (next() % len) as usize;
                        let piece: Vec<u8> = b.get(i.min(j)..i.max(j)).unwrap_or_default().to_vec();
                        let at = (next() % len) as usize;
                        b.splice(at.min(b.len())..at.min(b.len()), piece);
                    }
                    _ => {
                        let i = (next() % len) as usize;
                        b.insert(i.min(b.len()), if next() % 2 == 0 { b'\r' } else { b'@' });
                    }
                }
                if let Ok(t) = parse(&b) {
                    for me in 0..t.people.len().min(20) {
                        let _ = t.relatives(me);
                    }
                    let _ = t.people_out();
                }
            }
        }
    }
}
