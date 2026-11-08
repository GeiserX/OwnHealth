"""Review accept never writes outside the item's profile, and held payloads follow the read scopes."""

from factories import obs
from sqlalchemy import text

from ownhealth.core.review import ReviewItemIn, create_item


def _item(engine, fam, who, kind="candidate_row", payload=None, **kw):
    pid = fam.profiles[who]
    if payload is None:
        o = obs(pid, **kw).model_dump(mode="json")
        payload = {"observation": o, "src": {"kind": "manual", "profile_id": str(pid)}}
    with engine.begin() as c:
        return create_item(
            c, ReviewItemIn(kind=kind, household_id=fam.household_id, profile_id=pid, payload=payload, reasons=["x"])
        )


def _count(engine, table, pid):
    with engine.connect() as c:
        return c.execute(text(f"select count(*) from {table} where profile_id = :p"), {"p": pid}).scalar()


def test_accept_edits_cannot_move_a_fact_to_another_profile(client_for, engine, committed_family):
    fam = committed_family
    sam = fam.profiles["sam"]  # alex is only a viewer on sam
    alex = client_for(fam.users["alex"])
    cand = _item(engine, fam, "alex")
    fhir = _item(
        engine,
        fam,
        "alex",
        kind="fhir_unmapped",
        payload={
            "projection": "record",
            "record_kind": "condition",
            "record": {"profile_id": str(fam.profiles["alex"]), "text": "Planted condition"},
            "src": {"kind": "fhir_import", "profile_id": str(fam.profiles["alex"]), "import_ref": "bundle#1"},
        },
    )
    for iid in (cand, fhir):
        r = alex.post(f"/api/review/{iid}/accept", json={"edits": {"profile_id": str(sam)}})
        assert r.status_code == 409, r.text
    assert _count(engine, "observation", sam) == 0 and _count(engine, "condition", sam) == 0
    assert alex.post(f"/api/review/{cand}/accept", json={"edits": {"value_num": "98"}}).status_code == 200
    assert _count(engine, "observation", fam.profiles["alex"]) == 1


def test_document_identity_target_needs_import(client_for, engine, committed_family):
    fam = committed_family
    iid = _item(
        engine, fam, "alex", kind="document_identity", payload={"suggested_profile_id": str(fam.profiles["sam"])}
    )
    r = client_for(fam.users["alex"]).post(f"/api/review/{iid}/accept", json={})
    assert r.status_code == 403, r.text
    with engine.connect() as c:
        assert c.execute(text("select status from review_item where id = :i"), {"i": iid}).scalar() == "open"


def test_caregiver_without_read_sensitive_cannot_see_a_sensitive_held_value(client_for, engine, committed_family):
    fam = committed_family
    hidden = _item(engine, fam, "carmen", value="1", printed_name="VIH Ac", sensitivity=["sexual_health"])
    plain = _item(engine, fam, "carmen")
    alex = client_for(fam.users["alex"])  # caregiver on carmen: no read:sensitive
    listed = alex.get("/api/review", params={"profile": "carmen"}).json()
    ids = {i["id"] for i in listed["items"]}
    assert str(plain) in ids and str(hidden) not in ids and listed["withheld"] == 1
    assert alex.get(f"/api/review/{hidden}").status_code == 404
    assert alex.post(f"/api/review/{hidden}/accept", json={}).status_code == 404
    assert alex.get(f"/api/review/{plain}").status_code == 200
