"""The audit log viewer: who (person or token) read or changed what, never the payloads.

A person sees every event on a profile they manage (owner, or guardian of an unclaimed profile),
and their own actions everywhere, including what their tokens did. A viewer of someone else's
profile does not see who else read it.
"""

from __future__ import annotations

from fastapi import APIRouter, Depends, Query
from sqlalchemy import Connection, text

from ownhealth.api._deps import get_conn
from ownhealth.auth.deps import SessionOnly
from ownhealth.auth.household import can_manage
from ownhealth.core.access import Principal, accessible_profiles, require

router = APIRouter(tags=["audit"])


@router.get("/audit")
def audit_log(
    profile: str | None = None,
    limit: int = Query(100, ge=1, le=500),
    before: int | None = Query(None, ge=1),
    conn: Connection = Depends(get_conn),
    principal: Principal = SessionOnly,
) -> dict:
    managed = [p.profile.id for p in accessible_profiles(conn, principal) if can_manage(p)]
    where = ["(e.profile_id = any(:managed) or e.actor_id = :me)"]
    params: dict = {"managed": managed, "me": principal.user_id, "limit": limit}
    if profile:
        where.append("e.profile_id = :pid")
        params["pid"] = require(conn, principal, profile).profile.id
    if before:
        where.append("e.id < :before")
        params["before"] = before
    rows = conn.execute(
        text(
            "select e.id, e.ts, e.actor_kind, e.via, e.action, e.tool, e.resource_type, e.resource_ids, e.src_ids,"
            " e.args_summary, e.rows_out, e.bytes_out, e.result_kind, e.withheld, u.display_name as user_name,"
            " t.label as token_label, p.slug as profile_slug"
            " from audit_event e left join app_user u on u.id = e.actor_id"
            " left join api_token t on t.id = e.token_id left join profile p on p.id = e.profile_id"
            f" where {' and '.join(where)} order by e.id desc limit :limit"
        ),
        params,
    ).mappings()
    items = []
    for r in rows:
        if r["actor_kind"] == "token":
            actor = f"{r['token_label'] or 'deleted token'} (token)"
        elif r["actor_kind"] == "system":
            actor = "OwnHealth"
        else:
            actor = r["user_name"] or "unknown"
        items.append(
            {
                "id": r["id"],
                "ts": r["ts"],
                "actor": actor,
                "actor_kind": r["actor_kind"],
                "via": r["via"],
                "action": r["action"],
                "tool": r["tool"],
                "profile": r["profile_slug"],
                "resource_type": r["resource_type"],
                "resource_ids": r["resource_ids"] or [],
                "src_ids": r["src_ids"] or [],
                "args_summary": r["args_summary"],
                "rows_out": r["rows_out"],
                "bytes_out": r["bytes_out"],
                "result_kind": r["result_kind"],
                "withheld": r["withheld"],
            }
        )
    return {"items": items, "next_before": items[-1]["id"] if len(items) == limit else None}
