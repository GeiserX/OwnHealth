"""Lane E route: check one drug for one profile (the same data block as the MCP `check_drug` tool)."""

from __future__ import annotations

from fastapi import APIRouter, Depends, HTTPException, Query
from sqlalchemy import Connection

from ownhealth import state
from ownhealth.api._deps import get_conn, get_principal, profile_access
from ownhealth.core.access import Principal
from ownhealth.core.audit import AuditEvent, hash_arg, write_audit

router = APIRouter(tags=["drug-check"])
SCOPES = ("read:genome", "read:records", "read:labs")


@router.get("/profiles/{p}/drug-check")
def drug_check(
    p: str,
    drug: str = Query(..., min_length=1, max_length=80),
    lang: str | None = Query(None),
    conn: Connection = Depends(get_conn),
    principal: Principal = Depends(get_principal),
) -> dict:
    perm = profile_access(conn, principal, p)
    if not any(s in perm.scopes for s in SCOPES):
        raise HTTPException(
            403, detail={"code": "forbidden", "message": "This needs read:genome, read:records or read:labs."}
        )
    try:
        out = state.check_drug(conn, perm.profile.id, drug, scopes=perm.scopes, lang=lang or principal.lang)
    except state.UnknownDrug:
        raise HTTPException(
            404, detail={"code": "drug_unknown", "message": "This drug name is not in the drug table."}
        ) from None
    srcs = sorted(
        {x["src"] for part in ("pgx", "current_medications_relevant", "relevant_labs") for x in out[part] if x["src"]}
    )
    write_audit(
        conn,
        AuditEvent(
            actor_kind=principal.actor_kind,
            actor_id=principal.user_id,
            token_id=principal.token_id,
            via="api" if principal.kind == "token" else "ui",
            action="read",
            tool="GET /api/profiles/{p}/drug-check",
            profile_id=perm.profile.id,
            resource_type="drug_check",
            resource_ids=[out["drug"]["cpic_drug_id"] or out["drug"]["name"]],
            src_ids=srcs,
            args_summary=hash_arg(drug),
            withheld={"by_scope": out["withheld"]["by_scope"]},
        ),
    )
    return out
