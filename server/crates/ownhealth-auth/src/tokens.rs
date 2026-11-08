//! Personal access tokens for MCP clients (`auth/tokens.py`). Format `oh_pat_<id8>_<secret43>`;
//! the row stores sha256(secret). Stubs until lane G lands them.

use ownhealth_core::access::Principal;
use ownhealth_core::{Error, Result};
use sqlx::PgConnection;
use uuid::Uuid;

pub const PREFIX: &str = "oh_pat_";
pub const DEFAULT_DAYS: i64 = 90;
pub const MAX_DAYS: i64 = 365;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewToken {
    pub id: Uuid,
    /// Shown once.
    pub plaintext: String,
    pub prefix: String,
}

#[derive(Debug, Clone)]
pub struct TokenIn<'a> {
    pub user_id: Uuid,
    pub label: &'a str,
    pub profile_ids: Vec<Uuid>,
    pub scopes: Vec<String>,
    pub default_profile: Option<Uuid>,
    pub days: i64,
    /// full | pseudonymous
    pub identity_mode: &'a str,
    pub lang: &'a str,
    pub units: &'a str,
}

pub async fn create_token(_conn: &mut PgConnection, _t: &TokenIn<'_>) -> Result<NewToken> {
    Err(Error::NotImplemented("lane G: tokens::create_token"))
}

/// The token's Principal (kind token), or None for an unknown, expired or revoked token.
pub async fn verify_token(_conn: &mut PgConnection, _plaintext: &str) -> Result<Option<Principal>> {
    Err(Error::NotImplemented("lane G: tokens::verify_token"))
}

pub async fn revoke_token(_conn: &mut PgConnection, _user_id: Uuid, _token_id: Uuid) -> Result<bool> {
    Err(Error::NotImplemented("lane G: tokens::revoke_token"))
}
