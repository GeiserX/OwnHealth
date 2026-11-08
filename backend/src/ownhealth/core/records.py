"""Records: conditions, medications (and supplements), allergies, immunizations, procedures.

Same lifecycle as observations: add, supersede with a new version, retract with a reason. The
typed columns are the query source; `fhir` is built from them unless an importer passes the
received resource.
"""

from __future__ import annotations

import json
from datetime import date, datetime
from typing import Literal
from uuid import UUID

from pydantic import BaseModel, Field
from sqlalchemy import Connection, text

from ownhealth.core import fhir, lifecycle
from ownhealth.core.observations import AcceptedBy, Committed, Sensitivity
from ownhealth.core.provenance import SrcIn, ensure_src

RecordKind = Literal["condition", "medication", "allergy", "immunization", "procedure"]
TABLES: dict[str, str] = {
    "condition": "condition",
    "medication": "medication_statement",
    "allergy": "allergy",
    "immunization": "immunization",
    "procedure": "procedure",
}


class _Common(BaseModel):
    profile_id: UUID
    effective_at: datetime | None = None
    sensitivity: list[Sensitivity] = Field(default_factory=list)
    fhir: dict | None = None


class ConditionIn(_Common):
    text: str
    code_system: str | None = None
    code: str | None = None
    clinical_status: str = "active"
    verification_status: Literal[
        "provisional", "confirmed", "refuted", "unconfirmed", "differential", "entered-in-error"
    ] = "provisional"
    onset: date | None = None
    abatement: date | None = None


class MedicationIn(_Common):
    kind: Literal["medication", "supplement"] = "medication"
    name: str
    ingredient: str | None = None
    strength: str | None = None
    dose_text: str | None = None
    code_system: str | None = None
    code: str | None = None
    started: date | None = None
    stopped: date | None = None
    med_status: Literal["active", "stopped", "on_hold"] = "active"
    reason_text: str | None = None


class AllergyIn(_Common):
    substance: str
    code_system: str | None = None
    code: str | None = None
    criticality: Literal["low", "high", "unable-to-assess"] | None = None
    reaction_text: str | None = None
    clinical_status: str = "active"
    verification_status: str = "confirmed"


class ImmunizationIn(_Common):
    vaccine: str
    code_system: str | None = None
    code: str | None = None
    occurred: date | None = None
    dose_number: int | None = None
    lot: str | None = None


class ProcedureIn(_Common):
    text: str
    code_system: str | None = None
    code: str | None = None
    performed: date | None = None


MODELS: dict[str, type[_Common]] = {
    "condition": ConditionIn,
    "medication": MedicationIn,
    "allergy": AllergyIn,
    "immunization": ImmunizationIn,
    "procedure": ProcedureIn,
}

RecordIn = ConditionIn | MedicationIn | AllergyIn | ImmunizationIn | ProcedureIn


def kind_of(record: RecordIn) -> str:
    for kind, model in MODELS.items():
        if type(record) is model:
            return kind
    raise TypeError(f"not a record model: {type(record).__name__}")


def add_record(
    conn: Connection,
    record: RecordIn,
    *,
    src: SrcIn | str,
    accepted_by: AcceptedBy,
    status: Literal["current", "provisional"] = "current",
    supersedes: UUID | None = None,
) -> Committed:
    kind = kind_of(record)
    table = TABLES[kind]
    if isinstance(src, SrcIn) and src.profile_id is None:
        src = src.model_copy(update={"profile_id": record.profile_id})
    src_id = ensure_src(conn, src, record.profile_id)
    data = record.model_dump(exclude={"fhir"})
    resource = record.fhir or fhir.BUILDERS[table](data)
    cols = list(data) + ["src_id", "accepted_by", "status", "supersedes", "fhir"]
    params = {
        **data,
        "src_id": src_id,
        "accepted_by": accepted_by,
        "status": status,
        "supersedes": supersedes,
        "fhir": json.dumps(resource, default=str),
    }
    values = [f"cast(:{c} as jsonb)" if c == "fhir" else f":{c}" for c in cols]
    rid = conn.execute(
        text(f"insert into {table} ({', '.join(cols)}) values ({', '.join(values)}) returning id"), params
    ).scalar_one()
    return Committed(id=rid, src_id=src_id)


def supersede_record(
    conn: Connection, old_id: UUID, new: RecordIn, *, src: SrcIn | str, accepted_by: AcceptedBy, reason: str
) -> Committed:
    """A correction: the new version replaces the old one, which becomes superseded."""
    table = TABLES[kind_of(new)]
    old = lifecycle.lock_current(conn, table, old_id)
    if old["profile_id"] != new.profile_id:
        raise lifecycle.LifecycleError("a correction cannot move a record to another profile")
    lifecycle.mark_superseded(conn, table, old_id, reason)
    return add_record(conn, new, src=src, accepted_by=accepted_by, supersedes=old_id)


def retract_record(conn: Connection, kind: RecordKind, record_id: UUID, reason: str) -> None:
    lifecycle.retract(conn, TABLES[kind], record_id, reason)


def get_record(conn: Connection, kind: RecordKind, record_id: UUID) -> dict | None:
    row = conn.execute(text(f"select * from {TABLES[kind]} where id = :id"), {"id": record_id}).mappings().first()
    return dict(row) if row else None


def record_history(conn: Connection, kind: RecordKind, record_id: UUID) -> list[dict]:
    return lifecycle.history(conn, TABLES[kind], record_id)


def list_records(
    conn: Connection,
    kind: RecordKind,
    profile_id: UUID,
    *,
    as_of: datetime | None = None,
    include_sensitive: bool = True,
) -> list[dict]:
    """Visible records of one kind for a profile (current view, or as of a past moment)."""
    where = ["profile_id = :p", lifecycle.visible_sql()]
    if not include_sensitive:
        where.append("sensitivity = '{}'")
    rows = conn.execute(
        text(f"select * from {TABLES[kind]} where {' and '.join(where)} order by recorded_at desc, id"),
        {"p": profile_id, "as_of": as_of},
    ).mappings()
    return [dict(r) for r in rows]


def active_medications(
    conn: Connection,
    profile_id: UUID,
    *,
    on: date | None = None,
    as_of: datetime | None = None,
    kind: Literal["medication", "supplement"] | None = None,
    include_sensitive: bool = True,
) -> list[dict]:
    """Medications taken on a date: med_status active, or started <= on < stopped."""
    on = on or (as_of.date() if as_of else date.today())
    rows = list_records(conn, "medication", profile_id, as_of=as_of, include_sensitive=include_sensitive)
    out = []
    for r in rows:
        if kind and r["kind"] != kind:
            continue
        started_ok = r["started"] is None or r["started"] <= on
        stopped_ok = r["stopped"] is None or r["stopped"] > on
        if (
            r["med_status"] == "active"
            and started_ok
            and stopped_ok
            or (r["med_status"] != "on_hold" and r["started"] and started_ok and r["stopped"] and stopped_ok)
        ):
            out.append(r)
    return sorted(out, key=lambda r: (r["name"].lower(), str(r["id"])))
