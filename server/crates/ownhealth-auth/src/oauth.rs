//! Lane E (v0.2): the authorization server's domain logic (mcp.md 2.2, security-privacy.md 5, 16).
//! The HTTP handlers are `ownhealth_mcp::oauth`; tables are `oauth_client`, `oauth_consent`,
//! `oauth_code`, `oauth_refresh_token` and `api_token` rows of kind `oauth` (1001_oauth.sql).
//!
//! - Clients are Client ID Metadata Documents: the `client_id` is an https URL; [`fetch_client`] gets
//!   it with `core::net::get(url, &Egress::cimd())` (size and time limits, private addresses
//!   refused), checks that its `client_id` equals the URL and that `redirect_uris` holds the one asked
//!   for, and caches it. No DCR.
//! - PKCE S256 only ([`pkce_s256`]); `resource` must be our canonical `/mcp` URI ([`canonical_resource`]);
//!   the code is single use, ten minutes.
//! - Approving a client needs step-up; the consent names each profile and scope (es/en) with
//!   checkboxes, only `read:summary` pre-checked; tokens expire after 90 days by default (365 at
//!   most) and are revocable from the UI. PATs keep working unchanged.
//! - [`verify_access_token`] is what the `/mcp` guard calls for a non-PAT bearer: a token whose audience
//!   is not our canonical URI is refused (RFC 8707).

use ownhealth_core::access::Principal;
use ownhealth_core::{Error, Result};
use serde::{Deserialize, Serialize};
use sqlx::PgConnection;
use uuid::Uuid;

pub const ACCESS_TOKEN_PREFIX: &str = "oh_oat_";
pub const REFRESH_TOKEN_PREFIX: &str = "oh_ort_";
pub const CODE_SECONDS: i64 = 600;
pub const DEFAULT_TOKEN_DAYS: i64 = 90;
pub const MAX_TOKEN_DAYS: i64 = 365;

/// `https://<host>/mcp` for a base URL.
pub fn canonical_resource(base_url: &str) -> String {
    format!("{}/mcp", base_url.trim_end_matches('/'))
}

/// base64url(sha256(verifier)) without padding (RFC 7636 4.2).
pub fn pkce_s256(verifier: &str) -> String {
    use base64::Engine as _;
    use sha2::{Digest, Sha256};
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

/// A client as its metadata document describes it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClientMetadata {
    pub client_id: String,
    #[serde(default)]
    pub client_name: Option<String>,
    #[serde(default)]
    pub client_uri: Option<String>,
    #[serde(default)]
    pub logo_uri: Option<String>,
    pub redirect_uris: Vec<String>,
}

/// Fetch (or read from the cache) and check a client's metadata document.
pub async fn fetch_client(_conn: &mut PgConnection, _client_id: &str) -> Result<ClientMetadata> {
    Err(Error::NotImplemented("oauth::fetch_client"))
}

/// What the person approves on the consent screen.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ConsentIn {
    pub client_id: String,
    pub scopes: Vec<String>,
    pub profile_ids: Vec<Uuid>,
    pub default_profile: Option<Uuid>,
    pub identity_mode: String,
    pub days: Option<i64>,
}

/// Record an approved consent (the caller checked step-up) and return its id.
pub async fn approve(_conn: &mut PgConnection, _user_id: Uuid, _consent: &ConsentIn) -> Result<Uuid> {
    Err(Error::NotImplemented("oauth::approve"))
}

/// Revoke a consent and every token issued under it.
pub async fn revoke_consent(_conn: &mut PgConnection, _user_id: Uuid, _consent_id: Uuid) -> Result<bool> {
    Err(Error::NotImplemented("oauth::revoke_consent"))
}

/// The Principal of an OAuth access token whose audience is `resource`; None for an unknown,
/// expired, revoked or foreign-audience token.
pub async fn verify_access_token(_conn: &mut PgConnection, _token: &str, _resource: &str) -> Result<Option<Principal>> {
    Err(Error::NotImplemented("oauth::verify_access_token"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_matches_the_rfc_7636_example_and_the_resource_is_the_mcp_uri() {
        assert_eq!(
            pkce_s256("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
        assert_eq!(
            canonical_resource("https://health.example/"),
            "https://health.example/mcp"
        );
    }
}
