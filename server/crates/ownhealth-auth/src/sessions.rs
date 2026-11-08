//! Browser sessions (`auth/sessions.py`). Stubs until lane G lands them.

use ownhealth_core::access::Principal;
use ownhealth_core::{Error, Result};
use sqlx::PgConnection;
use uuid::Uuid;

pub const COOKIE: &str = "oh_session";
pub const IDLE_MINUTES: i64 = 30;
pub const ABSOLUTE_HOURS: i64 = 12;
pub const STEP_UP_SECONDS: i64 = 300;
pub const METHODS: [&str; 4] = ["passkey", "recovery", "oidc", "password"];

/// The Principal (kind session, every scope, `session_id` set) for a live session secret, else None.
pub async fn resolve_session(_conn: &mut PgConnection, _secret: &str) -> Result<Option<Principal>> {
    Err(Error::NotImplemented("lane G: sessions::resolve_session"))
}

/// Returns the new session secret (the cookie value).
pub async fn create_session(
    _conn: &mut PgConnection,
    _user_id: Uuid,
    _ip: Option<&str>,
    _user_agent: Option<&str>,
    _method: &str,
) -> Result<String> {
    Err(Error::NotImplemented("lane G: sessions::create_session"))
}

/// 403 `step_up_required` (an `AccessDenied`) unless the session logged in within `max_age_seconds`.
pub async fn require_step_up(_conn: &mut PgConnection, _principal: &Principal, _max_age_seconds: i64) -> Result<()> {
    Err(Error::NotImplemented("lane G: sessions::require_step_up"))
}
