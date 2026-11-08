import pytest
from sqlalchemy import text

from ownhealth.auth.tokens import TokenError, create_token, revoke_token, verify_token
from ownhealth.core.access import AccessDenied


def test_pat_round_trip(conn, family):
    p = family.profiles
    new = create_token(conn, user_id=family.users["alex"], label="Claude Code · laptop", profile_ids=[p["alex"]])
    assert new.plaintext.startswith(new.prefix + "_") and len(new.plaintext) > 50
    stored = conn.execute(text("select secret_sha256, scopes from api_token where id = :i"), {"i": new.id}).one()
    assert new.plaintext.split("_")[-1].encode() not in bytes(stored.secret_sha256)
    assert stored.scopes == ["read:labs", "read:summary", "read:vitals"]
    principal = verify_token(conn, new.plaintext)
    assert principal.kind == "token" and principal.profile_ids == {p["alex"]}
    assert principal.default_profile == p["alex"]
    assert verify_token(conn, new.plaintext[:-1] + ("A" if new.plaintext[-1] != "A" else "B")) is None
    assert verify_token(conn, "oh_pat_nope") is None
    assert revoke_token(conn, new.id, family.users["alex"])
    assert verify_token(conn, new.plaintext) is None


def test_pat_rules(conn, family):
    u, p = family.users["sam"], family.profiles
    with pytest.raises(AccessDenied):
        create_token(conn, user_id=u, label="x", profile_ids=[p["carmen"]])  # no grant
    with pytest.raises(TokenError):
        create_token(conn, user_id=u, label="x", profile_ids=[p["sam"]], scopes=["read:everything"])
    with pytest.raises(TokenError):
        create_token(conn, user_id=u, label="x", profile_ids=[p["sam"]], days=400)
    t = create_token(conn, user_id=u, label="x", profile_ids=[p["sam"]], days=1)
    conn.execute(text("update api_token set expires_at = now() - interval '1 second' where id = :i"), {"i": t.id})
    assert verify_token(conn, t.plaintext) is None
