from datetime import date

import pytest
from factories import manual_src

from ownhealth.core.lifecycle import LifecycleError
from ownhealth.core.records import (
    ConditionIn,
    MedicationIn,
    active_medications,
    add_record,
    get_record,
    list_records,
    record_history,
    retract_record,
    supersede_record,
)


def test_medications_lifecycle_and_active_on_date(conn, family):
    p = family.profiles["alex"]
    old = add_record(
        conn,
        MedicationIn(
            profile_id=p,
            name="Atorvastatin",
            strength="20 mg",
            started=date(2022, 6, 20),
            stopped=date(2025, 12, 1),
            med_status="stopped",
        ),
        src=manual_src(),
        accepted_by="person",
    )
    new = add_record(
        conn,
        MedicationIn(profile_id=p, name="Atorvastatin", strength="20 mg", started=date(2026, 3, 10)),
        src=manual_src(),
        accepted_by="person",
    )
    add_record(
        conn,
        MedicationIn(profile_id=p, name="Omeprazole", strength="20 mg", started=date(2026, 4, 1)),
        src=manual_src(),
        accepted_by="person",
    )
    names = lambda rows: [(r["name"], r["started"]) for r in rows]  # noqa: E731
    assert names(active_medications(conn, p, on=date(2026, 11, 7))) == [
        ("Atorvastatin", date(2026, 3, 10)),
        ("Omeprazole", date(2026, 4, 1)),
    ]
    assert names(active_medications(conn, p, on=date(2024, 1, 1))) == [("Atorvastatin", date(2022, 6, 20))]
    assert names(active_medications(conn, p, on=date(2026, 1, 15))) == []
    row = get_record(conn, "medication", new.id)
    assert row["fhir"]["resourceType"] == "MedicationStatement" and row["src_id"] == new.src_id
    assert old.id != new.id


def test_condition_correction_and_retraction(conn, family):
    p = family.profiles["alex"]
    c1 = add_record(
        conn,
        ConditionIn(profile_id=p, text="Hypercholesterolemia", verification_status="provisional"),
        src=manual_src(),
        accepted_by="person",
    )
    c2 = supersede_record(
        conn,
        c1.id,
        ConditionIn(
            profile_id=p,
            text="Hypercholesterolaemia",
            code_system="icd10",
            code="E78.0",
            verification_status="confirmed",
        ),
        src=manual_src(),
        accepted_by="person",
        reason="confirmed by lipid clinic",
    )
    assert [r["id"] for r in record_history(conn, "condition", c2.id)] == [c1.id, c2.id]
    assert [r["id"] for r in list_records(conn, "condition", p)] == [c2.id]
    retract_record(conn, "condition", c2.id, "entered on the wrong profile")
    assert list_records(conn, "condition", p) == []
    with pytest.raises(LifecycleError):
        retract_record(conn, "condition", c2.id, "again")
    with pytest.raises(LifecycleError):
        supersede_record(
            conn,
            c1.id,
            ConditionIn(profile_id=family.profiles["sam"], text="x"),
            src=manual_src(),
            accepted_by="person",
            reason="move",
        )


def test_the_allergy_builder_carries_its_verification_status():
    from ownhealth.core import fhir

    refuted = fhir.allergy({"substance": "Penicillin", "verification_status": "refuted"})
    assert refuted["verificationStatus"]["coding"][0]["code"] == "refuted"
    assert fhir.allergy({"substance": "Penicillin"})["verificationStatus"]["coding"][0]["code"] == "confirmed"
    erased = fhir.allergy({"substance": "Penicillin", "verification_status": "entered-in-error"})
    assert "clinicalStatus" not in erased
    assert "verificationStatus" not in fhir.allergy({"substance": "Penicillin", "verification_status": "maybe"})
