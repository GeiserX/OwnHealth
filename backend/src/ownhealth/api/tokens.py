"""Personal access tokens for MCP clients: list, create (shown once, step-up), revoke."""

from __future__ import annotations

from typing import Literal
from uuid import UUID

from fastapi import APIRouter, Depends, HTTPException, Request
from pydantic import BaseModel, Field
from sqlalchemy import Connection, text

from ownhealth.api._deps import get_conn
from ownhealth.auth import tokens
from ownhealth.auth.deps import SessionOnly, StepUp, client_ip
from ownhealth.core.access import Principal, require
from ownhealth.core.audit import AuditEvent, write_audit

router = APIRouter(tags=["tokens"])

LIMIT_KEYS = (
    "rate_per_min",
    "egress_facts_per_day",
    "egress_bytes_per_day",
    "genome_raw_lookups_per_day",
    "document_pages_per_day",
    "proposals_per_day",
)


def _slugs(conn: Connection, ids: list[UUID]) -> dict[UUID, str]:
    rows = conn.execute(text("select id, slug from profile where id = any(:i)"), {"i": list(ids)}).all()
    return {r.id: r.slug for r in rows}


def token_out(row: dict, slugs: dict[UUID, str]) -> dict:
    return {
        "id": row["id"],
        "label": row["label"],
        "prefix": row["prefix"],
        "scopes": sorted(row["scopes"]),
        "profiles": [slugs[p] for p in row["profile_ids"] if p in slugs],
        "default_profile": slugs.get(row["default_profile"]),
        "identity_mode": row["identity_mode"],
        "lang": row["lang"],
        "units": row["units"],
        "limits": {k: row[k] for k in LIMIT_KEYS},
        "created_at": row["created_at"],
        "expires_at": row["expires_at"],
        "last_used_at": row["last_used_at"],
        "revoked_at": row["revoked_at"],
    }


def _list(conn: Connection, user_id: UUID) -> list[dict]:
    rows = tokens.list_tokens(conn, user_id)
    slugs = _slugs(conn, list({p for r in rows for p in r["profile_ids"]}))
    return [token_out(r, slugs) for r in rows]


@router.get("/tokens")
def list_my_tokens(conn: Connection = Depends(get_conn), principal: Principal = SessionOnly) -> dict:
    return {"items": _list(conn, principal.user_id)}


class TokenIn(BaseModel):
    label: str = Field(min_length=1, max_length=80)
    scopes: list[str] = Field(default_factory=lambda: list(tokens.PAT_DEFAULT_SCOPES), min_length=1)
    profiles: list[str] = Field(min_length=1, max_length=20)
    default_profile: str | None = None
    days: int = Field(tokens.DEFAULT_DAYS, ge=1, le=tokens.MAX_DAYS)
    identity_mode: Literal["full", "pseudonymous", "minimal"] = "full"
    lang: Literal["es", "en"] | None = None
    units: Literal["si", "conventional", "as_reported"] = "si"


@router.post("/tokens", status_code=201)
def create_my_token(
    body: TokenIn, request: Request, conn: Connection = Depends(get_conn), principal: Principal = StepUp
) -> dict:
    """The plaintext is in this response and nowhere else, ever."""
    ids = [require(conn, principal, ref).profile.id for ref in body.profiles]
    default = require(conn, principal, body.default_profile).profile.id if body.default_profile else None
    try:
        new = tokens.create_token(
            conn,
            user_id=principal.user_id,
            label=body.label.strip(),
            profile_ids=list(dict.fromkeys(ids)),
            scopes=body.scopes,
            default_profile=default,
            days=body.days,
            identity_mode=body.identity_mode,
            lang=body.lang or principal.lang,
            units=body.units,
        )
    except tokens.TokenError as exc:
        raise HTTPException(422, detail={"code": "bad_input", "message": str(exc)}) from exc
    write_audit(
        conn,
        AuditEvent(
            actor_kind="user",
            actor_id=principal.user_id,
            token_id=new.id,
            via="ui",
            action="token_create",
            tool="POST /api/tokens",
            resource_type="token",
            resource_ids=[str(new.id)],
            args_summary="scopes=" + ",".join(sorted(set(body.scopes))) + f";profiles={len(ids)};days={body.days}",
            ip=client_ip(request),
        ),
    )
    token = next(t for t in _list(conn, principal.user_id) if t["id"] == new.id)
    return {"token": token, "plaintext": new.plaintext}


@router.delete("/tokens/{token_id}")
def revoke_my_token(token_id: UUID, conn: Connection = Depends(get_conn), principal: Principal = SessionOnly) -> dict:
    """Revoking only takes access away, so it needs no step-up."""
    if not tokens.revoke_token(conn, token_id, principal.user_id):
        raise HTTPException(404, detail={"code": "unknown", "message": "Unknown or already revoked token."})
    write_audit(
        conn,
        AuditEvent(
            actor_kind="user",
            actor_id=principal.user_id,
            token_id=token_id,
            via="ui",
            action="token_revoke",
            tool="DELETE /api/tokens",
            resource_type="token",
            resource_ids=[str(token_id)],
        ),
    )
    return {"id": token_id, "revoked": True}
