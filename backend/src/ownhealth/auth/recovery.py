"""Ten single-use recovery codes, printed once at enrolment (security-privacy.md 5).

A code is 60 random bits as 12 Crockford base32 characters, shown as `XXXX-XXXX-XXXX`. We store
sha256 of the normalised code: at that entropy a slow hash adds nothing, and the unique index lets
the code alone identify its user. A recovery login does not count as step-up.
"""

from __future__ import annotations

import hashlib
import secrets
from uuid import UUID

from sqlalchemy import Connection, text

COUNT = 10
_ALPHABET = "0123456789ABCDEFGHJKMNPQRSTVWXYZ"  # Crockford: no I, L, O, U


def _normalise(code: str) -> str:
    c = "".join(ch for ch in code.upper() if ch.isalnum())
    return c.replace("O", "0").replace("I", "1").replace("L", "1")


def _hash(code: str) -> bytes:
    return hashlib.sha256(_normalise(code).encode()).digest()


def new_codes(conn: Connection, user_id: UUID) -> list[str]:
    """Replace the user's codes with ten new ones. Returns the plaintext, to show once."""
    conn.execute(text("delete from recovery_code where user_id = :u"), {"u": user_id})
    codes = []
    for _ in range(COUNT):
        raw = "".join(secrets.choice(_ALPHABET) for _ in range(12))
        codes.append(f"{raw[:4]}-{raw[4:8]}-{raw[8:]}")
    conn.execute(
        text("insert into recovery_code (user_id, code_sha256) values (:u, :h)"),
        [{"u": user_id, "h": _hash(c)} for c in codes],
    )
    return codes


def use_code(conn: Connection, code: str) -> UUID | None:
    """Consume one code. Returns its user, or None when unknown, used, or the user is disabled."""
    if len(_normalise(code)) != 12:
        return None
    row = conn.execute(
        text(
            "update recovery_code r set used_at = now() from app_user u where r.code_sha256 = :h"
            " and r.used_at is null and u.id = r.user_id and u.disabled_at is null returning r.user_id"
        ),
        {"h": _hash(code)},
    ).first()
    return row.user_id if row else None


def remaining(conn: Connection, user_id: UUID) -> int:
    return conn.execute(
        text("select count(*) from recovery_code where user_id = :u and used_at is null"), {"u": user_id}
    ).scalar_one()
