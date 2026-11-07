from datetime import datetime

import pytest
from factories import lab_document, manual_src, obs
from sqlalchemy import text
from sqlalchemy.exc import DBAPIError

from ownhealth.core.lifecycle import LifecycleError
from ownhealth.core.observations import (
    commit_observation,
    confirm_observation,
    get_observation,
    latest_per_group,
    list_observations,
    observation_history,
    retract_observation,
    supersede_observation,
)
from ownhealth.core.provenance import ProvenanceError, SrcIn, resolve_src


def test_document_value_needs_page_and_bbox(conn, family, blobs):
    doc_id, boxes = lab_document(conn, blobs, family)
    with pytest.raises(ValueError, match="needs document_id, page_no and bbox"):
        SrcIn(kind="document_region", document_id=doc_id, page_no=1)
    with pytest.raises(ValueError, match="needs import_ref"):
        SrcIn(kind="fhir_import")
    with pytest.raises(ProvenanceError):
        commit_observation(conn, obs(family.profiles["alex"]), src="src_unknown", accepted_by="auto")
    src = SrcIn(kind="document_region", document_id=doc_id, page_no=1, bbox=boxes[0])
    c = commit_observation(conn, obs(family.profiles["alex"], document_id=doc_id), src=src, accepted_by="auto")
    row = get_observation(conn, c.id)
    assert row["src_id"] == c.src_id and row["status"] == "current"
    assert row["fhir"]["resourceType"] == "Observation"
    assert row["fhir"]["code"]["coding"][0]["code"] == "13457-7"
    resolved = resolve_src(conn, c.src_id)
    assert resolved["profile_id"] == family.profiles["alex"]
    assert resolved["document"]["id"] == doc_id
    assert resolved["src"]["bbox"] == list(boxes[0])


def test_rows_are_immutable_and_corrections_supersede(conn, family):
    p = family.profiles["alex"]
    first = commit_observation(
        conn, obs(p, "410", group="glucose", loinc="2345-7"), src=manual_src(), accepted_by="person"
    )
    with pytest.raises(DBAPIError, match="immutable"), conn.begin_nested():
        conn.execute(text("update observation set printed_value = '140' where id = :id"), {"id": first.id})
    t_before = conn.execute(text("select now()")).scalar()
    fixed = supersede_observation(
        conn,
        first.id,
        obs(p, "140", group="glucose", loinc="2345-7"),
        src=manual_src(),
        accepted_by="person",
        reason="OCR misread, confirmed 140",
    )
    old = get_observation(conn, first.id)
    assert old["status"] == "superseded" and old["status_reason"] == "OCR misread, confirmed 140"
    assert get_observation(conn, fixed.id)["supersedes"] == first.id
    assert [r["id"] for r in observation_history(conn, fixed.id)] == [first.id, fixed.id]
    current = list_observations(conn, p, alias_group="glucose")
    assert [r["printed_value"] for r in current] == ["140"]
    # a superseded row cannot be superseded again, nor come back
    with pytest.raises(LifecycleError):
        supersede_observation(
            conn, first.id, obs(p, "150", group="glucose"), src=manual_src(), accepted_by="person", reason="again"
        )
    with pytest.raises(DBAPIError, match="not allowed"), conn.begin_nested():
        conn.execute(text("update observation set status = 'current' where id = :id"), {"id": first.id})
    assert t_before is not None


def test_as_of_sees_what_the_store_knew(conn, family):
    p = family.profiles["alex"]
    a = commit_observation(conn, obs(p, "101"), src=manual_src(), accepted_by="person")
    between = conn.execute(text("select clock_timestamp()")).scalar()
    retract_observation(conn, a.id, "entered in error")
    after = conn.execute(text("select clock_timestamp()")).scalar()
    assert list_observations(conn, p, alias_group="ldl_c") == []
    assert [r["id"] for r in list_observations(conn, p, alias_group="ldl_c", as_of=between)] == [a.id]
    assert list_observations(conn, p, alias_group="ldl_c", as_of=after) == []
    with pytest.raises(LifecycleError):
        retract_observation(conn, a.id, "twice")


def test_retract_needs_reason_and_provisional_confirms(conn, family):
    p = family.profiles["alex"]
    c = commit_observation(conn, obs(p), src=manual_src(), accepted_by="person", status="provisional")
    assert list_observations(conn, p) == []  # provisional is not current
    with pytest.raises(LifecycleError):
        retract_observation(conn, c.id, "  ")
    confirm_observation(conn, c.id)
    assert [r["id"] for r in latest_per_group(conn, p)] == [c.id]
    with pytest.raises(LifecycleError):
        confirm_observation(conn, c.id)


def test_effective_at_must_be_aware(conn, family):
    o = obs(family.profiles["alex"]).model_copy(update={"effective_at": datetime(2026, 1, 1)})
    with pytest.raises(ValueError):
        commit_observation(conn, o, src=manual_src(), accepted_by="person")
