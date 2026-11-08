"""PAT management over REST: step-up to create, shown once, revoke, and never by a token."""

from sqlalchemy import text

from ownhealth.auth.tokens import verify_token


def test_create_list_revoke(client_for, committed_family, engine):
    alex = committed_family.users["alex"]
    stale = client_for(alex, step_up=False)
    body = {"label": "Claude Code · laptop", "profiles": ["alex", "lucia"], "scopes": ["read:summary", "read:labs"]}
    r = stale.post("/api/tokens", json=body)
    assert r.status_code == 403 and r.json()["detail"]["code"] == "step_up_required"

    c = client_for(alex)
    r = c.post("/api/tokens", json=body)
    assert r.status_code == 201, r.text
    out = r.json()
    tok, plaintext = out["token"], out["plaintext"]
    assert plaintext.startswith(tok["prefix"] + "_")
    assert tok["profiles"] == ["alex", "lucia"] and tok["default_profile"] == "alex"
    assert tok["scopes"] == ["read:labs", "read:summary"] and tok["identity_mode"] == "full"
    assert tok["lang"] == "es" and tok["limits"]["rate_per_min"] == 60

    listed = c.get("/api/tokens").json()["items"]
    assert [t["id"] for t in listed] == [tok["id"]]
    assert plaintext not in str(listed) and "secret" not in str(listed)

    with engine.begin() as conn:
        assert verify_token(conn, plaintext) is not None
        ev = conn.execute(
            text("select action, args_summary from audit_event where token_id = :t"), {"t": tok["id"]}
        ).one()
        assert ev.action == "token_create" and plaintext not in (ev.args_summary or "")

    # the token itself may not manage tokens, or read the audit log, or the household
    as_token = client_for()
    hdr = {"Authorization": f"Bearer {plaintext}"}
    assert as_token.get("/api/tokens", headers=hdr).status_code == 403
    assert as_token.post("/api/tokens", json=body, headers=hdr).status_code == 403
    assert as_token.get("/api/audit", headers=hdr).status_code == 403
    assert as_token.get("/api/household", headers=hdr).status_code == 403

    # a profile without a grant cannot go on a token
    bad = c.post("/api/tokens", json={**body, "profiles": ["nobody"]})
    assert bad.status_code == 404
    sam = client_for(committed_family.users["sam"])
    assert sam.post("/api/tokens", json={**body, "profiles": ["sam", "carmen"]}).status_code == 404
    assert c.post("/api/tokens", json={**body, "scopes": ["read:everything"]}).status_code == 422
    assert c.post("/api/tokens", json={**body, "days": 400}).status_code == 422

    # Sam cannot revoke Alex's token; Alex can, once
    assert sam.delete(f"/api/tokens/{tok['id']}").status_code == 404
    assert c.delete(f"/api/tokens/{tok['id']}").json() == {"id": tok["id"], "revoked": True}
    assert c.delete(f"/api/tokens/{tok['id']}").status_code == 404
    with engine.begin() as conn:
        assert verify_token(conn, plaintext) is None
