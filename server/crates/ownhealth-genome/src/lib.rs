//! Lane D: genome (`genome/`, genomics.md). Imports OwnGenome facts files (schema in
//! `backend/genome-facts/`, read unchanged), lists findings behind the gate, and reads the
//! uploaded .vcf.gz/.tbi with noodles for `lookup_variant`. Demo hook `genome` (order 400).
//!
//! Every function below is a stub until lane D lands it: it returns `Error::NotImplemented`.
//! The constants are final.

use chrono::NaiveDate;
use futures::future::BoxFuture;
use ownhealth_core::access::Permission;
use ownhealth_core::blobs::BlobStore;
use ownhealth_core::ingest::ImportSummary;
use ownhealth_core::jobs::{self, Job, JobContext, JobError};
use ownhealth_core::{Error, Result};
use serde_json::Value;
use sqlx::PgConnection;
use uuid::Uuid;

pub const CALL_STATES: [&str; 5] = ["ref", "alt", "no_call", "not_on_array", "not_covered"];
pub const EVIDENCE: [&str; 4] = ["E1", "E2", "E3", "E4"];
pub const CONFIDENCE: [&str; 4] = ["high", "medium", "low", "unconfirmed"];
pub const NOTICE: &str = "Information for personal records, not a diagnosis.";

/// What a caller may see of a profile's genome on one surface (`genome/findings.py` `Gate`).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Gate {
    pub surface: String,
    pub genome: bool,
    pub raw: bool,
    pub sensitive: bool,
    pub minor: bool,
}

/// `surface`: api | mcp.
pub fn gate_for(_perm: &Permission, _surface: &str, _on: NaiveDate, _age_of_majority: i32) -> Result<Gate> {
    Err(Error::NotImplemented("lane D: genome::gate_for"))
}

pub fn validate_facts(_doc: &Value) -> Result<Vec<String>> {
    Err(Error::NotImplemented("lane D: genome::validate_facts"))
}

/// `attachments`: uploaded file name -> blob sha256 (the .vcf.gz and .tbi).
pub async fn import_genome_facts(
    _conn: &mut PgConnection,
    _blobs: &BlobStore,
    _document_id: Uuid,
    _attachments: &[(String, String)],
    _actor_user: Option<Uuid>,
) -> Result<ImportSummary> {
    Err(Error::NotImplemented("lane D: genome::import_genome_facts"))
}

#[derive(Debug, Clone, Default)]
pub struct FindingsQuery<'a> {
    pub category: Option<&'a str>,
    pub gene: Option<&'a str>,
    pub min_evidence: Option<&'a str>,
    pub as_of: Option<NaiveDate>,
}

/// The `Findings` JSON (findings, not_assessed, coverage, limitations, db_versions).
pub async fn list_findings(
    _conn: &mut PgConnection,
    _profile_id: Uuid,
    _gate: &Gate,
    _q: &FindingsQuery<'_>,
) -> Result<Value> {
    Err(Error::NotImplemented("lane D: genome::list_findings"))
}

#[derive(Debug, Clone, Default)]
pub struct VariantQuery<'a> {
    pub rsid: Option<&'a str>,
    pub hgvs: Option<&'a str>,
    pub region: Option<&'a str>,
    pub cursor: Option<&'a str>,
}

pub async fn lookup_variant(
    _conn: &mut PgConnection,
    _blobs: &BlobStore,
    _profile_id: Uuid,
    _gate: &Gate,
    _q: &VariantQuery<'_>,
) -> Result<Value> {
    Err(Error::NotImplemented("lane D: genome::lookup_variant"))
}

pub async fn pgx_for_drug(
    _conn: &mut PgConnection,
    _profile_id: Uuid,
    _cpic_drug_id: &str,
    _as_of: Option<NaiveDate>,
) -> Result<Vec<Value>> {
    Err(Error::NotImplemented("lane D: genome::pgx_for_drug"))
}

pub fn is_sensitive_gene(_gene: &str) -> bool {
    false
}

fn import_job(_ctx: JobContext, _job: Job) -> BoxFuture<'static, jobs::HandlerResult> {
    Box::pin(async { Err(JobError::from(Error::NotImplemented("lane D: job import.genome_facts"))) })
}

/// Register lane D's job handler (`import.genome_facts`), accept handlers
/// (`genome_reinterpretation`) and demo hook (`genome`, order 400).
pub fn register() {
    jobs::register_handler("import.genome_facts", import_job);
}
