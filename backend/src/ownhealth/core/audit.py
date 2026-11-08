"""Append-only audit log writer. Identifiers only: never values, document text or free-text arguments.

Write the event in the same transaction as the read it records. Free-text arguments go through
`hash_arg` so the log never says what was searched.
"""

from __future__ import annotations

import hashlib
import ipaddress
from typing import Literal
from uuid import UUID

from pydantic import BaseModel, field_validator
from sqlalchemy import Connection, text

ActorKind = Literal["user", "token", "share", "system"]
Via = Literal["ui", "api", "mcp", "job", "share", "export"]


class AuditEvent(BaseModel):
    actor_kind: ActorKind
    actor_id: UUID | None = None
    via: Via
    action: str  # read | search | propose | export | grant_change | login | purge | review | import ...
    token_id: UUID | None = None
    client: str | None = None
    tool: str | None = None
    profile_id: UUID | None = None
    resource_type: str | None = None
    resource_ids: list[str] | None = None
    src_ids: list[str] | None = None
    args_summary: str | None = None
    result_kind: str | None = "ok"
    rows_out: int | None = None
    bytes_out: int | None = None
    withheld: dict | None = None
    protocol_version: str | None = None
    ip: str | None = None
    user_agent: str | None = None

    @field_validator("ip", mode="before")
    @classmethod
    def _ip_or_none(cls, v: object) -> str | None:
        """Keep only a parseable address: the column is inet, and a bad cast would abort the read."""
        try:
            return str(ipaddress.ip_address(str(v).strip())) if v else None
        except ValueError:
            return None


def hash_arg(value: str) -> str:
    """Stable short hash for a free-text argument, so the log keeps no search terms."""
    return "h:" + hashlib.sha256(value.encode()).hexdigest()[:12]


def write_audit(conn: Connection, event: AuditEvent) -> int:
    data = event.model_dump()
    import json

    data["withheld"] = json.dumps(data["withheld"]) if data["withheld"] is not None else None
    row = conn.execute(
        text(
            "insert into audit_event (actor_kind, actor_id, token_id, client, via, action, tool, profile_id,"
            " resource_type, resource_ids, src_ids, args_summary, result_kind, rows_out, bytes_out, withheld,"
            " protocol_version, ip, user_agent) values (:actor_kind, :actor_id, :token_id, :client, :via,"
            " :action, :tool, :profile_id, :resource_type, :resource_ids, :src_ids, :args_summary, :result_kind,"
            " :rows_out, :bytes_out, cast(:withheld as jsonb), :protocol_version, cast(:ip as inet), :user_agent)"
            " returning id"
        ),
        data,
    ).scalar_one()
    return int(row)


def list_audit(
    conn: Connection,
    *,
    profile_ids: list[UUID] | None = None,
    actor_id: UUID | None = None,
    limit: int = 100,
    before_id: int | None = None,
) -> list[dict]:
    """Newest first. Filter by the profiles the caller may see, or by actor."""
    where = ["true"]
    params: dict = {"limit": limit}
    if profile_ids is not None:
        where.append("(profile_id = any(:pids))")
        params["pids"] = profile_ids
    if actor_id is not None:
        where.append("actor_id = :actor")
        params["actor"] = actor_id
    if before_id is not None:
        where.append("id < :before")
        params["before"] = before_id
    rows = conn.execute(
        text(f"select * from audit_event where {' and '.join(where)} order by id desc limit :limit"), params
    ).mappings()
    return [dict(r) for r in rows]
