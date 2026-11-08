"""First-run setup, passkey login, recovery codes and step-up, through the REST API."""

from sqlalchemy import text

from security.softkey import SoftKey


def _setup_owner(client, key):
    opts = client.post("/api/auth/setup/begin", json={"household_name": "Rivera", "display_name": "Alex Rivera"}).json()
    assert opts["authenticatorSelection"]["residentKey"] == "required"
    r = client.post("/api/auth/setup/finish", json={"credential": key.create(opts), "label": "laptop"})
    assert r.status_code == 200, r.text
    return r.json()


def test_first_run_setup_then_login(client_for, fresh_engine):
    anon = client_for(eng=fresh_engine)
    assert anon.get("/api/auth/state").json()["setup_needed"] is True
    key = SoftKey()
    out = _setup_owner(anon, key)
    assert out["profile"] == "alex" and len(out["recovery_codes"]) == 10
    me = anon.get("/api/me").json()  # the setup response set the session cookie
    assert me["user"]["display_name"] == "Alex Rivera"
    assert [(p["slug"], p["role"]) for p in me["profiles"]] == [("alex", "owner")]
    state = anon.get("/api/auth/state").json()
    assert state["setup_needed"] is False and state["user"]["passkeys"] == 1 and state["step_up_fresh"] is True

    # a second setup is refused: the instance has an owner
    again = client_for(eng=fresh_engine)
    assert again.post("/api/auth/setup/begin", json={"display_name": "Mallory"}).status_code == 409

    # log out, then log back in with the passkey
    assert anon.post("/api/auth/logout").json() == {"ok": True}
    assert anon.get("/api/me").status_code == 401
    opts = anon.post("/api/auth/login/begin").json()
    assertion = key.get(opts)
    r = anon.post("/api/auth/login/finish", json={"credential": assertion})
    assert r.status_code == 200, r.text
    assert anon.get("/api/me").status_code == 200
    # the same assertion again, or a new signature over the same challenge: the challenge is used up
    again = client_for(eng=fresh_engine)
    assert again.post("/api/auth/login/finish", json={"credential": assertion}).status_code == 401
    r = again.post("/api/auth/login/finish", json={"credential": key.get(opts)})  # higher sign count
    assert r.status_code == 401 and r.json()["detail"]["code"] == "challenge_invalid"
    with fresh_engine.connect() as c:
        assert (
            c.execute(text("select sign_count from passkey")).scalar() == key.count - 1
        )  # the re-signed attempt was refused
        assert c.execute(text("select count(*) from audit_event where action = 'login'")).scalar() == 2


def test_login_rejects_wrong_origin_and_unknown_key(client_for, fresh_engine):
    owner = client_for(eng=fresh_engine)
    key = SoftKey()
    _setup_owner(owner, key)
    c = client_for(eng=fresh_engine)
    bad_origin = key.get(c.post("/api/auth/login/begin").json(), origin="https://evil.example")
    assert c.post("/api/auth/login/finish", json={"credential": bad_origin}).status_code == 401
    stranger = SoftKey()
    stranger.user_handle = b"x" * 16
    r = c.post("/api/auth/login/finish", json={"credential": stranger.get(c.post("/api/auth/login/begin").json())})
    assert r.status_code == 401 and r.json()["detail"]["code"] == "passkey_unknown"
    forged = key.get(c.post("/api/auth/login/begin").json())
    forged["response"]["signature"] = forged["response"]["signature"][:-4] + "AAAA"
    assert c.post("/api/auth/login/finish", json={"credential": forged}).status_code == 401
    assert c.get("/api/me").status_code == 401


def test_recovery_code_logs_in_once_without_step_up(client_for, fresh_engine):
    owner = client_for(eng=fresh_engine)
    key = SoftKey()
    codes = _setup_owner(owner, key)["recovery_codes"]
    c = client_for(eng=fresh_engine)
    assert c.post("/api/auth/recovery", json={"code": "0000-0000-0000"}).status_code == 401
    r = c.post("/api/auth/recovery", json={"code": codes[3].lower()})  # case and dashes do not matter
    assert r.status_code == 200 and r.json()["recovery_codes_left"] == 9
    assert c.get("/api/me").status_code == 200
    state = c.get("/api/auth/state").json()
    assert state["method"] == "recovery" and state["step_up_fresh"] is False
    # step-up-gated: creating a token
    body = {"label": "x", "profiles": ["alex"]}
    assert c.post("/api/tokens", json=body).json()["detail"]["code"] == "step_up_required"
    # the code is used up
    assert client_for(eng=fresh_engine).post("/api/auth/recovery", json={"code": codes[3]}).status_code == 401
    # after a recovery login the person adds a new passkey, and step-up works with it
    new_key = SoftKey()
    opts = c.post("/api/auth/passkeys/begin").json()
    assert [d["id"] for d in opts["excludeCredentials"]]  # the old passkey is excluded
    assert c.post("/api/auth/passkeys/finish", json={"credential": new_key.create(opts)}).status_code == 200
    begin = c.post("/api/auth/step-up/begin").json()
    assert begin["method"] == "passkey"
    assert c.post("/api/auth/step-up/finish", json={"credential": new_key.get(begin["options"])}).json()["ok"]
    assert c.post("/api/tokens", json=body).status_code == 201


def test_step_up_rejects_another_users_passkey(client_for, fresh_engine):
    owner = client_for(eng=fresh_engine)
    key = SoftKey()
    _setup_owner(owner, key)
    with fresh_engine.begin() as c:
        hh = c.execute(text("select household_id from app_user")).scalar()
        sam = c.execute(
            text("insert into app_user (household_id, display_name) values (:h, 'Sam Rivera') returning id"), {"h": hh}
        ).scalar()
    sam_client = client_for(sam, step_up=False, eng=fresh_engine)
    sam_key = SoftKey()
    opts = sam_client.post("/api/auth/passkeys/begin").json()
    assert sam_client.post("/api/auth/passkeys/finish", json={"credential": sam_key.create(opts)}).status_code == 200
    begin = sam_client.post("/api/auth/step-up/begin").json()
    # Alex's key answering Sam's step-up challenge: refused, and the session stays not fresh
    key.user_handle = key.user_handle
    r = sam_client.post("/api/auth/step-up/finish", json={"credential": key.get(begin["options"])})
    assert r.status_code == 401
    assert sam_client.get("/api/auth/state").json()["step_up_fresh"] is False
    # Sam's own challenge and passkey, handed to Alex's stale session: refused too
    with fresh_engine.connect() as c:
        alex = c.execute(text("select id from app_user where display_name = 'Alex Rivera'")).scalar()
    alex_client = client_for(alex, step_up=False, eng=fresh_engine)
    begin = sam_client.post("/api/auth/step-up/begin").json()
    r = alex_client.post("/api/auth/step-up/finish", json={"credential": sam_key.get(begin["options"])})
    assert r.status_code == 401
    assert alex_client.get("/api/auth/state").json()["step_up_fresh"] is False


def test_step_up_challenge_is_bound_to_its_user(client_for, fresh_engine):
    """finish_login refuses a step-up challenge answered with another user's passkey."""
    import pytest

    from ownhealth.auth import passkeys

    owner = client_for(eng=fresh_engine)
    alex_key = SoftKey()
    _setup_owner(owner, alex_key)
    with fresh_engine.begin() as c:
        hh, alex = c.execute(text("select household_id, id from app_user")).one()
        sam = c.execute(
            text("insert into app_user (household_id, display_name) values (:h, 'Sam Rivera') returning id"), {"h": hh}
        ).scalar()
        sam_key = SoftKey()
        reg_opts = passkeys.begin_registration(c, user_id=sam, rp_id="testserver", origin="https://testserver")
        passkeys.finish_registration(
            c, user_id=sam, credential=sam_key.create(reg_opts), rp_id="testserver", origin="https://testserver"
        )
        opts = passkeys.begin_login(c, rp_id="testserver", user_id=sam)
        with pytest.raises(passkeys.PasskeyError):
            passkeys.finish_login(
                c, credential=alex_key.get(opts), rp_id="testserver", origin="https://testserver", purpose="step_up"
            )
