//! Lane C: structured imports and records (`ingest/fhir`, `ingest/apple_health`,
//! `ingest/manual.py`, `ingest/retract.py`, `wearables/`; connectors.md 2.2, 4, architecture.md 9).
//! Its demo hook is `demo/hooks/apple_fhir.py` (order 300).
//!
//! Every function below is a stub until lane C lands it: it returns `Error::NotImplemented`.

use chrono::{DateTime, FixedOffset, Utc};
use futures::future::BoxFuture;
use ownhealth_core::blobs::BlobStore;
use ownhealth_core::ingest::ImportSummary;
use ownhealth_core::jobs::{self, Job, JobContext, JobError};
use ownhealth_core::observations::Committed;
use ownhealth_core::{Error, Result};
use serde_json::Value;
use sqlx::PgConnection;
use uuid::Uuid;

pub mod fhir {
    use super::*;

    /// A FHIR/IPS bundle (json or ndjson) already stored as a blob.
    pub async fn import_fhir(
        _conn: &mut PgConnection,
        _blobs: &BlobStore,
        _document_id: Uuid,
    ) -> Result<ImportSummary> {
        Err(Error::NotImplemented("lane C: fhir::import_fhir"))
    }
}

pub mod apple_health {
    use super::*;

    pub async fn import_apple_health(
        _conn: &mut PgConnection,
        _blobs: &BlobStore,
        _document_id: Uuid,
    ) -> Result<ImportSummary> {
        Err(Error::NotImplemented("lane C: apple_health::import_apple_health"))
    }

    pub async fn rebuild_rollups(_conn: &mut PgConnection, _profile_id: Uuid, _metric: Option<&str>) -> Result<i64> {
        Err(Error::NotImplemented("lane C: apple_health::rebuild_rollups"))
    }
}

pub mod manual {
    use super::*;

    #[derive(Debug, Clone)]
    pub struct ManualObservation<'a> {
        pub profile_id: Uuid,
        pub user_id: Uuid,
        pub name: &'a str,
        pub value: &'a str,
        pub unit: Option<&'a str>,
        pub effective_at: DateTime<FixedOffset>,
        pub note: Option<&'a str>,
        pub category: Option<&'a str>,
        pub loinc: Option<&'a str>,
        pub corrects: Option<Uuid>,
        pub reason: Option<&'a str>,
    }

    pub async fn add_manual_observation(_conn: &mut PgConnection, _m: &ManualObservation<'_>) -> Result<Committed> {
        Err(Error::NotImplemented("lane C: manual::add_manual_observation"))
    }
}

pub mod wearables {
    use super::*;

    /// `resolution`: day | hour. The JSON shape of Python's `wearables.series`.
    pub async fn series(
        _conn: &mut PgConnection,
        _profile_id: Uuid,
        _metric: &str,
        _since: Option<DateTime<Utc>>,
        _until: Option<DateTime<Utc>>,
        _resolution: &str,
    ) -> Result<Value> {
        Err(Error::NotImplemented("lane C: wearables::series"))
    }

    pub fn metric_info(_key: &str) -> Option<Value> {
        None
    }
}

fn not_yet(what: &'static str) -> BoxFuture<'static, jobs::HandlerResult> {
    Box::pin(async move { Err(JobError::from(Error::NotImplemented(what))) })
}

fn import_fhir_job(_ctx: JobContext, _job: Job) -> BoxFuture<'static, jobs::HandlerResult> {
    not_yet("lane C: job import.fhir_bundle")
}

fn import_apple_health_job(_ctx: JobContext, _job: Job) -> BoxFuture<'static, jobs::HandlerResult> {
    not_yet("lane C: job import.apple_health")
}

/// Register lane C's job handlers (`import.fhir_bundle`, `import.apple_health`), accept handlers
/// (`fhir_unmapped`) and demo hook (`apple_fhir`, order 300).
pub fn register() {
    jobs::register_handler("import.fhir_bundle", import_fhir_job);
    jobs::register_handler("import.apple_health", import_apple_health_job);
}
