from decimal import Decimal

import pytest
from factories import lab_document, obs

from ownhealth.core import review
from ownhealth.core.acceptance import Check
from ownhealth.core.observations import get_observation, list_observations
from ownhealth.core.review import CandidateIn, ReviewError, ReviewItemIn


def _run(conn, doc_id):
    return review.create_extraction_run(conn, doc_id, extractor="rules:test", extractor_version="1", config_hash=b"t")


def _cand(family, bbox, conf=0.99, checks=None, value="97"):
    return CandidateIn(
        page_no=1,
        bbox=bbox,
        cells={"value": {"text": value}},
        row_conf=conf,
        checks=checks or [],
        observation=obs(family.profiles["alex"], value),
    )


def test_auto_accepted_row_keeps_page_and_bbox(conn, family, blobs):
    doc_id, boxes = lab_document(conn, blobs, family)
    run = _run(conn, doc_id)
    res = review.submit_candidate(conn, run, _cand(family, boxes[0]), allowance="template_text", identity="matched")
    assert res.state == "auto_accepted" and res.review_item_id is None
    row = get_observation(conn, res.observation_id)
    assert row["accepted_by"] == "auto" and row["document_id"] == doc_id and row["candidate_row_id"] is not None
    src = conn.exec_driver_sql("select page_no, bbox, kind from src where id = %s", (res.src_id,)).one()
    assert src.page_no == 1 and src.bbox == list(boxes[0]) and src.kind == "document_region"


def test_failed_check_goes_to_review_then_accept_with_edit(conn, family, blobs):
    doc_id, boxes = lab_document(conn, blobs, family)
    run = _run(conn, doc_id)
    bad = [Check(name="units", ok=False, reason="unit_dimension_mismatch")]
    res = review.submit_candidate(
        conn, run, _cand(family, boxes[0], checks=bad), allowance="template_text", identity="matched"
    )
    assert res.state == "in_review" and res.reasons == ["unit_dimension_mismatch"]
    assert list_observations(conn, family.profiles["alex"]) == []  # nothing silent, nothing committed
    item = review.get_item(conn, res.review_item_id)
    assert item["status"] == "open" and item["payload"]["checks"][0]["reason"] == "unit_dimension_mismatch"
    created = review.accept(
        conn, item["id"], family.users["alex"], edits={"value_canonical": "2.51", "unit_canonical": "mmol/L"}
    )
    row = get_observation(conn, created[0])
    assert row["accepted_by"] == "person" and row["reviewed_by"] == family.users["alex"]
    assert row["value_canonical"] == Decimal("2.51") and row["printed_value"] == "97"
    with pytest.raises(ReviewError):
        review.accept(conn, item["id"], family.users["alex"])  # once only


def test_reject_needs_reason_and_reopen(conn, family, blobs):
    doc_id, boxes = lab_document(conn, blobs, family)
    res = review.submit_candidate(
        conn, _run(conn, doc_id), _cand(family, boxes[0], conf=0.5), allowance="rules_text", identity="matched"
    )
    with pytest.raises(ReviewError):
        review.reject(conn, res.review_item_id, family.users["alex"], " ")
    review.reject(conn, res.review_item_id, family.users["alex"], "not my result")
    assert review.get_item(conn, res.review_item_id)["decision_reason"] == "not my result"
    review.reopen(conn, res.review_item_id, family.users["alex"])
    assert review.get_item(conn, res.review_item_id)["status"] == "open"


def test_bulk_accept_only_clean_rows_above_threshold(conn, family, blobs):
    doc_id, boxes = lab_document(conn, blobs, family)
    run = _run(conn, doc_id)
    low = review.submit_candidate(
        conn, run, _cand(family, boxes[0], conf=0.91), allowance="rules_text", identity="matched"
    )
    lower = review.submit_candidate(
        conn, run, _cand(family, boxes[1], conf=0.80, value="24"), allowance="rules_text", identity="matched"
    )
    failed = review.submit_candidate(
        conn,
        run,
        _cand(family, boxes[1], conf=0.99, value="25", checks=[Check(name="g", ok=False, reason="not_grounded")]),
        allowance="rules_text",
        identity="matched",
    )
    accepted = review.bulk_accept(conn, document_id=doc_id, user_id=family.users["alex"], min_row_conf=0.90)
    assert accepted == [low.review_item_id]
    assert review.get_item(conn, lower.review_item_id)["status"] == "open"
    assert review.get_item(conn, failed.review_item_id)["status"] == "open"
    assert review.count_open(conn, household_id=family.household_id) == 2


def test_identity_hint_is_not_enough_and_agent_items_need_a_handler(conn, family, blobs):
    doc_id, boxes = lab_document(conn, blobs, family)
    res = review.submit_candidate(
        conn, _run(conn, doc_id), _cand(family, boxes[0]), allowance="template_text", identity="hinted"
    )
    assert res.state == "in_review"
    item = review.create_item(
        conn, ReviewItemIn(kind="restore_check_failed", household_id=family.household_id, payload={})
    )
    with pytest.raises(ReviewError, match="dismiss"):
        review.accept(conn, item, family.users["alex"])
    review.dismiss(conn, item, family.users["alex"], "seen")
    assert review.get_item(conn, item)["status"] == "dismissed"


def test_an_edit_keeps_the_received_resource_in_step_with_the_columns(conn, family):
    from factories import manual_src

    received = {
        "resourceType": "Observation",
        "id": "upstream-1",
        "status": "final",
        "code": {"coding": [{"system": "http://loinc.org", "code": "13457-7"}], "text": "LDL"},
        "valueQuantity": {"value": 97, "unit": "mg/dL"},
        "meta": {"source": "synthetic"},
    }
    o = obs(family.profiles["alex"], "97").model_copy(update={"fhir": received})
    res = review.route_fact(
        conn,
        observation=o,
        src=manual_src(),
        checks=[Check(name="units", ok=False, reason="unit_dimension_mismatch")],
        row_conf=0.99,
        allowance="structured_valid",
        identity="matched",
    )
    created = review.accept(
        conn,
        res.review_item_id,
        family.users["alex"],
        edits={"value_num": "79", "printed_value": "79", "fhir": {"resourceType": "Observation", "valueString": "x"}},
    )
    resource = conn.exec_driver_sql("select fhir from observation where id = %s", (created[0],)).scalar()
    assert resource["valueQuantity"]["value"] == 79 and "valueString" not in resource
    assert resource["id"] == "upstream-1" and resource["meta"] == {"source": "synthetic"}  # the rest is kept
    assert resource["code"] == received["code"]  # an unedited part stays as received
