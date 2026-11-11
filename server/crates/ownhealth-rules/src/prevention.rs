//! Prevention (`data/prevention/`): which screenings and vaccines the country's guideline lists for a
//! person's age and sex, when each was last done, and when it is next due. The result is the
//! ownhealth.prevention/v1 document, computed on read and never stored:
//!
//! ```text
//! {schema, profile, as_of, rules: "<file> <version>", country, items, unmatched, prevention_hash}
//! item = {key, rule, area, kind, status, due, last: {date, src}, source, cite, src: [..], labs, reason_code}
//! ```
//!
//! Items come from two places, each with one stable key:
//! - a row of the country file (`es.yaml`, `us.yaml`): key = the row id;
//! - the rule pack's lab gaps already in the state body: a never-measured analyte keeps its rule id,
//!   a stale series is `stale:<rule>:<group>`. The pack stays their only home: the loader refuses a
//!   country row whose evidence the pack already checks.
//!
//! The document holds codes, dates and ids only: language and the caller are not inputs, so the
//! hash is the same for every token. `render` adds the words in one language and drops what the
//! caller's scopes do not cover.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;
use std::sync::OnceLock;

use chrono::{Datelike, Months, NaiveDate};
use ownhealth_core::Result;
use ownhealth_core::access::Scopes;
use ownhealth_core::canonical::canonical_json;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use sqlx::PgConnection;

use crate::cited::{self, CitedError};
use crate::engine::{self, Evaluation, casefold, iso};
use crate::pack::{Pack, int, text};

pub const SCHEMA: &str = "ownhealth.prevention/v1";
/// A flu season runs this many months from its start; after it the next season is what is due.
const SEASON_MONTHS: u32 = 6;
const KINDS: [&str; 4] = ["once", "every_n_months", "series", "seasonal"];
const AREAS: [&str; 2] = ["screening", "vaccine"];
/// The pack rule kinds whose analytes a country row may not evaluate again.
const PACK_OWNED: [&str; 2] = ["never_measured", "recency"];
pub const STATUSES: [&str; 5] = ["overdue", "due", "upcoming", "up_to_date", "not_assessed"];

macro_rules! shipped {
    ($($name:literal),* $(,)?) => {
        [$(($name, include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../../data/prevention/", $name)))),*]
    };
}

/// The shipped files, compiled in.
const SHIPPED: [(&str, &str); 6] = shipped!(
    "pack.yaml",
    "evidence.yaml",
    "templates.yaml",
    "vaccines.tsv",
    "es.yaml",
    "us.yaml",
);

#[derive(Debug, Clone)]
pub struct Row {
    pub id: String,
    pub kind: String,
    pub area: String,
    pub sex: Option<String>,
    pub from: i32,
    pub to: Option<i32>,
    pub months: Option<u32>,
    pub interval_for: HashMap<String, u32>,
    pub evidence: Vec<String>,
    pub count_from: Option<i32>,
    pub doses: Option<usize>,
    pub gap_days: Option<i64>,
    pub season_start: Option<(u32, u32)>,
    pub source: String,
    pub cite: String,
    pub name: Value,
}

#[derive(Debug, Clone)]
pub struct CountryRules {
    pub file: String,
    pub version: String,
    pub rows: Vec<Row>,
    /// Rows that name a rule-pack rule instead of evaluating it: (row id, pack rule id).
    pub pack_rules: Vec<(String, String)>,
    pub disclaimer: Option<Value>,
}

#[derive(Debug, Clone, Default)]
pub struct Group {
    pub id: String,
    pub vaccine: bool,
    pub aliases: Vec<Vec<String>>,
    pub snomed: Vec<String>,
    pub cvx: Vec<String>,
    pub loinc: Vec<String>,
    pub alias_groups: Vec<String>,
    pub name: Value,
}

#[derive(Debug, Clone)]
pub struct Prevention {
    pub version: String,
    pub upcoming_days: i64,
    pub countries: BTreeMap<String, CountryRules>,
    pub groups: BTreeMap<String, Group>,
    pub templates: Value,
}

fn err(file: &str, what: impl std::fmt::Display) -> CitedError {
    CitedError(format!("{file}: {what}"))
}

/// Accents and case ignored; words are runs of letters and digits.
pub fn words(s: &str) -> Vec<String> {
    use unicode_normalization::UnicodeNormalization;
    use unicode_normalization::char::canonical_combining_class;
    let folded: String = casefold(s)
        .nfkd()
        .filter(|c| canonical_combining_class(*c) == 0)
        .collect();
    folded
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_string)
        .collect()
}

fn holds(text: &[String], alias: &[String]) -> bool {
    !alias.is_empty() && text.windows(alias.len()).any(|w| w == alias)
}

fn strings(v: Option<&Value>) -> Vec<String> {
    v.and_then(Value::as_array)
        .map(|a| a.iter().map(text).collect())
        .unwrap_or_default()
}

fn date_field(doc: &Value, key: &str) -> Option<NaiveDate> {
    doc.get(key)
        .and_then(Value::as_str)
        .and_then(|s| NaiveDate::parse_from_str(s.trim(), "%Y-%m-%d").ok())
}

fn parse_vaccines(file: &str, tsv: &str) -> std::result::Result<Vec<Group>, CitedError> {
    let mut out = Vec::new();
    for (i, line) in tsv.lines().enumerate() {
        if line.trim().is_empty() || line.starts_with('#') {
            continue;
        }
        let cols: Vec<&str> = line.split('\t').collect();
        let [id, cvx, en, es, label_en, label_es] = cols[..] else {
            return Err(err(file, format!("line {}: 6 tab-separated columns expected", i + 1)));
        };
        let split = |s: &str| -> Vec<String> {
            s.split('|')
                .map(str::trim)
                .filter(|x| !x.is_empty())
                .map(str::to_string)
                .collect()
        };
        if label_en.trim().is_empty() || label_es.trim().is_empty() {
            return Err(err(file, format!("line {}: a label is empty", i + 1)));
        }
        out.push(Group {
            id: id.trim().to_string(),
            vaccine: true,
            aliases: split(en).iter().chain(&split(es)).map(|a| words(a)).collect(),
            cvx: split(cvx),
            name: json!({"en": label_en.trim(), "es": label_es.trim()}),
            ..Group::default()
        });
    }
    Ok(out)
}

fn parse_groups(file: &str, doc: &Value) -> std::result::Result<Vec<Group>, CitedError> {
    let groups = doc
        .get("groups")
        .and_then(Value::as_object)
        .ok_or_else(|| err(file, "no `groups`"))?;
    let mut out = Vec::new();
    for (id, g) in groups {
        let name = g
            .get("name")
            .cloned()
            .ok_or_else(|| err(file, format!("{id} has no name")))?;
        out.push(Group {
            id: id.clone(),
            vaccine: false,
            aliases: strings(g.get("aliases")).iter().map(|a| words(a)).collect(),
            snomed: strings(g.get("snomed")),
            loinc: strings(g.get("loinc")),
            alias_groups: strings(g.get("alias_groups")),
            name,
            ..Group::default()
        });
    }
    Ok(out)
}

fn age(v: Option<&Value>) -> Option<i32> {
    int(v).and_then(|n| i32::try_from(n).ok())
}

fn parse_row(
    file: &str,
    r: &Map<String, Value>,
    groups: &BTreeMap<String, Group>,
) -> std::result::Result<Row, CitedError> {
    let id = r
        .get("id")
        .map(text)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| err(file, "a row has no id"))?;
    let e = |what: &str| err(file, format!("{id}: {what}"));
    let kind = r.get("kind").map(text).unwrap_or_default();
    if !KINDS.contains(&kind.as_str()) {
        return Err(e(&format!("kind {kind:?} is not one of {KINDS:?}")));
    }
    let area = r.get("area").map(text).unwrap_or_default();
    if !AREAS.contains(&area.as_str()) {
        return Err(e(&format!("area {area:?} is not one of {AREAS:?}")));
    }
    let sex = r.get("sex").and_then(Value::as_str).map(str::to_string);
    if sex.as_deref().is_some_and(|s| !["female", "male"].contains(&s)) {
        return Err(e("sex is female or male"));
    }
    let ages = r
        .get("ages")
        .and_then(Value::as_array)
        .ok_or_else(|| e("no ages [from, to]"))?;
    let from = ages
        .first()
        .and_then(|v| age(Some(v)))
        .ok_or_else(|| e("no start age"))?;
    let to = ages.get(1).and_then(|v| age(Some(v)));
    let evidence = strings(r.get("evidence"));
    if evidence.is_empty() {
        return Err(e("no evidence groups"));
    }
    if let Some(g) = evidence.iter().find(|g| !groups.contains_key(*g)) {
        return Err(e(&format!("unknown evidence group {g}")));
    }
    let months = int(r.get("months")).and_then(|m| u32::try_from(m).ok());
    let interval_for: HashMap<String, u32> = r
        .get("interval_for")
        .and_then(Value::as_object)
        .map(|m| {
            m.iter()
                .filter_map(|(k, v)| int(Some(v)).and_then(|n| u32::try_from(n).ok()).map(|n| (k.clone(), n)))
                .collect()
        })
        .unwrap_or_default();
    let doses = int(r.get("doses")).and_then(|n| usize::try_from(n).ok());
    let gap_days = int(r.get("gap_days"));
    let season_start = r.get("season_start").and_then(Value::as_str).and_then(|s| {
        let (m, d) = s.split_once('-')?;
        let (m, d) = (m.parse().ok()?, d.parse().ok()?);
        NaiveDate::from_ymd_opt(2000, m, d).map(|_| (m, d))
    });
    match kind.as_str() {
        "every_n_months" if months.is_none() => return Err(e("every_n_months needs months")),
        "series" if doses.is_none() || gap_days.is_none() => return Err(e("series needs doses and gap_days")),
        "seasonal" if season_start.is_none() => return Err(e("seasonal needs season_start MM-DD")),
        _ => {}
    }
    let name = r.get("name").cloned().ok_or_else(|| e("no name"))?;
    Ok(Row {
        id: id.clone(),
        kind,
        area,
        sex,
        from,
        to,
        months,
        interval_for,
        evidence,
        count_from: age(r.get("count_from")),
        doses,
        gap_days,
        season_start,
        source: r.get("source").map(text).ok_or_else(|| e("no source"))?,
        cite: r.get("cite").map(text).unwrap_or_default(),
        name,
    })
}

/// The groups of alias groups a pack never_measured or recency rule already checks.
fn pack_owned(pack: &Pack) -> HashSet<String> {
    PACK_OWNED
        .iter()
        .flat_map(|k| pack.rules_of(k))
        .flat_map(|r| strings(r.get("match")))
        .collect()
}

/// Read and check every prevention file; `read` returns a file's bytes by name.
pub fn build(
    pack: &Pack,
    read: &dyn Fn(&str) -> std::result::Result<Vec<u8>, CitedError>,
) -> std::result::Result<Prevention, CitedError> {
    let at = |name: &str| Path::new("data/prevention").join(name);
    let head = cited::load_bytes(&at("pack.yaml"), &read("pack.yaml")?)?;
    let upcoming_days = int(head.doc.get("upcoming_days")).ok_or_else(|| err("pack.yaml", "no upcoming_days"))?;
    let statuses = strings(head.doc.get("statuses"));
    if statuses != STATUSES {
        return Err(err("pack.yaml", format!("statuses are {STATUSES:?}")));
    }
    let evidence = cited::load_bytes(&at("evidence.yaml"), &read("evidence.yaml")?)?;
    let templates = cited::load_bytes(&at("templates.yaml"), &read("templates.yaml")?)?;
    let tsv = String::from_utf8(read("vaccines.tsv")?).map_err(|e| err("vaccines.tsv", e))?;
    let mut groups = BTreeMap::new();
    for g in parse_groups("evidence.yaml", &evidence.doc)?
        .into_iter()
        .chain(parse_vaccines("vaccines.tsv", &tsv)?)
    {
        if groups.insert(g.id.clone(), g).is_some() {
            return Err(err("evidence.yaml", "a group id is used twice"));
        }
    }
    let owned = pack_owned(pack);
    for g in groups.values() {
        let mut hit: Vec<String> = g.alias_groups.iter().filter(|a| owned.contains(*a)).cloned().collect();
        hit.extend(
            g.loinc
                .iter()
                .filter_map(|l| pack.loinc_to_group.get(l))
                .filter(|a| owned.contains(*a))
                .cloned(),
        );
        if let Some(a) = hit.first() {
            return Err(err(
                "evidence.yaml",
                format!(
                    "group {} covers {a}, which the rule pack already checks; name the pack rule instead",
                    g.id
                ),
            ));
        }
    }
    let mut countries = BTreeMap::new();
    let files = head
        .doc
        .get("countries")
        .and_then(Value::as_object)
        .ok_or_else(|| err("pack.yaml", "no countries"))?;
    let mut ids = HashSet::new();
    for (code, file) in files {
        let file = text(file);
        let f = cited::load_bytes(&at(&file), &read(&file)?)?;
        if !f
            .doc
            .get("reviewer")
            .and_then(Value::as_str)
            .is_some_and(|s| !s.trim().is_empty())
        {
            return Err(err(&file, "no `reviewer`"));
        }
        if date_field(&f.doc, "reviewed").is_none() {
            return Err(err(&file, "no `reviewed` date (YYYY-MM-DD)"));
        }
        let mut rows = Vec::new();
        let mut pack_rules = Vec::new();
        for r in &f.rows {
            if let Some(rule) = r.get("pack_rule").map(text) {
                let id = r.get("id").map(text).unwrap_or_default();
                let known = pack
                    .rule(&rule)
                    .and_then(|p| p.get("kind"))
                    .and_then(Value::as_str)
                    .is_some_and(|k| PACK_OWNED.contains(&k));
                if !known {
                    return Err(err(
                        &file,
                        format!("{id}: pack_rule {rule} is not a never_measured or recency rule"),
                    ));
                }
                pack_rules.push((id, rule));
                continue;
            }
            rows.push(parse_row(&file, r, &groups)?);
        }
        for id in rows.iter().map(|r| &r.id).chain(pack_rules.iter().map(|(id, _)| id)) {
            if !ids.insert(id.clone()) {
                return Err(err(&file, format!("row id {id} is used twice")));
            }
        }
        countries.insert(
            code.clone(),
            CountryRules {
                version: format!("{file} {}", f.version),
                file,
                rows,
                pack_rules,
                disclaimer: f.doc.get("disclaimer").cloned(),
            },
        );
    }
    Ok(Prevention {
        version: head.version,
        upcoming_days,
        countries,
        groups,
        templates: templates.doc,
    })
}

/// The files of one directory (tests point it at a mutated copy).
pub fn load_dir(dir: &Path, pack: &Pack) -> std::result::Result<Prevention, CitedError> {
    build(pack, &|name| std::fs::read(dir.join(name)).map_err(|e| err(name, e)))
}

/// The shipped files, read once.
pub fn load(pack: &Pack) -> Result<&'static Prevention> {
    static LOADED: OnceLock<std::result::Result<Prevention, String>> = OnceLock::new();
    let loaded = LOADED.get_or_init(|| {
        build(pack, &|name| {
            SHIPPED
                .iter()
                .find(|(n, _)| *n == name)
                .map(|(_, body)| body.as_bytes().to_vec())
                .ok_or_else(|| err(name, "not shipped"))
        })
        .map_err(|e| e.0)
    });
    loaded
        .as_ref()
        .map_err(|e| ownhealth_core::Error::Other(anyhow::anyhow!("prevention pack: {e}")))
}

// ---------------------------------------------------------------- evaluation ----

/// One fact that may count as done.
#[derive(Debug, Clone)]
struct Fact {
    date: NaiveDate,
    src: String,
    group: String,
    lab: bool,
}

fn add_years(d: NaiveDate, years: i32) -> NaiveDate {
    let m = u32::try_from(years.max(0) * 12).unwrap_or(0);
    d.checked_add_months(Months::new(m)).unwrap_or(d)
}

fn add_months(d: NaiveDate, months: u32) -> NaiveDate {
    d.checked_add_months(Months::new(months)).unwrap_or(NaiveDate::MAX)
}

async fn facts(
    conn: &mut PgConnection,
    profile_id: uuid::Uuid,
    as_of: NaiveDate,
    prev: &Prevention,
) -> Result<(Vec<Fact>, Vec<Value>)> {
    let mut out = Vec::new();
    let mut unmatched = Vec::new();
    let vaccines: Vec<&Group> = prev.groups.values().filter(|g| g.vaccine).collect();
    let others: Vec<&Group> = prev.groups.values().filter(|g| !g.vaccine).collect();
    let imms: Vec<(String, Option<String>, Option<String>, NaiveDate, String)> = sqlx::query_as(
        "select vaccine, code_system, code, coalesce(occurred, effective_at::date), src_id from immunization \
         where profile_id = $1 and status = 'current' and coalesce(occurred, effective_at::date) <= $2 \
         order by 4, src_id",
    )
    .bind(profile_id)
    .bind(as_of)
    .fetch_all(&mut *conn)
    .await?;
    for (name, system, code, date, src) in imms {
        let w = words(&name);
        let cvx = system
            .as_deref()
            .is_some_and(|s| s.eq_ignore_ascii_case("cvx") || s.ends_with("/cvx"));
        let hits: Vec<&&Group> = vaccines
            .iter()
            .filter(|g| {
                (cvx && code.as_ref().is_some_and(|c| g.cvx.contains(c))) || g.aliases.iter().any(|a| holds(&w, a))
            })
            .collect();
        if hits.is_empty() {
            unmatched.push(json!({"name": name, "src": src}));
        }
        for g in hits {
            out.push(Fact {
                date,
                src: src.clone(),
                group: g.id.clone(),
                lab: false,
            });
        }
    }
    let procs: Vec<(String, Option<String>, Option<String>, NaiveDate, String)> = sqlx::query_as(
        "select text, code_system, code, coalesce(performed, effective_at::date), src_id from procedure \
         where profile_id = $1 and status = 'current' and coalesce(performed, effective_at::date) <= $2 \
         order by 4, src_id",
    )
    .bind(profile_id)
    .bind(as_of)
    .fetch_all(&mut *conn)
    .await?;
    for (name, system, code, date, src) in procs {
        let w = words(&name);
        let snomed = system.as_deref().is_some_and(|s| s.to_lowercase().contains("snomed"));
        for g in &others {
            if (snomed && code.as_ref().is_some_and(|c| g.snomed.contains(c))) || g.aliases.iter().any(|a| holds(&w, a))
            {
                out.push(Fact {
                    date,
                    src: src.clone(),
                    group: g.id.clone(),
                    lab: false,
                });
            }
        }
    }
    let loincs: Vec<String> = others.iter().flat_map(|g| g.loinc.clone()).collect();
    let alias_groups: Vec<String> = others.iter().flat_map(|g| g.alias_groups.clone()).collect();
    let obs: Vec<(Option<String>, String, NaiveDate, String)> = sqlx::query_as(
        "select loinc, alias_group, effective_at::date, src_id from observation where profile_id = $1 \
         and status = 'current' and effective_at < $2 and (loinc = any($3) or alias_group = any($4)) \
         order by 3, src_id",
    )
    .bind(profile_id)
    .bind(engine::day_end(as_of))
    .bind(&loincs)
    .bind(&alias_groups)
    .fetch_all(&mut *conn)
    .await?;
    for (loinc, group, date, src) in obs {
        for g in &others {
            if loinc.as_ref().is_some_and(|l| g.loinc.contains(l)) || g.alias_groups.contains(&group) {
                out.push(Fact {
                    date,
                    src: src.clone(),
                    group: g.id.clone(),
                    lab: true,
                });
            }
        }
    }
    Ok((out, unmatched))
}

/// The most recent season start on or before `day`.
fn season_on(day: NaiveDate, (m, d): (u32, u32)) -> NaiveDate {
    let this = NaiveDate::from_ymd_opt(day.year(), m, d).unwrap_or(day);
    if this <= day {
        this
    } else {
        NaiveDate::from_ymd_opt(day.year() - 1, m, d).unwrap_or(day)
    }
}

fn next_season(after: NaiveDate, start: (u32, u32)) -> NaiveDate {
    let s = season_on(after, start);
    if s >= after { s } else { add_months(s, 12) }
}

fn item(row: &Row, prev: &Prevention, birth: NaiveDate, as_of: NaiveDate, facts: &[Fact]) -> Option<Value> {
    let start = add_years(birth, row.from);
    let end = row.to.map(|t| add_years(birth, t + 1));
    if end.is_some_and(|e| as_of >= e) {
        return None;
    }
    let mine: Vec<&Fact> = facts.iter().filter(|f| row.evidence.contains(&f.group)).collect();
    let floor = row.count_from.map(|a| add_years(birth, a));
    let counted: Vec<&Fact> = mine
        .iter()
        .copied()
        .filter(|f| floor.is_none_or(|fl| f.date >= fl))
        .collect();
    // before its window a row shows only when the record already holds a matching fact
    if mine.is_empty() && as_of < start && (start - as_of).num_days() > prev.upcoming_days {
        return None;
    }
    let (next, can_overdue) = match row.kind.as_str() {
        "once" => (if counted.is_empty() { Some(start) } else { None }, false),
        "every_n_months" => {
            let base = row.months.unwrap_or(12);
            let next = counted
                .iter()
                .map(|f| add_months(f.date, row.interval_for.get(&f.group).copied().unwrap_or(base)))
                .max()
                .map_or(start, |n| n.max(start));
            (Some(next), !counted.is_empty())
        }
        "series" => {
            let mut days: Vec<NaiveDate> = counted.iter().map(|f| f.date).collect();
            days.dedup();
            let doses = row.doses.unwrap_or(1);
            match days.last() {
                _ if days.len() >= doses => (None, false),
                None => (Some(start), false),
                Some(last) => (
                    Some((*last + chrono::Duration::days(row.gap_days.unwrap_or(0))).max(start)),
                    true,
                ),
            }
        }
        _ => {
            let s = row.season_start.unwrap_or((10, 1));
            let current = season_on(as_of, s);
            let in_season = as_of < add_months(current, SEASON_MONTHS);
            let done = counted.iter().any(|f| f.date >= current);
            let next = if in_season && !done {
                current
            } else {
                next_season(as_of + chrono::Duration::days(1), s)
            };
            (
                Some(next.max(if start > as_of { next_season(start, s) } else { start })),
                false,
            )
        }
    };
    let status = match next {
        None => "up_to_date",
        Some(n) if (n - as_of).num_days() > prev.upcoming_days => "up_to_date",
        Some(n) if n > as_of => "upcoming",
        Some(_) if can_overdue => "overdue",
        Some(_) => "due",
    };
    let last = mine.iter().max_by_key(|f| (f.date, f.src.clone()));
    let mut src: Vec<String> = counted.iter().map(|f| f.src.clone()).collect();
    if let Some(l) = last
        && !src.contains(&l.src)
    {
        src.push(l.src.clone());
    }
    src.dedup();
    Some(json!({
        "key": row.id,
        "rule": row.id,
        "area": row.area,
        "kind": row.kind,
        "status": status,
        "due": next.map(iso),
        "last": last.map(|f| json!({"date": iso(f.date), "src": f.src})),
        "source": row.source,
        "cite": row.cite,
        "src": src,
        "labs": if last.is_some_and(|f| f.lab) { json!(true) } else { Value::Null },
    }))
}

fn not_assessed(row: &Row, reason: &str) -> Value {
    json!({
        "key": row.id, "rule": row.id, "area": row.area, "kind": row.kind, "status": "not_assessed",
        "source": row.source, "cite": row.cite, "src": [], "reason_code": reason,
    })
}

/// The rule pack's lab gaps of the state body, as items.
fn pack_items(body: &Value, pack: &Pack) -> Vec<Value> {
    let cite = |rule: &str| {
        pack.rule(rule)
            .and_then(|r| r.get("cite"))
            .cloned()
            .unwrap_or(Value::Null)
    };
    let list = |k: &str| body.get(k).and_then(Value::as_array).cloned().unwrap_or_default();
    let mut out = Vec::new();
    for n in list("never_measured") {
        let rule = n.get("rule").map(text).unwrap_or_default();
        out.push(json!({
            "key": rule, "rule": rule, "area": "lab", "kind": "never_measured", "status": "due",
            "group": n.get("group"), "cite": cite(&rule), "src": [], "labs": true,
        }));
    }
    for s in list("stale") {
        let rule = s.get("rule").map(text).unwrap_or_default();
        let group = s.get("group").map(text).unwrap_or_default();
        let last = s.get("last").and_then(Value::as_str).and_then(engine::parse_date);
        let window = pack.rule(&rule).and_then(|r| int(r.get("window_days"))).unwrap_or(0);
        out.push(json!({
            "key": format!("stale:{rule}:{group}"), "rule": rule, "area": "lab", "kind": "recency",
            "status": "overdue", "group": group,
            "due": last.map(|d| iso(d + chrono::Duration::days(window))),
            "last": last.map(|d| json!({"date": iso(d), "src": s.get("src")})),
            "cite": cite(&rule), "src": s.get("src").into_iter().cloned().collect::<Vec<_>>(), "labs": true,
        }));
    }
    out
}

fn drop_nulls(v: Value) -> Value {
    match v {
        Value::Object(m) => Value::Object(
            m.into_iter()
                .filter(|(_, v)| !v.is_null())
                .map(|(k, v)| (k, drop_nulls(v)))
                .collect(),
        ),
        Value::Array(a) => Value::Array(a.into_iter().map(drop_nulls).collect()),
        other => other,
    }
}

/// The hash of a prevention document, as `state_hash` hashes the state body.
pub fn prevention_hash(doc: &Value) -> Result<String> {
    let mut clean = doc.clone();
    if let Some(m) = clean.as_object_mut() {
        m.shift_remove("prevention_hash");
    }
    let bytes = canonical_json(&clean).map_err(|e| ownhealth_core::Error::Other(anyhow::anyhow!("{e}")))?;
    Ok(format!("sha256:{}", hex::encode(Sha256::digest(bytes))))
}

/// The document for one evaluation (the dashboard already holds one) and its state body.
pub async fn from_evaluation(conn: &mut PgConnection, ev: &Evaluation, body: &Value, pack: &Pack) -> Result<Value> {
    let prev = load(pack)?;
    let as_of = ev.as_of;
    let country = ev.profile.country.as_deref().filter(|c| !c.is_empty());
    let rules = country.and_then(|c| prev.countries.get(c));
    let mut items = Vec::new();
    let mut unmatched = Vec::new();
    if let Some(rules) = rules {
        let (facts, unknown) = facts(&mut *conn, ev.profile_id, as_of, prev).await?;
        unmatched = unknown;
        let sex = ev
            .profile
            .sex_at_birth
            .as_deref()
            .filter(|s| ["female", "male"].contains(s));
        for row in &rules.rows {
            match (ev.profile.birth_date, &row.sex, sex) {
                (_, Some(want), Some(have)) if want != have => {}
                (None, _, _) => items.push(not_assessed(row, "no_birth_date")),
                (Some(birth), Some(_), None) => {
                    if item(row, prev, birth, as_of, &[]).is_some() {
                        items.push(not_assessed(row, "no_sex"));
                    }
                }
                (Some(birth), _, _) => items.extend(item(row, prev, birth, as_of, &facts)),
            }
        }
    }
    items.extend(pack_items(body, pack));
    let rank = |v: &Value| {
        let s = v.get("status").and_then(Value::as_str).unwrap_or_default();
        STATUSES.iter().position(|x| *x == s).unwrap_or(STATUSES.len())
    };
    let due_of = |v: &Value| {
        v.get("due")
            .and_then(Value::as_str)
            .map_or_else(|| "9999".to_string(), str::to_string)
    };
    items.sort_by(|a, b| {
        rank(a)
            .cmp(&rank(b))
            .then_with(|| due_of(a).cmp(&due_of(b)))
            .then_with(|| text(&a["key"]).cmp(&text(&b["key"])))
    });
    let reason = match (country, rules) {
        (None, _) => Some("no_country"),
        (Some(_), None) => Some("country_not_covered"),
        _ => None,
    };
    let mut doc = drop_nulls(json!({
        "schema": SCHEMA,
        "profile": ev.slug,
        "as_of": iso(as_of),
        "rules": rules.map(|r| r.version.clone()),
        "country": country,
        "reason_code": reason,
        "items": items,
        "unmatched": unmatched,
    }));
    let hash = prevention_hash(&doc)?;
    if let Some(m) = doc.as_object_mut() {
        m.insert("prevention_hash".into(), json!(hash));
    }
    Ok(doc)
}

/// The document for one profile on one day.
pub async fn build_document(conn: &mut PgConnection, profile_id: uuid::Uuid, as_of: NaiveDate) -> Result<Value> {
    let (body, ev) = crate::build_with_evaluation(&mut *conn, profile_id, as_of, None).await?;
    let pack = crate::load_pack()?;
    from_evaluation(conn, &ev, &body, &pack).await
}

/// Whether the caller may see one item: every item is a record (read:records), and one that a lab
/// value shows also needs read:labs.
pub fn visible(item: &Value, scopes: &Scopes) -> bool {
    scopes.contains("read:records") && (item.get("labs") != Some(&Value::Bool(true)) || scopes.contains("read:labs"))
}

fn fill(template: &str, params: &[(&str, String)]) -> String {
    params
        .iter()
        .fold(template.to_string(), |t, (k, v)| t.replace(&format!("{{{k}}}"), v))
}

fn pick(v: Option<&Value>, lang: &str) -> Option<String> {
    let v = v?;
    v.get(lang)
        .or_else(|| v.get("en"))
        .and_then(Value::as_str)
        .map(str::to_string)
}

/// The document in one language for one caller: every item gains `title` and `why` (or `reason`
/// for not_assessed); items the scopes do not cover are left out and counted in `withheld`.
pub fn render(doc: &Value, pack: &Pack, lang: &str, scopes: &Scopes) -> Result<Value> {
    let prev = load(pack)?;
    let lang = if lang == "es" { "es" } else { "en" };
    let rules = doc
        .get("country")
        .and_then(Value::as_str)
        .and_then(|c| prev.countries.get(c));
    let rows: HashMap<&str, &Row> = rules
        .map(|r| r.rows.iter().map(|row| (row.id.as_str(), row)).collect())
        .unwrap_or_default();
    let t = |path: &[&str]| -> Option<String> {
        let mut v = &prev.templates;
        for p in path {
            v = v.get(*p)?;
        }
        pick(Some(v), lang)
    };
    let mut items = Vec::new();
    let mut withheld = 0;
    for it in doc.get("items").and_then(Value::as_array).into_iter().flatten() {
        if !visible(it, scopes) {
            withheld += 1;
            continue;
        }
        let mut out = it.as_object().cloned().unwrap_or_default();
        let rule = it.get("rule").map(text).unwrap_or_default();
        let (title, why) = if let Some(row) = rows.get(rule.as_str()) {
            let open = row.to.is_none();
            let key = match row.kind.as_str() {
                "every_n_months" | "once" if open => format!("{}_open", row.kind),
                k => k.to_string(),
            };
            let months = row.months.unwrap_or(0).to_string();
            let why = t(&["why", &key]).map(|w| {
                fill(
                    &w,
                    &[
                        ("months", months),
                        ("from", row.from.to_string()),
                        ("to", row.to.map(|n| n.to_string()).unwrap_or_default()),
                        ("doses", row.doses.unwrap_or(1).to_string()),
                    ],
                )
            });
            (pick(Some(&row.name), lang), why)
        } else {
            let group = it.get("group").map(text).unwrap_or_default();
            let why = if it.get("kind").and_then(Value::as_str) == Some("never_measured") {
                pack.text("due.never", lang, &[("rule", crate::pack::P::from(&rule))])?
            } else {
                pack.text(
                    "due.stale",
                    lang,
                    &[
                        ("rule", crate::pack::P::from(&rule)),
                        (
                            "window_months",
                            crate::pack::P::I(crate::rendering::window_months(pack, &rule)),
                        ),
                    ],
                )?
            };
            (Some(pack.name(&group, lang)), Some(why))
        };
        out.insert("title".into(), json!(title));
        out.insert("why".into(), json!(why));
        if let Some(code) = it.get("reason_code").and_then(Value::as_str) {
            out.insert("reason".into(), json!(t(&["reasons", code])));
        }
        items.push(Value::Object(out));
    }
    let mut out = doc.as_object().cloned().unwrap_or_default();
    out.insert("items".into(), Value::Array(items));
    out.insert("lang".into(), json!(lang));
    if let Some(code) = doc.get("reason_code").and_then(Value::as_str) {
        out.insert("reason".into(), json!(t(&["reasons", code])));
    }
    if let Some(d) = rules.and_then(|r| r.disclaimer.as_ref()) {
        out.insert("disclaimer".into(), json!(pick(Some(d), lang)));
    }
    if !scopes.contains("read:records") {
        out.shift_remove("unmatched");
    }
    out.insert("withheld".into(), json!(withheld));
    Ok(Value::Object(out))
}
