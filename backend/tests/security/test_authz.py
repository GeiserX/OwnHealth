"""REST authorization on every router: who resolves the caller, and what a viewer can and cannot do.

Family (demo/family.py): Sam owns sam, is viewer on alex, guardian on lucia, and has no grant on carmen.
"""

import pytest
from fastapi.routing import APIRoute

from ownhealth.api._deps import get_principal
from ownhealth.app import discover_routers

# Routes reachable without a login: they are the login itself.
PUBLIC = {
    "/auth/state",
    "/auth/setup/begin",
    "/auth/setup/finish",
    "/auth/setup/password",
    "/auth/login/begin",
    "/auth/login/finish",
    "/auth/recovery",
    "/auth/password/login",
    "/auth/logout",
    "/auth/invite/{code}",
    "/auth/enroll/begin",
    "/auth/enroll/finish",
    "/auth/enroll/password",
    "/auth/oidc/login",
    "/auth/oidc/callback",
}


def _calls(dependant) -> set:
    out = {dependant.call}
    for d in dependant.dependencies:
        out |= _calls(d)
    return out


def _api_routes():
    for name, router in discover_routers():
        for route in router.routes:
            if isinstance(route, APIRoute):
                yield name, route


def test_every_route_resolves_the_caller():
    missing = [
        f"{name}: {sorted(r.methods)} {r.path}"
        for name, r in _api_routes()
        if r.path not in PUBLIC and get_principal not in _calls(r.dependant)
    ]
    assert not missing, "routes without get_principal (use ownhealth.auth.deps):\n" + "\n".join(missing)


def test_public_list_is_exact():
    paths = {r.path for _, r in _api_routes()}
    assert paths >= PUBLIC, f"stale entries: {PUBLIC - paths}"


def _fill(path: str, slug: str) -> str:
    return (
        path.replace("{p}", slug)
        .replace("{kind}", "condition")
        .replace("{metric}", "steps")
        .replace("{id}", "00000000-0000-0000-0000-000000000000")
    )


@pytest.mark.parametrize("anonymous", [True, False])
def test_profile_routes_refuse_ungranted_profiles(client_for, committed_family, anonymous):
    """Every GET under /profiles/{p}: nobody logged in gets 401; Sam asking for Carmen gets 404."""
    c = client_for(None if anonymous else committed_family.users["sam"])
    leaks = []
    for _, r in _api_routes():
        if "{p}" not in r.path or "GET" not in r.methods:
            continue
        # required query parameters get a dummy value, so validation (422) cannot hide a missing check
        query = {q.alias: "x" for q in r.dependant.query_params if q.field_info.is_required()}
        resp = c.get("/api" + _fill(r.path, "carmen"), params=query)
        want = {401} if anonymous else {403, 404}
        if resp.status_code not in want:
            leaks.append(f"GET {r.path}: {resp.status_code}")
    assert not leaks, leaks


def test_viewer_cannot_write_or_manage(client_for, committed_family):
    """Sam is viewer on Alex: reading is fine, every write and management route is refused."""
    sam = client_for(committed_family.users["sam"])
    assert sam.get("/api/profiles").status_code == 200
    alex_id = str(committed_family.users["alex"])
    attempts = [
        ("PUT", "/api/profiles/alex/locks", {"locks": []}),
        ("PUT", "/api/profiles/alex/sensitive", {"agents": True}),
        ("PUT", "/api/grants", {"user_id": alex_id, "profile": "alex", "role": "viewer"}),
        ("POST", "/api/profiles/alex/export", {"fmt": "zip"}),
        ("DELETE", "/api/profiles/alex?confirm=alex", None),
        ("DELETE", f"/api/grants?user_id={alex_id}&profile=alex", None),
    ]
    for method, url, body in attempts:
        r = sam.request(method, url, json=body)
        assert r.status_code == 403, f"{method} {url}: {r.status_code} {r.text}"
    files = [("files", ("x.pdf", b"%PDF-1.4 synthetic", "application/pdf"))]
    assert sam.post("/api/imports", data={"profile": "alex"}, files=files).status_code == 403
    # no grant at all: 404, same as a profile that does not exist
    assert sam.put("/api/profiles/carmen/locks", json={"locks": []}).status_code == 404
    assert sam.post("/api/profiles/carmen/export", json={"fmt": "zip"}).status_code == 404


def test_profile_perm_dependency(client_for, committed_family):
    """The dependency other lanes use: grant -> 404, scope or role action -> 403, manage -> 403."""
    from fastapi import APIRouter

    from ownhealth.auth.deps import ProfilePerm
    from ownhealth.core.access import Permission

    r = APIRouter()

    @r.get("/t/{p}/labs")
    def labs(perm: Permission = ProfilePerm(scope="read:labs")):
        return {"slug": perm.profile.slug}

    @r.post("/t/{p}/write")
    def write(perm: Permission = ProfilePerm(action="write")):
        return {"ok": True}

    @r.post("/t/{p}/manage")
    def manage(perm: Permission = ProfilePerm(manage=True)):
        return {"ok": True}

    from fastapi import FastAPI
    from fastapi.responses import JSONResponse
    from fastapi.testclient import TestClient

    from ownhealth.core.access import AccessDenied

    logged_in = client_for(committed_family.users["sam"])  # builds the runtime and the session cookie
    app = FastAPI()
    app.state.runtime = logged_in.app.state.runtime
    app.include_router(r, prefix="/api")

    @app.exception_handler(AccessDenied)
    async def _denied(request, exc):  # the same mapping as ownhealth.app
        status = 404 if exc.code == "profile_not_granted" else 403
        return JSONResponse(status_code=status, content={"detail": {"code": exc.code, "message": str(exc)}})

    sam = TestClient(app, base_url="https://testserver", cookies=dict(logged_in.cookies))
    assert sam.get("/api/t/sam/labs").json() == {"slug": "sam"}
    assert sam.get("/api/t/carmen/labs").status_code == 404
    assert sam.get("/api/t/alex/labs").json()["detail"]["code"] == "insufficient_scope"  # viewer: summary only
    assert sam.post("/api/t/alex/write").status_code == 403
    assert sam.post("/api/t/lucia/write").status_code == 200  # guardian
    assert sam.post("/api/t/lucia/manage").status_code == 200  # guardian of an unclaimed profile
    assert sam.post("/api/t/alex/manage").status_code == 403
    assert TestClient(app, base_url="https://testserver").get("/api/t/sam/labs").status_code == 401
