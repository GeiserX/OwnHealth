//! Lane H: operations (`ops/`; architecture.md 13-15). Backups are a pg_dump plus hard links of
//! the referenced blobs and a manifest (`ownhealth.backup/v1`); restore and verify-restore compare
//! table counts and state hashes. Lane H also ports the base demo seed (`demo/base.py`) as the
//! demo hook `base` with order 0, and `/metrics`.
//!
//! Every function below is a stub until lane H lands it: it returns `Error::NotImplemented`.

use std::path::{Path, PathBuf};

use ownhealth_core::config::Settings;
use ownhealth_core::{Error, Result};
use serde_json::Value;

pub const MANIFEST_SCHEMA: &str = "ownhealth.backup/v1";

/// Write a backup under `dest`; returns the archive path.
pub async fn backup(_settings: &Settings, _dest: &Path) -> Result<PathBuf> {
    Err(Error::NotImplemented("lane H: ops::backup"))
}

/// Restore into the configured (empty) database; the same report as `verify_restore`.
pub async fn restore(_settings: &Settings, _archive: &Path) -> Result<Value> {
    Err(Error::NotImplemented("lane H: ops::restore"))
}

/// Restore into a scratch database and compare; the report Python printed.
pub async fn verify_restore(_settings: &Settings, _archive: &Path) -> Result<Value> {
    Err(Error::NotImplemented("lane H: ops::verify_restore"))
}

/// Prometheus text for `GET /metrics`.
pub async fn metrics(_rt: &ownhealth_core::Runtime) -> Result<String> {
    Err(Error::NotImplemented("lane H: ops::metrics"))
}

/// Register lane H's demo hooks: `base` (order 0, the port of `demo/base.py`) and any of its own (900).
pub fn register() {}
