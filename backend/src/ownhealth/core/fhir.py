"""Build the FHIR R4-shaped JSON we store in `fhir` from the typed columns.

Imported resources keep the received JSON instead; these builders are for rows we create
(document extraction, manual entry, demo seed). The full exporter (lane G) wraps these in Bundles.
"""

from __future__ import annotations

from datetime import date, datetime
from decimal import Decimal
from typing import Any


def _iso(v: date | datetime | None) -> str | None:
    return v.isoformat() if v is not None else None


def _num(v: Decimal | float | int | None) -> float | int | None:
    if v is None:
        return None
    d = Decimal(str(v))
    return int(d) if d == d.to_integral_value() else float(d)


def _drop_none(d: dict[str, Any]) -> dict[str, Any]:
    return {k: v for k, v in d.items() if v not in (None, [], {})}


def _code(system_url: str | None, code: str | None, display: str) -> dict:
    coding = [_drop_none({"system": system_url, "code": code, "display": display})] if code else []
    return _drop_none({"coding": coding, "text": display})


CODE_SYSTEMS = {
    "loinc": "http://loinc.org",
    "snomed": "http://snomed.info/sct",
    "icd10": "http://hl7.org/fhir/sid/icd-10",
    "rxnorm": "http://www.nlm.nih.gov/research/umls/rxnorm",
    "cvx": "http://hl7.org/fhir/sid/cvx",
    "atc": "http://www.whocc.no/atc",
}


def system_url(code_system: str | None) -> str | None:
    if code_system is None:
        return None
    return CODE_SYSTEMS.get(code_system.lower(), code_system)


def observation(o: dict) -> dict:
    """o: the observation columns (ObservationIn.model_dump())."""
    value: dict[str, Any] = {}
    if o.get("value_num") is not None:
        value["valueQuantity"] = _drop_none(
            {
                "value": _num(o["value_num"]),
                "comparator": o.get("value_cmp"),
                "unit": o.get("printed_unit"),
                "system": "http://unitsofmeasure.org" if o.get("unit_ucum") else None,
                "code": o.get("unit_ucum"),
            }
        )
    elif o.get("value_code") is not None:
        value["valueCodeableConcept"] = {"text": o["value_code"]}
    else:
        value["valueString"] = o.get("value_text") or o["printed_value"]
    ref = _drop_none(
        {
            "low": _drop_none({"value": _num(o.get("ref_low")), "unit": o.get("printed_unit")}),
            "high": _drop_none({"value": _num(o.get("ref_high")), "unit": o.get("printed_unit")}),
            "text": o.get("printed_ref_range"),
        }
    )
    return _drop_none(
        {
            "resourceType": "Observation",
            "status": "preliminary" if o.get("status") == "provisional" else "final",
            "category": [
                {
                    "coding": [
                        {"system": "http://terminology.hl7.org/CodeSystem/observation-category", "code": o["category"]}
                    ]
                }
            ],
            "code": _code("http://loinc.org", o.get("loinc"), o["printed_name"]),
            "effectiveDateTime": _iso(o.get("effective_at")),
            **value,
            "interpretation": [
                {
                    "coding": [
                        {
                            "system": "http://terminology.hl7.org/CodeSystem/v3-ObservationInterpretation",
                            "code": o["interpretation"],
                        }
                    ]
                }
            ]
            if o.get("interpretation")
            else None,
            "referenceRange": [ref] if ref else None,
            "method": {"text": o["method"]} if o.get("method") else None,
        }
    )


def condition(c: dict) -> dict:
    return _drop_none(
        {
            "resourceType": "Condition",
            "clinicalStatus": {
                "coding": [
                    {"system": "http://terminology.hl7.org/CodeSystem/condition-clinical", "code": c["clinical_status"]}
                ]
            },
            "verificationStatus": {
                "coding": [
                    {
                        "system": "http://terminology.hl7.org/CodeSystem/condition-ver-status",
                        "code": c["verification_status"],
                    }
                ]
            },
            "code": _code(system_url(c.get("code_system")), c.get("code"), c["text"]),
            "onsetDateTime": _iso(c.get("onset")),
            "abatementDateTime": _iso(c.get("abatement")),
        }
    )


def medication_statement(m: dict) -> dict:
    status = {"active": "active", "stopped": "stopped", "on_hold": "on-hold"}[m.get("med_status", "active")]
    return _drop_none(
        {
            "resourceType": "MedicationStatement",
            "status": status,
            "medicationCodeableConcept": _code(
                system_url(m.get("code_system")),
                m.get("code"),
                " ".join(x for x in (m["name"], m.get("strength")) if x),
            ),
            "effectivePeriod": _drop_none({"start": _iso(m.get("started")), "end": _iso(m.get("stopped"))}),
            "dosage": [{"text": m["dose_text"]}] if m.get("dose_text") else None,
            "category": {"text": "supplement"} if m.get("kind") == "supplement" else None,
        }
    )


ALLERGY_VERIFICATION = ("unconfirmed", "presumed", "confirmed", "refuted", "entered-in-error")


def allergy(a: dict) -> dict:
    ver = a.get("verification_status") or "confirmed"
    ver = ver if ver in ALLERGY_VERIFICATION else None  # a free string: an unknown code is left out
    return _drop_none(
        {
            "resourceType": "AllergyIntolerance",
            # FHIR ait-1: no clinicalStatus on an entered-in-error allergy.
            "clinicalStatus": None
            if ver == "entered-in-error"
            else {
                "coding": [
                    {
                        "system": "http://terminology.hl7.org/CodeSystem/allergyintolerance-clinical",
                        "code": a.get("clinical_status", "active"),
                    }
                ]
            },
            "verificationStatus": {
                "coding": [
                    {"system": "http://terminology.hl7.org/CodeSystem/allergyintolerance-verification", "code": ver}
                ]
            }
            if ver
            else None,
            "code": _code(system_url(a.get("code_system")), a.get("code"), a["substance"]),
            "criticality": a.get("criticality"),
            "reaction": [{"description": a["reaction_text"]}] if a.get("reaction_text") else None,
        }
    )


def immunization(i: dict) -> dict:
    return _drop_none(
        {
            "resourceType": "Immunization",
            "status": "completed",
            "vaccineCode": _code(system_url(i.get("code_system")), i.get("code"), i["vaccine"]),
            "occurrenceDateTime": _iso(i.get("occurred")),
            "lotNumber": i.get("lot"),
            "protocolApplied": [{"doseNumberPositiveInt": i["dose_number"]}] if i.get("dose_number") else None,
        }
    )


def procedure(p: dict) -> dict:
    return _drop_none(
        {
            "resourceType": "Procedure",
            "status": "completed",
            "code": _code(system_url(p.get("code_system")), p.get("code"), p["text"]),
            "performedDateTime": _iso(p.get("performed")),
        }
    )


BUILDERS = {
    "observation": observation,
    "condition": condition,
    "medication_statement": medication_statement,
    "allergy": allergy,
    "immunization": immunization,
    "procedure": procedure,
}
