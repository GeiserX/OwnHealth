"""FHIR R4 resource -> internal rows (architecture.md 9). Pure functions, no database.

Codes arrive as received; the received JSON is kept in `fhir`. Each mapper returns a `Mapped`
(rows plus the structural checks that decide auto-accept) or None when the resource type has no
projection, in which case the importer stores it and lists it in review as "stored, not projected".
"""

from __future__ import annotations

import re
import unicodedata
from dataclasses import dataclass, field
from datetime import UTC, date, datetime
from decimal import Decimal, InvalidOperation

from ownhealth.core.acceptance import Check
from ownhealth.core.fhir import CODE_SYSTEMS
from ownhealth.core.records import AllergyIn, ConditionIn, ImmunizationIn, MedicationIn, ProcedureIn, RecordIn

LOINC = "http://loinc.org"
UCUM = "http://unitsofmeasure.org"
_SYSTEM_NAMES = {url: name for name, url in CODE_SYSTEMS.items()}
# Resources that give context to others (patient, references) and are kept in the bundle blob only.
SUPPORTING = {"Patient", "Composition", "Medication", "Organization", "Practitioner", "PractitionerRole", "Device"}
_SECURITY = {  # v3-ActCode confidentiality / sensitivity labels -> our sensitivity tags
    "PSY": "mental_health",
    "PSYTHPN": "mental_health",
    "MH": "mental_health",
    "SUD": "mental_health",
    "ETH": "mental_health",
    "SEX": "sexual_health",
    "STD": "sexual_health",
    "HIV": "sexual_health",
    "SDV": "reproductive",
}
_CATEGORIES = {
    "laboratory": "laboratory",
    "vital-signs": "vital-signs",
    "activity": "activity",
    "survey": "survey",
    "social-history": "survey",
    "exam": "exam",
    "imaging": "exam",
    "procedure": "exam",
}
_INTERP = {"L", "N", "H", "LL", "HH", "A"}


@dataclass
class ObsRow:
    """One observation row before normalization. `suffix` tells components apart in import_ref."""

    suffix: str
    category: str
    printed_name: str
    printed_value: str
    printed_unit: str | None
    printed_ref_range: str | None
    value_num: Decimal | None
    value_cmp: str | None
    value_text: str | None
    value_code: str | None
    ref_low: Decimal | None
    ref_high: Decimal | None
    loinc: str | None
    unit_ucum: str | None
    interpretation: str | None
    effective_at: datetime | None
    checks: list[Check] = field(default_factory=list)


@dataclass
class Mapped:
    kind: str  # "observation" | "record" | "skip"
    observations: list[ObsRow] = field(default_factory=list)
    record: RecordIn | None = None
    record_kind: str | None = None
    checks: list[Check] = field(default_factory=list)
    skip_reason: str | None = None  # entered-in-error, not-done ... (counted, never projected)
    unmapped_note: str | None = None  # project nothing, queue as stored-not-projected with this note


# --------------------------------------------------------------- primitives ----


def loinc_valid(code: str | None) -> bool:
    """Format plus the LOINC mod-10 check digit, so a typo or a shifted column is caught."""
    if not code or not re.fullmatch(r"\d{1,7}-\d", code):
        return False
    body, check = code.split("-")
    total = 0
    for i, ch in enumerate(reversed(body)):
        d = int(ch)
        if i % 2 == 0:
            d *= 2
            d = d - 9 if d > 9 else d
        total += d
    return (10 - total % 10) % 10 == int(check)


def concept_text(cc: dict | None) -> str | None:
    if not cc:
        return None
    if cc.get("text"):
        return cc["text"]
    for c in cc.get("coding") or []:
        if c.get("display"):
            return c["display"]
    for c in cc.get("coding") or []:
        if c.get("code"):
            return c["code"]
    return None


def first_coding(cc: dict | None, prefer: str | None = None) -> tuple[str | None, str | None]:
    """(short code system name or URL, code) of the preferred system, else the first coding."""
    codings = [c for c in (cc or {}).get("coding") or [] if c.get("code")]
    if prefer:
        codings = sorted(codings, key=lambda c: c.get("system") != prefer)
    if not codings:
        return None, None
    c = codings[0]
    system = c.get("system")
    return _SYSTEM_NAMES.get(system, system), c["code"]


def loinc_of(cc: dict | None) -> str | None:
    for c in (cc or {}).get("coding") or []:
        if c.get("system") == LOINC and c.get("code"):
            return c["code"]
    return None


def parse_datetime(v: str | None) -> datetime | None:
    """FHIR date or dateTime. A date alone becomes 00:00 UTC; a time without zone is invalid FHIR."""
    if not v:
        return None
    try:
        if len(v) == 4:
            return datetime(int(v), 1, 1, tzinfo=UTC)
        if len(v) == 7:
            return datetime(int(v[:4]), int(v[5:7]), 1, tzinfo=UTC)
        if len(v) == 10:
            d = date.fromisoformat(v)
            return datetime(d.year, d.month, d.day, tzinfo=UTC)
        dt = datetime.fromisoformat(v.replace("Z", "+00:00"))
        return dt if dt.tzinfo else None
    except ValueError:
        return None


def parse_date(v: str | None) -> date | None:
    dt = parse_datetime(v)
    return dt.date() if dt else None


def _dec(v) -> Decimal | None:
    if v is None or isinstance(v, bool):
        return None
    try:
        return Decimal(str(v))
    except InvalidOperation:
        return None


def _num_text(v) -> str:
    return format(Decimal(str(v)), "f") if isinstance(v, (int, float)) and not isinstance(v, bool) else str(v)


def sensitivity(resource: dict) -> list[str]:
    tags = {
        _SECURITY[s.get("code")]
        for s in (resource.get("meta") or {}).get("security") or []
        if s.get("code") in _SECURITY
    }
    return sorted(tags)


def status_code(cc: dict | None, default: str) -> str:
    for c in (cc or {}).get("coding") or []:
        if c.get("code"):
            return c["code"]
    return default


def name_tokens(s: str) -> set[str]:
    s = unicodedata.normalize("NFKD", s).encode("ascii", "ignore").decode().lower()
    return {t for t in re.split(r"[^a-z0-9]+", s) if len(t) > 1}


def match_patient(patient: dict | None, display_name: str, birth_date: date | None) -> str:
    """Identity of a bundle against the profile it is imported into: matched | hinted | mismatch | missing.

    Matched needs both the name and the birth date. A different birth date is a mismatch even when
    the name agrees; one of the two alone is only a hint."""
    if patient is None:
        return "missing"
    tokens: set[str] = set()
    for n in patient.get("name") or []:
        tokens |= name_tokens(" ".join([*(n.get("given") or []), n.get("family") or "", n.get("text") or ""]))
    want = name_tokens(display_name)
    name_ok = bool(tokens and want) and (want <= tokens or tokens <= want)
    pdob = parse_date(patient.get("birthDate"))
    if pdob and birth_date and pdob != birth_date:
        return "mismatch"
    dob_ok = pdob is not None and pdob == birth_date
    if name_ok and dob_ok:
        return "matched"
    if name_ok or dob_ok:
        return "hinted"
    return "mismatch" if tokens else "missing"


# ------------------------------------------------------------- observations ----


def _value(src: dict) -> dict:
    """Value fields of an Observation or one of its components; {} when there is no value."""
    q = src.get("valueQuantity")
    if q is not None and q.get("value") is not None:
        ucum = q.get("code") if q.get("system") == UCUM else None
        checks = []
        if not ucum:
            checks.append(Check(name="ucum_unit", ok=False, reason="unit_unconvertible", detail="no UCUM code"))
        return {
            "printed_value": (q.get("comparator") or "") + _num_text(q["value"]),
            "printed_unit": q.get("unit") or q.get("code"),
            "value_num": _dec(q["value"]),
            "value_cmp": q.get("comparator") if q.get("comparator") in ("<", "<=", ">", ">=") else None,
            "unit_ucum": ucum,
            "checks": checks,
        }
    if src.get("valueCodeableConcept") is not None:
        txt = concept_text(src["valueCodeableConcept"]) or ""
        return {"printed_value": txt, "value_code": first_coding(src["valueCodeableConcept"])[1] or txt}
    for key in ("valueString", "valueBoolean", "valueInteger", "valueDateTime", "valueTime"):
        if key in src and src[key] is not None:
            raw = src[key]
            if key == "valueInteger":
                return {"printed_value": str(raw), "value_num": Decimal(raw)}
            return {"printed_value": str(raw).lower() if key == "valueBoolean" else str(raw), "value_text": str(raw)}
    return {}


def _ref_range(src: dict) -> tuple[str | None, Decimal | None, Decimal | None]:
    rr = (src.get("referenceRange") or [None])[0]
    if not rr:
        return None, None, None
    low = _dec((rr.get("low") or {}).get("value"))
    high = _dec((rr.get("high") or {}).get("value"))
    printed = rr.get("text")
    if not printed and (low is not None or high is not None):
        if low is not None and high is not None:
            printed = f"{low} - {high}"
        else:
            printed = f"< {high}" if high is not None else f"> {low}"
    return printed, low, high


def _interp(src: dict) -> str | None:
    for cc in src.get("interpretation") or []:
        for c in cc.get("coding") or []:
            if c.get("code") in _INTERP:
                return c["code"]
    return None


def _obs_row(suffix: str, category: str, code: dict, src: dict, when: datetime | None) -> ObsRow | None:
    val = _value(src)
    if not val:
        return None
    name = concept_text(code) or "unnamed"
    loinc = loinc_of(code)
    checks = list(val.pop("checks", []))
    if not loinc_valid(loinc):
        checks.append(Check(name="loinc_code", ok=False, reason="loinc_unknown", detail=loinc or "no LOINC coding"))
    if when is None:
        checks.append(Check(name="effective_date", ok=False, reason="date_invalid"))
    printed_range, low, high = _ref_range(src)
    return ObsRow(
        suffix=suffix,
        category=category,
        printed_name=name,
        printed_value=val["printed_value"],
        printed_unit=val.get("printed_unit"),
        printed_ref_range=printed_range,
        value_num=val.get("value_num"),
        value_cmp=val.get("value_cmp"),
        value_text=val.get("value_text"),
        value_code=val.get("value_code"),
        ref_low=low,
        ref_high=high,
        loinc=loinc,
        unit_ucum=val.get("unit_ucum"),
        interpretation=_interp(src),
        effective_at=when,
        checks=checks,
    )


def observation(r: dict) -> Mapped:
    if r.get("status") in ("entered-in-error", "cancelled"):
        return Mapped("skip", skip_reason=f"status_{r['status']}")
    cat = "laboratory"
    for cc in r.get("category") or []:
        code = status_code(cc, "")
        if code in _CATEGORIES:
            cat = _CATEGORIES[code]
            break
    when = parse_datetime(
        r.get("effectiveDateTime") or r.get("effectiveInstant") or (r.get("effectivePeriod") or {}).get("start")
    ) or parse_datetime(r.get("issued"))
    rows = []
    main = _obs_row("", cat, r.get("code") or {}, r, when)
    if main:
        rows.append(main)
    for i, comp in enumerate(r.get("component") or []):
        row = _obs_row(f"#component{i}", cat, comp.get("code") or {}, comp, when)
        if row:
            rows.append(row)
    if not rows:
        if r.get("hasMember"):
            return Mapped("skip", skip_reason="panel_grouping")  # its members arrive as their own resources
        return Mapped("observation", unmapped_note="Observation without a value")
    return Mapped("observation", observations=rows)


# ------------------------------------------------------------------ records ----


def _effective(r: dict, *keys: str) -> datetime | None:
    for k in keys:
        if r.get(k):
            return parse_datetime(r[k])
    return None


def condition(r: dict, profile_id) -> Mapped:
    ver = status_code(r.get("verificationStatus"), "unconfirmed")
    if ver == "entered-in-error":
        return Mapped("skip", skip_reason="status_entered-in-error")
    if ver not in ("provisional", "confirmed", "refuted", "unconfirmed", "differential"):
        ver = "unconfirmed"
    system, code = first_coding(r.get("code"), prefer="http://snomed.info/sct")
    text = concept_text(r.get("code"))
    if not text:
        return Mapped("record", unmapped_note="Condition without a code or text")
    rec = ConditionIn(
        profile_id=profile_id,
        text=text,
        code_system=system,
        code=code,
        clinical_status=status_code(r.get("clinicalStatus"), "active"),
        verification_status=ver,
        onset=parse_date(r.get("onsetDateTime") or (r.get("onsetPeriod") or {}).get("start")),
        abatement=parse_date(r.get("abatementDateTime") or (r.get("abatementPeriod") or {}).get("start")),
        effective_at=_effective(r, "recordedDate", "onsetDateTime"),
        sensitivity=sensitivity(r),
        fhir=r,
    )
    return Mapped("record", record=rec, record_kind="condition", checks=_code_check(code))


def _code_check(code: str | None) -> list[Check]:
    return [] if code else [Check(name="code", ok=False, reason="unmapped", detail="no coded concept")]


def allergy(r: dict, profile_id) -> Mapped:
    if status_code(r.get("verificationStatus"), "") == "entered-in-error":
        return Mapped("skip", skip_reason="status_entered-in-error")
    text = concept_text(r.get("code"))
    if not text:
        return Mapped("record", unmapped_note="AllergyIntolerance without a substance")
    system, code = first_coding(r.get("code"))
    reaction = None
    for rx in r.get("reaction") or []:
        parts = [concept_text(m) for m in rx.get("manifestation") or []]
        reaction = rx.get("description") or ", ".join(p for p in parts if p) or None
        if reaction:
            break
    crit = r.get("criticality") if r.get("criticality") in ("low", "high", "unable-to-assess") else None
    rec = AllergyIn(
        profile_id=profile_id,
        substance=text,
        code_system=system,
        code=code,
        criticality=crit,
        reaction_text=reaction,
        clinical_status=status_code(r.get("clinicalStatus"), "active"),
        verification_status=status_code(r.get("verificationStatus"), "confirmed"),
        effective_at=_effective(r, "recordedDate", "onsetDateTime"),
        sensitivity=sensitivity(r),
        fhir=r,
    )
    return Mapped("record", record=rec, record_kind="allergy", checks=_code_check(code))


def immunization(r: dict, profile_id) -> Mapped:
    if r.get("status") == "entered-in-error":
        return Mapped("skip", skip_reason="status_entered-in-error")
    if r.get("status") == "not-done":
        return Mapped("record", unmapped_note="Immunization not given (status not-done)")
    text = concept_text(r.get("vaccineCode"))
    if not text:
        return Mapped("record", unmapped_note="Immunization without a vaccine code")
    system, code = first_coding(r.get("vaccineCode"))
    dose = None
    for pa in r.get("protocolApplied") or []:
        if isinstance(pa.get("doseNumberPositiveInt"), int):
            dose = pa["doseNumberPositiveInt"]
    rec = ImmunizationIn(
        profile_id=profile_id,
        vaccine=text,
        code_system=system,
        code=code,
        occurred=parse_date(r.get("occurrenceDateTime")),
        dose_number=dose,
        lot=r.get("lotNumber"),
        effective_at=_effective(r, "occurrenceDateTime", "recorded"),
        sensitivity=sensitivity(r),
        fhir=r,
    )
    checks = _code_check(code)
    if rec.occurred is None:
        checks.append(Check(name="occurrence", ok=False, reason="date_invalid"))
    return Mapped("record", record=rec, record_kind="immunization", checks=checks)


def procedure(r: dict, profile_id) -> Mapped:
    if r.get("status") in ("entered-in-error", "not-done"):
        return Mapped("skip", skip_reason=f"status_{r['status']}")
    text = concept_text(r.get("code"))
    if not text:
        return Mapped("record", unmapped_note="Procedure without a code or text")
    system, code = first_coding(r.get("code"), prefer="http://snomed.info/sct")
    performed = r.get("performedDateTime") or (r.get("performedPeriod") or {}).get("start")
    rec = ProcedureIn(
        profile_id=profile_id,
        text=text,
        code_system=system,
        code=code,
        performed=parse_date(performed),
        effective_at=parse_datetime(performed),
        sensitivity=sensitivity(r),
        fhir=r,
    )
    return Mapped("record", record=rec, record_kind="procedure", checks=_code_check(code))


_MED_STATUS = {
    "active": "active",
    "intended": "active",
    "completed": "stopped",
    "stopped": "stopped",
    "cancelled": "stopped",
    "on-hold": "on_hold",
}


def medication(r: dict, profile_id, resolve) -> Mapped:
    """MedicationStatement and MedicationRequest; a request becomes a statement with the source status.
    `resolve(reference)` returns a contained or bundled resource (for medicationReference)."""
    status = r.get("status")
    if status == "entered-in-error":
        return Mapped("skip", skip_reason="status_entered-in-error")
    if status not in _MED_STATUS:
        return Mapped("record", unmapped_note=f"{r['resourceType']} with status {status}")
    concept = r.get("medicationCodeableConcept")
    if concept is None and r.get("medicationReference"):
        med = resolve((r["medicationReference"] or {}).get("reference")) or {}
        concept = med.get("code")
    name = concept_text(concept)
    if not name:
        return Mapped("record", unmapped_note=f"{r['resourceType']} without a medication code")
    system, code = first_coding(concept, prefer="http://www.nlm.nih.gov/research/umls/rxnorm")
    if r["resourceType"] == "MedicationStatement":
        period = r.get("effectivePeriod") or {}
        started = parse_date(period.get("start") or r.get("effectiveDateTime"))
        stopped = parse_date(period.get("end"))
        dosage = r.get("dosage") or []
        effective = parse_datetime(r.get("dateAsserted") or period.get("start") or r.get("effectiveDateTime"))
    else:
        validity = (r.get("dispenseRequest") or {}).get("validityPeriod") or {}
        started = parse_date(validity.get("start") or r.get("authoredOn"))
        stopped = parse_date(validity.get("end")) if _MED_STATUS[status] == "stopped" else None
        dosage = r.get("dosageInstruction") or []
        effective = parse_datetime(r.get("authoredOn"))
    dose_text = next((d.get("text") for d in dosage if d.get("text")), None)
    med_status = _MED_STATUS[status]
    if med_status == "stopped" and stopped is None:
        stopped = parse_date(r.get("dateAsserted")) if r["resourceType"] == "MedicationStatement" else None
    rec = MedicationIn(
        profile_id=profile_id,
        name=name,
        code_system=system,
        code=code,
        dose_text=dose_text,
        started=started,
        stopped=stopped,
        med_status=med_status,
        reason_text=next((concept_text(c) for c in r.get("reasonCode") or [] if concept_text(c)), None),
        effective_at=effective,
        sensitivity=sensitivity(r),
        fhir=r,
    )
    return Mapped("record", record=rec, record_kind="medication", checks=_code_check(code))
