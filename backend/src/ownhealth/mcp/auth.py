"""The HTTP door of /mcp: Origin check and personal access token (mcp.md 1, 2.1).

Wraps the SDK's streamable HTTP endpoint. A request whose `Origin` is not the configured public URL
is 403. A missing or bad token is 401 with `WWW-Authenticate: Bearer resource_metadata=...`. A valid
PAT becomes a Principal stored in the ASGI scope, where the MCP handlers read it. Browser sessions
and the dev user never reach /mcp: an agent always holds its own token.
"""

from __future__ import annotations

import json
from dataclasses import dataclass
from urllib.parse import urlsplit

import anyio
from starlette.requests import Request
from starlette.responses import JSONResponse
from starlette.types import Receive, Scope, Send

from ownhealth import runtime
from ownhealth.auth import tokens
from ownhealth.core.access import Principal

STATE_KEY = "oh_principal"


def origin_of(url: str) -> str:
    parts = urlsplit(url)
    return f"{parts.scheme}://{parts.netloc}".lower()


def resource_metadata_url(base_url: str) -> str:
    return base_url.rstrip("/") + "/.well-known/oauth-protected-resource"


async def protected_resource_metadata(request: Request) -> JSONResponse:
    """RFC 9728 metadata the 401 points at. PATs only in v0.1, so no authorization server is named."""
    from ownhealth.core.access import ALL_SCOPES

    base = runtime.get().settings.base_url.rstrip("/")
    return JSONResponse(
        {
            "resource": base + "/mcp",
            "resource_name": "OwnHealth",
            "bearer_methods_supported": ["header"],
            "scopes_supported": sorted(ALL_SCOPES),
        }
    )


async def _send_json(send: Send, status: int, body: dict, headers: dict[str, str] | None = None) -> None:
    raw = json.dumps(body).encode()
    hdrs = [(b"content-type", b"application/json"), (b"content-length", str(len(raw)).encode())]
    hdrs += [(k.lower().encode(), v.encode()) for k, v in (headers or {}).items()]
    await send({"type": "http.response.start", "status": status, "headers": hdrs})
    await send({"type": "http.response.body", "body": raw})


def _verify(plaintext: str) -> Principal | None:
    rt = runtime.get()
    with rt.engine.begin() as conn:
        return tokens.verify_token(conn, plaintext)


@dataclass
class PatGuard:
    """ASGI wrapper around the SDK endpoint."""

    inner: object

    async def __call__(self, scope: Scope, receive: Receive, send: Send) -> None:
        if scope["type"] != "http":
            await self.inner(scope, receive, send)  # type: ignore[operator]
            return
        settings = runtime.get().settings
        headers = {k.decode("latin-1").lower(): v.decode("latin-1") for k, v in scope.get("headers", [])}
        origin = headers.get("origin")
        if origin is not None and origin.rstrip("/").lower() != origin_of(settings.base_url):
            await _send_json(send, 403, {"error": "origin_not_allowed", "message": "Origin not allowed."})
            return
        challenge = {"WWW-Authenticate": f'Bearer resource_metadata="{resource_metadata_url(settings.base_url)}"'}
        scheme, _, value = headers.get("authorization", "").partition(" ")
        if scheme.lower() != "bearer" or not value.strip():
            await _send_json(send, 401, {"error": "invalid_token", "message": "A bearer token is required."}, challenge)
            return
        principal = await anyio.to_thread.run_sync(_verify, value.strip())
        if principal is None:
            challenge["WWW-Authenticate"] += ', error="invalid_token"'
            await _send_json(
                send, 401, {"error": "invalid_token", "message": "Unknown, expired or revoked token."}, challenge
            )
            return
        state = scope.setdefault("state", {})
        state[STATE_KEY] = principal
        await self.inner(scope, receive, send)  # type: ignore[operator]


def principal_from(ctx) -> Principal | None:
    request = getattr(ctx, "request", None)
    if request is None:
        return None
    return request.scope.get("state", {}).get(STATE_KEY)


def client_of(ctx) -> tuple[str | None, str | None]:
    """(ip, user agent) of the HTTP request behind a context."""
    request = getattr(ctx, "request", None)
    if request is None:
        return None, None
    ip = request.client.host if request.client else None
    if ip and not any(c.isdigit() for c in ip):
        ip = None  # 'testclient' and similar are not addresses
    return ip, request.headers.get("user-agent")
