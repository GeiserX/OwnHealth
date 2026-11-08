"""The password fallback exists only where WebAuthn cannot run: plain HTTP on a LAN address."""

import dataclasses

from ownhealth.auth.password import check_password, hash_password, passkeys_possible

LAN = "http://192.168.1.20:8080"


def test_secure_context_rule():
    assert passkeys_possible("https://health.example.lan")
    assert passkeys_possible("http://localhost:8080")
    assert not passkeys_possible("http://127.0.0.1:8080")  # an IP is never a WebAuthn RP ID
    assert passkeys_possible("http://ownhealth.localhost")
    assert not passkeys_possible(LAN) and not passkeys_possible("http://health.example.lan")


def test_hash():
    h = hash_password("correct horse battery")
    assert h.startswith("scrypt$") and "correct" not in h
    assert check_password("correct horse battery", h) and not check_password("correct horse batterx", h)
    assert hash_password("correct horse battery") != h  # salted
    assert not check_password("x", None) and not check_password("x", "garbage")


def test_lan_install_uses_password(client_for, fresh_engine, https_settings):
    lan = dataclasses.replace(https_settings, base_url=LAN)
    c = client_for(eng=fresh_engine, settings=lan)
    state = c.get("/api/auth/state").json()
    assert state["passkeys_possible"] is False and state["password_allowed"] is True
    assert c.post("/api/auth/setup/begin", json={"display_name": "Alex Rivera"}).json()["detail"]["code"] == (
        "passkeys_unavailable"
    )
    assert (
        c.post("/api/auth/setup/password", json={"display_name": "Alex Rivera", "password": "short"}).status_code == 422
    )
    r = c.post("/api/auth/setup/password", json={"display_name": "Alex Rivera", "password": "a long synthetic pass"})
    assert r.status_code == 200, r.text
    assert "secure" not in r.headers["set-cookie"].lower()  # plain HTTP: a Secure cookie would never come back
    assert c.get("/api/me").status_code == 200

    other = client_for(eng=fresh_engine, settings=lan)
    assert (
        other.post("/api/auth/password/login", json={"name": "alex rivera", "password": "wrong one"}).status_code == 401
    )
    assert (
        other.post(
            "/api/auth/password/login", json={"name": "Alex Rivera", "password": "a long synthetic pass"}
        ).status_code
        == 200
    )
    # step-up with the password
    assert other.post("/api/auth/step-up/begin").json() == {"method": "password"}
    assert other.post("/api/auth/step-up/finish", json={"password": "nope"}).status_code == 401
    assert other.post("/api/auth/step-up/finish", json={"password": "a long synthetic pass"}).json()["ok"]

    # behind HTTPS the same instance refuses passwords: passkeys work there
    https = client_for(eng=fresh_engine)
    r = https.post("/api/auth/password/login", json={"name": "Alex Rivera", "password": "a long synthetic pass"})
    assert r.status_code == 403 and r.json()["detail"]["code"] == "passkeys_available"
