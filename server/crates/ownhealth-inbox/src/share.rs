//! Lane B: phone pushes (ingestion.md 3). The PWA share target (Android) posts multipart to
//! `POST /api/ingest/share` from a logged-in session; the profile is chosen on the share screen. The
//! iOS Shortcut posts the shared file to `POST /api/ingest/shortcut` with a push token
//! (`oh_ing_...`, `core::sources::issue_token`) of a `shortcut` source bound to one profile.
//! Both store the files and call `core::arrive::arrive` once per file.

use ownhealth_core::arrive::Arrived;
use ownhealth_core::blobs::BlobStore;
use ownhealth_core::documents::StoredFile;
use ownhealth_core::error::{Error, Result};
use sqlx::PgConnection;
use uuid::Uuid;

/// Files already in the blob store, from a share or a Shortcut, for one profile.
pub async fn ingest_files(
    _conn: &mut PgConnection,
    _blobs: &BlobStore,
    _household_id: Uuid,
    _profile_id: Uuid,
    _source_id: Option<Uuid>,
    _channel: &str,
    _files: &[StoredFile],
) -> Result<Vec<Arrived>> {
    Err(Error::NotImplemented("share::ingest_files"))
}
