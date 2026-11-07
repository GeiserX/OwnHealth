"""Lane G: passkey login with py_webauthn (security-privacy.md 5). Contract stubs."""

from __future__ import annotations

from uuid import UUID

from sqlalchemy import Connection


def begin_registration(conn: Connection, *, user_id: UUID, rp_id: str, origin: str) -> dict:
    raise NotImplementedError("lane G: auth.passkeys.begin_registration")


def finish_registration(conn: Connection, *, user_id: UUID, credential: dict, rp_id: str, origin: str) -> UUID:
    raise NotImplementedError("lane G: auth.passkeys.finish_registration")


def begin_login(conn: Connection, *, rp_id: str) -> dict:
    raise NotImplementedError("lane G: auth.passkeys.begin_login")


def finish_login(conn: Connection, *, credential: dict, rp_id: str, origin: str) -> UUID:
    """Returns the user id; the caller creates the session with `auth.sessions.create_session`."""
    raise NotImplementedError("lane G: auth.passkeys.finish_login")
