"""Who is calling: a PAT (Bearer), a browser session (cookie), or the dev user (OWNHEALTH_DEV_AUTH=1)."""

from __future__ import annotations

from sqlalchemy import Connection, text

from ownhealth.auth import sessions, tokens
from ownhealth.config import Settings
from ownhealth.core.access import ALL_SCOPES, Principal


def dev_principal(conn: Connection) -> Principal | None:
    """The first user holding an owner grant. Only for OWNHEALTH_DEV_AUTH=1 (demo, tests, dev)."""
    row = conn.execute(
        text(
            "select u.id, u.household_id, u.locale from app_user u join profile_grant g on g.user_id = u.id"
            " where g.role = 'owner' and u.disabled_at is null order by u.created_at, u.id limit 1"
        )
    ).first()
    if row is None:
        return None
    return Principal(kind="dev", user_id=row.id, household_id=row.household_id, scopes=ALL_SCOPES, lang=row.locale)


def resolve(
    conn: Connection, settings: Settings, *, authorization: str | None, session_secret: str | None
) -> Principal | None:
    if authorization:
        scheme, _, value = authorization.partition(" ")
        if scheme.lower() == "bearer" and value:
            return tokens.verify_token(conn, value.strip())
        return None
    if session_secret:
        principal = sessions.resolve_session(conn, session_secret)
        if principal is not None:
            return principal
    if settings.dev_auth:
        return dev_principal(conn)
    return None
