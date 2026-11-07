"""Browser sessions (lane G owns this module).

Contract: the cookie `oh_session` carries a random secret; `user_session.secret_sha256` stores its
hash. Idle timeout 30 minutes, absolute 12 hours, HttpOnly + Secure + SameSite=Lax.
"""

from __future__ import annotations

from sqlalchemy import Connection

from ownhealth.core.access import Principal

COOKIE = "oh_session"


def resolve_session(conn: Connection, secret: str) -> Principal | None:
    """The Principal (kind='session', every scope) for a live session secret, else None.

    Must reject expired, idle and revoked sessions and refresh last_seen_at.
    """
    raise NotImplementedError("lane G: sessions")


def create_session(conn: Connection, user_id, *, ip: str | None, user_agent: str | None) -> str:
    """Start a session after a passkey or OIDC login. Returns the cookie secret."""
    raise NotImplementedError("lane G: sessions")


def require_step_up(conn: Connection, principal: Principal, max_age_seconds: int = 300) -> None:
    """Raise AccessDenied('step_up_required', ...) unless the session had a fresh passkey assertion."""
    raise NotImplementedError("lane G: step-up")
