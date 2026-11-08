"""Household roles, relatives without a login, invites and claiming, locks, switches, the audit viewer."""

from sqlalchemy import text

from ownhealth.core.access import Principal, accessible_profiles, effective_permission
from security.softkey import SoftKey


def _perm(engine, user, profile):
    with engine.connect() as c:
        hh = c.execute(text("select household_id from app_user where id = :u"), {"u": user}).scalar()
        return effective_permission(c, Principal(kind="session", user_id=user, household_id=hh), profile)


def test_household_view_is_scoped(client_for, committed_family):
    f = committed_family
    alex = client_for(f.users["alex"]).get("/api/household").json()
    assert {m["display_name"] for m in alex["members"]} == {"Alex Rivera", "Sam Rivera"}
    by_slug = {p["slug"]: p for p in alex["profiles"]}
    assert by_slug["alex"]["can_manage"] and by_slug["lucia"]["can_manage"]  # guardian, unclaimed
    assert not by_slug["carmen"]["can_manage"] and not by_slug["sam"]["can_manage"]  # caregiver, viewer
    # Alex sees all grants on profiles he manages, plus his own; not Sam's grant on sam
    seen = {(g["user_id"], g["profile"], g["role"]) for g in alex["grants"]}
    assert (str(f.users["sam"]), "alex", "viewer") in seen
    assert (str(f.users["alex"]), "sam", "viewer") in seen
    assert (str(f.users["sam"]), "sam", "owner") not in seen


def test_relative_guardian_manages_until_claimed(client_for, committed_family, engine):
    f = committed_family
    alex = client_for(f.users["alex"])
    r = alex.post(
        "/api/household/profiles",
        json={"display_name": "Mateo Rivera", "role": "guardian", "relationship": "child", "birth_date": "2022-03-01"},
    )
    assert r.status_code == 201, r.text
    slug = r.json()["slug"]
    assert slug == "mateo"
    # the guardian grants Sam guardian too, with step-up
    stale = client_for(f.users["alex"], step_up=False)
    body = {"user_id": str(f.users["sam"]), "profile": slug, "role": "guardian"}
    assert stale.put("/api/grants", json=body).json()["detail"]["code"] == "step_up_required"
    assert alex.put("/api/grants", json=body).status_code == 200
    assert _perm(engine, f.users["sam"], r.json()["id"]).role == "guardian"
    # a minor's sensitive genome switch stays off
    assert alex.put(f"/api/profiles/{slug}/sensitive", json={"ui": True}).status_code == 409
    # the caregiver of Carmen cannot manage her grants or locks, nor export
    assert alex.put("/api/grants", json={**body, "profile": "carmen", "role": "viewer"}).status_code == 403
    assert alex.put("/api/profiles/carmen/locks", json={"locks": ["read:labs"]}).status_code == 403
    assert alex.post("/api/profiles/carmen/export", json={"fmt": "zip"}).status_code == 403
    # nobody becomes owner through a grant, and the owner's grant does not change
    assert alex.put("/api/grants", json={**body, "role": "owner"}).status_code == 422
    owner_body = {"user_id": str(f.users["alex"]), "profile": "alex", "role": "viewer"}
    assert alex.put("/api/grants", json=owner_body).status_code == 409


def test_invite_claims_profile(client_for, committed_family, engine):
    f = committed_family
    alex = client_for(f.users["alex"])
    # Alex is Carmen's caregiver: he may invite her to claim her own profile, nothing more
    inv = alex.post("/api/household/invites", json={"display_name": "Carmen Ortega", "claim_profile": "carmen"})
    assert inv.status_code == 201, inv.text
    code = inv.json()["code"]
    assert inv.json()["url"].endswith(f"/enroll?code={code}")
    sam = client_for(f.users["sam"])
    assert sam.post("/api/household/invites", json={"display_name": "x", "claim_profile": "carmen"}).status_code == 404

    carmen = client_for()
    info = carmen.get(f"/api/auth/invite/{code}").json()
    assert info["display_name"] == "Carmen Ortega" and info["claims_profile"] is True
    key = SoftKey()
    opts = carmen.post("/api/auth/enroll/begin", json={"code": code}).json()
    done = carmen.post("/api/auth/enroll/finish", json={"code": code, "credential": key.create(opts)})
    assert done.status_code == 200, done.text
    assert len(done.json()["recovery_codes"]) == 10
    me = carmen.get("/api/me").json()
    assert [(p["slug"], p["role"], p["is_self"]) for p in me["profiles"]] == [("carmen", "owner", True)]
    # the invite is used up
    assert client_for().get(f"/api/auth/invite/{code}").status_code == 404
    # Alex keeps his caregiver grant; now Carmen manages it
    assert _perm(engine, f.users["alex"], f.profiles["carmen"]).role == "caregiver"
    body = {"user_id": str(f.users["alex"]), "profile": "carmen", "role": "viewer"}
    assert carmen.put("/api/grants", json=body).status_code == 200
    assert _perm(engine, f.users["alex"], f.profiles["carmen"]).role == "viewer"
    assert carmen.delete(f"/api/grants?user_id={f.users['alex']}&profile=carmen").status_code == 200
    with engine.connect() as c:
        alex_p = Principal(kind="session", user_id=f.users["alex"], household_id=f.household_id)
        assert "carmen" not in {p.profile.slug for p in accessible_profiles(c, alex_p)}


def test_locks_and_switches(client_for, committed_family, engine):
    f = committed_family
    alex, stale = client_for(f.users["alex"]), client_for(f.users["alex"], step_up=False)
    # adding a lock narrows access: no step-up
    assert "read:summary" in _perm(engine, f.users["sam"], f.profiles["alex"]).scopes
    r = stale.put("/api/profiles/alex/locks", json={"locks": ["read:summary"]})
    assert r.json()["locks"] == ["read:summary"]
    assert "read:summary" not in _perm(engine, f.users["sam"], f.profiles["alex"]).scopes
    assert "read:summary" in _perm(engine, f.users["alex"], f.profiles["alex"]).scopes  # not the owner
    # removing it widens access: step-up
    assert stale.put("/api/profiles/alex/locks", json={"locks": []}).json()["detail"]["code"] == "step_up_required"
    assert alex.put("/api/profiles/alex/locks", json={"locks": []}).status_code == 200
    assert alex.put("/api/profiles/alex/locks", json={"locks": ["read:nope"]}).status_code == 422
    # switches: on needs step-up, off does not
    assert stale.put("/api/profiles/alex/sensitive", json={"agents": True}).status_code == 403
    assert alex.put("/api/profiles/alex/sensitive", json={"ui": True, "agents": True}).json() == {
        "ok": True,
        "ui": True,
        "agents": True,
    }
    assert stale.put("/api/profiles/alex/sensitive", json={"agents": False}).json()["agents"] is False


def test_audit_viewer_scope(client_for, committed_family, engine):
    f = committed_family
    from ownhealth.core.audit import AuditEvent, write_audit

    with engine.begin() as c:
        for actor, profile in (("sam", "alex"), ("alex", "carmen"), ("alex", "sam")):
            write_audit(
                c,
                AuditEvent(
                    actor_kind="user",
                    actor_id=f.users[actor],
                    via="ui",
                    action="read",
                    tool="GET /x",
                    profile_id=f.profiles[profile],
                ),
            )
    alex = client_for(f.users["alex"]).get("/api/audit").json()["items"]
    mine = {(e["actor"], e["profile"]) for e in alex}
    assert ("Sam Rivera", "alex") in mine  # who read my profile
    assert ("Alex Rivera", "carmen") in mine and ("Alex Rivera", "sam") in mine  # what I did
    sam = client_for(f.users["sam"]).get("/api/audit").json()["items"]
    assert ("Alex Rivera", "sam") in {(e["actor"], e["profile"]) for e in sam}
    # Sam is only a viewer of Alex: he does not see who else read Alex, nor Alex's caregiving reads
    assert ("Alex Rivera", "carmen") not in {(e["actor"], e["profile"]) for e in sam}
    filtered = client_for(f.users["alex"]).get("/api/audit", params={"profile": "alex"}).json()["items"]
    assert filtered and {e["profile"] for e in filtered} == {"alex"}
    assert client_for(f.users["sam"]).get("/api/audit", params={"profile": "carmen"}).status_code == 404


def test_guardian_stops_managing_once_claimed(client_for, committed_family, engine):
    from ownhealth.auth import household

    f = committed_family
    alex = client_for(f.users["alex"])
    kid = alex.post("/api/household/profiles", json={"display_name": "Mateo Rivera", "role": "guardian"}).json()
    assert alex.put(f"/api/profiles/{kid['slug']}/locks", json={"locks": []}).status_code == 200
    with engine.begin() as c:
        _, code = household.invite_member(
            c,
            household_id=f.household_id,
            invited_by=f.users["alex"],
            display_name="Mateo Rivera",
            claim_profile_id=kid["id"],
        )
        household.redeem_invite(c, code)
    assert _perm(engine, f.users["alex"], kid["id"]).role == "guardian"  # the grant stays
    assert alex.put(f"/api/profiles/{kid['slug']}/locks", json={"locks": []}).status_code == 403
    assert alex.post(f"/api/profiles/{kid['slug']}/export", json={"fmt": "zip"}).status_code == 403


def test_a_relatives_profile_always_has_a_manager(client_for, committed_family):
    alex = client_for(committed_family.users["alex"])
    r = alex.post("/api/household/profiles", json={"display_name": "Pilar Ortega", "role": "caregiver"})
    assert r.status_code == 400 and r.json()["detail"]["code"] == "bad_input", r.text


def test_a_caregiver_cannot_take_over_a_managed_profile_with_a_claim_invite(client_for, committed_family, engine):
    f = committed_family
    alex, sam = client_for(f.users["alex"]), client_for(f.users["sam"])
    # Alex is Lucía's guardian and makes Sam her caregiver: Sam may not send the claim invite
    grant = {"user_id": str(f.users["sam"]), "profile": "lucia", "role": "caregiver"}
    assert alex.put("/api/grants", json=grant).status_code == 200
    r = sam.post("/api/household/invites", json={"display_name": "Lucía Rivera", "claim_profile": "lucia"})
    assert r.status_code == 403 and r.json()["detail"]["code"] == "forbidden", r.text
    # the guardian may
    assert (
        alex.post("/api/household/invites", json={"display_name": "Lucía Rivera", "claim_profile": "lucia"}).status_code
        == 201
    )
    # a caregiver's invite for a profile nobody manages stops working once someone does
    inv = alex.post("/api/household/invites", json={"display_name": "Carmen Ortega", "claim_profile": "carmen"})
    assert inv.status_code == 201, inv.text
    with engine.begin() as c:
        c.execute(
            text(  # Sam becomes Carmen's guardian after Alex (her caregiver) sent the invite
                "insert into profile_grant (user_id, profile_id, role, categories)"
                " select :u, :p, 'guardian', categories from profile_grant where user_id = :a and profile_id = :p"
            ),
            {"u": f.users["sam"], "p": f.profiles["carmen"], "a": f.users["alex"]},
        )
    carmen = client_for(eng=engine)
    key = SoftKey()
    opts = carmen.post("/api/auth/enroll/begin", json={"code": inv.json()["code"]}).json()
    r = carmen.post("/api/auth/enroll/finish", json={"code": inv.json()["code"], "credential": key.create(opts)})
    assert r.status_code == 400 and r.json()["detail"]["code"] == "invite_invalid", r.text
    assert _perm(engine, f.users["sam"], f.profiles["carmen"]).role == "guardian"
