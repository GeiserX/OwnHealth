//! Gadgetbridge SQLite export (lane D): the Android app's database file, fully offline. Opened
//! read-only with rusqlite (bundled SQLite) in `spawn_blocking`; device tables chosen by device type
//! (Mi Band / Amazfit / Huami activity samples, heart rate, sleep, SpO2 where present). Job kind
//! `import.gadgetbridge`, payload `{profile_id, household_id, blob_sha256, filename}` (as the other
//! structured importers, `core::documents::job_kind`).

use ownhealth_core::error::Error;
use ownhealth_core::jobs::{self, Job, JobContext};

pub const JOB: &str = "import.gadgetbridge";

async fn run(_ctx: JobContext, _job: Job) -> jobs::HandlerResult {
    Err(Error::NotImplemented("import.gadgetbridge").into())
}

pub fn register() {
    jobs::register_handler(JOB, |ctx, job| Box::pin(run(ctx, job)));
}
