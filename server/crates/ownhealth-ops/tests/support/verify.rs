use std::path::Path;

use ownhealth_core::{Result, config::Settings};
use serde_json::Value;
use tokio::sync::Mutex;

// Tests in this binary can run concurrently against the same Postgres cluster.
static VERIFICATIONS: Mutex<()> = Mutex::const_new(());

pub async fn verify_restore(settings: &Settings, archive: &Path) -> Result<Value> {
    let _serial = VERIFICATIONS.lock().await;
    ownhealth_ops::verify_restore(settings, archive).await
}
