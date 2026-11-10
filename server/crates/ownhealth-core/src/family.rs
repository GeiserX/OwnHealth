//! Family history (v0.3 Insight): one versioned row per relative of one profile, stored with its
//! FHIR R4 FamilyMemberHistory (migration 1110).
//!
//! Same lifecycle as records: add, supersede with a new version, retract with a reason. A relative's
//! conditions live inside the version, so a changed condition is a new version. The link to the
//! relative's own profile (`family_member_link`) sits outside the lifecycle trigger and is copied to
//! each new version in the same transaction. Nothing here enters the hashed state body.
//!
//! The vocabulary is shipped data: `data/family/relationships.yaml` (HL7 v3 RoleCode family codes, how
//! the pedigree places them) and `data/family/conditions.yaml` (condition kinds, SNOMED CT codes).
//! Every row carries an https `cite` and both es and en names; the loader refuses anything else.

use std::sync::OnceLock;

use chrono::{DateTime, Datelike, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use sqlx::PgConnection;
use sqlx::types::Json;
use uuid::Uuid;

use crate::error::{Error, Result};
use crate::fhir::{UCUM, system_url};
use crate::lifecycle;
use crate::observations::{ACCEPTED_BY, Committed, SENSITIVITY};
use crate::provenance::{SrcIn, SrcRef, ensure_src};

pub const TABLE: &str = "family_member";
pub const ROLE_CODE: &str = "http://terminology.hl7.org/CodeSystem/v3-RoleCode";
pub const GENDER: &str = "http://hl7.org/fhir/administrative-gender";
pub const SNOMED: &str = "http://snomed.info/sct";
const ACT_CODE: &str = "http://terminology.hl7.org/CodeSystem/v3-ActCode";
/// An approximate onset age travels as `onsetString` with this prefix (`~52`): FHIR R4 Age has no
/// "about" comparator.
pub const APPROX: &str = "~";
const SEXES: [&str; 3] = ["female", "male", "unknown"];
const MAX_CONDITIONS: usize = 40;

const RELATIONSHIPS_YAML: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../../data/family/relationships.yaml"
));
const CONDITIONS_YAML: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../../data/family/conditions.yaml"
));

// ------------------------------------------------------------------ catalog ----

/// One relationship row of `relationships.yaml`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Relationship {
    pub code: String,
    pub en: String,
    pub es: String,
    pub generation: i32,
    pub side: String,
    pub degree: i32,
    pub sex: Option<String>,
    pub slot: i32,
    pub union: Option<String>,
    pub child_of: Option<String>,
    pub cite: String,
}

/// One condition kind of `conditions.yaml`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConditionKind {
    pub kind: String,
    pub en: String,
    pub es: String,
    pub short_en: String,
    pub short_es: String,
    pub group: String,
    pub code: Option<String>,
    pub cite: String,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
struct File<T> {
    version: String,
    rows: Vec<T>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Catalog {
    /// `relationships.yaml 0.1.0, conditions.yaml 0.1.0`
    pub version: String,
    pub relationships: Vec<Relationship>,
    pub conditions: Vec<ConditionKind>,
}

impl Catalog {
    pub fn relationship(&self, code: &str) -> Option<&Relationship> {
        self.relationships.iter().find(|r| r.code == code)
    }

    pub fn condition(&self, kind: &str) -> Option<&ConditionKind> {
        self.conditions.iter().find(|c| c.kind == kind)
    }

    /// The kind a received SNOMED code stands for.
    pub fn kind_for_code(&self, code: &str) -> Option<&ConditionKind> {
        self.conditions.iter().find(|c| c.code.as_deref() == Some(code))
    }
}

const SIDES: [&str; 4] = ["paternal", "maternal", "own", "partner"];
const GROUPS: [&str; 6] = [
    "cardiovascular",
    "metabolic",
    "cancer",
    "neurological",
    "respiratory",
    "other",
];
/// The couples of the drawing; the proband is in `self` and a child of `parents`.
const UNIONS: [&str; 5] = ["pgp", "mgp", "parents", "self", "inlaws"];

fn named(what: &str, key: &str, en: &str, es: &str, cite: &str) -> std::result::Result<(), String> {
    if !cite.starts_with("https://") {
        return Err(format!("{what} {key}: cite must be an https URL"));
    }
    if en.trim().is_empty() || es.trim().is_empty() {
        return Err(format!("{what} {key}: needs both an en and an es name"));
    }
    Ok(())
}

/// Parse and check both files. Public so tests can feed broken input.
pub fn load(relationships: &str, conditions: &str) -> std::result::Result<Catalog, String> {
    let rel: File<Relationship> =
        serde_yaml::from_str(relationships).map_err(|e| format!("relationships.yaml: {e}"))?;
    let cond: File<ConditionKind> = serde_yaml::from_str(conditions).map_err(|e| format!("conditions.yaml: {e}"))?;
    let mut seen = std::collections::BTreeSet::new();
    for r in &rel.rows {
        named("relationship", &r.code, &r.en, &r.es, &r.cite)?;
        if !seen.insert(r.code.clone()) {
            return Err(format!("relationship {} appears twice", r.code));
        }
        if !SIDES.contains(&r.side.as_str()) {
            return Err(format!("relationship {}: unknown side {}", r.code, r.side));
        }
        if r.sex.as_deref().is_some_and(|s| !SEXES.contains(&s)) {
            return Err(format!("relationship {}: unknown sex", r.code));
        }
        for u in [&r.union, &r.child_of].into_iter().flatten() {
            if !UNIONS.contains(&u.as_str()) {
                return Err(format!("relationship {}: unknown union {u}", r.code));
            }
        }
    }
    let mut seen = std::collections::BTreeSet::new();
    let mut codes = std::collections::BTreeSet::new();
    for c in &cond.rows {
        named("condition", &c.kind, &c.en, &c.es, &c.cite)?;
        if c.short_en.trim().is_empty() || c.short_es.trim().is_empty() {
            return Err(format!("condition {}: needs both short names", c.kind));
        }
        if !seen.insert(c.kind.clone()) {
            return Err(format!("condition {} appears twice", c.kind));
        }
        if !GROUPS.contains(&c.group.as_str()) {
            return Err(format!("condition {}: unknown group {}", c.kind, c.group));
        }
        if let Some(code) = &c.code
            && !codes.insert(code.clone())
        {
            return Err(format!("condition code {code} appears twice"));
        }
    }
    if !seen.contains("other") {
        return Err("conditions.yaml needs the kind other".into());
    }
    Ok(Catalog {
        version: format!("relationships.yaml {}, conditions.yaml {}", rel.version, cond.version),
        relationships: rel.rows,
        conditions: cond.rows,
    })
}

/// The shipped catalog, loaded once.
pub fn catalog() -> Result<&'static Catalog> {
    static CATALOG: OnceLock<std::result::Result<Catalog, String>> = OnceLock::new();
    CATALOG
        .get_or_init(|| load(RELATIONSHIPS_YAML, CONDITIONS_YAML))
        .as_ref()
        .map_err(|e| Error::Other(anyhow::anyhow!("family data: {e}")))
}

// ------------------------------------------------------------------- models ----

/// One condition of a relative, as stored in `family_member.conditions`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FamilyCondition {
    pub kind: String,
    #[serde(default)]
    pub text: Option<String>,
    /// As received (an import), or `snomed` with the catalog code when a person picked the kind.
    #[serde(default)]
    pub code_system: Option<String>,
    #[serde(default)]
    pub code: Option<String>,
    #[serde(default)]
    pub onset_age: Option<i32>,
    #[serde(default)]
    pub onset_approx: bool,
}

/// A new version of a relative.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemberIn {
    pub profile_id: Uuid,
    pub relationship: String,
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default)]
    pub sex_at_birth: Option<String>,
    #[serde(default)]
    pub born_year: Option<i32>,
    #[serde(default)]
    pub deceased: bool,
    #[serde(default)]
    pub death_age: Option<i32>,
    #[serde(default)]
    pub conditions: Vec<FamilyCondition>,
    #[serde(default)]
    pub sensitivity: Vec<String>,
    /// The received FamilyMemberHistory (an import); otherwise built from the fields.
    #[serde(default)]
    pub fhir: Option<Value>,
}

impl MemberIn {
    pub fn new(profile_id: Uuid, relationship: &str) -> MemberIn {
        MemberIn {
            profile_id,
            relationship: relationship.into(),
            display_name: None,
            sex_at_birth: None,
            born_year: None,
            deceased: false,
            death_age: None,
            conditions: Vec::new(),
            sensitivity: Vec::new(),
            fhir: None,
        }
    }

    /// Check every field and fill what a person leaves out: a trimmed name (empty is none), the sex
    /// the relationship implies, the catalog code of a picked condition kind.
    pub fn normalized(mut self) -> Result<MemberIn> {
        let cat = catalog()?;
        let invalid = |m: String| Err(Error::Invalid(m));
        let Some(rel) = cat.relationship(&self.relationship) else {
            return invalid(format!("relationship: unknown code {:?}", self.relationship));
        };
        self.display_name = self
            .display_name
            .map(|n| n.trim().to_string())
            .filter(|n| !n.is_empty());
        if self.display_name.as_ref().is_some_and(|n| n.chars().count() > 80) {
            return invalid("display_name: at most 80 characters".into());
        }
        if self.sex_at_birth.is_none() {
            self.sex_at_birth = rel.sex.clone();
        }
        if let Some(s) = &self.sex_at_birth
            && !SEXES.contains(&s.as_str())
        {
            return invalid(format!("sex_at_birth: unknown value {s:?}"));
        }
        let this_year = Utc::now().year();
        if self.born_year.is_some_and(|y| !(1800..=this_year).contains(&y)) {
            return invalid(format!("born_year: between 1800 and {this_year}"));
        }
        if self.death_age.is_some_and(|a| !(0..=130).contains(&a)) {
            return invalid("death_age: between 0 and 130".into());
        }
        if self.death_age.is_some() {
            self.deceased = true;
        }
        if let Some(s) = self.sensitivity.iter().find(|s| !SENSITIVITY.contains(&s.as_str())) {
            return invalid(format!("sensitivity: unknown value {s:?}"));
        }
        if self.conditions.len() > MAX_CONDITIONS {
            return invalid(format!("conditions: at most {MAX_CONDITIONS}"));
        }
        for c in &mut self.conditions {
            let Some(kind) = cat.condition(&c.kind) else {
                return invalid(format!("conditions: unknown kind {:?}", c.kind));
            };
            c.text = c.text.take().map(|t| t.trim().to_string()).filter(|t| !t.is_empty());
            if c.text.as_ref().is_some_and(|t| t.chars().count() > 200) {
                return invalid("conditions: text at most 200 characters".into());
            }
            if kind.kind == "other" && c.text.is_none() {
                return invalid("conditions: a condition of kind other needs its text".into());
            }
            if c.onset_age.is_some_and(|a| !(0..=130).contains(&a)) {
                return invalid("conditions: onset_age between 0 and 130".into());
            }
            if c.onset_age.is_none() {
                c.onset_approx = false;
            }
            if c.code.is_none() && c.code_system.is_none() && kind.code.is_some() {
                c.code_system = Some("snomed".into());
                c.code = kind.code.clone();
            }
        }
        Ok(self)
    }
}

/// What a new version does with the link to the relative's own profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkChange {
    Keep,
    To(Uuid),
    Remove,
}

/// A stored version, with its link.
#[derive(Debug, Clone, PartialEq, Serialize, sqlx::FromRow)]
pub struct Member {
    pub id: Uuid,
    pub profile_id: Uuid,
    pub relationship: String,
    pub display_name: Option<String>,
    pub sex_at_birth: Option<String>,
    pub born_year: Option<i32>,
    pub deceased: bool,
    pub death_age: Option<i32>,
    pub conditions: Json<Vec<FamilyCondition>>,
    pub sensitivity: Vec<String>,
    pub src_id: String,
    pub status: String,
    pub status_reason: Option<String>,
    pub supersedes: Option<Uuid>,
    pub recorded_at: DateTime<Utc>,
    pub linked_profile_id: Option<Uuid>,
}

// --------------------------------------------------------------------- FHIR ----

fn drop_empty(m: Map<String, Value>) -> Value {
    Value::Object(
        m.into_iter()
            .filter(|(_, v)| !(v.is_null() || v.as_array().is_some_and(Vec::is_empty)))
            .collect(),
    )
}

fn age(value: i32) -> Value {
    json!({"value": value, "unit": "a", "system": UCUM, "code": "a"})
}

/// The FamilyMemberHistory we store for a version we create. The export adds `patient` and `meta`.
pub fn fhir(m: &MemberIn) -> Result<Value> {
    let cat = catalog()?;
    let rel = cat
        .relationship(&m.relationship)
        .ok_or_else(|| Error::Invalid(format!("relationship: unknown code {:?}", m.relationship)))?;
    let conditions: Vec<Value> = m
        .conditions
        .iter()
        .map(|c| {
            let label = cat.condition(&c.kind).map(|k| k.en.as_str()).unwrap_or(c.kind.as_str());
            let coding: Vec<Value> = match (system_url(c.code_system.as_deref()), &c.code) {
                (Some(system), Some(code)) => vec![json!({"system": system, "code": code})],
                _ => vec![],
            };
            let mut code = Map::new();
            code.insert("coding".into(), json!(coding));
            code.insert("text".into(), json!(c.text.as_deref().unwrap_or(label)));
            let mut out = Map::new();
            out.insert("code".into(), drop_empty(code));
            match (c.onset_age, c.onset_approx) {
                (Some(a), true) => {
                    out.insert("onsetString".into(), json!(format!("{APPROX}{a}")));
                }
                (Some(a), false) => {
                    out.insert("onsetAge".into(), age(a));
                }
                _ => {}
            }
            Value::Object(out)
        })
        .collect();
    let mut r = Map::new();
    r.insert("resourceType".into(), json!("FamilyMemberHistory"));
    // the sensitivity travels with the export, so an import of it stays hidden without read:sensitive
    let mut labels: Vec<&str> = m
        .sensitivity
        .iter()
        .filter_map(|s| match s.as_str() {
            "mental_health" => Some("PSY"),
            "sexual_health" => Some("SEX"),
            "reproductive" => Some("SDV"),
            _ => None,
        })
        .collect();
    labels.sort_unstable();
    labels.dedup();
    if !labels.is_empty() {
        let security: Vec<Value> = labels.iter().map(|c| json!({"system": ACT_CODE, "code": c})).collect();
        r.insert("meta".into(), json!({"security": security}));
    }
    r.insert("status".into(), json!("completed"));
    r.insert(
        "relationship".into(),
        json!({"coding": [{"system": ROLE_CODE, "code": rel.code}], "text": rel.en}),
    );
    r.insert("name".into(), json!(m.display_name));
    if let Some(s) = &m.sex_at_birth {
        r.insert("sex".into(), json!({"coding": [{"system": GENDER, "code": s}]}));
    }
    if let Some(y) = m.born_year {
        r.insert("bornDate".into(), json!(format!("{y:04}")));
    }
    match (m.deceased, m.death_age) {
        (_, Some(a)) => {
            r.insert("deceasedAge".into(), age(a));
        }
        (true, None) => {
            r.insert("deceasedBoolean".into(), json!(true));
        }
        _ => {}
    }
    r.insert("condition".into(), json!(conditions));
    Ok(drop_empty(r))
}

// --------------------------------------------------------------- the store ----

const COLUMNS: &str = "m.id, m.profile_id, m.relationship, m.display_name, m.sex_at_birth, m.born_year, m.deceased, \
                       m.death_age, m.conditions, m.sensitivity, m.src_id, m.status, m.status_reason, m.supersedes, \
                       m.recorded_at, l.linked_profile_id";

/// A linked profile must be another profile of the same household.
async fn check_link(conn: &mut PgConnection, profile_id: Uuid, linked: Uuid) -> Result<()> {
    if linked == profile_id {
        return Err(Error::Invalid(
            "linked_profile: a profile cannot be its own relative".into(),
        ));
    }
    let same: Option<bool> = sqlx::query_scalar(
        "select a.household_id = b.household_id from profile a, profile b where a.id = $1 and b.id = $2",
    )
    .bind(profile_id)
    .bind(linked)
    .fetch_optional(&mut *conn)
    .await?;
    if same != Some(true) {
        return Err(Error::Invalid("linked_profile: not a profile of this household".into()));
    }
    Ok(())
}

/// Insert a version (and its link). The caller has checked access.
pub async fn add(
    conn: &mut PgConnection,
    member: &MemberIn,
    src: &SrcRef,
    accepted_by: &str,
    supersedes: Option<Uuid>,
    link: Option<Uuid>,
) -> Result<Committed> {
    if !ACCEPTED_BY.contains(&accepted_by) {
        return Err(Error::Invalid(format!("accepted_by: unknown value {accepted_by:?}")));
    }
    let member = member.clone().normalized()?;
    let profile_id = member.profile_id;
    if let Some(l) = link {
        check_link(&mut *conn, profile_id, l).await?;
    }
    let src = match src {
        SrcRef::New(s) if s.profile_id.is_none() => SrcRef::New(SrcIn {
            profile_id: Some(profile_id),
            ..s.clone()
        }),
        other => other.clone(),
    };
    let src_id = ensure_src(&mut *conn, &src, Some(profile_id)).await?;
    let resource = match &member.fhir {
        Some(r) => r.clone(),
        None => fhir(&member)?,
    };
    let id: Uuid = sqlx::query_scalar(
        "insert into family_member (profile_id, relationship, display_name, sex_at_birth, born_year, deceased, \
         death_age, conditions, sensitivity, src_id, accepted_by, supersedes, fhir) \
         values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13) returning id",
    )
    .bind(profile_id)
    .bind(&member.relationship)
    .bind(&member.display_name)
    .bind(&member.sex_at_birth)
    .bind(member.born_year)
    .bind(member.deceased)
    .bind(member.death_age)
    .bind(Json(&member.conditions))
    .bind(&member.sensitivity)
    .bind(&src_id)
    .bind(accepted_by)
    .bind(supersedes)
    .bind(&resource)
    .fetch_one(&mut *conn)
    .await?;
    if let Some(l) = link {
        sqlx::query("insert into family_member_link (family_member_id, linked_profile_id) values ($1, $2)")
            .bind(id)
            .bind(l)
            .execute(&mut *conn)
            .await?;
    }
    Ok(Committed { id, src_id })
}

/// A correction: the new version replaces the old one, which becomes superseded. The link moves to the
/// new version unless `link` changes it.
pub async fn supersede(
    conn: &mut PgConnection,
    old_id: Uuid,
    new: &MemberIn,
    src: &SrcRef,
    accepted_by: &str,
    reason: &str,
    link: LinkChange,
) -> Result<Committed> {
    let mut tx = sqlx::Connection::begin(&mut *conn).await?;
    let old = lifecycle::lock_current(&mut tx, TABLE, old_id).await?;
    if old.get("profile_id").and_then(Value::as_str) != Some(new.profile_id.to_string().as_str()) {
        return Err(Error::Lifecycle(
            "a correction cannot move a relative to another profile".into(),
        ));
    }
    let linked = match link {
        LinkChange::Keep => linked_profile(&mut tx, old_id).await?,
        LinkChange::To(p) => Some(p),
        LinkChange::Remove => None,
    };
    lifecycle::mark_superseded(&mut tx, TABLE, old_id, Some(reason)).await?;
    let committed = add(&mut tx, new, src, accepted_by, Some(old_id), linked).await?;
    tx.commit().await?;
    Ok(committed)
}

/// Withdraw a relative (entered in error, removed by a person). Reason required; nothing is deleted.
pub async fn retract(conn: &mut PgConnection, member_id: Uuid, reason: &str) -> Result<()> {
    lifecycle::retract(conn, TABLE, member_id, reason).await
}

pub async fn linked_profile(conn: &mut PgConnection, member_id: Uuid) -> Result<Option<Uuid>> {
    Ok(
        sqlx::query_scalar("select linked_profile_id from family_member_link where family_member_id = $1")
            .bind(member_id)
            .fetch_optional(conn)
            .await?,
    )
}

pub async fn get(conn: &mut PgConnection, member_id: Uuid) -> Result<Option<Member>> {
    let sql = format!(
        "select {COLUMNS} from family_member m left join family_member_link l on l.family_member_id = m.id \
         where m.id = $1"
    );
    Ok(sqlx::query_as::<_, Member>(sqlx::AssertSqlSafe(sql))
        .bind(member_id)
        .fetch_optional(conn)
        .await?)
}

/// The current relatives of a profile, oldest first. Rows with a sensitivity tag only with
/// `include_sensitive`.
pub async fn list(conn: &mut PgConnection, profile_id: Uuid, include_sensitive: bool) -> Result<Vec<Member>> {
    let sql = format!(
        "select {COLUMNS} from family_member m left join family_member_link l on l.family_member_id = m.id \
         where m.profile_id = $1 and m.status = 'current' and ($2 or m.sensitivity = '{{}}') \
         order by m.recorded_at, m.id"
    );
    Ok(sqlx::query_as::<_, Member>(sqlx::AssertSqlSafe(sql))
        .bind(profile_id)
        .bind(include_sensitive)
        .fetch_all(conn)
        .await?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_shipped_catalog_loads() {
        let cat = catalog().expect("catalog");
        assert!(
            cat.relationship("FTH")
                .is_some_and(|r| r.generation == -1 && r.degree == 1)
        );
        assert_eq!(cat.kind_for_code("22298006").map(|k| k.kind.as_str()), Some("mi"));
        assert!(cat.version.starts_with("relationships.yaml 0.1.0"));
    }

    #[test]
    fn the_loader_refuses_a_row_without_https_cite_or_a_language() {
        let cond = "version: 0.1.0\nrows:\n  - {kind: other, en: other, es: otra, short_en: o, short_es: o, group: other, code: null, cite: \"https://x.example\"}\n";
        let row = |cite: &str, es: &str| {
            format!(
                "version: 0.1.0\nrows:\n  - {{code: FTH, en: father, es: \"{es}\", generation: -1, side: paternal, degree: 1, sex: male, slot: 1, union: parents, child_of: null, cite: \"{cite}\"}}\n"
            )
        };
        assert!(load(&row("https://a.example", "padre"), cond).is_ok());
        assert!(matches!(load(&row("http://a.example", "padre"), cond), Err(e) if e.contains("https")));
        assert!(matches!(load(&row("https://a.example", " "), cond), Err(e) if e.contains("es name")));
    }

    #[test]
    fn a_member_fills_sex_and_codes_and_refuses_bad_values() {
        let pid = Uuid::nil();
        let mut m = MemberIn::new(pid, "FTH");
        m.display_name = Some("  ".into());
        m.death_age = Some(64);
        m.conditions = vec![FamilyCondition {
            kind: "mi".into(),
            text: None,
            code_system: None,
            code: None,
            onset_age: Some(52),
            onset_approx: false,
        }];
        let n = m.clone().normalized().expect("valid");
        assert_eq!(n.display_name, None);
        assert_eq!(n.sex_at_birth.as_deref(), Some("male"));
        assert!(n.deceased);
        assert_eq!(n.conditions[0].code.as_deref(), Some("22298006"));
        let r = fhir(&n).expect("fhir");
        assert_eq!(r["deceasedAge"]["value"], 64);
        assert_eq!(r["condition"][0]["onsetAge"]["value"], 52);
        assert_eq!(r["relationship"]["coding"][0]["code"], "FTH");

        let mut bad = m.clone();
        bad.relationship = "XYZ".into();
        assert!(bad.normalized().is_err());
        let mut bad = m.clone();
        bad.conditions[0].kind = "other".into();
        assert!(matches!(bad.normalized(), Err(e) if e.to_string().contains("needs its text")));
        let mut bad = m;
        bad.born_year = Some(1700);
        assert!(bad.normalized().is_err());
    }
}
