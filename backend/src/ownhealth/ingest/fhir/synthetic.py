"""A synthetic International Patient Summary (IPS) bundle, in the shape of the HL7 IPS examples:
`Bundle.type = document`, a Composition first with its sections, then the Patient and the entries
the sections reference. Synthetic people only; the issuing organization is the fictional LabNorte.
"""

from __future__ import annotations

from datetime import date

SNOMED = "http://snomed.info/sct"
LOINC = "http://loinc.org"


def _ref(kind: str, rid: str) -> dict:
    return {"reference": f"urn:uuid:{kind.lower()}-{rid}"}


def ips_bundle(
    *, bundle_id: str, given: str, family: str, birth: date, with_observations: bool = True, extra: list[dict] = ()
) -> dict:
    pid = f"{given.lower()}-{family.lower()}"
    patient = {
        "resourceType": "Patient",
        "id": pid,
        "name": [{"family": family, "given": [given]}],
        "birthDate": birth.isoformat(),
    }
    resources = [
        {
            "resourceType": "Condition",
            "id": f"{pid}-htn",
            "clinicalStatus": {
                "coding": [{"system": "http://terminology.hl7.org/CodeSystem/condition-clinical", "code": "active"}]
            },
            "verificationStatus": {
                "coding": [
                    {"system": "http://terminology.hl7.org/CodeSystem/condition-ver-status", "code": "confirmed"}
                ]
            },
            "code": {"coding": [{"system": SNOMED, "code": "38341003", "display": "Hypertensive disorder"}]},
            "subject": {"reference": f"Patient/{pid}"},
            "onsetDateTime": "2015-03",
        },
        {
            "resourceType": "AllergyIntolerance",
            "id": f"{pid}-pen",
            "clinicalStatus": {
                "coding": [
                    {"system": "http://terminology.hl7.org/CodeSystem/allergyintolerance-clinical", "code": "active"}
                ]
            },
            "verificationStatus": {
                "coding": [
                    {
                        "system": "http://terminology.hl7.org/CodeSystem/allergyintolerance-verification",
                        "code": "confirmed",
                    }
                ]
            },
            "criticality": "high",
            "code": {"coding": [{"system": SNOMED, "code": "764146007", "display": "Penicillin"}]},
            "patient": {"reference": f"Patient/{pid}"},
            "reaction": [{"manifestation": [{"coding": [{"system": SNOMED, "code": "271807003", "display": "Rash"}]}]}],
        },
        {
            "resourceType": "MedicationStatement",
            "id": f"{pid}-enalapril",
            "status": "active",
            "medicationReference": {"reference": f"Medication/{pid}-med-enalapril"},
            "subject": {"reference": f"Patient/{pid}"},
            "effectivePeriod": {"start": "2015-04-02"},
            "dosage": [{"text": "10 mg once a day"}],
        },
        {
            "resourceType": "Medication",
            "id": f"{pid}-med-enalapril",
            "code": {
                "coding": [{"system": "http://www.whocc.no/atc", "code": "C09AA02", "display": "enalapril"}],
                "text": "Enalapril 10 mg",
            },
        },
        {
            "resourceType": "Immunization",
            "id": f"{pid}-flu-2025",
            "status": "completed",
            "vaccineCode": {
                "coding": [{"system": "http://hl7.org/fhir/sid/cvx", "code": "197", "display": "Influenza, seasonal"}]
            },
            "patient": {"reference": f"Patient/{pid}"},
            "occurrenceDateTime": "2025-10-20",
            "lotNumber": "SYN-0425",
        },
        {
            "resourceType": "Procedure",
            "id": f"{pid}-cataract",
            "status": "completed",
            "code": {"coding": [{"system": SNOMED, "code": "110473004", "display": "Cataract surgery"}]},
            "subject": {"reference": f"Patient/{pid}"},
            "performedDateTime": "2022-09-12",
        },
    ]
    if with_observations:
        resources.append(
            {
                "resourceType": "Observation",
                "id": f"{pid}-hba1c",
                "status": "final",
                "category": [
                    {
                        "coding": [
                            {
                                "system": "http://terminology.hl7.org/CodeSystem/observation-category",
                                "code": "laboratory",
                            }
                        ]
                    }
                ],
                "code": {"coding": [{"system": LOINC, "code": "4548-4", "display": "Hemoglobin A1c"}]},
                "subject": {"reference": f"Patient/{pid}"},
                "effectiveDateTime": "2026-05-12T09:00:00+02:00",
                "valueQuantity": {"value": 5.9, "unit": "%", "system": "http://unitsofmeasure.org", "code": "%"},
                "referenceRange": [{"low": {"value": 4.0}, "high": {"value": 5.6}}],
                "interpretation": [
                    {
                        "coding": [
                            {
                                "system": "http://terminology.hl7.org/CodeSystem/v3-ObservationInterpretation",
                                "code": "H",
                            }
                        ]
                    }
                ],
            }
        )
    resources += list(extra)
    sections = {
        "11450-4": ("Problem list", "Condition"),
        "48765-2": ("Allergies and intolerances", "AllergyIntolerance"),
        "10160-0": ("Medication summary", "MedicationStatement"),
        "11369-6": ("Immunizations", "Immunization"),
        "47519-4": ("History of procedures", "Procedure"),
        "30954-2": ("Results", "Observation"),
    }
    composition = {
        "resourceType": "Composition",
        "id": f"{bundle_id}-composition",
        "status": "final",
        "type": {"coding": [{"system": LOINC, "code": "60591-5", "display": "Patient summary Document"}]},
        "subject": {"reference": f"urn:uuid:patient-{pid}"},
        "date": "2026-11-01T10:00:00+01:00",
        "author": [{"reference": "urn:uuid:organization-labnorte"}],
        "title": f"Patient Summary of {given} {family}",
        "section": [
            {
                "title": title,
                "code": {"coding": [{"system": LOINC, "code": code}]},
                "entry": [_ref(r["resourceType"], r["id"]) for r in resources if r["resourceType"] == rtype],
            }
            for code, (title, rtype) in sections.items()
            if any(r["resourceType"] == rtype for r in resources)
        ],
    }
    org = {"resourceType": "Organization", "id": "labnorte", "name": "LabNorte (synthetic)"}
    entries = [composition, patient, org, *resources]
    return {
        "resourceType": "Bundle",
        "id": bundle_id,
        "identifier": {"system": "urn:oid:2.16.724.4.99", "value": bundle_id},
        "type": "document",
        "timestamp": "2026-11-01T10:00:00+01:00",
        "entry": [{"fullUrl": f"urn:uuid:{r['resourceType'].lower()}-{r['id']}", "resource": r} for r in entries],
    }
