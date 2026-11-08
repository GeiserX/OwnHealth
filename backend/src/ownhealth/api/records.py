"""Records and manual entry (lane C): conditions, medications, allergies, vaccines, procedures, and
observations typed by hand. A correction is a new version; the old one stays, superseded.

Reads need `read:records` and write one audit row. Writes need the role action `write` and a browser
session: a personal access token writes only through MCP `propose`, into the review queue.
"""

from __future__ import annotations

from datetime import datetime
from typing import Literal
from uuid import UUID

from fastapi import APIRouter, Depends, HTTPException
from pydantic import BaseModel, Field, ValidationError
from sqlalchemy import Connection

import ownhealth.ingest.fhir  # noqa: F401 - registers the accept handler for held FHIR records
from ownhealth.api._deps import get_conn, get_principal, profile_access
from ownhealth.core import records as rec
from ownhealth.core.access import AccessDenied, Permission, Principal
from ownhealth.core.audit import AuditEvent, write_audit
from ownhealth.core.observations import get_observation
from ownhealth.core.provenance import SrcIn
from ownhealth.ingest.manual import add_manual_observation

router = APIRouter(prefix="/profiles/{p}", tags=["records"])

Kind = Literal["condition", "medication", "allergy", "immunization", "procedure"]
_HIDDEN = {"profile_id", "src_id", "fhir", "supersedes", "superseded_at", "status", "status_reason", "id"}


def title(kind: str, r: dict) -> str:
    if kind == "medication":
        return " ".join(x for x in (r["name"], r.get("strength")) if x)
    return r[{"condition": "text", "allergy": "substance", "immunization": "vaccine", "procedure": "text"}[kind]]


def record_out(kind: str, r: dict) -> dict:
    return {
        "id": r["id"],
        "kind": kind,
        "title": title(kind, r),
        "detail": {k: v for k, v in r.items() if k not in _HIDDEN and k != "recorded_at"},
        "status": r["status"],
        "status_reason": r.get("status_reason"),
        "supersedes": r.get("supersedes"),
        "src": r["src_id"],
        "recorded_at": r["recorded_at"],
    }


def _audit(conn: Connection, principal: Principal, perm: Permission, action: str, rtype: str, ids: list) -> None:
    write_audit(
        conn,
        AuditEvent(
            actor_kind=principal.actor_kind,
            actor_id=principal.user_id,
            token_id=principal.token_id,
            via="api" if principal.kind == "token" else "ui",
            action=action,
            tool="records",
            profile_id=perm.profile.id,
            resource_type=rtype,
            resource_ids=[str(i) for i in ids],
            rows_out=len(ids),
        ),
    )


def _writer(conn: Connection, principal: Principal, p: str) -> Permission:
    if principal.kind == "token":
        raise AccessDenied("forbidden", "Tokens cannot write here; agents propose changes through MCP `propose`.")
    return profile_access(conn, principal, p, action="write")


def _model(kind: str, profile_id: UUID, body: dict) -> rec.RecordIn:
    data = {k: v for k, v in body.items() if k not in ("profile_id", "fhir", "reason")}
    try:
        return rec.MODELS[kind].model_validate({**data, "profile_id": profile_id})
    except ValidationError as exc:
        raise HTTPException(
            422,
            detail={
                "code": "bad_input",
                "message": "Invalid record.",
                "errors": exc.errors(include_url=False, include_input=False, include_context=False),
            },
        ) from None


def _src(principal: Principal, profile_id: UUID) -> SrcIn:
    return SrcIn(
        kind="manual", profile_id=profile_id, actor_kind="user", actor_id=principal.user_id, label="Entered by hand"
    )


def _owned(conn: Connection, kind: str, record_id: UUID, perm: Permission) -> dict:
    row = rec.get_record(conn, kind, record_id)
    if row is None or row["profile_id"] != perm.profile.id:
        raise HTTPException(404, detail={"code": "unknown", "message": "No such record."})
    return row


# ---------------------------------------------------------------- records ----


@router.get("/records/{kind}")
def list_records(
    p: str,
    kind: Kind,
    history: bool = False,
    conn: Connection = Depends(get_conn),
    principal: Principal = Depends(get_principal),
) -> dict:
    """Current records, newest first; `history=true` adds superseded and retracted versions."""
    perm = profile_access(conn, principal, p, scope="read:records")
    rows = rec.list_records(conn, kind, perm.profile.id, include_sensitive=perm.has("read:sensitive"))
    if history:
        seen = {r["id"] for r in rows}
        older = [h for r in rows for h in rec.record_history(conn, kind, r["id"]) if h["id"] not in seen]
        rows += older
    _audit(conn, principal, perm, "read", kind, [r["id"] for r in rows])
    return {"items": [record_out(kind, r) for r in rows]}


@router.post("/records/{kind}", status_code=201)
def add(
    p: str, kind: Kind, body: dict, conn: Connection = Depends(get_conn), principal: Principal = Depends(get_principal)
) -> dict:
    perm = _writer(conn, principal, p)
    committed = rec.add_record(
        conn, _model(kind, perm.profile.id, body), src=_src(principal, perm.profile.id), accepted_by="person"
    )
    _audit(conn, principal, perm, "write", kind, [committed.id])
    return record_out(kind, rec.get_record(conn, kind, committed.id))


@router.post("/records/{kind}/{record_id}/correct", status_code=201)
def correct(
    p: str,
    kind: Kind,
    record_id: UUID,
    body: dict,
    conn: Connection = Depends(get_conn),
    principal: Principal = Depends(get_principal),
) -> dict:
    """Body: the full new version plus `reason`. The old version becomes superseded."""
    perm = _writer(conn, principal, p)
    _owned(conn, kind, record_id, perm)
    committed = rec.supersede_record(
        conn,
        record_id,
        _model(kind, perm.profile.id, body),
        src=_src(principal, perm.profile.id),
        accepted_by="person",
        reason=str(body.get("reason") or "corrected"),
    )
    _audit(conn, principal, perm, "correct", kind, [record_id, committed.id])
    return record_out(kind, rec.get_record(conn, kind, committed.id))


class RetractIn(BaseModel):
    reason: str = Field(min_length=1, max_length=500)


@router.post("/records/{kind}/{record_id}/retract")
def retract(
    p: str,
    kind: Kind,
    record_id: UUID,
    body: RetractIn,
    conn: Connection = Depends(get_conn),
    principal: Principal = Depends(get_principal),
) -> dict:
    perm = _writer(conn, principal, p)
    _owned(conn, kind, record_id, perm)
    rec.retract_record(conn, kind, record_id, body.reason.strip())
    _audit(conn, principal, perm, "retract", kind, [record_id])
    return record_out(kind, rec.get_record(conn, kind, record_id))


# ------------------------------------------------------ manual observations ----


class ManualObservationIn(BaseModel):
    name: str = Field(min_length=1, max_length=200)
    value: str = Field(min_length=1, max_length=100)
    unit: str | None = Field(default=None, max_length=50)
    effective_at: datetime
    note: str | None = Field(default=None, max_length=4000)
    category: Literal["laboratory", "vital-signs", "survey", "exam", "activity"] | None = None
    loinc: str | None = Field(default=None, pattern=r"^\d{1,7}-\d$")
    corrects: UUID | None = None  # the observation this value replaces
    reason: str | None = Field(default=None, max_length=500)


@router.post("/manual-observations", status_code=201)
def manual_observation(
    p: str,
    body: ManualObservationIn,
    conn: Connection = Depends(get_conn),
    principal: Principal = Depends(get_principal),
) -> dict:
    perm = _writer(conn, principal, p)
    if body.effective_at.tzinfo is None:
        raise HTTPException(422, detail={"code": "bad_input", "message": "effective_at needs a UTC offset."})
    if body.corrects is not None:
        old = get_observation(conn, body.corrects)
        if old is None or old["profile_id"] != perm.profile.id:
            raise HTTPException(404, detail={"code": "unknown", "message": "No such observation."})
    committed = add_manual_observation(
        conn,
        profile_id=perm.profile.id,
        user_id=principal.user_id,
        name=body.name.strip(),
        value=body.value.strip(),
        unit=(body.unit or "").strip() or None,
        effective_at=body.effective_at,
        note=body.note,
        category=body.category,
        loinc=body.loinc,
        corrects=body.corrects,
        reason=body.reason,
    )
    _audit(conn, principal, perm, "write", "observation", [committed.id])
    row = get_observation(conn, committed.id)
    return {
        "id": row["id"],
        "src": row["src_id"],
        "status": row["status"],
        "supersedes": row["supersedes"],
        "printed_name": row["printed_name"],
        "printed_value": row["printed_value"],
        "printed_unit": row["printed_unit"],
        "alias_group": row["alias_group"],
        "effective_at": row["effective_at"],
    }
