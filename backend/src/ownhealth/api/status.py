"""Lane E routes: the deterministic status (the same body `get_health_status` serves), the dashboard
page and the installed rule pack."""

from __future__ import annotations

from datetime import date

from fastapi import APIRouter, Depends, Query
from sqlalchemy import Connection

from ownhealth import state
from ownhealth.api._deps import get_conn, get_principal, profile_access
from ownhealth.core.access import Principal, today_utc
from ownhealth.core.audit import AuditEvent, write_audit
from ownhealth.state.pack import cpic_snapshot, load_pack

router = APIRouter(tags=["status"])


def _audit(conn: Connection, principal: Principal, profile_id, tool: str, state_hash: str) -> None:
    write_audit(
        conn,
        AuditEvent(
            actor_kind=principal.actor_kind,
            actor_id=principal.user_id,
            token_id=principal.token_id,
            via="api" if principal.kind == "token" else "ui",
            action="read",
            tool=tool,
            profile_id=profile_id,
            resource_type="state",
            resource_ids=[state_hash],
        ),
    )


@router.get("/profiles/{p}/status")
def get_status(
    p: str,
    as_of: date | None = Query(None),
    lang: str | None = Query(None),
    units: str | None = Query(None),
    identity_mode: str | None = Query(None),
    conn: Connection = Depends(get_conn),
    principal: Principal = Depends(get_principal),
) -> dict:
    perm = profile_access(conn, principal, p, scope="read:summary")
    body = state.build_state(conn, perm.profile.id, as_of=as_of or today_utc())
    _audit(conn, principal, perm.profile.id, "GET /api/profiles/{p}/status", body["state_hash"])
    return state.render(
        body,
        lang=lang or principal.lang,
        units=units or principal.units,
        identity_mode=identity_mode or principal.identity_mode,
        scopes=perm.scopes,
    )


@router.get("/profiles/{p}/dashboard")
def get_dashboard(
    p: str,
    as_of: date | None = Query(None),
    lang: str | None = Query(None),
    conn: Connection = Depends(get_conn),
    principal: Principal = Depends(get_principal),
) -> dict:
    perm = profile_access(conn, principal, p, scope="read:summary")
    out = state.dashboard(
        conn, perm.profile.id, as_of=as_of or today_utc(), lang=lang or principal.lang, scopes=perm.scopes
    )
    _audit(conn, principal, perm.profile.id, "GET /api/profiles/{p}/dashboard", out["state_hash"])
    return out


@router.get("/rules")
def get_rules(principal: Principal = Depends(get_principal)) -> dict:
    """The installed pack: version, hash and every rule with its citation. No personal data."""
    pack = load_pack()
    keep = ("id", "kind", "file", "system", "match", "window_days", "cite", "basis", "note", "verify")
    return {
        "version": pack.version,
        "sha256": pack.sha256,
        "systems": [{"key": s.key, "file": s.file, "name": s.names, "source": s.source} for s in pack.systems],
        "rules": [{k: r[k] for k in keep if k in r} for r in pack.rules],
        "cpic": {k: cpic_snapshot()[k] for k in ("source", "license", "retrieved")},
    }
