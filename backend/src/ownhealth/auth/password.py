"""Password fallback, only where passkeys cannot run.

WebAuthn needs a secure context: https, or http on localhost / 127.0.0.1 / *.localhost. An install
reached over plain HTTP at a LAN address (http://192.168.1.20:8080) cannot create or use a passkey,
so there, and only there, the owner may set a password. Put the app behind HTTPS (the compose file's
reverse proxy) and passkeys work; the password routes then answer 403 `passkeys_available`.

Hash: scrypt (n=2^14, r=8, p=1, 16 MiB), stored as `scrypt$n$r$p$salt$hash` in base64.
"""

from __future__ import annotations

import base64
import hashlib
import hmac
import secrets
from urllib.parse import urlsplit
from uuid import UUID

from sqlalchemy import Connection, text

MIN_LENGTH = 12
_N, _R, _P = 2**14, 8, 1
# WebAuthn refuses an IP address as an RP ID, so a loopback IP gets the password fallback instead.
_LOCAL_HOSTS = {"localhost"}


def passkeys_possible(base_url: str) -> bool:
    """True when a browser at `base_url` is a secure context, so WebAuthn can run."""
    parts = urlsplit(base_url)
    host = (parts.hostname or "").lower()
    return parts.scheme == "https" or host in _LOCAL_HOSTS or host.endswith(".localhost")


def hash_password(password: str) -> str:
    salt = secrets.token_bytes(16)
    digest = hashlib.scrypt(password.encode(), salt=salt, n=_N, r=_R, p=_P, dklen=32)
    b = lambda v: base64.b64encode(v).decode()  # noqa: E731
    return f"scrypt${_N}${_R}${_P}${b(salt)}${b(digest)}"


def check_password(password: str, encoded: str | None) -> bool:
    if not encoded:
        return False
    try:
        algo, n, r, p, salt, digest = encoded.split("$")
        if algo != "scrypt":
            return False
        want = base64.b64decode(digest)
        got = hashlib.scrypt(
            password.encode(), salt=base64.b64decode(salt), n=int(n), r=int(r), p=int(p), dklen=len(want)
        )
    except (ValueError, TypeError):
        return False
    return hmac.compare_digest(got, want)


def validate(password: str) -> None:
    if len(password) < MIN_LENGTH:
        raise ValueError(f"Use at least {MIN_LENGTH} characters.")


def set_password(conn: Connection, user_id: UUID, password: str) -> None:
    validate(password)
    conn.execute(
        text("update app_user set password_hash = :h where id = :u"), {"h": hash_password(password), "u": user_id}
    )


def verify_user_password(conn: Connection, user_id: UUID, password: str) -> bool:
    encoded = conn.execute(
        text("select password_hash from app_user where id = :u and disabled_at is null"), {"u": user_id}
    ).scalar()
    return check_password(password, encoded)
