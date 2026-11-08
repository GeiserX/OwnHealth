"""Lane F: what one MCP token used today and over the last 30 days, against its limits (mcp.md 9.4).
Only the token's owner sees it; another user's token id answers 404."""

from __future__ import annotations

from uuid import UUID

from fastapi import APIRouter, Depends, HTTPException
from sqlalchemy import Connection, text

from ownhealth.api._deps import get_conn, get_principal
from ownhealth.core.access import Principal
from ownhealth.mcp import limits
from ownhealth.mcp.tools import tz

router = APIRouter(tags=["mcp"])


@router.get("/tokens/{token_id}/usage")
def token_usage(token_id: UUID, principal: Principal = Depends(get_principal), conn: Connection = Depends(get_conn)):
    row = (
        conn.execute(
            text(
                "select id, rate_per_min, egress_facts_per_day, egress_bytes_per_day, genome_raw_lookups_per_day,"
                " proposals_per_day from api_token where id = :id and user_id = :u"
            ),
            {"id": token_id, "u": principal.user_id},
        )
        .mappings()
        .first()
    )
    if row is None or principal.kind == "token":
        raise HTTPException(status_code=404, detail={"code": "unknown", "message": "No such token."})
    day = limits.today(tz())
    used = limits.usage(conn, token_id, day)
    return {
        "token_id": str(token_id),
        "day": day.isoformat(),
        "today": {
            "calls": used.calls,
            "facts": used.facts,
            "bytes": used.bytes,
            "lookups": used.lookups,
            "proposals": used.proposals,
        },
        "limits": {k: row[k] for k in limits.DEFAULTS},
        "history": [{**h, "day": h["day"].isoformat()} for h in limits.history(conn, token_id)],
    }
