"""Generic OIDC login against a fake provider: code flow + PKCE, signed ID token, state and nonce."""

import base64
import hashlib
import time
from urllib.parse import parse_qs, urlsplit

import httpx
from joserfc import jwt
from joserfc.jwk import RSAKey
from sqlalchemy import text

from ownhealth.auth import oidc

ISSUER = "https://id.example.test"


class FakeIdP:
    def __init__(self):
        self.key = RSAKey.generate_key(2048, parameters={"kid": "k1"}, private=True)
        self.codes: dict[str, dict] = {}
        self.override: dict = {}
        self.sign_with = self.key

    def handler(self, request: httpx.Request) -> httpx.Response:
        url = str(request.url)
        if url == f"{ISSUER}/.well-known/openid-configuration":
            return httpx.Response(
                200,
                json={
                    "issuer": ISSUER,
                    "authorization_endpoint": f"{ISSUER}/authorize",
                    "token_endpoint": f"{ISSUER}/token",
                    "jwks_uri": f"{ISSUER}/jwks",
                },
            )
        if url == f"{ISSUER}/jwks":
            return httpx.Response(200, json={"keys": [self.key.as_dict(private=False)]})
        if url == f"{ISSUER}/token":
            form = parse_qs(request.content.decode())
            grant = self.codes.pop(form["code"][0], None)
            if grant is None:
                return httpx.Response(400, json={"error": "invalid_grant"})
            verifier = form["code_verifier"][0]
            digest = base64.urlsafe_b64encode(hashlib.sha256(verifier.encode()).digest()).rstrip(b"=").decode()
            if digest != grant["challenge"]:
                return httpx.Response(400, json={"error": "invalid_grant", "error_description": "pkce"})
            now = int(time.time())
            claims = {
                "iss": ISSUER,
                "aud": "ownhealth",
                "sub": grant["sub"],
                "exp": now + 300,
                "iat": now,
                "auth_time": now,
                "nonce": grant["nonce"],
                "name": grant["name"],
                **self.override,
            }
            token = jwt.encode({"alg": "RS256", "kid": "k1"}, claims, self.sign_with)
            return httpx.Response(200, json={"id_token": token, "access_token": "at", "token_type": "Bearer"})
        return httpx.Response(404)

    def authorize(self, location: str, sub: str, name: str = "Sam Rivera") -> dict:
        """What the provider does after the person logs in: returns the callback query."""
        q = {k: v[0] for k, v in parse_qs(urlsplit(location).query).items()}
        assert q["code_challenge_method"] == "S256" and q["client_id"] == "ownhealth"
        code = f"code-{len(self.codes)}-{q['state'][:6]}"
        self.codes[code] = {"nonce": q["nonce"], "challenge": q["code_challenge"], "sub": sub, "name": name}
        return {"state": q["state"], "code": code, "_q": q}


def _wire(client, idp):
    client.app.state.oidc_config = oidc.OIDCConfig(
        issuer=ISSUER, client_id="ownhealth", client_secret="s", label="Pocket ID"
    )
    client.app.state.oidc_http = httpx.Client(transport=httpx.MockTransport(idp.handler))
    oidc._discovery.clear()
    return client


def _go(client, idp, sub, mode="login", **params):
    r = client.get("/api/auth/oidc/login", params={"mode": mode, **params}, follow_redirects=False)
    assert r.status_code == 302, r.text
    cb = idp.authorize(r.headers["location"], sub)
    q = cb.pop("_q")
    return client.get("/api/auth/oidc/callback", params=cb, follow_redirects=False), q


def test_first_run_then_login_and_rejections(client_for, fresh_engine):
    idp = FakeIdP()
    c = _wire(client_for(eng=fresh_engine), idp)
    assert c.get("/api/auth/state").json()["oidc"] == {"enabled": True, "label": "Pocket ID"}
    r, q = _go(c, idp, "sub-sam")
    assert r.status_code == 302 and r.headers["location"] == "/"
    assert q["redirect_uri"] == "https://testserver/api/auth/oidc/callback"
    me = c.get("/api/me").json()
    assert me["user"]["display_name"] == "Sam Rivera" and me["profiles"][0]["role"] == "owner"
    with fresh_engine.connect() as conn:
        assert conn.execute(text("select oidc_subject from app_user")).scalar() == f"{ISSUER}#sub-sam"

    # the same person again: a new session; a stranger: not linked
    again = _wire(client_for(eng=fresh_engine), idp)
    assert _go(again, idp, "sub-sam")[0].headers["location"] == "/"
    assert again.get("/api/me").status_code == 200
    stranger = _wire(client_for(eng=fresh_engine), idp)
    assert _go(stranger, idp, "sub-mallory")[0].headers["location"] == "/login?auth_error=oidc_not_linked"
    assert stranger.get("/api/me").status_code == 401

    # a forged signature, a token for another client, a wrong nonce: all refused
    for tamper in ("key", "aud", "nonce"):
        bad = _wire(client_for(eng=fresh_engine), idp)
        idp.sign_with, idp.override = idp.key, {}
        if tamper == "key":
            idp.sign_with = RSAKey.generate_key(2048, parameters={"kid": "k1"}, private=True)
        elif tamper == "aud":
            idp.override = {"aud": "another-app"}
        else:
            idp.override = {"nonce": "replayed"}
        r, _ = _go(bad, idp, "sub-sam")
        assert r.headers["location"] == "/login?auth_error=oidc_failed", tamper
        assert bad.get("/api/me").status_code == 401, tamper
    idp.sign_with, idp.override = idp.key, {}

    # a state is single use
    r = c.get("/api/auth/oidc/login", follow_redirects=False)
    cb = idp.authorize(r.headers["location"], "sub-sam")
    cb.pop("_q")
    assert c.get("/api/auth/oidc/callback", params=cb, follow_redirects=False).headers["location"] == "/"
    replay = c.get("/api/auth/oidc/callback", params=cb, follow_redirects=False)
    assert "auth_error=challenge_invalid" in replay.headers["location"]


def test_enroll_by_invite_and_step_up(client_for, committed_family, engine):
    idp = FakeIdP()
    alex = client_for(committed_family.users["alex"])
    code = alex.post(
        "/api/household/invites", json={"display_name": "Carmen Ortega", "claim_profile": "carmen"}
    ).json()["code"]
    carmen = _wire(client_for(), idp)
    r, _ = _go(carmen, idp, f"sub-carmen-{code[:6]}", mode="enroll", invite=code)
    assert r.headers["location"] == "/", r.headers["location"]
    me = carmen.get("/api/me").json()
    assert [(p["slug"], p["role"]) for p in me["profiles"]] == [("carmen", "owner")]

    # step-up through the provider: only the same subject counts
    with engine.connect() as c:
        uid = c.execute(
            text("select user_id from profile where id = :p"), {"p": committed_family.profiles["carmen"]}
        ).scalar()
    stale = _wire(client_for(uid, step_up=False, method="oidc"), idp)
    begin = stale.post("/api/auth/step-up/begin").json()
    assert begin == {"method": "oidc", "url": "/api/auth/oidc/login?mode=step_up"}
    r, q = _go(stale, idp, "sub-someone-else", mode="step_up")
    assert q["prompt"] == "login" and q["max_age"] == "0"
    assert "oidc_wrong_user" in r.headers["location"]
    assert stale.get("/api/auth/state").json()["step_up_fresh"] is False
    r, _ = _go(stale, idp, f"sub-carmen-{code[:6]}", mode="step_up")
    assert r.headers["location"] == "/"
    assert stale.get("/api/auth/state").json()["step_up_fresh"] is True


def test_the_provider_client_is_shared_and_closed_with_the_app(https_settings, engine):
    from fastapi.testclient import TestClient
    from starlette.requests import Request

    from ownhealth.api.auth import _oidc_http
    from ownhealth.app import create_app

    app = create_app(https_settings, engine)
    with TestClient(app):
        req = Request({"type": "http", "app": app})
        first = _oidc_http(req)
        assert _oidc_http(req) is first
    assert first.is_closed


def test_the_callback_only_counts_in_the_browser_that_started_it(client_for, fresh_engine):
    idp = FakeIdP()
    owner = _wire(client_for(eng=fresh_engine), idp)
    assert _go(owner, idp, "sub-sam")[0].headers["location"] == "/"
    with fresh_engine.connect() as c:
        uid = c.execute(text("select id from app_user")).scalar()

    # link CSRF: the attacker (logged in as the owner here) starts a link and sends the URL to a victim
    attacker = _wire(client_for(uid, eng=fresh_engine, method="oidc"), idp)
    r = attacker.get("/api/auth/oidc/login", params={"mode": "link"}, follow_redirects=False)
    cb = idp.authorize(r.headers["location"], "sub-victim")
    cb.pop("_q")
    victim = _wire(client_for(eng=fresh_engine), idp)
    r = victim.get("/api/auth/oidc/callback", params=cb, follow_redirects=False)
    assert "auth_error=oidc_failed" in r.headers["location"], r.headers["location"]
    with fresh_engine.connect() as c:
        assert (
            c.execute(text("select oidc_subject from app_user where id = :u"), {"u": uid}).scalar().endswith("#sub-sam")
        )

    # login CSRF: an unused callback URL for the attacker's account does not log a victim in
    r = attacker.get("/api/auth/oidc/login", follow_redirects=False)
    cb = idp.authorize(r.headers["location"], "sub-sam")
    cb.pop("_q")
    victim = _wire(client_for(eng=fresh_engine), idp)
    r = victim.get("/api/auth/oidc/callback", params=cb, follow_redirects=False)
    assert "auth_error=oidc_failed" in r.headers["location"], r.headers["location"]
    assert victim.get("/api/me").status_code == 401

    # a step-up started by one session does not mark another session of the same browser
    stale = _wire(client_for(uid, eng=fresh_engine, step_up=False, method="oidc"), idp)
    r = stale.get("/api/auth/oidc/login", params={"mode": "step_up"}, follow_redirects=False)
    cb = idp.authorize(r.headers["location"], "sub-sam")
    cb.pop("_q")
    other = client_for(uid, eng=fresh_engine, step_up=False, method="oidc")
    stale.cookies.set("oh_session", other.cookies.get("oh_session"))
    r = stale.get("/api/auth/oidc/callback", params=cb, follow_redirects=False)
    assert "auth_error=oidc_wrong_user" in r.headers["location"], r.headers["location"]
    assert other.get("/api/auth/state").json()["step_up_fresh"] is False


def test_step_up_needs_proof_of_a_fresh_provider_login(client_for, fresh_engine):
    idp = FakeIdP()
    owner = _wire(client_for(eng=fresh_engine), idp)
    assert _go(owner, idp, "sub-sam")[0].headers["location"] == "/"
    with fresh_engine.connect() as c:
        uid = c.execute(text("select id from app_user")).scalar()
    stale = _wire(client_for(uid, eng=fresh_engine, step_up=False, method="oidc"), idp)
    idp.override = {"auth_time": None}  # a provider that reused an old SSO session and says nothing
    r, _ = _go(stale, idp, "sub-sam", mode="step_up")
    assert "auth_error=oidc_not_fresh" in r.headers["location"], r.headers["location"]
    assert stale.get("/api/auth/state").json()["step_up_fresh"] is False


def test_linking_a_provider_needs_a_fresh_login(client_for, committed_family):
    idp = FakeIdP()
    stale = _wire(client_for(committed_family.users["alex"], step_up=False), idp)
    r = stale.get("/api/auth/oidc/login", params={"mode": "link"}, follow_redirects=False)
    assert r.status_code == 403 and r.json()["detail"]["code"] == "step_up_required", r.text
    fresh = _wire(client_for(committed_family.users["alex"]), idp)
    assert fresh.get("/api/auth/oidc/login", params={"mode": "link"}, follow_redirects=False).status_code == 302
