"""FastAPI dependencies for routers of every lane.

    from ownhealth.auth.deps import ProfilePerm, StepUp, SessionOnly

    @router.get("/profiles/{p}/observations")
    def observations(perm: Permission = ProfilePerm(scope="read:labs"), conn = Depends(get_conn)): ...

    @router.post("/profiles/{p}/records/{kind}")
    def add(perm: Permission = ProfilePerm(action="write"), ...): ...

`ProfilePerm` resolves the caller (`api._deps.get_principal`: session cookie, PAT or dev auth, with
the CSRF check) and the `{p}` path parameter (slug, `me` or uuid) through `core.access.require`, so
a missing grant answers 404 `profile_not_granted` and a missing scope or role action 403. It is the
same check as calling `profile_access(conn, principal, p, scope=..., action=...)` in the body.
`StepUp` adds a fresh login (5 minutes) on top: tokens, grants, export, deletion.
`SessionOnly` refuses PATs: management routes are for a person in the browser.
"""

from __future__ import annotations

from fastapi import Depends
from sqlalchemy import Connection

from ownhealth.api._deps import get_conn, get_principal
from ownhealth.auth.household import require_manage
from ownhealth.auth.sessions import STEP_UP_SECONDS, require_step_up
from ownhealth.core.access import AccessDenied, Permission, Principal, require


def session_only(principal: Principal = Depends(get_principal)) -> Principal:
    if principal.kind == "token":
        raise AccessDenied("forbidden", "Access tokens cannot do this. Use the web app.")
    return principal


def step_up(conn: Connection = Depends(get_conn), principal: Principal = Depends(session_only)) -> Principal:
    require_step_up(conn, principal, STEP_UP_SECONDS)
    return principal


SessionOnly = Depends(session_only)
StepUp = Depends(step_up)


def ProfilePerm(scope: str | None = None, action: str | None = None, manage: bool = False):  # noqa: N802
    """Depends() resolving `{p}` to the caller's Permission, checking scope, action, or management."""

    def dep(p: str, conn: Connection = Depends(get_conn), principal: Principal = Depends(get_principal)) -> Permission:
        perm = require(conn, principal, p, scope=scope, action=action)
        return require_manage(perm) if manage else perm

    return Depends(dep)


def client_ip(request) -> str | None:
    """The peer address when it is an IP (proxies: run uvicorn with --proxy-headers), else None."""
    import ipaddress

    host = request.client.host if request.client else None
    try:
        return str(ipaddress.ip_address(host)) if host else None
    except ValueError:
        return None
