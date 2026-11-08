//! Lane E: rules and state (`state/`, `rules/`; architecture.md 11, mcp.md 4, 5.6). The rule pack
//! (`backend/src/ownhealth/rules/pack/*.yaml`) and the CPIC snapshot are read unchanged. Canonical
//! JSON and `state_hash` are already in `ownhealth_core::canonical` (byte-identical to Python).
//!
//! Every function below is a stub until lane E lands it: it returns `Error::NotImplemented`.

use chrono::{DateTime, NaiveDate, Utc};
use futures::future::BoxFuture;
use ownhealth_core::access::Scopes;
use ownhealth_core::jobs::{self, Job, JobContext, JobError};
use ownhealth_core::{Error, Result};
use serde_json::Value;
use sqlx::PgConnection;
use uuid::Uuid;

pub use ownhealth_core::canonical::{canonical_json, state_hash};

/// `rules-<pack version>+<sha>`, as Python's `rules_version()`.
pub fn rules_version() -> Result<String> {
    Err(Error::NotImplemented("lane E: rules::rules_version"))
}

/// The language-neutral body with `state_hash`; `known_at` is a knowledge cut, not hashed.
pub async fn build_state(
    _conn: &mut PgConnection,
    _profile_id: Uuid,
    _as_of: NaiveDate,
    _known_at: Option<DateTime<Utc>>,
) -> Result<Value> {
    Err(Error::NotImplemented("lane E: rules::build_state"))
}

/// `identity_mode`: pseudonymous | named. `scopes`: names left-out sections in withheld.by_scope.
pub fn render(
    _body: &Value,
    _lang: &str,
    _units: &str,
    _identity_mode: &str,
    _scopes: Option<&Scopes>,
) -> Result<Value> {
    Err(Error::NotImplemented("lane E: rules::render"))
}

/// ok | watch | out | stale | nodata
pub async fn status_word(
    _conn: &mut PgConnection,
    _profile_id: Uuid,
    _alias_group: &str,
    _as_of: NaiveDate,
) -> Result<&'static str> {
    Err(Error::NotImplemented("lane E: rules::status_word"))
}

pub async fn dashboard(
    _conn: &mut PgConnection,
    _profile_id: Uuid,
    _as_of: NaiveDate,
    _lang: &str,
    _scopes: &Scopes,
) -> Result<Value> {
    Err(Error::NotImplemented("lane E: rules::dashboard"))
}

/// mcp.md 5.6 `data`. An unknown drug is `Error::NotFound`.
pub async fn check_drug(
    _conn: &mut PgConnection,
    _profile_id: Uuid,
    _drug: &str,
    _scopes: &Scopes,
    _lang: &str,
    _as_of: Option<NaiveDate>,
) -> Result<Value> {
    Err(Error::NotImplemented("lane E: rules::check_drug"))
}

/// `{cpic_drug_id, name}` or None.
pub async fn resolve_drug(_conn: &mut PgConnection, _name: &str) -> Result<Option<Value>> {
    Err(Error::NotImplemented("lane E: rules::resolve_drug"))
}

fn rebuild_job(_ctx: JobContext, _job: Job) -> BoxFuture<'static, jobs::HandlerResult> {
    Box::pin(async { Err(JobError::from(Error::NotImplemented("lane E: job state.rebuild"))) })
}

/// Register lane E's job handler (`state.rebuild`) and demo hook (order 500, if any).
pub fn register() {
    jobs::register_handler("state.rebuild", rebuild_job);
}
