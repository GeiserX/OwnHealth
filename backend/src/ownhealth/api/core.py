"""Foundation routes: who am I, which profiles can I see, where did a value come from."""

from __future__ import annotations

from datetime import date

from fastapi import APIRouter, Depends, HTTPException, Query
from sqlalchemy import Connection, text

from ownhealth.api._deps import get_conn, get_principal, get_settings
from ownhealth.config import Settings
from ownhealth.core import review
from ownhealth.core.access import (
    AccessDenied,
    Permission,
    Principal,
    accessible_profiles,
    effective_permission,
)
from ownhealth.core.audit import AuditEvent, write_audit
from ownhealth.core.documents import document_is_sensitive
from ownhealth.core.provenance import resolve_src
from ownhealth.mcp import gate as gate_mod

router = APIRouter(tags=["core"])


def profile_summary(perm: Permission, principal: Principal, on: date, age_of_majority: int) -> dict:
    p = perm.profile
    parts = [w for w in p.display_name.split() if w]
    initials = "".join(w[0] for w in parts[:2]).upper()
    return {
        "id": p.id,
        "slug": p.slug,
        "display_name": p.display_name,
        "initials": initials,
        "role": perm.role,
        "relationship": p.relationship,
        "age": p.age_on(on),
        "sex_at_birth": p.sex_at_birth,
        "country": p.country,
        "is_minor": p.is_minor(on, age_of_majority),
        "is_self": p.user_id is not None and p.user_id == principal.user_id,
        "unit_pref": p.unit_pref,
        "genome_sensitive_ui": p.genome_sensitive_ui,
        "scopes": sorted(perm.scopes),
    }


_RELATIONSHIP_ORDER = {"self": 0, "partner": 1, "child": 2, "parent": 3}


def summaries(perms: list[Permission], principal: Principal, settings: Settings) -> list[dict]:
    """The caller first, then the household as the mockup lists it: partner, children, parents, others."""
    today = date.today()
    rows = [profile_summary(p, principal, today, settings.age_of_majority) for p in perms]
    rows.sort(key=lambda r: (not r["is_self"], _RELATIONSHIP_ORDER.get(r["relationship"], 4), r["display_name"]))
    return rows


@router.get("/me")
def me(
    conn: Connection = Depends(get_conn),
    principal: Principal = Depends(get_principal),
    settings: Settings = Depends(get_settings),
) -> dict:
    user = (
        conn.execute(
            text(
                "select u.id, u.display_name, u.locale, h.id as household_id, h.name as household_name"
                " from app_user u join household h on h.id = u.household_id where u.id = :u"
            ),
            {"u": principal.user_id},
        )
        .mappings()
        .first()
    )
    perms = accessible_profiles(conn, principal)
    return {
        "user": {"id": user["id"], "display_name": user["display_name"], "locale": user["locale"]},
        "household": {"id": user["household_id"], "name": user["household_name"]},
        "auth": {"kind": principal.kind},
        "profiles": summaries(perms, principal, settings),
        "review_open": review.count_open(
            conn, household_id=user["household_id"], profile_ids=[p.profile.id for p in perms]
        ),
        "demo": settings.demo,
    }


@router.get("/profiles")
def profiles(
    conn: Connection = Depends(get_conn),
    principal: Principal = Depends(get_principal),
    settings: Settings = Depends(get_settings),
) -> list[dict]:
    return summaries(accessible_profiles(conn, principal), principal, settings)


@router.get("/sources/{src_id}")
def source(
    src_id: str,
    conn: Connection = Depends(get_conn),
    principal: Principal = Depends(get_principal),
    settings: Settings = Depends(get_settings),
    include_text: bool = Query(False),
) -> dict:
    """Provenance of one value: document, page, bbox, extraction, and the facts that cite it.

    An id on a profile the caller cannot read answers 404, the same as an id that never existed.
    """
    resolved = resolve_src(conn, src_id)
    if resolved is None or resolved["profile_id"] is None:
        raise HTTPException(404, detail={"code": "unknown", "message": "Unknown source."})
    try:
        perm = effective_permission(conn, principal, resolved["profile_id"])
    except AccessDenied:
        raise HTTPException(404, detail={"code": "unknown", "message": "Unknown source."}) from None
    # the scope of the fact behind the source decides, as in MCP get_source (mcp.md 5.7)
    unknown = HTTPException(404, detail={"code": "unknown", "message": "Unknown source."})
    if not any(perm.has(sc) for sc in gate_mod.READ_SCOPES):
        raise unknown
    gate = gate_mod.genome_gate(perm, date.today(), settings.age_of_majority)
    hidden = gate_mod.hidden_reasons(conn, perm, gate, [src_id]).get(src_id)
    if hidden:
        if hidden[0] == "scope" and hidden[1] != "genome":
            raise AccessDenied("insufficient_scope", f"This needs read:{hidden[1]}.")
        raise unknown
    s = resolved["src"]
    status = resolved["facts"][0]["status"] if resolved["facts"] else "gone"
    page_text = None
    if (
        include_text
        and s["document_id"]
        and s["page_no"]
        and "read:documents_text" in perm.scopes
        and (perm.has("read:sensitive") or not document_is_sensitive(conn, s["document_id"]))
    ):
        page_text = conn.execute(
            text("select text_content from document_page where document_id = :d and page_no = :p"),
            {"d": s["document_id"], "p": s["page_no"]},
        ).scalar()
    write_audit(
        conn,
        AuditEvent(
            actor_kind=principal.actor_kind,
            actor_id=principal.user_id,
            token_id=principal.token_id,
            via="ui",
            action="read",
            tool="GET /api/sources",
            profile_id=resolved["profile_id"],
            resource_type="src",
            src_ids=[src_id],
        ),
    )
    return {
        "src_id": src_id,
        "kind": s["kind"],
        "label": s["label"],
        "status": status,
        "facts": [
            {"table": f["table"], "id": f["id"], "status": f["status"], "status_reason": f["status_reason"]}
            for f in resolved["facts"]
        ],
        "document": resolved["document"],
        "location": {"page": s["page_no"], "bbox": s["bbox"], "units": "pt"} if s["page_no"] else None,
        "import_ref": s["import_ref"],
        "created_at": s["created_at"],
        "text": {"trust": "untrusted_text", "origin": "pdf_text_layer", "text": page_text} if page_text else None,
    }
