//! Lane B: the watched folder (ingestion.md 3, connectors.md 2.1). Polling, because inotify is
//! unreliable on SMB and NFS mounts. `<OWNHEALTH_INBOX_DIR>/<profile-slug>/<file>`: a file is taken
//! once its size has not changed for STABLE_SECONDS; after its blob commits it moves to
//! `<root>/.done/`, on failure to `<root>/.failed/` with a `.reason.txt`. The per-source dedup key is
//! the relative path, size and mtime. Source `config` (kind `folder`): `{}` (the root comes from the
//! settings) or `{"subdir": "..."}`.

use ownhealth_core::error::Error;
use ownhealth_core::jobs::{JobContext, JobError};
use ownhealth_core::sources::{RunOutcome, Source};

pub const STABLE_SECONDS: u64 = 10;
pub const DEFAULT_POLL_SECONDS: i32 = 60;

/// The `folder` poller.
pub async fn poll_folder(_ctx: JobContext, _source: Source) -> Result<RunOutcome, JobError> {
    Err(Error::NotImplemented("folder::poll_folder").into())
}

pub fn register() {
    ownhealth_core::sources::register_poller("folder", |ctx, source| Box::pin(poll_folder(ctx, source)));
}
