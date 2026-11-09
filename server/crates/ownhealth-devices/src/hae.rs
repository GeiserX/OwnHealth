//! Health Auto Export receiver (lane D): the app's REST automation posts JSON v2
//! (`{"data": {"metrics": [{name, units, data: [{date, qty, ...}]}], "workouts": [...]}}`) on a
//! schedule. Each push is stored as a blob, mapped to wearable metrics, and inserted idempotently
//! (`wearable_sample` unique key), so 7 days of 15-minute pushes, overlapping, import with 0 duplicates.

use ownhealth_core::error::{Error, Result};
use ownhealth_core::sources::{RunOutcome, Source};
use sqlx::PgConnection;

/// The largest push body we accept.
pub const MAX_BODY_BYTES: usize = 50 * 1024 * 1024;

/// One push from the source's device: store, map, insert. Returns what the run did (the route records it
/// with `core::sources::{begin_run, finish_run}`).
pub async fn import_push(
    _conn: &mut PgConnection,
    _blobs: &ownhealth_core::blobs::BlobStore,
    _source: &Source,
    _body: &[u8],
) -> Result<RunOutcome> {
    Err(Error::NotImplemented("hae::import_push"))
}
