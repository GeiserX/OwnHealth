"""CSRF protection for state-changing REST calls made with the session cookie.

Two layers. The cookie is SameSite=Lax, so a browser does not send it on a cross-site POST, PUT,
PATCH or DELETE. And every unsafe request that rides on a cookie (or on dev auth) must come from our
own origin: the browser's `Origin` header (or `Sec-Fetch-Site`) must match OWNHEALTH_BASE_URL or the
request's own host. Every current browser sends `Origin` on a fetch with those methods, and a page on
another site cannot forge it. Bearer tokens are not ambient credentials and skip the check.
"""

from __future__ import annotations

from urllib.parse import urlsplit

from fastapi import HTTPException, Request

from ownhealth.core.access import Principal

SAFE_METHODS = {"GET", "HEAD", "OPTIONS"}


def _origin(url: str) -> str:
    p = urlsplit(url)
    return f"{p.scheme}://{p.netloc}".lower()


def allowed_origins(request: Request, base_url: str) -> set[str]:
    host = request.headers.get("host")
    out = {_origin(base_url)}
    if host:
        out |= {f"https://{host}".lower(), f"http://{host}".lower()}
    return out


def check(request: Request, principal: Principal, base_url: str) -> None:
    """Raise 403 `csrf_failed` for a cross-site unsafe request carried by a cookie or dev auth."""
    if principal.kind == "token":
        return
    check_origin(request, base_url)


def check_origin(request: Request, base_url: str) -> None:
    """The same-origin check alone, for the public login routes (no principal yet)."""
    if request.method in SAFE_METHODS:
        return
    site = request.headers.get("sec-fetch-site")
    if site == "cross-site":
        raise HTTPException(403, detail={"code": "csrf_failed", "message": "Cross-site request refused."})
    origin = request.headers.get("origin")
    if origin is None:
        referer = request.headers.get("referer")
        origin = _origin(referer) if referer else None
    if origin is not None and origin.lower() not in allowed_origins(request, base_url):
        raise HTTPException(403, detail={"code": "csrf_failed", "message": "Cross-site request refused."})
