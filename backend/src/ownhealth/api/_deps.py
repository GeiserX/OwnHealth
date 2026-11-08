"""Shared FastAPI dependencies: settings, a DB connection per request, the caller, profile access."""

from __future__ import annotations

from collections.abc import Iterator
from uuid import UUID

from fastapi import Depends, HTTPException, Request
from sqlalchemy import Connection

from ownhealth.auth import csrf
from ownhealth.auth import principal as principal_mod
from ownhealth.auth.sessions import COOKIE
from ownhealth.blobs import BlobStore
from ownhealth.config import Settings
from ownhealth.core.access import AccessDenied, Permission, Principal, require
from ownhealth.runtime import Runtime


def get_runtime(request: Request) -> Runtime:
    return request.app.state.runtime


def get_settings(request: Request) -> Settings:
    return request.app.state.runtime.settings


def get_blobs(request: Request) -> BlobStore:
    return request.app.state.runtime.blobs


def get_conn(request: Request) -> Iterator[Connection]:
    """One transaction per request: committed when the endpoint returns, rolled back on error."""
    with request.app.state.runtime.engine.begin() as conn:
        yield conn


def get_principal(request: Request, conn: Connection = Depends(get_conn)) -> Principal:
    p = principal_mod.resolve(
        conn,
        request.app.state.runtime.settings,
        authorization=request.headers.get("authorization"),
        session_secret=request.cookies.get(COOKIE),
    )
    if p is None:
        raise HTTPException(status_code=401, detail={"code": "unauthenticated", "message": "Log in first."})
    if p.kind == "token":
        # A PAT works only on /mcp (mcp/auth.py), where the review queue, the rate limit, the egress
        # budget, the genome and page caps and the identity mode apply. REST is the person's web app.
        raise AccessDenied("forbidden", "Access tokens work only on /mcp. Use the web app.")
    csrf.check(request, p, request.app.state.runtime.settings.base_url)
    return p


def profile_access(
    conn: Connection, principal: Principal, profile: str | UUID, scope: str | None = None, action: str | None = None
) -> Permission:
    """`require` for routers; AccessDenied becomes a 403 through the app's exception handler."""
    return require(conn, principal, profile, scope=scope, action=action)
