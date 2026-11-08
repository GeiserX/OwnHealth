"""FHIR R4 import: IPS document bundles, US Core collections, NDJSON, review routing, idempotency."""

import json

import pytest
from sqlalchemy import text

from ownhealth.core import review
from ownhealth.core.records import active_medications, list_records
from ownhealth.ingest.fhir import import_fhir
from ownhealth.ingest.fhir.mapping import loinc_valid, match_patient

JSON = "application/fhir+json"


def _items(conn, doc, kind=None):
    rows = review.list_items(conn, document_id=doc, status="open", kind=kind)
    return sorted(rows, key=lambda r: r["payload"].get("resource_type") or "")


def _obs(conn, profile):
    return (
        conn.execute(
            text(
                "select o.*, s.import_ref, s.kind as src_kind from observation o join src s on s.id = o.src_id"
                " where o.profile_id = :p order by o.printed_name"
            ),
            {"p": profile},
        )
        .mappings()
        .all()
    )


def test_ips_document_bundle_projects_records_and_queues_the_rest(conn, family, blobs, store_doc, fhir_file):
    p = family.profiles["alex"]
    doc = store_doc(conn, family, "alex", fhir_file("ips-alex.json"), "ips.json", JSON)
    s = import_fhir(conn, blobs, document_id=doc)
    # 5 records + HbA1c + 2 blood pressure components; Encounter (unmapped) and the bad LOINC wait
    assert (s.accepted, s.queued, s.skipped) == (8, 2, 1)
    assert s.reasons == {"unmapped": 1, "loinc_unknown": 1, "skip:status_entered-in-error": 1}

    [cond] = list_records(conn, "condition", p)
    assert (cond["text"], cond["code_system"], cond["code"]) == ("Hypertensive disorder", "snomed", "38341003")
    assert cond["verification_status"] == "confirmed" and cond["accepted_by"] == "import"
    assert cond["fhir"]["id"] == "alex-rivera-htn"  # the received JSON is kept
    [med] = active_medications(conn, p)
    assert (med["name"], med["code_system"], med["code"]) == ("Enalapril 10 mg", "atc", "C09AA02")  # via Medication
    [allergy] = list_records(conn, "allergy", p)
    assert allergy["criticality"] == "high" and allergy["reaction_text"] == "Rash"
    [vac] = list_records(conn, "immunization", p)
    assert str(vac["occurred"]) == "2025-10-20" and vac["lot"] == "SYN-0425"
    assert len(list_records(conn, "procedure", p)) == 1

    obs = {o["printed_name"]: o for o in _obs(conn, p)}
    assert set(obs) == {"Hemoglobin A1c", "Systolic blood pressure", "Diastolic blood pressure"}
    a1c = obs["Hemoglobin A1c"]
    assert (a1c["loinc"], a1c["unit_ucum"], a1c["printed_value"], a1c["interpretation"]) == ("4548-4", "%", "5.9", "H")
    assert a1c["src_kind"] == "fhir_import" and a1c["import_ref"] == "ips-alex-2026/Observation/alex-rivera-hba1c"
    assert a1c["report_key"] == "ips-alex-2026/DiagnosticReport/alex-dr-1"
    assert a1c["accepted_by"] == "auto" and a1c["document_id"] == doc
    assert obs["Systolic blood pressure"]["import_ref"].endswith("/Observation/alex-bp-1#component0")
    assert obs["Systolic blood pressure"]["category"] == "vital-signs"

    unmapped, held = _items(conn, doc, "fhir_unmapped"), _items(conn, doc, "candidate_row")
    assert [i["payload"]["resource_type"] for i in unmapped] == ["Encounter"]
    assert unmapped[0]["payload"]["note"] == "stored, not projected"
    assert unmapped[0]["payload"]["resource"]["id"] == "alex-enc-1"  # stored as received
    assert held[0]["reasons"] == ["loinc_unknown"]
    # the injection text in Observation.note stays inside the stored resource, never in a typed column
    assert "Ignore previous" in json.dumps(held[0]["payload"]["observation"]["fhir"])
    flags = conn.execute(text("select flags, identity_match from document where id = :d"), {"d": doc}).one()
    assert flags.identity_match == "matched" and flags.flags["import"]["accepted"] == 8


def test_same_bundle_twice_and_reexport_do_not_double_count(conn, family, blobs, store_doc, fhir_file):
    p = family.profiles["alex"]
    data = fhir_file("ips-alex.json")
    doc = store_doc(conn, family, "alex", data, "ips.json", JSON)
    first = import_fhir(conn, blobs, document_id=doc)
    again = import_fhir(conn, blobs, document_id=doc)  # a job retry: the stored summary, no new rows
    assert again.as_dict() == first.as_dict()
    # a new export of the same record (new Bundle.id, new bytes): every resource is a duplicate
    bundle = json.loads(data)
    bundle["id"] = "ips-alex-2026-b"
    doc2 = store_doc(conn, family, "alex", json.dumps(bundle).encode(), "ips2.json", JSON)
    second = import_fhir(conn, blobs, document_id=doc2)
    assert (second.accepted, second.queued) == (0, 0)
    assert second.skipped == 11 and second.reasons["duplicate"] == 10
    assert len(_obs(conn, p)) == 3 and len(list_records(conn, "condition", p)) == 1


def test_changed_resource_waits_in_review_and_accepting_supersedes(conn, family, blobs, store_doc, fhir_file):
    p = family.profiles["alex"]
    data = json.loads(fhir_file("ips-alex.json"))
    import_fhir(conn, blobs, document_id=store_doc(conn, family, "alex", json.dumps(data).encode(), "a.json", JSON))
    for e in data["entry"]:
        if e["resource"]["resourceType"] == "Condition" and e["resource"]["id"] == "alex-rivera-htn":
            e["resource"]["clinicalStatus"]["coding"][0]["code"] = "remission"
    data["id"] = "ips-alex-2027"
    doc2 = store_doc(conn, family, "alex", json.dumps(data).encode(), "b.json", JSON)
    s = import_fhir(conn, blobs, document_id=doc2)
    assert s.queued == 1 and s.reasons["conflict"] == 1 and s.reasons["duplicate"] == 9
    [item] = _items(conn, doc2, "fhir_unmapped")
    assert item["payload"]["projection"] == "record" and item["reasons"] == ["conflict"]
    [new_id] = review.accept(conn, item["id"], family.users["alex"])
    [cond] = list_records(conn, "condition", p)
    assert cond["id"] == new_id and cond["clinical_status"] == "remission" and cond["supersedes"] is not None
    assert cond["accepted_by"] == "person"


def _a1c_values(conn, profile):
    return conn.execute(
        text(
            "select printed_value, status from observation where profile_id = :p and loinc = '4548-4'"
            " order by recorded_at"
        ),
        {"p": profile},
    ).all()


def _correct_a1c(data: dict) -> None:
    for e in data["entry"]:
        if e["resource"].get("id") == "alex-rivera-hba1c":
            e["resource"]["valueQuantity"]["value"] = 6.4  # the lab corrected the value upstream
    data["id"] = "ips-alex-2027"


def test_a_corrected_observation_names_the_version_it_replaces(conn, family, blobs, store_doc, fhir_file):
    p = family.profiles["alex"]
    data = json.loads(fhir_file("ips-alex.json"))
    import_fhir(conn, blobs, document_id=store_doc(conn, family, "alex", json.dumps(data).encode(), "a.json", JSON))
    [(old_value, _)] = _a1c_values(conn, p)
    old_id = conn.execute(
        text("select id from observation where profile_id = :p and loinc = '4548-4'"), {"p": p}
    ).scalar()
    _correct_a1c(data)
    doc2 = store_doc(conn, family, "alex", json.dumps(data).encode(), "b.json", JSON)
    import_fhir(conn, blobs, document_id=doc2)
    [item] = [i for i in _items(conn, doc2, "candidate_row") if "conflict" in i["reasons"]]
    assert old_value == "5.9" and item["payload"]["supersedes"] == str(old_id)


def test_accepting_a_corrected_observation_supersedes_the_old_value(conn, family, blobs, store_doc, fhir_file):
    p = family.profiles["alex"]
    data = json.loads(fhir_file("ips-alex.json"))
    import_fhir(conn, blobs, document_id=store_doc(conn, family, "alex", json.dumps(data).encode(), "a.json", JSON))
    _correct_a1c(data)
    doc2 = store_doc(conn, family, "alex", json.dumps(data).encode(), "b.json", JSON)
    import_fhir(conn, blobs, document_id=doc2)
    [item] = [i for i in _items(conn, doc2, "candidate_row") if "conflict" in i["reasons"]]
    review.accept(conn, item["id"], family.users["alex"])
    assert sorted(_a1c_values(conn, p)) == [("5.9", "superseded"), ("6.4", "current")]


def _asthma(rid: str, subject: str) -> dict:
    return {
        "resourceType": "Condition",
        "id": rid,
        "clinicalStatus": {
            "coding": [{"system": "http://terminology.hl7.org/CodeSystem/condition-clinical", "code": "active"}]
        },
        "verificationStatus": {
            "coding": [{"system": "http://terminology.hl7.org/CodeSystem/condition-ver-status", "code": "confirmed"}]
        },
        "code": {"coding": [{"system": "http://snomed.info/sct", "code": "195967001", "display": "Asthma"}]},
        "subject": {"reference": subject},
    }


OTHER_PATIENT = {
    "resourceType": "Patient",
    "id": "p-889",
    "identifier": [{"system": "urn:oid:2.16.724.4.77", "value": "HN-889"}],
    "name": [{"family": "Rivera", "given": ["Alex"]}],
    "birthDate": "1979-05-14",
}


@pytest.mark.parametrize("with_patient", [False, True])
def test_another_source_reusing_a_resource_id_is_a_separate_fact(
    conn, family, blobs, store_doc, fhir_file, with_patient
):
    """FHIR ids are unique per server only: another provider's Condition/alex-rivera-htn is not a
    new version of ours."""
    p = family.profiles["alex"]
    import_fhir(conn, blobs, document_id=store_doc(conn, family, "alex", fhir_file("ips-alex.json"), "a.json", JSON))
    entries = [{"resource": _asthma("alex-rivera-htn", "Patient/p-889" if with_patient else "Patient/alex-rivera")}]
    if with_patient:
        entries.insert(0, {"fullUrl": "urn:uuid:p-889", "resource": OTHER_PATIENT})
    other = {"resourceType": "Bundle", "id": "other-hospital-1", "type": "collection", "entry": entries}
    doc2 = store_doc(conn, family, "alex", json.dumps(other).encode(), "o.json", JSON)
    s = import_fhir(conn, blobs, document_id=doc2)
    assert "conflict" not in s.reasons and "duplicate" not in s.reasons
    for item in _items(conn, doc2):
        assert not item["payload"].get("supersedes")
        review.accept(conn, item["id"], family.users["alex"])
    assert sorted(c["text"] for c in list_records(conn, "condition", p)) == ["Asthma", "Hypertensive disorder"]


def test_patient_mismatch_holds_every_fact_for_review(conn, family, blobs, store_doc, fhir_file):
    p = family.profiles["sam"]
    doc = store_doc(conn, family, "sam", fhir_file("ips-alex.json"), "ips.json", JSON)  # Alex's file, Sam's profile
    s = import_fhir(conn, blobs, document_id=doc)
    assert s.accepted == 0 and s.queued == 10
    assert s.reasons["identity_mismatch"] == 9
    assert _obs(conn, p) == [] and list_records(conn, "condition", p) == []
    held = [i for i in _items(conn, doc, "fhir_unmapped") if i["payload"]["projection"] == "record"]
    assert {i["payload"]["record_kind"] for i in held} == {
        "condition",
        "allergy",
        "medication",
        "immunization",
        "procedure",
    }
    # a person confirms one: it becomes a record, accepted by that person
    cond = next(i for i in held if i["payload"]["record_kind"] == "condition")
    review.accept(conn, cond["id"], family.users["sam"])
    [row] = list_records(conn, "condition", p)
    assert row["accepted_by"] == "person"
    unmapped = next(i for i in _items(conn, doc, "fhir_unmapped") if i["payload"]["projection"] is None)
    with pytest.raises(review.ReviewError, match="stored, not projected"):
        review.accept(conn, unmapped["id"], family.users["sam"])


def test_us_core_collection_medication_requests_and_sensitivity(conn, family, blobs, store_doc, fhir_file):
    p = family.profiles["sam"]
    doc = store_doc(conn, family, "sam", fhir_file("uscore-sam.json"), "sam.json", JSON)
    s = import_fhir(conn, blobs, document_id=doc)
    assert (s.accepted, s.queued) == (4, 2)
    assert s.reasons == {"unit_unconvertible": 1, "unmapped": 1}
    meds = {m["name"]: m for m in list_records(conn, "medication", p)}
    assert meds["Levothyroxine 50 mcg"]["med_status"] == "active"
    amox = meds["Amoxicillin 500 mg"]
    assert amox["med_status"] == "stopped" and str(amox["stopped"]) == "2026-02-08"
    assert [m["name"] for m in active_medications(conn, p)] == ["Levothyroxine 50 mcg"]
    [mood] = list_records(conn, "condition", p)
    assert mood["sensitivity"] == ["mental_health"]
    assert list_records(conn, "condition", p, include_sensitive=False) == []
    [weight] = _obs(conn, p)
    assert weight["effective_at"].isoformat() == "2026-10-02T11:30:00+00:00"  # -04:00 kept as the same instant
    assert [i["payload"]["resource_type"] for i in _items(conn, doc, "fhir_unmapped")] == ["CarePlan"]


def test_ndjson_bulk_lines(conn, family, blobs, store_doc, fhir_file):
    p = family.profiles["carmen"]
    doc = store_doc(conn, family, "carmen", fhir_file("bulk-carmen.ndjson"), "x.ndjson", "application/fhir+ndjson")
    s = import_fhir(conn, blobs, document_id=doc)
    assert (s.accepted, s.queued) == (2, 1)
    [vac] = list_records(conn, "immunization", p)
    assert vac["dose_number"] == 5
    [item] = _items(conn, doc, "fhir_unmapped")
    assert "not-done" in item["payload"]["note"] and item["payload"]["json_pointer"] == "L4"


def test_match_patient_rules():
    from datetime import date

    pat = {"name": [{"given": ["Lucia"], "family": "Rivera"}], "birthDate": "2017-06-21"}
    assert match_patient(pat, "Lucía Rivera", date(2017, 6, 21)) == "matched"  # accents ignored
    assert match_patient(pat, "Lucía Rivera", date(2017, 6, 22)) == "mismatch"
    assert match_patient({"name": pat["name"]}, "Lucía Rivera", date(2017, 6, 21)) == "hinted"
    assert match_patient({"birthDate": "2017-06-21"}, "Lucía Rivera", date(2017, 6, 21)) == "hinted"
    assert match_patient({"name": [{"family": "Ortega"}]}, "Lucía Rivera", None) == "mismatch"
    assert match_patient(None, "Lucía Rivera", None) == "missing"


@pytest.mark.parametrize(
    ("code", "ok"),
    [("718-7", True), ("13457-7", True), ("4548-4", True), ("2345-8", False), ("2345", False), (None, False)],
)
def test_loinc_check_digit(code, ok):
    assert loinc_valid(code) is ok


def _ferritin_bundle(fhir_file, *, display: str, unit_text: str) -> dict:
    """One Observation with a valid LOINC (2276-4, ferritin), a vendor display no alias table knows, and a
    printed unit text beside its UCUM code."""
    ips = json.loads(fhir_file("ips-alex.json"))
    patient = next(e for e in ips["entry"] if e["resource"]["resourceType"] == "Patient")
    obs = {
        "resourceType": "Observation",
        "id": "vendor-ferritin-1",
        "status": "final",
        "category": [
            {"coding": [{"system": "http://terminology.hl7.org/CodeSystem/observation-category", "code": "laboratory"}]}
        ],
        "code": {"coding": [{"system": "http://loinc.org", "code": "2276-4", "display": display}]},
        "subject": {"reference": "Patient/alex-rivera"},
        "effectiveDateTime": "2026-06-02T09:00:00+02:00",
        "valueQuantity": {"value": 85, "unit": unit_text, "system": "http://unitsofmeasure.org", "code": "ng/mL"},
    }
    return {"resourceType": "Bundle", "id": "vendor-1", "type": "collection", "entry": [patient, {"resource": obs}]}


def _ferritin(conn, family, blobs, store_doc, fhir_file, display):
    data = _ferritin_bundle(fhir_file, display=display, unit_text="nanogram per millilitre")
    import_fhir(conn, blobs, document_id=store_doc(conn, family, "alex", json.dumps(data).encode(), "v.json", JSON))
    return [o for o in _obs(conn, family.profiles["alex"]) if o["loinc"] == "2276-4" and o["status"] == "current"]


def test_received_loinc_and_ucum_drive_the_derived_layer(conn, family, blobs, store_doc, fhir_file):
    """A valid received LOINC sets the alias group and a received UCUM code sets the unit, even when the
    printed name and unit text are unknown to the tables."""
    [o] = _ferritin(conn, family, blobs, store_doc, fhir_file, "VNDR-FERR serum assay")
    assert o["alias_group"] == "ferritin" and o["unit_ucum"] == "ng/mL"
    assert o["value_canonical"] is not None and o["unit_canonical"] is not None
    assert o["printed_unit"] == "nanogram per millilitre"  # the printed text stays the truth layer


def test_rederive_keeps_the_received_ucum_code(conn, family, blobs, store_doc, fhir_file):
    from ownhealth.normalize import rederive

    [o] = _ferritin(conn, family, blobs, store_doc, fhir_file, "Ferritin")
    assert o["unit_ucum"] == "ng/mL" and o["value_canonical"] is not None
    rederive(conn, profile_id=family.profiles["alex"])
    [again] = [x for x in _obs(conn, family.profiles["alex"]) if x["loinc"] == "2276-4" and x["status"] == "current"]
    assert (again["unit_ucum"], again["value_canonical"], again["alias_group"]) == (
        o["unit_ucum"],
        o["value_canonical"],
        o["alias_group"],
    )
