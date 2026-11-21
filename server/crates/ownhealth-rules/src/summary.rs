//! The printed doctor summary (`ownhealth.summary/v1`): what goes on the two A4 pages a person hands
//! over at a visit, built from the same evaluation the state hash covers. Computed on read, never
//! stored. `ownhealth_ops::summary_pdf` lays it out.
//!
//! ```text
//! model = build(conn, profile, scopes, &Options { lang, specialty, sensitive_open, as_of })
//! ```
//!
//! - every value carries the `src` id it came from, and every row the id of the row it shows (the
//!   IPS export, `ownhealth_ops::ips`, puts those rows' FHIR in its Bundle); a marker also carries its
//!   lab range, date and the value before it; a series of the specialty with no result reads "not
//!   measured";
//! - procedures done on or before the date are listed newest first, as vaccines are;
//! - flags come ranked from the status renderer, so the order and the wording are the status page's;
//! - genome rows are the actionable PGx findings, plus the sensitive findings only when the owner
//!   ticked them for this print and the caller may see them (`sensitive_open`), never for a minor;
//! - a domain the caller cannot read is left out and named in `withheld.by_scope`;
//! - `slots` (calculators, due items, family) are named and empty until the slices that fill them.
//!
//! The words are `data/summary/templates.yaml`; the four marker lists are `specialties.yaml`. Both are
//! compiled in, and `tests/summary.rs` loads the same files through `cited::load`.

use std::sync::OnceLock;

use chrono::NaiveDate;
use ownhealth_core::access::{Principal, Scopes};
use ownhealth_core::records::list_records;
use ownhealth_core::{Error, Result};
use ownhealth_genome::{FindingsQuery, Gate};
use serde_json::{Map, Value, json};
use sqlx::{PgConnection, PgPool, Postgres, Transaction};
use uuid::Uuid;

use crate::engine::{Point, date_of, iso, str_of};
use crate::glance::actionable_phenotype;
use crate::num;
use crate::pack::{Pack, load_pack, text, truthy};
use crate::rendering::render_with;

pub const SCHEMA: &str = "ownhealth.summary/v1";
pub const SPECIALTIES: [&str; 5] = ["gp", "cardiology", "endocrinology", "hematology", "ips"];
pub const LANGS: [&str; 2] = ["en", "es"];

/// The shipped files, compiled in.
pub const FILES: [(&str, &str); 2] = [
    (
        "templates.yaml",
        include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../../data/summary/templates.yaml"
        )),
    ),
    (
        "specialties.yaml",
        include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../../data/summary/specialties.yaml"
        )),
    ),
];

/// What the person picked for this print.
#[derive(Debug, Clone)]
pub struct Options {
    pub lang: String,
    pub specialty: String,
    /// The owner ticked the sensitive findings for this print and the caller's gate is open
    /// (`ownhealth_genome::gate_for(.., "ui", ..).sensitive`). A minor's stay withheld regardless.
    pub sensitive_open: bool,
    pub as_of: NaiveDate,
}

#[derive(Debug, Clone)]
struct Specialty {
    id: String,
    names: Map<String, Value>,
    markers: Vec<String>,
    cite: String,
    slots: Slots,
}

/// The computed sections one view prints (`slots` of specialties.yaml); empty: left out.
#[derive(Debug, Clone, Default)]
struct Slots {
    /// Calculator ids, in print order.
    calculators: Vec<String>,
    /// Prevention areas whose open items print.
    due: Vec<String>,
    /// Family pattern ids.
    family: Vec<String>,
}

/// The prevention areas a `due` slot may name: the guideline's screenings and vaccines, and the rule
/// pack's lab gaps (a test never measured, or one older than its rule allows). The record's own
/// document gaps are a matter between the person and their providers, not for the sheet.
const DUE_AREAS: [&str; 3] = ["screening", "vaccine", "lab"];
/// The prevention statuses a `due` slot prints: what is open, never what is up to date.
const DUE_OPEN: [&str; 3] = ["overdue", "due", "upcoming"];

#[derive(Debug)]
struct Data {
    version: String,
    templates: Value,
    specialties: Vec<Specialty>,
}

fn version_of(doc: &Value, name: &str) -> Result<String> {
    doc.get("version")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| Error::Invalid(format!("{name}: no version")))
}

/// The two files: templates, then specialties.
fn parse(templates: &str, specialties: &str) -> Result<Data> {
    let yaml = |name: &str, s: &str| -> Result<Value> {
        serde_yaml::from_str(s).map_err(|e| Error::Invalid(format!("{name}: {e}")))
    };
    let tpl = yaml(FILES[0].0, templates)?;
    let spec = yaml(FILES[1].0, specialties)?;
    let list = |v: Option<&Value>| -> Vec<String> {
        v.and_then(Value::as_array)
            .map(|a| a.iter().map(text).collect())
            .unwrap_or_default()
    };
    let patterns: Vec<&str> = crate::family::load()?.rules.iter().map(|r| r.id.as_str()).collect();
    let specialties = spec
        .get("rows")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|r| {
            let s = |k: &str| r.get(k).and_then(Value::as_str).unwrap_or_default().to_string();
            let id = s("id");
            let slot = |k: &str| list(r.get("slots").and_then(|v| v.get(k)));
            let slots = Slots {
                calculators: slot("calculators"),
                due: slot("due"),
                family: slot("family"),
            };
            let known = |ids: &[String], all: &[&str], what: &str| -> Result<()> {
                match ids.iter().find(|i| !all.contains(&i.as_str())) {
                    Some(bad) => Err(Error::Invalid(format!(
                        "specialties.yaml: {id} names the {what} {bad:?}, which the code does not know"
                    ))),
                    None => Ok(()),
                }
            };
            known(&slots.calculators, &crate::calc::IDS, "calculator")?;
            known(&slots.due, &DUE_AREAS, "prevention area")?;
            known(&slots.family, &patterns, "family pattern")?;
            Ok(Specialty {
                names: r.get("names").and_then(Value::as_object).cloned().unwrap_or_default(),
                markers: list(r.get("markers")),
                cite: s("cite"),
                slots,
                id,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let ids: Vec<&str> = specialties.iter().map(|s| s.id.as_str()).collect();
    if ids != SPECIALTIES {
        return Err(Error::Invalid(format!(
            "specialties.yaml lists {ids:?}, the code knows {SPECIALTIES:?}"
        )));
    }
    Ok(Data {
        version: format!("{}, {}", version_of(&tpl, FILES[0].0)?, version_of(&spec, FILES[1].0)?),
        templates: tpl.get("templates").cloned().unwrap_or(Value::Null),
        specialties,
    })
}

fn data() -> Result<&'static Data> {
    static DATA: OnceLock<std::result::Result<Data, String>> = OnceLock::new();
    DATA.get_or_init(|| parse(FILES[0].1, FILES[1].1).map_err(|e| e.to_string()))
        .as_ref()
        .map_err(|e| Error::Invalid(e.clone()))
}

/// `summary-templates 0.2.0, summary-specialties 0.2.0`.
pub fn version() -> Result<String> {
    Ok(data()?.version.clone())
}

/// Every template, flattened to `section.key` -> text in one language.
fn labels(templates: &Value, lang: &str) -> Map<String, Value> {
    fn walk(v: &Value, at: &str, lang: &str, out: &mut Map<String, Value>) {
        let Some(m) = v.as_object() else { return };
        if let Some(t) = m.get(lang).and_then(Value::as_str) {
            out.insert(at.to_string(), json!(t));
            return;
        }
        for (k, v) in m {
            let key = if at.is_empty() { k.clone() } else { format!("{at}.{k}") };
            walk(v, &key, lang, out);
        }
    }
    let mut out = Map::new();
    walk(templates, "", lang, &mut out);
    out
}

/// `{name}` fields replaced; a field without a value stays as written.
pub fn fill(template: &str, params: &[(&str, String)]) -> String {
    params
        .iter()
        .fold(template.to_string(), |t, (k, v)| t.replace(&format!("{{{k}}}"), v))
}

fn label(l: &Map<String, Value>, key: &str) -> String {
    l.get(key).and_then(Value::as_str).unwrap_or(key).to_string()
}

fn s(m: &Map<String, Value>, k: &str) -> Value {
    m.get(k)
        .and_then(Value::as_str)
        .filter(|v| !v.trim().is_empty())
        .map_or(Value::Null, |v| json!(v.trim()))
}

fn point(p: &Point, lang: &str) -> Value {
    json!({
        "id": p.id,
        "value": num::localized(&p.shown(), lang),
        "unit": p.unit,
        "date": iso(p.date),
        "src": p.src,
    })
}

/// The lab range in the sheet's number format, built from its numeric bounds as the UI's `refRange`
/// does ('4,0 - 5,6', '< 3,0', '> 30'). A one-sided bound is inclusive or strict as the normalizer
/// read it from the printed range ('Hasta 40', '≤ 40' and 'up to 40' are inclusive, '< 40' is not),
/// and an inclusive one prints as '<=' or '>=', which the PDF's WinAnsi font can show. A range with
/// no numbers prints as the lab wrote it. `sex` and `age` (on the draw date) pick the range among
/// several printed per sex or age, as the normalizer picked it.
fn range(p: &Point, lang: &str, sex: Option<&str>, age: Option<i32>) -> Option<String> {
    let n = |d: &rust_decimal::Decimal| num::localized(&num::num(d), lang);
    let picked = || picked(p, sex, age);
    match (&p.ref_low, &p.ref_high) {
        (Some(lo), Some(hi)) => Some(format!("{} - {}", n(lo), n(hi))),
        (None, Some(hi)) => {
            let op = if picked().is_some_and(|r| r.high_inclusive) {
                "<="
            } else {
                "<"
            };
            Some(format!("{op} {}", n(hi)))
        }
        (Some(lo), None) => {
            let op = if picked().is_some_and(|r| r.low_inclusive) {
                ">="
            } else {
                ">"
            };
            Some(format!("{op} {}", n(lo)))
        }
        (None, None) => p.ref_text.clone(),
    }
}

/// The range the stored bounds came from: the printed range parsed again by the normalizer, among
/// the ranges whose bounds are the stored ones the one `pick_range` gives for the person (as the
/// normalizer chose it), else the first. Inclusivity is not stored; the printed text is all the
/// normalizer read it from.
fn picked(p: &Point, sex: Option<&str>, age: Option<i32>) -> Option<ownhealth_normalize::RefRange> {
    let printed = p.ref_text.as_deref()?;
    ["es", "en"].iter().find_map(|locale| {
        let same: Vec<_> = ownhealth_normalize::parse_range(printed, locale, p.unit.as_deref())
            .unwrap_or_default()
            .into_iter()
            .filter(|r| (r.low, r.high) == (p.ref_low, p.ref_high))
            .collect();
        ownhealth_normalize::pick_range(&same, sex, age)
            .or_else(|| same.first())
            .cloned()
    })
}

/// The marker table: one row per series of the specialty, in its order.
fn markers(
    pack: &Pack,
    ev: &crate::engine::Evaluation,
    spec: &Specialty,
    flags: &[(String, i64)],
    lang: &str,
) -> Vec<Value> {
    let sex = ev.profile.sex_at_birth.as_deref();
    spec.markers
        .iter()
        .map(|group| {
            let marker = ev.marker(group);
            let status = marker.map_or("nodata", |m| m.status);
            let status_text = pack.text(&format!("status.{status}"), lang, &[]).unwrap_or_default();
            let points = ev.series.get(group).map_or(&[][..], Vec::as_slice);
            let rank = flags.iter().filter(|(g, _)| g == group).map(|(_, r)| *r).min();
            let mut row = json!({
                "group": group,
                "name": pack.name(group, lang),
                "status": status,
                "status_text": status_text,
                "flag_rank": rank,
            });
            match points.split_last() {
                None => row["not_measured"] = json!(true),
                Some((last, before)) => {
                    if let (Some(m), Value::Object(p)) = (row.as_object_mut(), point(last, lang)) {
                        m.extend(p);
                        m.insert(
                            "range".into(),
                            json!(range(last, lang, sex, ev.profile.age_on(last.date))),
                        );
                        m.insert("previous".into(), before.last().map_or(Value::Null, |b| point(b, lang)));
                    }
                }
            }
            row
        })
        .collect()
}

/// Each rendered lab flag's series and rank. The renderer keeps the body's flag order and leaves
/// some out, so a rendered flag is the next unused body flag with the same src and rule: two series
/// that share one src each keep their own flag.
fn flag_groups(rendered: &[Value], body: &[Map<String, Value>]) -> Vec<(String, i64)> {
    let mut used = vec![false; body.len()];
    let mut out = Vec::new();
    for f in rendered {
        let hit = body.iter().enumerate().find(|(i, b)| {
            !used[*i] && b.get("src") == f.get("src") && b.get("rule").and_then(Value::as_str) == f["rule"].as_str()
        });
        if let Some((i, b)) = hit {
            used[i] = true;
            if let Some(g) = str_of(b, "group") {
                out.push((g.to_string(), f["rank"].as_i64().unwrap_or(0)));
            }
        }
    }
    out
}

/// How a medicine is taken: the dose and the schedule, both when the record has both.
fn taken(m: &Map<String, Value>) -> Value {
    match (s(m, "dose_text"), s(m, "schedule_text")) {
        (Value::String(d), Value::String(w)) if d != w => json!(format!("{d} · {w}")),
        (d, Value::Null) => d,
        (_, w) => w,
    }
}

/// The person's recorded decision on a medicine or supplement, in words with its day ("keep, on
/// 2026-04-02"); never the reason, which is the person's own text. A decision dated after the sheet's
/// date is not on it yet.
fn decision(m: &Map<String, Value>, l: &Map<String, Value>, as_of: NaiveDate) -> Value {
    let Some(d) = str_of(m, "decision") else {
        return Value::Null;
    };
    if date_of(m, "decision_on").is_some_and(|on| on > as_of) {
        return Value::Null;
    }
    let words = label(l, &format!("decision.{d}"));
    match s(m, "decision_on") {
        Value::String(on) => json!(fill(&label(l, "decision_on"), &[("decision", words), ("date", on)])),
        _ => json!(words),
    }
}

/// Genome rows: actionable PGx, and sensitive findings when the gate let them through.
fn genome_rows(findings: &Value, l: &Map<String, Value>) -> Vec<Value> {
    findings
        .get("findings")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_object)
        .filter(|f| {
            let pgx =
                matches!(str_of(f, "category"), Some("pgx" | "hla")) && actionable_phenotype(str_of(f, "phenotype"));
            pgx || f.get("sensitive").is_some_and(truthy)
        })
        .map(|f| {
            let title = str_of(f, "title").map(str::to_string).unwrap_or_else(|| {
                ["gene", "diplotype", "phenotype"]
                    .iter()
                    .filter_map(|k| str_of(f, k))
                    .collect::<Vec<_>>()
                    .join(" ")
            });
            let drugs: Vec<String> = f
                .get("drugs")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect())
                .unwrap_or_default();
            let sensitive = f.get("sensitive").is_some_and(truthy);
            json!({
                "title": title,
                "category": f.get("category"),
                "drugs": drugs,
                "evidence": f.get("evidence"),
                "sensitive": sensitive,
                "note": if sensitive { json!(label(l, "sensitive")) } else { Value::Null },
                "src": f.get("src"),
            })
        })
        .collect()
}

fn src_list(v: &Value) -> Vec<String> {
    match v {
        Value::String(s) => vec![s.clone()],
        // two inputs from one report name its src once
        Value::Array(a) => a.iter().filter_map(Value::as_str).fold(Vec::new(), |mut out, s| {
            if !out.iter().any(|o| o == s) {
                out.push(s.to_string());
            }
            out
        }),
        _ => Vec::new(),
    }
}

/// Every src id the model cites, in first-seen order.
fn cited(model: &Value) -> Vec<String> {
    fn walk(v: &Value, out: &mut Vec<String>) {
        match v {
            Value::Object(m) => {
                for (k, v) in m {
                    if k == "src" {
                        for s in src_list(v) {
                            if !out.contains(&s) {
                                out.push(s);
                            }
                        }
                    } else {
                        walk(v, out);
                    }
                }
            }
            Value::Array(a) => a.iter().for_each(|v| walk(v, out)),
            _ => {}
        }
    }
    let mut out = Vec::new();
    walk(model, &mut out);
    out
}

/// A src label that is a code, not words: the kind itself, or the older English words for 'manual'.
fn is_code(label: &str, kind: &str) -> bool {
    let l = label.trim().to_lowercase();
    l == kind || (kind == "manual" && ["manual entry", "entered by hand"].contains(&l.as_str()))
}

/// src id -> what a reader can find: the document, its page and date (the src label), else the
/// kind of source and the day it was recorded. Newest first, then provider and page, so the pages
/// of one report sit together.
async fn sources(conn: &mut PgConnection, ids: &[String], l: &Map<String, Value>) -> Result<Vec<Value>> {
    type Row = (
        String,
        String,
        Option<String>,
        Option<i32>,
        Option<String>,
        Option<chrono::DateTime<chrono::Utc>>,
        chrono::DateTime<chrono::Utc>,
    );
    let rows: Vec<Row> = sqlx::query_as(
        "select s.id, s.kind, s.label, s.page_no, d.provider_name, d.collected_at, s.created_at \
         from src s left join document d on d.id = s.document_id where s.id = any($1)",
    )
    .bind(ids)
    .fetch_all(conn)
    .await?;
    let mut out: Vec<(std::cmp::Reverse<NaiveDate>, String, i32, String, String)> = rows
        .into_iter()
        .map(|(id, kind, lbl, page, provider, collected, created)| {
            let lbl = lbl.map(|x| x.trim().to_string()).filter(|x| !x.is_empty());
            // the date a reader knows the source by: the document's, the one its label names, else
            // the day it was recorded
            let named = lbl.as_deref().and_then(|x| {
                x.split(" · ")
                    .find_map(|p| NaiveDate::parse_from_str(p, "%Y-%m-%d").ok())
            });
            let date = collected
                .map(|c| c.date_naive())
                .or(named)
                .unwrap_or(created.date_naive());
            let words = label(l, &format!("kind.{kind}"));
            let built = || {
                let parts: Vec<String> = [
                    provider.clone(),
                    collected.map(|c| iso(c.date_naive())),
                    page.map(|p| format!("p{p}")),
                ]
                .into_iter()
                .flatten()
                .collect();
                (!parts.is_empty()).then(|| parts.join(" · "))
            };
            // a label that starts with a code ('manual', or the older 'manual entry') reads as the
            // kind's own words, as the UI's sourceLabel does; with no other part it gains the date
            let what = match lbl {
                Some(x) => match x.split_once(" · ") {
                    Some((first, rest)) if is_code(first, &kind) => format!("{words} · {rest}"),
                    None if is_code(&x, &kind) => format!("{words} · {}", iso(date)),
                    _ => x,
                },
                None => built().unwrap_or_else(|| format!("{words} · {}", iso(date))),
            };
            (
                std::cmp::Reverse(date),
                provider.unwrap_or_default(),
                page.unwrap_or(0),
                what,
                id,
            )
        })
        .collect();
    out.sort();
    Ok(out
        .into_iter()
        .map(|(_, _, _, what, id)| json!({"src": id, "label": what}))
        .collect())
}

/// A calculator unit as a lab report writes it: UCUM's `{1.73_m2}` annotation is 1.73 m2.
fn calc_unit(u: &str) -> String {
    u.replace("{1.73_m2}", "1.73 m2")
}

/// The refusals that say a formula is not made for the person's age (CKD-EPI before 18, FIB-4 before
/// 35, SCORE2 and PREVENT outside their ranges): the sheet leaves those calculators out, as the
/// calculators document already leaves out SCORE2-OP below 70.
const NOT_FOR_AGE: [&str; 3] = ["not_adult", "age_below_35", "age_out_of_range"];

/// The calculators slot from a rendered ownhealth.calculators/v1 document: one row per listed id, in
/// the view's order, holding that calculator's newest item (the document lists each oldest first). A
/// result carries its value, unit, date, the src ids of its inputs and the notes that qualify it; a
/// refusal carries the reason in words and no value, and one for the person's age is left out
/// (`NOT_FOR_AGE`). Every row names its formula (`rule`, `source`, `cite`).
fn calculator_rows(doc: &Value, ids: &[String], lang: &str) -> Vec<Value> {
    let items = doc["items"].as_array().map_or(&[][..], Vec::as_slice);
    ids.iter()
        .filter_map(|id| items.iter().rfind(|i| i["id"] == id.as_str()))
        .filter(|i| !NOT_FOR_AGE.contains(&i["reason_code"].as_str().unwrap_or_default()))
        .map(|i| {
            let ok = i["status"] == "ok";
            json!({
                "rule": i["id"],
                "name": i["name"],
                "status": i["status"],
                "value": if ok { json!(num::localized(&text(&i["value"]), lang)) } else { Value::Null },
                "unit": i["unit"].as_str().map(calc_unit),
                // a refusal is dated by the day it was asked, which is no fact of the record
                "date": if ok { i["date"].clone() } else { Value::Null },
                "text": if ok { Value::Null } else { i["text"].clone() },
                // what qualifies a result (a draw not labelled fasting, a unit factor, a cutoff band)
                "notes": if ok { i["notes"].clone() } else { json!([]) },
                "src": src_list(&i["src"]),
                "source": i["version"],
                "cite": i["cite"],
            })
        })
        .collect()
}

/// The due slot from a rendered ownhealth.prevention/v1 document: the open items of the listed areas
/// (what the person's decisions hide is not there). An item due or overdue that was never recorded has
/// no date: its date is where the age window opened, not a deadline (as in get_health_status). A rule
/// pack lab item names no guideline of its own, so its row names the pack (`pack`) beside the link.
fn due_rows(doc: &Value, areas: &[String], l: &Map<String, Value>, pack: &str) -> Vec<Value> {
    doc["items"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|i| areas.iter().any(|a| i["area"] == a.as_str()))
        .filter_map(|i| {
            let status = i["status"].as_str().filter(|s| DUE_OPEN.contains(s))?;
            let last = i.get("last").filter(|l| l.is_object());
            let dated = last.is_some() || status == "upcoming";
            Some(json!({
                "rule": i["rule"],
                "area": i["area"],
                "title": i["title"],
                "status": status,
                "status_text": label(l, &format!("due_status.{status}")),
                "due": if dated { i.get("due").cloned().unwrap_or(Value::Null) } else { Value::Null },
                "last": last.map_or(Value::Null, |l| json!({"date": l["date"], "src": l["src"]})),
                "src": src_list(&i["src"]),
                "source": i["source"].as_str().filter(|s| !s.is_empty()).unwrap_or(pack),
                "cite": i["cite"],
            }))
        })
        .collect()
}

/// The family slot from a rendered ownhealth.family/v1 document built from the person's own family
/// history (never its hints): the listed patterns, each with the relatives it names, or why it could
/// not be checked.
fn family_rows(doc: &Value, ids: &[String], lang: &str) -> Result<Vec<Value>> {
    let cat = ownhealth_core::family::catalog()?;
    Ok(doc["items"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|i| ids.iter().any(|id| i["rule"] == id.as_str()))
        .map(|i| {
            let mut rel: Vec<String> = Vec::new();
            for code in i["rel"].as_array().into_iter().flatten().filter_map(Value::as_str) {
                let name = cat
                    .relationship(code)
                    .map_or(code, |r| if lang == "es" { r.es.as_str() } else { r.en.as_str() });
                if !rel.iter().any(|n| n == name) {
                    rel.push(name.to_string());
                }
            }
            json!({
                "rule": i["rule"],
                "title": i["title"][lang],
                "status": i["status"],
                "relatives": rel.join(", "),
                "text": i.get("reason").map_or(Value::Null, |r| r[lang].clone()),
                "src": src_list(&i["src"]),
                "source": i["source"],
                "cite": i["cite"],
            })
        })
        .collect())
}

/// The summary model for one profile, read with `scopes`, without a viewer: the family slot needs one
/// (`build_for`), so it stays empty. The IPS export prints no family slot.
pub async fn build(conn: &mut PgConnection, profile_id: Uuid, scopes: &Scopes, opts: &Options) -> Result<Value> {
    build_for(conn, None, profile_id, scopes, opts).await
}

/// The summary model for one profile as `viewer` reads it with `scopes`. A relative linked to another
/// profile feeds a family pattern only when the viewer may read that profile, as on the Family screen.
pub async fn build_for(
    conn: &mut PgConnection,
    viewer: Option<&Principal>,
    profile_id: Uuid,
    scopes: &Scopes,
    opts: &Options,
) -> Result<Value> {
    let (model, state) = read_for(&mut *conn, viewer, profile_id, scopes, opts).await?;
    crate::store_snapshot(&mut *conn, profile_id, &state, &*load_pack()?).await?;
    Ok(model)
}

/// A transaction that reads one sheet from one snapshot: REPEATABLE READ, so a correction saved
/// during the print shows on all of the sheet or none of it. It ends by storing the state it read
/// (`store_read_snapshot`). The audit row follows in a READ COMMITTED transaction of its own,
/// stamped with this one's start (`now()` in it), and commits before the sheet goes out.
pub async fn begin_read(pool: &PgPool) -> sqlx::Result<Transaction<'static, Postgres>> {
    pool.begin_with("begin isolation level repeatable read").await
}

/// `build_for` without its one write: the model, and the state body it was read from. The caller
/// stores the body with `store_read_snapshot` in the same transaction (`begin_read`).
pub async fn read_for(
    conn: &mut PgConnection,
    viewer: Option<&Principal>,
    profile_id: Uuid,
    scopes: &Scopes,
    opts: &Options,
) -> Result<(Value, Value)> {
    let lang = if opts.lang == "es" { "es" } else { "en" };
    let pack = load_pack()?;
    let d = data()?;
    let spec = d
        .specialties
        .iter()
        .find(|s| s.id == opts.specialty)
        .ok_or_else(|| Error::Invalid(format!("unknown specialty {:?}", opts.specialty)))?;
    let l = labels(&d.templates, lang);
    let (body, ev) = crate::evaluate_state(&mut *conn, profile_id, opts.as_of, None).await?;
    let rendered = render_with(&pack, &body, lang, "as_reported", "full", Some(scopes))?;
    let allowed = |s: &str| scopes.contains(s);
    let (labs_ok, records_ok, genome_ok) = (allowed("read:labs"), allowed("read:records"), allowed("read:genome"));

    // flags, ranked by the renderer
    let rendered_flags = rendered["data"]["flags"].as_array().map_or(&[][..], Vec::as_slice);
    let flag_groups = flag_groups(rendered_flags, &ev.flags);
    let flags: Vec<Value> = rendered_flags
        .iter()
        .map(|f| json!({"rank": f["rank"].as_i64().unwrap_or(0), "text": f["text"], "src": src_list(&f["src"])}))
        .collect();

    let prof = &ev.profile;
    let person = json!({
        "name": prof.display_name,
        "sex": prof.sex_at_birth.as_deref().filter(|s| ["female", "male"].contains(s))
            .and_then(|s| pack.text(&format!("sex.{s}"), lang, &[]).ok()),
        "age": prof.age_on(opts.as_of).map(|a| fill(&label(&l, "age"), &[("n", a.to_string())])),
        "born": prof.birth_date.map(|b| fill(&label(&l, "born"), &[("date", iso(b))])),
    });

    let mut conditions = Vec::new();
    let mut allergies = Vec::new();
    let mut medications = Vec::new();
    let mut vaccines = Vec::new();
    let mut procedures = Vec::new();
    if records_ok {
        conditions = ev
            .conditions
            .iter()
            .map(|c| json!({"id": c.get("id"), "name": s(c, "text"), "since": s(c, "onset"), "src": s(c, "src_id")}))
            .collect();
        for a in list_records(&mut *conn, "allergy", profile_id, None, false).await? {
            // the sheet's date: an allergy recorded as starting later is not on it yet
            let started = date_of(&a, "effective_at").is_none_or(|d| d <= opts.as_of);
            let active = started
                && !matches!(str_of(&a, "clinical_status"), Some("inactive" | "resolved"))
                && !matches!(str_of(&a, "verification_status"), Some("refuted" | "entered-in-error"));
            if active {
                let crit = str_of(&a, "criticality").map(|c| label(&l, &format!("criticality.{c}")));
                allergies.push(json!({
                    "id": a.get("id"),
                    "substance": s(&a, "substance"),
                    "criticality": crit,
                    "reaction": s(&a, "reaction_text"),
                    "src": s(&a, "src_id"),
                }));
            }
        }
        allergies.sort_by_key(|a| a["substance"].as_str().unwrap_or_default().to_lowercase());
        for m in ev.medications.iter().chain(&ev.supplements) {
            let supplement = str_of(m, "kind") == Some("supplement");
            medications.push(json!({
                "id": m.get("id"),
                "name": s(m, "name"),
                "strength": s(m, "strength"),
                "kind": if supplement { json!(label(&l, "supplement")) } else { Value::Null },
                "schedule": taken(m),
                "prescriber": s(m, "prescriber"),
                "decision": decision(m, &l, opts.as_of),
                "since": s(m, "started"),
                "src": s(m, "src_id"),
            }));
        }
        let mut imm = list_records(&mut *conn, "immunization", profile_id, None, false).await?;
        imm.retain(|v| date_of(v, "occurred").is_none_or(|d| d <= opts.as_of));
        imm.sort_by_key(|v| std::cmp::Reverse(date_of(v, "occurred")));
        vaccines = imm
            .iter()
            .map(|v| {
                json!({
                    "id": v.get("id"),
                    "vaccine": s(v, "vaccine"),
                    "date": s(v, "occurred"),
                    "dose": v.get("dose_number").filter(|x| truthy(x)),
                    "src": s(v, "src_id"),
                })
            })
            .collect();
        let mut done = list_records(&mut *conn, "procedure", profile_id, None, false).await?;
        done.retain(|p| date_of(p, "performed").is_none_or(|d| d <= opts.as_of));
        done.sort_by_key(|p| std::cmp::Reverse(date_of(p, "performed")));
        procedures = done
            .iter()
            .map(|p| json!({"id": p.get("id"), "procedure": s(p, "text"), "date": s(p, "performed"), "src": s(p, "src_id")}))
            .collect();
    }

    let mut genome = Vec::new();
    let (mut w_sensitive, mut w_minor) = (0, 0);
    if genome_ok {
        let findings = if opts.sensitive_open && !ev.minor {
            let q = FindingsQuery {
                min_evidence: Some("E2"),
                as_of: Some(opts.as_of),
                ..FindingsQuery::default()
            };
            ownhealth_genome::list_findings(&mut *conn, profile_id, &Gate::new(true, false), &q).await?
        } else {
            crate::genome::list_findings(&mut *conn, profile_id, ev.minor, opts.as_of).await?
        };
        genome = genome_rows(&findings, &l);
        w_sensitive = findings["withheld_sensitive"].as_i64().unwrap_or(0);
        w_minor = findings["withheld_minor"].as_i64().unwrap_or(0);
    }

    let markers = if labs_ok {
        markers(&pack, &ev, spec, &flag_groups, lang)
    } else {
        Vec::new()
    };
    // the slots this view prints, each from its own cited document; a domain the caller cannot read
    // leaves its slot empty (and named in by_scope)
    let slots = &spec.slots;
    let mut calculators = Vec::new();
    if labs_ok && !slots.calculators.is_empty() {
        let doc = crate::calc::document_for(&mut *conn, profile_id, opts.as_of, scopes).await?;
        calculators = calculator_rows(&crate::calc::render(&doc, lang)?, &slots.calculators, lang);
    }
    let mut due = Vec::new();
    if records_ok && !slots.due.is_empty() {
        let doc = crate::prevention::from_evaluation(&mut *conn, &ev, &body, &pack).await?;
        due = due_rows(
            &crate::prevention::render(&doc, &pack, lang, scopes)?,
            &slots.due,
            &l,
            &pack.version,
        );
    }
    let mut family = Vec::new();
    if let (true, false, Some(viewer)) = (records_ok, slots.family.is_empty(), viewer) {
        // a relative tagged sensitive never prints, as the person's own sensitive records do not (the
        // tick covers genome findings only)
        let mut fam_scopes = scopes.clone();
        fam_scopes.remove("read:sensitive");
        let rel = crate::family::relatives(&mut *conn, viewer, profile_id, &fam_scopes, opts.as_of).await?;
        let doc = crate::family::document(&ev.slug, opts.as_of, &rel)?;
        family = family_rows(&crate::family::render(&doc)?, &slots.family, lang)?;
    }

    let by_scope: Vec<&str> = [("labs", labs_ok), ("records", records_ok), ("genome", genome_ok)]
        .into_iter()
        .filter(|(_, ok)| !ok)
        .map(|(d, _)| d)
        .collect();

    let mut model = json!({
        "schema": SCHEMA,
        "profile": ev.slug,
        "as_of": iso(opts.as_of),
        "lang": lang,
        "specialty": {
            "id": spec.id,
            "name": spec.names.get(lang).cloned().unwrap_or(Value::Null),
            "cite": spec.cite,
        },
        "rules": body["rules"],
        "summary_rules": d.version,
        "state_hash": body["state_hash"],
        "person": person,
        "conditions": conditions,
        "allergies": allergies,
        "medications": medications,
        "genome": genome,
        "flags": flags,
        "markers": markers,
        "vaccines": vaccines,
        "procedures": procedures,
        "slots": {"calculators": calculators, "due": due, "family": family},
        "withheld": {"by_scope": by_scope, "sensitive": w_sensitive, "minor_rule": w_minor},
    });
    let ids = cited(&model);
    model["sources"] = json!(sources(&mut *conn, &ids, &l).await?);
    model["labels"] = Value::Object(l);
    Ok((model, body))
}

/// The src ids a model cites (rows, flags, markers and their previous values), first-seen order.
pub fn cited_srcs(model: &Value) -> Vec<String> {
    let mut m = model.clone();
    if let Some(o) = m.as_object_mut() {
        o.shift_remove("sources");
    }
    cited(&m)
}

/// Every specialty's marker list, for the tests: (id, markers).
pub fn specialty_markers() -> Result<Vec<(String, Vec<String>)>> {
    Ok(data()?
        .specialties
        .iter()
        .map(|s| (s.id.clone(), s.markers.clone()))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(v: Value) -> Map<String, Value> {
        v.as_object().cloned().unwrap_or_default()
    }

    #[test]
    fn two_series_that_share_a_src_keep_their_own_flag() {
        let body = [
            map(json!({"src": "src_a", "rule": "lab_range", "group": "ldl_c"})),
            map(json!({"src": "src_a", "rule": "lab_range", "group": "hdl_c"})),
            map(json!({"src": "src_b", "rule": "lab_range", "group": "tsh"})),
        ];
        let rendered = [
            json!({"rank": 1, "src": "src_a", "rule": "lab_range"}),
            json!({"rank": 2, "src": "src_a", "rule": "lab_range"}),
            json!({"rank": 3, "src": "src_b", "rule": "lab_range"}),
        ];
        assert_eq!(
            flag_groups(&rendered, &body),
            vec![("ldl_c".into(), 1), ("hdl_c".into(), 2), ("tsh".into(), 3)]
        );
    }

    #[test]
    fn a_one_sided_range_keeps_the_comparator_the_lab_printed() {
        let p = |lo: Option<&str>, hi: Option<&str>, text: Option<&str>| Point {
            id: uuid::Uuid::nil(),
            date: chrono::NaiveDate::from_ymd_opt(2026, 1, 1).unwrap_or_default(),
            value: None,
            canonical: None,
            canonical_unit: None,
            printed_value: String::new(),
            unit: None,
            unit_ucum: None,
            ref_text: text.map(Into::into),
            ref_low: lo.and_then(|v| v.parse().ok()),
            ref_high: hi.and_then(|v| v.parse().ok()),
            flag: None,
            loinc: None,
            method: None,
            lab: None,
            src: "src_x".into(),
            src_label: None,
            src_kind: "lab".into(),
            derivation: "printed".into(),
        };
        let r = |pt: Point, lang: &str| range(&pt, lang, None, None);
        // the PDF font is WinAnsi, which has no '≤' or '≥': the sheet writes '<=' and '>='
        assert_eq!(r(p(None, Some("5.6"), Some("≤ 5.6")), "es").as_deref(), Some("<= 5,6"));
        assert_eq!(r(p(None, Some("5.6"), Some("<=5.6")), "en").as_deref(), Some("<= 5.6"));
        assert_eq!(r(p(Some("30"), None, Some("≥ 30")), "es").as_deref(), Some(">= 30"));
        assert_eq!(r(p(Some("30"), None, Some(">= 30")), "en").as_deref(), Some(">= 30"));
        assert_eq!(r(p(None, Some("200"), Some("< 200")), "en").as_deref(), Some("< 200"));
        assert_eq!(r(p(None, Some("200"), None), "en").as_deref(), Some("< 200"));
        assert_eq!(r(p(Some("30"), None, None), "en").as_deref(), Some("> 30"));
        assert_eq!(
            r(p(Some("4.0"), Some("5.6"), Some("4.0-5.6")), "es").as_deref(),
            Some("4,0 - 5,6")
        );
        assert_eq!(r(p(None, None, Some("negativo")), "es").as_deref(), Some("negativo"));
    }

    #[test]
    fn a_range_in_words_keeps_the_inclusivity_the_normalizer_gave_it() {
        let p = |lo: Option<&str>, hi: Option<&str>, text: &str| Point {
            id: uuid::Uuid::nil(),
            date: chrono::NaiveDate::from_ymd_opt(2026, 1, 1).unwrap_or_default(),
            value: None,
            canonical: None,
            canonical_unit: None,
            printed_value: String::new(),
            unit: None,
            unit_ucum: None,
            ref_text: Some(text.into()),
            ref_low: lo.and_then(|v| v.parse().ok()),
            ref_high: hi.and_then(|v| v.parse().ok()),
            flag: None,
            loinc: None,
            method: None,
            lab: None,
            src: "src_x".into(),
            src_label: None,
            src_kind: "lab".into(),
            derivation: "printed".into(),
        };
        let r = |pt: Point, lang: &str| range(&pt, lang, None, None);
        assert_eq!(r(p(None, Some("40"), "Hasta 40"), "es").as_deref(), Some("<= 40"));
        assert_eq!(r(p(None, Some("40"), "HASTA 40"), "en").as_deref(), Some("<= 40"));
        assert_eq!(r(p(None, Some("5.6"), "up to 5.6"), "en").as_deref(), Some("<= 5.6"));
        assert_eq!(
            r(p(None, Some("5.6"), "inferior a 5,6"), "es").as_deref(),
            Some("<= 5,6")
        );
        assert_eq!(r(p(None, Some("5.6"), "=< 5,6"), "es").as_deref(), Some("<= 5,6"));
        assert_eq!(r(p(Some("30"), None, "Desde 30"), "es").as_deref(), Some(">= 30"));
        assert_eq!(r(p(Some("30"), None, "superior a 30"), "en").as_deref(), Some(">= 30"));
        assert_eq!(r(p(Some("30"), None, "=> 30"), "en").as_deref(), Some(">= 30"));
        // only the bare symbol is strict
        assert_eq!(r(p(None, Some("130"), "< 130"), "es").as_deref(), Some("< 130"));
        assert_eq!(r(p(Some("90"), None, "> 90"), "en").as_deref(), Some("> 90"));
        // the bound picked for the person is the one whose comparator counts
        let two = "Hombres: < 40\nMujeres: hasta 50";
        assert_eq!(r(p(None, Some("50"), two), "es").as_deref(), Some("<= 50"));
        assert_eq!(r(p(None, Some("40"), two), "es").as_deref(), Some("< 40"));
        // equal bounds, different comparators: the person's sex and age pick the range, as the
        // normalizer picked it
        let same = "Hombres: < 40\nMujeres: hasta 40";
        let who = |sex: &str| range(&p(None, Some("40"), same), "es", Some(sex), Some(45));
        assert_eq!(who("female").as_deref(), Some("<= 40"));
        assert_eq!(who("male").as_deref(), Some("< 40"));
    }

    #[test]
    fn a_slot_naming_an_id_the_code_does_not_know_is_refused() {
        let (tpl, spec) = (FILES[0].1, FILES[1].1);
        assert!(parse(tpl, spec).is_ok());
        for (from, to) in [
            ("calculators: [homa_ir,", "calculators: [homa_ir_2,"),
            ("due: [screening,", "due: [document,"),
            ("family: [fam.ascvd]", "family: [fam.unknown]"),
        ] {
            assert!(spec.contains(from), "{from}");
            let bad = spec.replacen(from, to, 1);
            let err = parse(tpl, &bad).err().map(|e| e.to_string()).unwrap_or_default();
            assert!(err.contains("the code does not know"), "{to}: {err}");
        }
    }

    /// A flow mapping splits at a comma: `{en: Flags, by rank, es: ...}` reads as `en: Flags` and a
    /// key `by rank`. Every template is exactly its two languages.
    #[test]
    fn every_template_is_one_text_in_each_language() -> Result<()> {
        fn walk(v: &Value, at: &str, bad: &mut Vec<String>) {
            let Some(m) = v.as_object() else { return };
            if m.contains_key("en") || m.contains_key("es") {
                let keys: Vec<&str> = m.keys().map(String::as_str).collect();
                if keys != ["en", "es"] || m.values().any(|t| !t.is_string()) {
                    bad.push(format!("{at}: {keys:?}"));
                }
                return;
            }
            for (k, v) in m {
                walk(v, &format!("{at}.{k}"), bad);
            }
        }
        let mut bad = Vec::new();
        walk(&data()?.templates, "templates", &mut bad);
        assert_eq!(bad, Vec::<String>::new());
        Ok(())
    }

    #[test]
    fn a_decision_reads_as_the_persons_choice_with_its_day() -> Result<()> {
        let d = data()?;
        for (lang, want, undated) in [
            ("en", "decided not to buy it again on 2025-12-15", "decided to stop it"),
            ("es", "decidió no volver a comprarlo el 2025-12-15", "decidió dejarlo"),
        ] {
            let l = labels(&d.templates, lang);
            let m = map(json!({"decision": "do_not_repurchase", "decision_on": "2025-12-15", "decision_reason": "x"}));
            let on = |d: &str| NaiveDate::parse_from_str(d, "%Y-%m-%d").unwrap_or_default();
            assert_eq!(decision(&m, &l, on("2025-12-15")), json!(want));
            assert_eq!(
                decision(&map(json!({"decision": "stop"})), &l, on("2025-01-01")),
                json!(undated)
            );
            assert!(decision(&map(json!({})), &l, on("2026-01-01")).is_null());
            // a sheet dated before the decision does not print it
            assert!(decision(&m, &l, on("2025-12-14")).is_null(), "{lang}");
        }
        Ok(())
    }

    #[test]
    fn a_schedule_keeps_the_dose_beside_it() {
        let m = |v: Value| taken(&map(v));
        assert_eq!(
            m(json!({"dose_text": "2 tablets", "schedule_text": "nightly"})),
            json!("2 tablets · nightly")
        );
        assert_eq!(m(json!({"dose_text": "2 tablets"})), json!("2 tablets"));
        assert_eq!(m(json!({"schedule_text": "nightly"})), json!("nightly"));
        assert_eq!(
            m(json!({"dose_text": "nightly", "schedule_text": "nightly"})),
            json!("nightly")
        );
        assert_eq!(m(json!({})), Value::Null);
    }
}
