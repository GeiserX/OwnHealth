"""Single-use, short-lived secrets in `auth_challenge`: WebAuthn challenges, OIDC state, invites."""

from __future__ import annotations

import json
from dataclasses import dataclass
from datetime import UTC, datetime, timedelta
from uuid import UUID

from sqlalchemy import Connection, text

TTL = timedelta(minutes=5)


class ChallengeError(ValueError):
    """Unknown, expired, already used, or for another purpose. `code` is stable."""

    def __init__(self, code: str, message: str):
        super().__init__(message)
        self.code = code


@dataclass(frozen=True)
class Challenge:
    id: UUID
    purpose: str
    challenge: bytes
    user_id: UUID | None
    session_id: UUID | None
    data: dict


def issue(
    conn: Connection,
    purpose: str,
    challenge: bytes,
    *,
    user_id: UUID | None = None,
    session_id: UUID | None = None,
    data: dict | None = None,
    ttl: timedelta = TTL,
) -> UUID:
    conn.execute(text("delete from auth_challenge where expires_at < now() - interval '1 day'"))
    return conn.execute(
        text(
            "insert into auth_challenge (purpose, challenge, user_id, session_id, data, expires_at)"
            " values (:p, :c, :u, :s, cast(:d as jsonb), :e) returning id"
        ),
        {
            "p": purpose,
            "c": challenge,
            "u": user_id,
            "s": session_id,
            "d": json.dumps(data or {}, default=str),
            "e": datetime.now(UTC) + ttl,
        },
    ).scalar_one()


def consume(conn: Connection, challenge: bytes, purposes: tuple[str, ...]) -> Challenge:
    """Mark the challenge used and return it. Raises ChallengeError when it cannot be used."""
    row = conn.execute(
        text(
            "update auth_challenge set used_at = now() where challenge = :c and used_at is null"
            " and expires_at > now() returning id, purpose, challenge, user_id, session_id, data"
        ),
        {"c": challenge},
    ).first()
    if row is None:
        raise ChallengeError("challenge_invalid", "This request expired or was already used. Start again.")
    if row.purpose not in purposes:
        raise ChallengeError("challenge_invalid", "This request belongs to another step. Start again.")
    return Challenge(row.id, row.purpose, bytes(row.challenge), row.user_id, row.session_id, row.data or {})


def peek(conn: Connection, challenge: bytes, purposes: tuple[str, ...]) -> Challenge | None:
    """The live challenge without consuming it (used to show an invite before it is redeemed)."""
    row = conn.execute(
        text(
            "select id, purpose, challenge, user_id, session_id, data from auth_challenge where challenge = :c"
            " and used_at is null and expires_at > now() and purpose = any(:p)"
        ),
        {"c": challenge, "p": list(purposes)},
    ).first()
    if row is None:
        return None
    return Challenge(row.id, row.purpose, bytes(row.challenge), row.user_id, row.session_id, row.data or {})
