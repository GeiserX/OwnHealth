"""Browser sessions and step-up (security-privacy.md 5).

The cookie `oh_session` carries a random secret; `user_session.secret_sha256` stores its hash.
Idle timeout 30 minutes, absolute 12 hours, HttpOnly + SameSite=Lax, Secure when the base URL is https.
Step-up: a fresh passkey assertion (or OIDC re-login, or the password where passkeys cannot run)
within the last 5 minutes, recorded in `user_session.step_up_at`.
"""

from __future__ import annotations

import hashlib
import secrets
from datetime import UTC, datetime, timedelta
from uuid import UUID

from sqlalchemy import Connection, text

from ownhealth.core.access import ALL_SCOPES, AccessDenied, Principal

COOKIE = "oh_session"
IDLE = timedelta(minutes=30)
ABSOLUTE = timedelta(hours=12)
STEP_UP_SECONDS = 300
METHODS = ("passkey", "recovery", "oidc", "password")


def _hash(secret: str) -> bytes:
    return hashlib.sha256(secret.encode()).digest()


def resolve_session(conn: Connection, secret: str) -> Principal | None:
    """The Principal (kind='session', every scope) for a live session secret, else None.

    Rejects expired, idle and revoked sessions and disabled users; refreshes last_seen_at.
    """
    if not secret:
        return None
    row = conn.execute(
        text(
            "update user_session s set last_seen_at = now() from app_user u"
            " where s.secret_sha256 = :h and u.id = s.user_id and s.revoked_at is null"
            " and s.expires_at > now() and s.last_seen_at > now() - :idle and u.disabled_at is null"
            " returning s.id, s.user_id, s.method, u.household_id, u.locale"
        ),
        {"h": _hash(secret), "idle": IDLE},
    ).first()
    if row is None:
        return None
    return Principal(
        kind="session",
        user_id=row.user_id,
        household_id=row.household_id,
        scopes=ALL_SCOPES,
        lang=row.locale,
        extra={"session_id": row.id, "method": row.method},
    )


def create_session(
    conn: Connection,
    user_id: UUID,
    *,
    ip: str | None,
    user_agent: str | None,
    method: str = "passkey",
    step_up: bool | None = None,
) -> str:
    """Start a session after a login. Returns the cookie secret.

    A passkey, OIDC or password login is itself a fresh authentication, so it counts as step-up;
    a recovery-code login does not (the person should enrol a new passkey first).
    """
    if method not in METHODS:
        raise ValueError(f"unknown login method {method}")
    if step_up is None:
        step_up = method != "recovery"
    secret = secrets.token_urlsafe(32)
    conn.execute(
        text(
            "insert into user_session (user_id, secret_sha256, expires_at, step_up_at, ip, user_agent, method)"
            " values (:u, :h, :exp, :su, cast(:ip as inet), :ua, :m)"
        ),
        {
            "u": user_id,
            "h": _hash(secret),
            "exp": datetime.now(UTC) + ABSOLUTE,
            "su": datetime.now(UTC) if step_up else None,
            "ip": ip,
            "ua": (user_agent or "")[:300] or None,
            "m": method,
        },
    )
    return secret


def revoke_session(conn: Connection, secret: str) -> bool:
    res = conn.execute(
        text("update user_session set revoked_at = now() where secret_sha256 = :h and revoked_at is null"),
        {"h": _hash(secret)},
    )
    return res.rowcount > 0


def revoke_user_sessions(conn: Connection, user_id: UUID) -> int:
    res = conn.execute(
        text("update user_session set revoked_at = now() where user_id = :u and revoked_at is null"), {"u": user_id}
    )
    return res.rowcount


def mark_step_up(conn: Connection, session_id: UUID) -> None:
    conn.execute(text("update user_session set step_up_at = now() where id = :id"), {"id": session_id})


def session_id(principal: Principal) -> UUID | None:
    return principal.extra.get("session_id") if principal.kind == "session" else None


def require_step_up(conn: Connection, principal: Principal, max_age_seconds: int = STEP_UP_SECONDS) -> None:
    """Raise AccessDenied('step_up_required', ...) unless the session had a fresh authentication.

    Tokens never pass. The dev principal (OWNHEALTH_DEV_AUTH=1, demo and tests only) always passes.
    """
    if principal.kind == "dev":
        return
    sid = session_id(principal)
    if sid is None:
        raise AccessDenied("step_up_required", "This needs a browser session with a fresh login.")
    fresh = conn.execute(
        text(
            "select 1 from user_session where id = :id and revoked_at is null"
            " and step_up_at > now() - make_interval(secs => :age)"
        ),
        {"id": sid, "age": max_age_seconds},
    ).first()
    if fresh is None:
        raise AccessDenied("step_up_required", "Confirm it is you: use your passkey again.")
