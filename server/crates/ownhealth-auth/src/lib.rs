//! Lane G: security and auth (`auth/`, `export/`; security-privacy.md, mcp.md 2).
//!
//! The foundation already implements what every request needs: `principal::resolve` (Bearer PAT,
//! session cookie, dev auth, in that order), `principal::dev_principal` and the `csrf` check. The
//! rest are typed stubs returning `Error::NotImplemented` until lane G lands them; the hashing
//! formats they must keep are in RUST-CONTRACTS.md (PATs, scrypt passwords, recovery codes,
//! session secrets, passkey rows).

pub mod csrf;
pub mod principal;
pub mod sessions;
pub mod tokens;

pub mod export {
    use ownhealth_core::blobs::BlobStore;
    use ownhealth_core::{Error, Result};
    use serde_json::Value;
    use sqlx::PgConnection;
    use uuid::Uuid;

    pub const FORMAT: &str = "ownhealth.export/v1";

    /// Stream the export zip into `out` and return its manifest (no blob copy).
    pub async fn export_profile(
        _conn: &mut PgConnection,
        _blobs: &BlobStore,
        _profile_id: Uuid,
        _out: &mut (dyn std::io::Write + Send),
        _fmt: &str,
    ) -> Result<Value> {
        Err(Error::NotImplemented("lane G: export::export_profile"))
    }
}

/// Register lane G's demo hook (order 700, if any).
pub fn register() {}
