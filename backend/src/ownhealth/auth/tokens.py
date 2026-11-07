"""Personal access tokens (PATs) for MCP clients.

Format `oh_pat_<id8>_<secret43>`: id8 is the lookup key, secret43 is 256 random bits in base64url.
We store sha256(secret); a 256-bit random secret needs no slow hash. The plaintext is returned
once, at creation.
"""

from __future__ import annotations

import base64
import hashlib
import hmac
import secrets
import string
from dataclasses import dataclass
from datetime import UTC, datetime, timedelta
from uuid import UUID

from sqlalchemy import Connection, text

from ownhealth.core.access import ALL_SCOPES, PAT_DEFAULT_SCOPES, AccessDenied, Principal

PREFIX = "oh_pat_"
DEFAULT_DAYS = 90
MAX_DAYS = 365
_ID_ALPHABET = string.ascii_letters + string.digits


class TokenError(ValueError):
    pass


@dataclass(frozen=True)
class NewToken:
    id: UUID
    plaintext: str  # shown once
    prefix: str


def _hash(secret: str) -> bytes:
    return hashlib.sha256(secret.encode()).digest()


def create_token(
    conn: Connection,
    *,
    user_id: UUID,
    label: str,
    profile_ids: list[UUID],
    scopes: list[str] | tuple[str, ...] = PAT_DEFAULT_SCOPES,
    default_profile: UUID | None = None,
    days: int = DEFAULT_DAYS,
    identity_mode: str = "full",
    lang: str = "es",
    units: str = "si",
) -> NewToken:
    """Mint a PAT. Profiles must be ones the user holds a grant on; scopes must be known."""
    if not profile_ids:
        raise TokenError("a token needs at least one profile")
    unknown = set(scopes) - ALL_SCOPES
    if unknown:
        raise TokenError(f"unknown scopes {sorted(unknown)}")
    if not 1 <= days <= MAX_DAYS:
        raise TokenError(f"expiry must be 1 to {MAX_DAYS} days")
    granted = set(
        conn.execute(
            text("select profile_id from profile_grant where user_id = :u and profile_id = any(:p)"),
            {"u": user_id, "p": list(profile_ids)},
        ).scalars()
    )
    missing = set(profile_ids) - granted
    if missing:
        raise AccessDenied("profile_not_granted", "The token names a profile you have no grant on.")
    if default_profile is not None and default_profile not in profile_ids:
        raise TokenError("default_profile must be one of the token's profiles")
    key = "".join(secrets.choice(_ID_ALPHABET) for _ in range(8))
    secret = base64.urlsafe_b64encode(secrets.token_bytes(32)).rstrip(b"=").decode()
    prefix = PREFIX + key
    token_id = conn.execute(
        text(
            "insert into api_token (user_id, label, prefix, secret_sha256, scopes, profile_ids, default_profile,"
            " identity_mode, lang, units, expires_at) values (:u, :l, :px, :h, :s, :p, :dp, :im, :lang, :units,"
            " :exp) returning id"
        ),
        {
            "u": user_id,
            "l": label,
            "px": prefix,
            "h": _hash(secret),
            "s": sorted(set(scopes)),
            "p": list(profile_ids),
            "dp": default_profile or profile_ids[0],
            "im": identity_mode,
            "lang": lang,
            "units": units,
            "exp": datetime.now(UTC) + timedelta(days=days),
        },
    ).scalar_one()
    return NewToken(id=token_id, plaintext=f"{prefix}_{secret}", prefix=prefix)


def verify_token(conn: Connection, plaintext: str, *, touch: bool = True) -> Principal | None:
    """The Principal for a valid, unexpired, unrevoked PAT; None otherwise."""
    if not plaintext.startswith(PREFIX):
        return None
    body = plaintext[len(PREFIX) :]
    key, sep, secret = body.partition("_")
    if not sep or len(key) != 8 or not secret:
        return None
    row = (
        conn.execute(
            text(
                "select t.*, u.household_id, u.disabled_at from api_token t join app_user u on u.id = t.user_id"
                " where t.prefix = :px"
            ),
            {"px": PREFIX + key},
        )
        .mappings()
        .first()
    )
    if row is None or not hmac.compare_digest(bytes(row["secret_sha256"]), _hash(secret)):
        return None
    now = datetime.now(UTC)
    if row["revoked_at"] is not None or row["expires_at"] <= now or row["disabled_at"] is not None:
        return None
    if touch:
        conn.execute(text("update api_token set last_used_at = now() where id = :id"), {"id": row["id"]})
    return Principal(
        kind="token",
        user_id=row["user_id"],
        household_id=row["household_id"],
        scopes=frozenset(row["scopes"]),
        token_id=row["id"],
        token_label=row["label"],
        profile_ids=frozenset(row["profile_ids"]),
        default_profile=row["default_profile"],
        identity_mode=row["identity_mode"],
        lang=row["lang"],
        units=row["units"],
        extra={
            "limits": {
                k: row[k]
                for k in (
                    "rate_per_min",
                    "egress_facts_per_day",
                    "egress_bytes_per_day",
                    "genome_raw_lookups_per_day",
                    "document_pages_per_day",
                    "proposals_per_day",
                )
            }
        },
    )


def revoke_token(conn: Connection, token_id: UUID, user_id: UUID) -> bool:
    res = conn.execute(
        text("update api_token set revoked_at = now() where id = :id and user_id = :u and revoked_at is null"),
        {"id": token_id, "u": user_id},
    )
    return res.rowcount > 0


def list_tokens(conn: Connection, user_id: UUID) -> list[dict]:
    rows = conn.execute(
        text(
            "select id, label, prefix, scopes, profile_ids, default_profile, identity_mode, lang, units,"
            " rate_per_min, egress_facts_per_day, egress_bytes_per_day, genome_raw_lookups_per_day,"
            " document_pages_per_day, proposals_per_day, created_at, expires_at, last_used_at, revoked_at"
            " from api_token where user_id = :u order by created_at desc"
        ),
        {"u": user_id},
    ).mappings()
    return [dict(r) for r in rows]
