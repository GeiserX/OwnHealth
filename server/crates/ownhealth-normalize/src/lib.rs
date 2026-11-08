//! Lane A: normalization (`normalize/`, docs/design/ingestion.md 8, architecture.md 10).
//!
//! Printed value, unit and range stay the truth layer; everything here is derived and
//! recomputable. Importers (lanes B and C) and `propose` (lane F) call these functions; nobody
//! else parses units. The terminology tables are read unchanged from
//! `backend/src/ownhealth/normalize/terminology/` (embedded at build time).
//!
//! Every function below is a stub until lane A lands it: it returns `Error::NotImplemented`.

use chrono::{DateTime, FixedOffset, NaiveDate};
use futures::future::BoxFuture;
use ownhealth_core::acceptance::Check;
use ownhealth_core::jobs::{self, Job, JobContext, JobError};
use ownhealth_core::observations::ObservationIn;
use ownhealth_core::{Error, Result};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::PgConnection;
use uuid::Uuid;

const LANE: &str = "lane A (ownhealth-normalize)";

pub const NORMALIZER_VERSION: &str = "0.1.0";

/// `norm-<NORMALIZER_VERSION>+t<hash of every table>`, the same string Python computes.
pub fn derivation_version() -> Result<String> {
    Err(Error::NotImplemented("lane A: normalize::derivation_version"))
}

/// es | en | de | fr
pub type Locale = str;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ParsedValue {
    pub printed: String,
    /// None for text results.
    pub value: Option<Decimal>,
    /// <, >, <=, >=
    pub comparator: Option<String>,
    /// negative | positive | not_detected | trace
    pub text_code: Option<String>,
    /// A review code when the parse is not trusted (ambiguous_separator ...).
    pub reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RefRange {
    pub low: Option<Decimal>,
    pub high: Option<Decimal>,
    pub text: String,
    /// {sex, age_min, age_max, pregnancy}
    pub applies_to: Value,
    pub low_inclusive: bool,
    pub high_inclusive: bool,
    pub text_code: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Candidate {
    pub alias_group: String,
    pub loinc: Option<String>,
    pub property: Option<String>,
    pub method: Option<String>,
    pub confidence: f64,
    /// The alias row that matched, or "loinc" for a given code.
    pub matched: String,
    pub unit_ok: bool,
    pub specimen_ok: bool,
    pub partial: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Resolution {
    /// Always set: an uncoded row stays usable under its group.
    pub alias_group: String,
    pub loinc: Option<String>,
    pub property: Option<String>,
    pub method: Option<String>,
    /// A panel or category name, never coded as an analyte.
    pub refused: bool,
    /// unmapped name_partial unit_dimension_mismatch method_uncertain
    pub reasons: Vec<String>,
    pub confidence: f64,
    pub candidates: Vec<Candidate>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Normalized {
    pub observation: ObservationIn,
    pub checks: Vec<Check>,
}

/// What `resolve_analyte` needs besides the printed name.
#[derive(Debug, Clone, Default)]
pub struct ResolveQuery<'a> {
    pub lang: &'a str,
    pub unit: Option<&'a str>,
    pub specimen: Option<&'a str>,
    pub method: Option<&'a str>,
    pub provider: Option<&'a str>,
    pub household_id: Option<Uuid>,
    pub text_result: bool,
}

/// One printed row to normalize (`normalize_row` keyword arguments).
#[derive(Debug, Clone)]
pub struct RowIn<'a> {
    pub profile_id: Uuid,
    pub printed_name: &'a str,
    pub printed_value: &'a str,
    pub printed_unit: Option<&'a str>,
    pub printed_range: Option<&'a str>,
    pub printed_flag: Option<&'a str>,
    pub effective_at: DateTime<FixedOffset>,
    pub lang: &'a str,
    pub specimen: Option<&'a str>,
    pub provider: Option<&'a str>,
    pub loinc: Option<&'a str>,
    pub method: Option<&'a str>,
    pub document_id: Option<Uuid>,
}

pub fn parse_value(_printed: &str, _locale: &Locale, _unit: Option<&str>) -> Result<ParsedValue> {
    Err(Error::NotImplemented("lane A: normalize::parse_value"))
}

pub fn to_ucum(_printed_unit: &str) -> Option<String> {
    None
}

pub fn canonical_unit(_alias_group: &str) -> Option<String> {
    None
}

/// `{ucum: label}` for a group, or None.
pub fn group_units(_alias_group: &str) -> Option<Vec<(String, String)>> {
    None
}

pub fn display_name(_alias_group: &str, _lang: &str) -> Option<String> {
    None
}

pub fn convert(_value: Decimal, _from_ucum: &str, _to_ucum: &str, _alias_group: &str) -> Result<Option<Decimal>> {
    Err(Error::NotImplemented("lane A: normalize::convert"))
}

pub fn parse_range(_printed: &str, _locale: &Locale, _unit: Option<&str>) -> Result<Vec<RefRange>> {
    Err(Error::NotImplemented("lane A: normalize::parse_range"))
}

pub fn pick_range<'r>(_ranges: &'r [RefRange], _sex: Option<&str>, _age_years: Option<i32>) -> Option<&'r RefRange> {
    None
}

/// L | N | H | A | None
pub fn interpret(
    _value: Option<Decimal>,
    _text_code: Option<&str>,
    _r: Option<&RefRange>,
    _comparator: Option<&str>,
) -> Option<&'static str> {
    None
}

pub async fn resolve_analyte(
    _conn: Option<&mut PgConnection>,
    _printed_name: &str,
    _q: &ResolveQuery<'_>,
) -> Result<Resolution> {
    Err(Error::NotImplemented("lane A: normalize::resolve_analyte"))
}

pub async fn normalize_row(_conn: &mut PgConnection, _row: &RowIn<'_>) -> Result<Normalized> {
    Err(Error::NotImplemented("lane A: normalize::normalize_row"))
}

#[derive(Debug, Clone, Default)]
pub struct AliasIn<'a> {
    pub household_id: Uuid,
    pub alias: &'a str,
    pub alias_group: &'a str,
    pub lang: Option<&'a str>,
    pub added_by: Option<Uuid>,
    pub unit: Option<&'a str>,
    pub specimen: Option<&'a str>,
    pub loinc: Option<&'a str>,
}

/// Python `AliasError` is `Error::Invalid` here.
pub async fn add_alias(_conn: &mut PgConnection, _a: &AliasIn<'_>) -> Result<Uuid> {
    Err(Error::NotImplemented("lane A: normalize::add_alias"))
}

/// Recompute the derived layer; supersedes with status_reason `rederive <version>`. Returns rows changed.
pub async fn rederive(_conn: &mut PgConnection, _profile_id: Option<Uuid>, _since: Option<NaiveDate>) -> Result<i64> {
    Err(Error::NotImplemented("lane A: normalize::rederive"))
}

fn rederive_job(_ctx: JobContext, _job: Job) -> BoxFuture<'static, jobs::HandlerResult> {
    Box::pin(async { Err(JobError::from(Error::NotImplemented("lane A: job normalize.rederive"))) })
}

/// Register lane A's job handlers (`normalize.rederive`) and demo hook (order 100, if any).
pub fn register() {
    tracing::debug!(lane = LANE, "register");
    jobs::register_handler("normalize.rederive", rederive_job);
}
