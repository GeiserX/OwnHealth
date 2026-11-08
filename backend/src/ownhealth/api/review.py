"""Review queue routes (foundation). The split view UI (lane I) uses these."""

from __future__ import annotations

from uuid import UUID

from fastapi import APIRouter, Depends, HTTPException, Query
from pydantic import BaseModel
from sqlalchemy import Connection

from ownhealth.api._deps import get_conn, get_principal
from ownhealth.core import review
from ownhealth.core.access import (
    AccessDenied,
    Permission,
    Principal,
    accessible_profiles,
    effective_permission,
    require,
)
from ownhealth.core.audit import AuditEvent, write_audit

router = APIRouter(prefix="/review", tags=["review"])


class AcceptIn(BaseModel):
    edits: dict | None = None


class RejectIn(BaseModel):
    reason: str


class BulkAcceptIn(BaseModel):
    document_id: UUID
    min_row_conf: float = 0.0


def _item_out(item: dict) -> dict:
    return {
        k: item[k]
        for k in (
            "id",
            "kind",
            "profile_id",
            "status",
            "reasons",
            "document_id",
            "candidate_row_id",
            "observation_id",
            "payload",
            "row_conf",
            "proposed_by_token",
            "context_src_ids",
            "decided_by",
            "decided_at",
            "decision_reason",
            "result_ids",
            "created_at",
        )
    }


OBS_SCOPE = {
    "laboratory": "read:labs",
    "vital-signs": "read:vitals",
    "survey": "read:vitals",
    "exam": "read:vitals",
    "activity": "read:wearables",
}


def _visible(item: dict, perm: Permission) -> bool:
    """A held payload carries the values it would write: show it only to someone whose scopes would
    show the committed fact (its category scope, and read:sensitive for a sensitive one)."""
    p = item.get("payload") or {}
    observations = [p.get("observation")] + [r.get("observation") for r in p.get("rows") or [] if isinstance(r, dict)]
    observations = [o for o in observations if isinstance(o, dict)]
    record = p.get("record") if isinstance(p.get("record"), dict) else None
    for fact in [*observations, *([record] if record else [])]:
        if fact.get("sensitivity") and not perm.has("read:sensitive"):
            return False
    if record is not None and not perm.has("read:records"):
        return False
    return all(perm.has(OBS_SCOPE.get(o.get("category") or "laboratory", "read:labs")) for o in observations)


def _check(conn: Connection, principal: Principal, item: dict | None) -> dict:
    if item is None:
        raise HTTPException(404, detail={"code": "not_found", "message": "No such review item."})
    if item["profile_id"] is None:
        if item["household_id"] != principal.household_id:
            raise HTTPException(404, detail={"code": "not_found", "message": "No such review item."})
        return item
    try:
        perm = effective_permission(conn, principal, item["profile_id"])
    except AccessDenied:
        raise HTTPException(404, detail={"code": "not_found", "message": "No such review item."}) from None
    if not perm.can("review"):
        raise AccessDenied("forbidden", f"The role {perm.role} cannot review on this profile.")
    if not _visible(item, perm):
        raise HTTPException(404, detail={"code": "not_found", "message": "No such review item."})
    return item


def _audit(conn: Connection, principal: Principal, item: dict, action: str) -> None:
    write_audit(
        conn,
        AuditEvent(
            actor_kind=principal.actor_kind,
            actor_id=principal.user_id,
            via="ui",
            action=action,
            tool="review",
            profile_id=item["profile_id"],
            resource_type="review_item",
            resource_ids=[str(item["id"])],
        ),
    )


@router.get("")
def list_items(
    status: str = Query("open"),
    profile: str | None = None,
    kind: str | None = None,
    document_id: UUID | None = None,
    limit: int = Query(100, le=500),
    offset: int = 0,
    conn: Connection = Depends(get_conn),
    principal: Principal = Depends(get_principal),
) -> dict:
    perms = accessible_profiles(conn, principal)
    pids = [p.profile.id for p in perms if p.can("review")]
    if profile:
        pids = [p.profile.id for p in perms if p.profile.slug == profile and p.can("review")]
    items = review.list_items(
        conn,
        household_id=principal.household_id,
        profile_ids=pids,
        status=status or None,
        kind=kind,
        document_id=document_id,
        limit=limit,
        offset=offset,
    )
    by_profile = {p.profile.id: p for p in perms}
    shown = [i for i in items if i["profile_id"] is None or _visible(i, by_profile[i["profile_id"]])]
    return {
        "items": [_item_out(i) for i in shown],
        "open": review.count_open(conn, household_id=principal.household_id, profile_ids=pids),
        "withheld": len(items) - len(shown),
    }


@router.get("/{item_id}")
def get_item(item_id: UUID, conn: Connection = Depends(get_conn), principal: Principal = Depends(get_principal)):
    return _item_out(_check(conn, principal, review.get_item(conn, item_id)))


@router.post("/{item_id}/accept")
def accept(
    item_id: UUID, body: AcceptIn, conn: Connection = Depends(get_conn), principal: Principal = Depends(get_principal)
) -> dict:
    item = _check(conn, principal, review.get_item(conn, item_id))
    if item["kind"] == "document_identity":
        # accepting moves the document: the target needs the same check as POST /documents/{id}/assign
        target = (body.edits or {}).get("profile_id") or (item["payload"] or {}).get("suggested_profile_id")
        if target:
            require(conn, principal, str(target), action="import")
    created = review.accept(conn, item_id, principal.user_id, body.edits)
    _audit(conn, principal, item, "review_accept")
    return {"id": item_id, "status": "accepted", "result_ids": created}


@router.post("/{item_id}/reject")
def reject(
    item_id: UUID, body: RejectIn, conn: Connection = Depends(get_conn), principal: Principal = Depends(get_principal)
) -> dict:
    item = _check(conn, principal, review.get_item(conn, item_id))
    review.reject(conn, item_id, principal.user_id, body.reason)
    _audit(conn, principal, item, "review_reject")
    return {"id": item_id, "status": "rejected"}


@router.post("/{item_id}/reopen")
def reopen(item_id: UUID, conn: Connection = Depends(get_conn), principal: Principal = Depends(get_principal)):
    item = _check(conn, principal, review.get_item(conn, item_id))
    review.reopen(conn, item_id, principal.user_id)
    _audit(conn, principal, item, "review_reopen")
    return {"id": item_id, "status": "open"}


@router.post("/{item_id}/dismiss")
def dismiss(
    item_id: UUID,
    body: RejectIn | None = None,
    conn: Connection = Depends(get_conn),
    principal: Principal = Depends(get_principal),
) -> dict:
    item = _check(conn, principal, review.get_item(conn, item_id))
    review.dismiss(conn, item_id, principal.user_id, body.reason if body else None)
    _audit(conn, principal, item, "review_dismiss")
    return {"id": item_id, "status": "dismissed"}


@router.post("/bulk-accept")
def bulk_accept(
    body: BulkAcceptIn, conn: Connection = Depends(get_conn), principal: Principal = Depends(get_principal)
) -> dict:
    from sqlalchemy import text

    doc = (
        conn.execute(text("select profile_id, household_id from document where id = :d"), {"d": body.document_id})
        .mappings()
        .first()
    )
    if doc is None or doc["profile_id"] is None:
        raise HTTPException(404, detail={"code": "not_found", "message": "No such document."})
    perm = effective_permission(conn, principal, doc["profile_id"])
    if not perm.can("review"):
        raise AccessDenied("forbidden", f"The role {perm.role} cannot review on this profile.")
    accepted = review.bulk_accept(
        conn, document_id=body.document_id, user_id=principal.user_id, min_row_conf=body.min_row_conf
    )
    write_audit(
        conn,
        AuditEvent(
            actor_kind=principal.actor_kind,
            actor_id=principal.user_id,
            via="ui",
            action="review_bulk_accept",
            tool="review",
            profile_id=doc["profile_id"],
            resource_type="document",
            resource_ids=[str(body.document_id)],
            rows_out=len(accepted),
        ),
    )
    return {"accepted": accepted}
