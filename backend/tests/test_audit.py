import pytest
from sqlalchemy import text
from sqlalchemy.exc import DBAPIError

from ownhealth.core.audit import AuditEvent, hash_arg, list_audit, write_audit


def test_audit_is_append_only(conn, family):
    eid = write_audit(
        conn,
        AuditEvent(
            actor_kind="user",
            actor_id=family.users["alex"],
            via="ui",
            action="read",
            profile_id=family.profiles["alex"],
            src_ids=["src_a12"],
            rows_out=1,
        ),
    )
    rows = list_audit(conn, profile_ids=[family.profiles["alex"]])
    assert rows[0]["id"] == eid and rows[0]["src_ids"] == ["src_a12"]
    for stmt in ("update audit_event set action = 'x' where id = :id", "delete from audit_event where id = :id"):
        with pytest.raises(DBAPIError, match="append-only"), conn.begin_nested():
            conn.execute(text(stmt), {"id": eid})
    with pytest.raises(DBAPIError, match="append-only"), conn.begin_nested():
        conn.execute(text("truncate audit_event"))


def test_hash_arg_hides_search_terms():
    h = hash_arg("HIV test")
    assert "HIV" not in h and h == hash_arg("HIV test")


def test_a_bad_ip_is_stored_as_null_and_does_not_abort_the_write(conn, family):
    for raw, stored in (("testclient", None), ("1.2.3.4, 5.6.7.8", None), (" 10.0.0.1 ", "10.0.0.1"), ("::1", "::1")):
        eid = write_audit(conn, AuditEvent(actor_kind="user", via="api", action="read", ip=raw))
        got = conn.execute(text("select host(ip) from audit_event where id = :id"), {"id": eid}).scalar()
        assert got == stored
