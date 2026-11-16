//! Family patterns (`data/family/patterns.yaml`): what the person's own family history shows that a
//! guideline names, such as a parent with heart disease before 55. The result is the
//! ownhealth.family/v1 document, computed on read and never stored:
//!
//! ```text
//! {schema, profile, as_of, rules: "<file> <version>", items, family_hash}
//! item = {rule, status, basis, source, cite, members: [id], rel: [RoleCode], kinds: [kind], src: [src id], reason_code}
//! ```
//!
//! A rule matches the condition `kind` only, never free text: a condition of kind `other` never
//! counts. Only relatives the caller may read feed a pattern: a row with a sensitivity tag needs
//! read:sensitive, and a relative linked to a profile the caller cannot read (the pedigree masks it)
//! counts for nothing. The document holds codes and ids only, so every language and identity mode
//! gets the same `family_hash`; `render` adds the words in es and en.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::OnceLock;

use chrono::NaiveDate;
use ownhealth_core::Result;
use ownhealth_core::access::{Principal, Scopes, effective_permission};
use ownhealth_core::canonical::canonical_json;
use ownhealth_core::family::{Catalog, FamilyCondition};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use sqlx::PgConnection;
use sqlx::types::Json;
use uuid::Uuid;

use crate::cited::{self, CitedError};
use crate::engine::{day_end, iso};

pub const SCHEMA: &str = "ownhealth.family/v1";
const FILE: &str = "patterns.yaml";
const SHIPPED: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../../data/family/patterns.yaml"
));
const BASES: [&str; 2] = ["guideline", "pack_default"];
const SIDES: [&str; 2] = ["paternal", "maternal"];
/// The relatives of the proband's own branch who share both of the proband's sides: full siblings and
/// the proband's descendants. A half-sibling, a parent of unknown side (PRN), a niece or a nephew may
/// share only one, and which one is not known, so for a same-side count they sit on neither.
const BOTH_SIDES: [&str; 8] = ["BRO", "SIS", "SIB", "SON", "DAU", "CHILD", "GRNDSON", "GRNDDAU"];
/// How many patterns the `family` section of get_health_status lists; the rest are counted.
pub const STATUS_TOP: usize = 3;

/// One clause of a rule; the rule fires when any clause matches.
#[derive(Debug, Clone)]
pub struct Clause {
    pub relationships: Vec<String>,
    pub degree: Vec<i32>,
    /// Empty with `same_kind`: every kind but `other`, counted kind by kind.
    pub kinds: Vec<String>,
    pub same_kind: bool,
    pub sex: Option<String>,
    /// (male, female) onset age a condition must be below.
    pub before: Option<(i32, i32)>,
    pub count: usize,
    pub same_side: bool,
}

#[derive(Debug, Clone)]
pub struct Rule {
    pub id: String,
    pub source: String,
    pub basis: String,
    pub cite: String,
    pub any: Vec<Clause>,
}

#[derive(Debug, Clone)]
pub struct Patterns {
    pub version: String,
    pub rules: Vec<Rule>,
    pub templates: Value,
}

fn err(what: impl std::fmt::Display) -> CitedError {
    CitedError(format!("{FILE}: {what}"))
}

/// An absent list is empty; a present one must be a list of strings.
fn strings(v: Option<&Value>) -> Option<Vec<String>> {
    let Some(v) = v else { return Some(Vec::new()) };
    v.as_array()?.iter().map(|x| x.as_str().map(str::to_string)).collect()
}

/// An absent flag is false; a present one must be true or false.
fn flag(v: Option<&Value>) -> Option<bool> {
    v.map_or(Some(false), Value::as_bool)
}

fn clause(rule: &str, i: usize, v: &Value, cat: &Catalog) -> std::result::Result<Clause, CitedError> {
    let at = |what: &str| err(format!("{rule} any[{i}]: {what}"));
    let m = v.as_object().ok_or_else(|| at("is not a mapping"))?;
    for k in m.keys() {
        if ![
            "relationships",
            "degree",
            "kinds",
            "same_kind",
            "sex",
            "before",
            "count",
            "same_side",
        ]
        .contains(&k.as_str())
        {
            return Err(at(&format!("unknown key {k}")));
        }
    }
    let relationships = strings(m.get("relationships")).ok_or_else(|| at("relationships is a list of strings"))?;
    for r in &relationships {
        match cat.relationship(r) {
            Some(row) if row.degree > 0 => {}
            Some(_) => return Err(at(&format!("{r} is not a blood relative"))),
            None => return Err(at(&format!("unknown relationship {r}"))),
        }
    }
    let degree: Vec<i32> = match m.get("degree") {
        None => vec![1, 2],
        Some(v) => v
            .as_array()
            .ok_or_else(|| at("degree is not a list"))?
            .iter()
            .map(|d| {
                d.as_i64()
                    .and_then(|d| i32::try_from(d).ok())
                    .filter(|d| (1..=3).contains(d))
                    .ok_or_else(|| at("degree is 1, 2 or 3"))
            })
            .collect::<std::result::Result<_, _>>()?,
    };
    let kinds = strings(m.get("kinds")).ok_or_else(|| at("kinds is a list of strings"))?;
    let same_kind = flag(m.get("same_kind")).ok_or_else(|| at("same_kind is true or false"))?;
    if same_kind == !kinds.is_empty() {
        return Err(at("needs either kinds or same_kind"));
    }
    for k in &kinds {
        // a free-text condition never counts: `other` holds no comparable kind
        if k == "other" || cat.condition(k).is_none() {
            return Err(at(&format!("kind {k} cannot be matched")));
        }
    }
    let sex = match m.get("sex") {
        None => None,
        Some(s) => Some(
            s.as_str()
                .filter(|s| ["female", "male"].contains(s))
                .ok_or_else(|| at("sex is female or male"))?
                .to_string(),
        ),
    };
    let before = match m.get("before") {
        None => None,
        Some(b) => {
            let age = |k: &str| {
                b.get(k)
                    .and_then(Value::as_i64)
                    .and_then(|a| i32::try_from(a).ok())
                    .filter(|a| (1..=130).contains(a))
                    .ok_or_else(|| at(&format!("before.{k} is an age")))
            };
            Some((age("male")?, age("female")?))
        }
    };
    let count = match m.get("count") {
        None => 1,
        Some(c) => c
            .as_u64()
            .and_then(|c| usize::try_from(c).ok())
            .filter(|c| *c >= 1)
            .ok_or_else(|| at("count is 1 or more"))?,
    };
    let same_side = flag(m.get("same_side")).ok_or_else(|| at("same_side is true or false"))?;
    Ok(Clause {
        relationships,
        degree,
        kinds,
        same_kind,
        sex,
        before,
        count,
        same_side,
    })
}

/// Check one patterns file against the family catalog.
pub fn parse(bytes: &[u8], cat: &Catalog) -> std::result::Result<Patterns, CitedError> {
    let file = cited::load_bytes(std::path::Path::new(FILE), bytes)?;
    let templates = file.doc.get("templates").cloned().unwrap_or(Value::Null);
    let mut seen = BTreeSet::new();
    let mut rules = Vec::new();
    for row in &file.rows {
        let id = row
            .get("id")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| err("a row has no id"))?
            .to_string();
        if !seen.insert(id.clone()) {
            return Err(err(format!("{id} appears twice")));
        }
        let field = |k: &str| {
            row.get(k)
                .and_then(Value::as_str)
                .filter(|s| !s.trim().is_empty())
                .map(str::to_string)
                .ok_or_else(|| err(format!("{id}: no {k}")))
        };
        let basis = field("basis")?;
        if !BASES.contains(&basis.as_str()) {
            return Err(err(format!("{id}: unknown basis {basis}")));
        }
        for part in ["title", "why"] {
            if templates.get(part).and_then(|t| t.get(&id)).is_none() {
                return Err(err(format!("{id}: no {part} template")));
            }
        }
        let any = row
            .get("any")
            .and_then(Value::as_array)
            .filter(|a| !a.is_empty())
            .ok_or_else(|| err(format!("{id}: no clauses under any")))?
            .iter()
            .enumerate()
            .map(|(i, c)| clause(&id, i, c, cat))
            .collect::<std::result::Result<_, _>>()?;
        rules.push(Rule {
            source: field("source")?,
            cite: field("cite")?,
            basis,
            id,
            any,
        });
    }
    Ok(Patterns {
        version: file.version,
        rules,
        templates,
    })
}

/// The shipped file, read once.
pub fn load() -> Result<&'static Patterns> {
    static LOADED: OnceLock<std::result::Result<Patterns, String>> = OnceLock::new();
    LOADED
        .get_or_init(|| {
            let cat = ownhealth_core::family::catalog().map_err(|e| e.to_string())?;
            parse(SHIPPED.as_bytes(), cat).map_err(|e| e.0)
        })
        .as_ref()
        .map_err(|e| ownhealth_core::Error::Other(anyhow::anyhow!("family patterns: {e}")))
}

// ---------------------------------------------------------------- evaluation ----

/// One relative as a pattern sees it.
#[derive(Debug, Clone)]
pub struct Relative {
    pub id: Uuid,
    pub relationship: String,
    pub sex: Option<String>,
    pub conditions: Vec<FamilyCondition>,
    pub src: String,
}

/// One relative a clause matched, with the kinds that matched.
struct Hit<'a> {
    rel: &'a Relative,
    kind: &'a str,
}

/// The onset age a relative's condition must be below: by their sex, the lower one when unknown.
fn limit(before: (i32, i32), sex: Option<&str>) -> i32 {
    match sex {
        Some("male") => before.0,
        Some("female") => before.1,
        _ => before.0.min(before.1),
    }
}

/// What one clause matched: (relative id, kind) pairs that fire it, and the relatives with a
/// matching kind but no onset age.
struct Run {
    fired: BTreeSet<(Uuid, String)>,
    undated: BTreeSet<Uuid>,
}

fn run(c: &Clause, cat: &Catalog, relatives: &[Relative]) -> Run {
    let mut hits: Vec<Hit<'_>> = Vec::new();
    let mut undated = BTreeSet::new();
    for r in relatives {
        let Some(row) = cat.relationship(&r.relationship) else {
            continue;
        };
        let blood = row.degree > 0 && c.degree.contains(&row.degree);
        if !(blood && (c.relationships.is_empty() || c.relationships.contains(&r.relationship))) {
            continue;
        }
        if c.sex.as_deref().is_some_and(|s| r.sex.as_deref() != Some(s)) {
            continue;
        }
        for cond in &r.conditions {
            let kind = cond.kind.as_str();
            let wanted = if c.same_kind {
                kind != "other" && cat.condition(kind).is_some()
            } else {
                c.kinds.iter().any(|k| k == kind)
            };
            if !wanted {
                continue;
            }
            match (c.before, cond.onset_age) {
                (None, _) => hits.push(Hit { rel: r, kind }),
                (Some(b), Some(age)) if age < limit(b, r.sex.as_deref()) => hits.push(Hit { rel: r, kind }),
                (Some(_), Some(_)) => {}
                (Some(_), None) => {
                    undated.insert(r.id);
                }
            }
        }
    }
    // the groups a count is reached in: one per (kind with same_kind) x (side with same_side)
    let mut groups: BTreeMap<(String, String), BTreeSet<(Uuid, String)>> = BTreeMap::new();
    for h in &hits {
        let kind = if c.same_kind { h.kind } else { "" };
        let side = cat.relationship(&h.rel.relationship).map_or("", |r| r.side.as_str());
        let sides: Vec<&str> = match (c.same_side, side) {
            (false, _) => vec![""],
            (true, "own") if BOTH_SIDES.contains(&h.rel.relationship.as_str()) => SIDES.to_vec(),
            // a half-sibling or a parent of unknown side: the side is not known
            (true, "own") => Vec::new(),
            (true, s) => vec![s],
        };
        for s in sides {
            groups
                .entry((kind.to_string(), s.to_string()))
                .or_default()
                .insert((h.rel.id, h.kind.to_string()));
        }
    }
    let fired = groups
        .into_values()
        .filter(|g| g.iter().map(|(id, _)| id).collect::<BTreeSet<_>>().len() >= c.count)
        .flatten()
        .collect();
    Run { fired, undated }
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

/// One rule over the relatives: a pattern that is present, one that cannot be checked because an
/// onset age is missing, or nothing.
fn item(rule: &Rule, cat: &Catalog, relatives: &[Relative]) -> Option<Value> {
    let mut fired: BTreeSet<(Uuid, String)> = BTreeSet::new();
    let mut undated: BTreeSet<Uuid> = BTreeSet::new();
    for c in &rule.any {
        let r = run(c, cat, relatives);
        fired.extend(r.fired);
        undated.extend(r.undated);
    }
    let mut kinds: Vec<&str> = fired.iter().map(|(_, k)| k.as_str()).collect();
    kinds.sort_unstable();
    kinds.dedup();
    let (status, ids, reason) = if !fired.is_empty() {
        ("present", fired.iter().map(|(id, _)| *id).collect(), None)
    } else if !undated.is_empty() {
        ("not_assessed", undated, Some("no_onset_age"))
    } else {
        return None;
    };
    // drawing order: oldest generation first, then left to right, then id
    let mut members: Vec<&Relative> = relatives.iter().filter(|r| ids.contains(&r.id)).collect();
    let place = |r: &Relative| {
        cat.relationship(&r.relationship)
            .map_or((i32::MAX, i32::MAX), |x| (x.generation, x.slot))
    };
    members.sort_by(|a, b| place(a).cmp(&place(b)).then_with(|| a.id.cmp(&b.id)));
    let rel: Vec<&str> = members.iter().map(|r| r.relationship.as_str()).collect();
    let mut src: Vec<&str> = members.iter().map(|r| r.src.as_str()).collect();
    src.sort_unstable();
    src.dedup();
    Some(drop_nulls(json!({
        "rule": rule.id,
        "status": status,
        "reason_code": reason,
        "basis": rule.basis,
        "source": rule.source,
        "cite": rule.cite,
        "members": members.iter().map(|r| r.id).collect::<Vec<_>>(),
        "rel": rel,
        "kinds": kinds,
        "src": src,
    })))
}

/// The hash of a family document, as `state_hash` hashes the state body.
pub fn family_hash(doc: &Value) -> Result<String> {
    let mut clean = doc.clone();
    if let Some(m) = clean.as_object_mut() {
        m.shift_remove("family_hash");
    }
    let bytes = canonical_json(&clean).map_err(|e| ownhealth_core::Error::Other(anyhow::anyhow!("{e}")))?;
    Ok(format!("sha256:{}", hex::encode(Sha256::digest(bytes))))
}

/// The document for one profile's relatives, in rule order: present patterns first.
pub fn document(profile: &str, as_of: NaiveDate, relatives: &[Relative]) -> Result<Value> {
    let pat = load()?;
    let cat = ownhealth_core::family::catalog()?;
    document_with(pat, cat, profile, as_of, relatives)
}

/// `document` with a given patterns file (tests feed a mutated one).
pub fn document_with(
    pat: &Patterns,
    cat: &Catalog,
    profile: &str,
    as_of: NaiveDate,
    relatives: &[Relative],
) -> Result<Value> {
    let mut items: Vec<Value> = pat.rules.iter().filter_map(|r| item(r, cat, relatives)).collect();
    // stable sort: present before not_assessed, rule order inside each
    items.sort_by_key(|i| i["status"] != "present");
    let mut doc = json!({
        "schema": SCHEMA,
        "profile": profile,
        "as_of": iso(as_of),
        "rules": format!("{FILE} {}", pat.version),
        "items": items,
    });
    let hash = family_hash(&doc)?;
    if let Some(m) = doc.as_object_mut() {
        m.insert("family_hash".into(), json!(hash));
    }
    Ok(doc)
}

type Row = (
    Uuid,
    String,
    Option<String>,
    Json<Vec<FamilyCondition>>,
    String,
    Option<Uuid>,
);

/// The relatives of one profile the caller may read, as the store knew them at the end of `as_of`:
/// sensitive rows only with read:sensitive, and no relative linked to a profile the caller cannot
/// read (read:records there).
pub async fn relatives(
    conn: &mut PgConnection,
    principal: &Principal,
    profile_id: Uuid,
    scopes: &Scopes,
    as_of: NaiveDate,
) -> Result<Vec<Relative>> {
    let sql = format!(
        "select m.id, m.relationship, m.sex_at_birth, m.conditions, m.src_id, l.linked_profile_id \
         from family_member m left join family_member_link l on l.family_member_id = m.id \
         where m.profile_id = $1 and ($3 or m.sensitivity = '{{}}') and {} \
         order by m.recorded_at, m.id",
        ownhealth_core::lifecycle::visible_sql("m", "$2")
    );
    let rows: Vec<Row> = sqlx::query_as(sqlx::AssertSqlSafe(sql))
        .bind(profile_id)
        .bind(day_end(as_of))
        .bind(scopes.contains("read:sensitive"))
        .fetch_all(&mut *conn)
        .await?;
    let mut out = Vec::with_capacity(rows.len());
    for (id, relationship, sex, conditions, src, linked) in rows {
        if let Some(linked) = linked {
            let readable = match effective_permission(&mut *conn, principal, linked).await {
                Ok(perm) => perm.has("read:records"),
                Err(ownhealth_core::Error::AccessDenied { .. }) => false,
                Err(e) => return Err(e),
            };
            if !readable {
                continue;
            }
        }
        out.push(Relative {
            id,
            relationship,
            sex,
            conditions: conditions.0,
            src,
        });
    }
    Ok(out)
}

/// The document of one profile for one caller. Every pattern is a record: without read:records
/// there is none.
pub async fn build_document(
    conn: &mut PgConnection,
    principal: &Principal,
    profile: &ownhealth_core::access::Profile,
    scopes: &Scopes,
    as_of: NaiveDate,
) -> Result<Value> {
    let rel = if scopes.contains("read:records") {
        relatives(conn, principal, profile.id, scopes, as_of).await?
    } else {
        Vec::new()
    };
    document(&profile.slug, as_of, &rel)
}

/// The words of one template in both languages, as the pedigree's labels are.
fn both(v: Option<&Value>) -> Value {
    json!({"en": v.and_then(|v| v.get("en")), "es": v.and_then(|v| v.get("es"))})
}

/// The document with its words: every item gains `title` and `why`, and `reason` for one that could
/// not be checked, each as `{en, es}` so the screen can switch language without a new request.
pub fn render(doc: &Value) -> Result<Value> {
    let pat = load()?;
    let t = |part: &str, key: &str| both(pat.templates.get(part).and_then(|p| p.get(key)));
    let mut out = doc.as_object().cloned().unwrap_or_default();
    let items: Vec<Value> = doc["items"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|it| {
            let mut m = it.as_object().cloned().unwrap_or_default();
            let rule = it["rule"].as_str().unwrap_or_default();
            m.insert("title".into(), t("title", rule));
            m.insert("why".into(), t("why", rule));
            if let Some(code) = it["reason_code"].as_str() {
                m.insert("reason".into(), t("reasons", code));
            }
            Value::Object(m)
        })
        .collect();
    out.insert("items".into(), Value::Array(items));
    Ok(Value::Object(out))
}

/// Hex digits of `family_hash` the status section carries, as `family_hash12`.
pub const STATUS_HASH_HEX: usize = 12;

/// How many relationship codes one item of the `family` status section names; `rel_total` counts
/// the rest, so a large family cannot push the 2 KB page over.
pub const STATUS_REL: usize = 4;

/// The `family` section of get_health_status, by rule id and RoleCode only (no URLs, no names):
/// - `items`: the first `STATUS_TOP` present patterns, each flat so the text form prints one line per
///   pattern: `rule`, `rel` (its distinct RoleCodes in drawing order, at most `STATUS_REL`, joined by
///   spaces) and `rel_total` when it has more;
/// - `pattern_total` when more patterns are present than it lists;
/// - `not_assessed`: the rule ids that could not be checked (an onset age is missing), so a reader
///   can tell "no pattern" from "could not check";
/// - `family_hash12`: the first `STATUS_HASH_HEX` hex digits of the document's `family_hash`, enough to
///   tell that the patterns changed. The whole hash does not fit beside a full demo page in 2 KB, and
///   the key is not `family_hash` so no client mistakes it for the whole one.
///
/// None when no pattern is present or not assessed: the 2 KB page spends nothing on an empty list.
pub fn status_section(doc: &Value) -> Option<Value> {
    let all: Vec<&Value> = doc["items"].as_array().into_iter().flatten().collect();
    let with = |status: &str| -> Vec<&Value> { all.iter().copied().filter(|i| i["status"] == status).collect() };
    let present = with("present");
    let unchecked: Vec<&Value> = with("not_assessed").iter().map(|i| &i["rule"]).collect();
    if present.is_empty() && unchecked.is_empty() {
        return None;
    }
    let items: Vec<Value> = present
        .iter()
        .take(STATUS_TOP)
        .map(|i| {
            let mut codes: Vec<&str> = Vec::new();
            for c in i["rel"].as_array().into_iter().flatten().filter_map(Value::as_str) {
                if !codes.contains(&c) {
                    codes.push(c);
                }
            }
            let mut m = Map::new();
            m.insert("rule".into(), i["rule"].clone());
            m.insert(
                "rel".into(),
                json!(codes.iter().take(STATUS_REL).copied().collect::<Vec<_>>().join(" ")),
            );
            if codes.len() > STATUS_REL {
                m.insert("rel_total".into(), json!(codes.len()));
            }
            Value::Object(m)
        })
        .collect();
    let mut m = Map::new();
    // the scalars named for the section, so the text form does not read them as fields of the status
    if present.len() > items.len() {
        m.insert("pattern_total".into(), json!(present.len()));
    }
    let hash = doc["family_hash"].as_str().unwrap_or_default();
    let short: String = hash
        .trim_start_matches("sha256:")
        .chars()
        .take(STATUS_HASH_HEX)
        .collect();
    m.insert("family_hash12".into(), json!(short));
    if !items.is_empty() {
        m.insert("items".into(), Value::Array(items));
    }
    if !unchecked.is_empty() {
        m.insert("not_assessed".into(), json!(unchecked));
    }
    Some(Value::Object(m))
}
