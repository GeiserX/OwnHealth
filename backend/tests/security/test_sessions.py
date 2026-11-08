"""Session lifetime, step-up, cookie flags, CSRF."""

import dataclasses

import pytest
from sqlalchemy import text

from ownhealth.auth.sessions import COOKIE, create_session, require_step_up, resolve_session, revoke_session
from ownhealth.core.access import AccessDenied, Principal


def test_session_lifetime(conn, family):
    alex = family.users["alex"]
    secret = create_session(conn, alex, ip="192.0.2.1", user_agent="pytest")
    p = resolve_session(conn, secret)
    assert p.kind == "session" and p.user_id == alex and p.household_id == family.household_id
    stored = conn.execute(text("select secret_sha256 from user_session where user_id = :u"), {"u": alex}).scalar()
    assert secret.encode() not in bytes(stored)  # only the hash is kept
    assert resolve_session(conn, secret + "x") is None
    # idle for 31 minutes
    conn.execute(
        text("update user_session set last_seen_at = now() - interval '31 minutes' where user_id = :u"), {"u": alex}
    )
    assert resolve_session(conn, secret) is None
    # absolute limit
    s2 = create_session(conn, alex, ip=None, user_agent=None)
    conn.execute(
        text("update user_session set expires_at = now() - interval '1 second' where secret_sha256 = sha256(:s)"),
        {"s": s2.encode()},
    )
    assert resolve_session(conn, s2) is None
    # revoked
    s3 = create_session(conn, alex, ip=None, user_agent=None)
    assert revoke_session(conn, s3) and resolve_session(conn, s3) is None
    # disabled user
    s4 = create_session(conn, alex, ip=None, user_agent=None)
    conn.execute(text("update app_user set disabled_at = now() where id = :u"), {"u": alex})
    assert resolve_session(conn, s4) is None


def test_step_up_window(conn, family):
    alex = family.users["alex"]
    fresh = resolve_session(conn, create_session(conn, alex, ip=None, user_agent=None))
    require_step_up(conn, fresh)
    conn.execute(
        text("update user_session set step_up_at = now() - interval '301 seconds' where id = :i"),
        {"i": fresh.extra["session_id"]},
    )
    with pytest.raises(AccessDenied) as e:
        require_step_up(conn, fresh)
    assert e.value.code == "step_up_required"
    recovered = resolve_session(conn, create_session(conn, alex, ip=None, user_agent=None, method="recovery"))
    with pytest.raises(AccessDenied):
        require_step_up(conn, recovered)
    token = Principal(kind="token", user_id=alex, household_id=family.household_id)
    with pytest.raises(AccessDenied):
        require_step_up(conn, token)


def test_cookie_flags(client_for, committed_family, https_settings, engine):
    from security.softkey import SoftKey  # noqa: F401  (same package import path as the other tests)

    c = client_for(committed_family.users["alex"])
    r = c.post("/api/auth/logout")
    cookie = r.headers["set-cookie"]
    assert COOKIE in cookie
    # a login on https sets HttpOnly, Secure and SameSite=Lax
    from fastapi import Response

    from ownhealth.api.auth import _set_cookie

    resp = Response()
    _set_cookie(resp, "s", https_settings)
    header = resp.headers["set-cookie"].lower()
    assert "httponly" in header and "secure" in header and "samesite=lax" in header and "max-age=43200" in header
    resp = Response()
    _set_cookie(resp, "s", dataclasses.replace(https_settings, base_url="http://localhost:8080"))
    assert "secure" not in resp.headers["set-cookie"].lower()


def test_csrf_refuses_cross_site_writes(client_for, committed_family):
    c = client_for(committed_family.users["alex"])
    body = {"locks": ["read:genome"]}
    evil = c.put("/api/profiles/alex/locks", json=body, headers={"Origin": "https://evil.example"})
    assert evil.status_code == 403 and evil.json()["detail"]["code"] == "csrf_failed"
    site = c.put("/api/profiles/alex/locks", json=body, headers={"Sec-Fetch-Site": "cross-site"})
    assert site.status_code == 403
    ref = c.put("/api/profiles/alex/locks", json=body, headers={"Referer": "https://evil.example/page"})
    assert ref.status_code == 403
    ok = c.put("/api/profiles/alex/locks", json=body, headers={"Origin": "https://testserver"})
    assert ok.status_code == 200, ok.text
    # reads are not state-changing; the public login routes check the origin too
    assert c.get("/api/me", headers={"Origin": "https://evil.example"}).status_code == 200
    assert c.post("/api/auth/login/begin", headers={"Origin": "https://evil.example"}).status_code == 403
