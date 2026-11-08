"""Genome routes (lane D): the genome page, one variant lookup, and the two consent switches.

    GET  /api/profiles/{p}/genome?min_evidence=E3      read:genome
    GET  /api/profiles/{p}/genome/variant?rsid=|hgvs=|region=&cursor=   read:genome_raw
    POST /api/profiles/{p}/genome/consent {ui?, agents?}   owner (or guardian of a minor), step-up

A browser session is gated by the profile's UI switch, a token by the agents switch (mcp.md 9.7).
Every read writes one audit row with identifiers only, never a genotype.
"""

from __future__ import annotations

from datetime import date

from fastapi import APIRouter, Depends, HTTPException, Query
from fastapi.responses import JSONResponse
from pydantic import BaseModel
from sqlalchemy import Connection

from ownhealth.api._deps import get_blobs, get_conn, get_principal, get_settings, profile_access
from ownhealth.auth import sessions
from ownhealth.blobs import BlobStore
from ownhealth.config import Settings
from ownhealth.core.access import AccessDenied, Permission, Principal, age_of_majority, set_genome_switches
from ownhealth.core.audit import AuditEvent, write_audit
from ownhealth.genome import EVIDENCE, GenomeQueryError, gate_for, list_findings, lookup_variant
from ownhealth.genome.findings import visible_rows, withheld_reason
from ownhealth.genome.model import NOTICE

router = APIRouter(tags=["genome"])

TILE_CATEGORIES = ("pgx", "carrier", "hla", "risk_marker", "trait", "prs")


def _surface(principal: Principal) -> str:
    return "agents" if principal.kind == "token" else "ui"


def _gate(conn: Connection, perm: Permission, principal: Principal, settings: Settings):
    aom = age_of_majority(conn, perm.profile.household_id, settings.age_of_majority)
    return gate_for(perm, surface=_surface(principal), on=date.today(), age_of_majority=aom)


def _audit(conn: Connection, principal: Principal, perm: Permission, *, action: str, tool: str, **kw) -> None:
    write_audit(
        conn,
        AuditEvent(
            actor_kind=principal.actor_kind,
            actor_id=principal.user_id,
            token_id=principal.token_id,
            client=principal.token_label,
            via="api" if principal.kind == "token" else "ui",
            action=action,
            tool=tool,
            profile_id=perm.profile.id,
            **kw,
        ),
    )


def _tiles(findings: list[dict], locked: dict[str, int]) -> list[dict]:
    tiles = []
    for cat in TILE_CATEGORIES:
        rows = [f for f in findings if f["category"] == cat]
        hidden = locked.get(cat, 0)
        if not rows and not hidden:
            continue
        actionable = sum(1 for f in rows if f["phenotype"] and not f["phenotype"].lower().startswith("normal"))
        genes = ", ".join(dict.fromkeys(f["gene"] or f["title"] for f in rows))[:120]
        detail = genes
        if cat == "pgx" and actionable:
            detail = f"{actionable} actionable · {genes}"
        if hidden:
            detail = f"{detail} · {hidden} hidden" if detail else f"{hidden} hidden"
        tiles.append({"key": cat, "value": str(len(rows)), "detail": detail})
    return tiles


@router.get("/profiles/{p}/genome")
def genome_overview(
    p: str,
    min_evidence: str = Query("E3"),
    lang: str | None = Query(None),
    conn: Connection = Depends(get_conn),
    principal: Principal = Depends(get_principal),
    settings: Settings = Depends(get_settings),
) -> dict:
    if min_evidence not in EVIDENCE:
        raise HTTPException(400, detail={"code": "bad_input", "message": f"min_evidence is one of {list(EVIDENCE)}"})
    perm = profile_access(conn, principal, p, scope="read:genome")
    gate = _gate(conn, perm, principal, settings)
    found = list_findings(conn, perm.profile.id, gate=gate, min_evidence=min_evidence)
    locked_rows = []
    if _surface(principal) == "ui" and not gate.minor:
        locked_rows = [
            {"category": r["category"], "gene": r["gene"]}
            for r in visible_rows(conn, perm.profile.id)
            if r["status"] == "current" and withheld_reason(r, gate) == "sensitive"
        ]
    locked_by_cat: dict[str, int] = {}
    for r in locked_rows:
        locked_by_cat[r["category"]] = locked_by_cat.get(r["category"], 0) + 1
    assay = found.assay
    p_ = perm.profile
    can_consent = principal.kind != "token" and (perm.role == "owner" or (perm.role == "guardian" and gate.minor))
    _audit(
        conn,
        principal,
        perm,
        action="read",
        tool="GET /api/profiles/{p}/genome",
        resource_type="genome_finding",
        resource_ids=[f["id"] for f in found.findings],
        src_ids=[f["src"] for f in found.findings],
        rows_out=len(found.findings),
        withheld={"sensitive": found.withheld_sensitive, "minor_rule": found.withheld_minor},
    )
    return {
        "dataset": None
        if assay is None
        else {
            "assay": assay["type"],
            "platform": assay["platform"],
            "mean_depth": assay["mean_depth"],
            "build": assay["build"],
            "source_tool": assay["source_tool"],
            "generated_utc": assay["generated_utc"],
            "imported_at": assay["imported_at"],
            "db_versions": found.db_versions,
            "raw_files": assay["raw_files"],
            "src": assay["src"],
        },
        "tiles": _tiles(found.findings, locked_by_cat),
        "findings": found.findings,
        "coverage": found.coverage,
        "not_assessed": found.not_assessed,
        "withheld": {"sensitive": found.withheld_sensitive, "minor_rule": found.withheld_minor},
        "locked": locked_rows,
        "consent": {"ui": p_.genome_sensitive_ui, "agents": p_.genome_sensitive_agents, "can_change": can_consent},
        "is_minor": gate.minor,
        "limitations": found.limitations,
        "notice": NOTICE,
    }


@router.get("/profiles/{p}/genome/variant", response_model=None)
def genome_variant(
    p: str,
    rsid: str | None = Query(None),
    hgvs: str | None = Query(None),
    region: str | None = Query(None),
    cursor: str | None = Query(None),
    conn: Connection = Depends(get_conn),
    blobs: BlobStore = Depends(get_blobs),
    principal: Principal = Depends(get_principal),
    settings: Settings = Depends(get_settings),
) -> dict | JSONResponse:
    perm = profile_access(conn, principal, p, scope="read:genome_raw")
    gate = _gate(conn, perm, principal, settings)
    query = rsid or hgvs or region or ""
    try:
        out = lookup_variant(
            conn, blobs, perm.profile.id, gate=gate, rsid=rsid, hgvs=hgvs, region=region, cursor=cursor
        )
    except GenomeQueryError as exc:
        _audit(
            conn,
            principal,
            perm,
            action="lookup",
            tool="GET /api/profiles/{p}/genome/variant",
            resource_type="genome_position",
            args_summary=query[:120],
            result_kind=exc.code,
        )
        # a response, not an exception, so the audit row above commits with the request
        return JSONResponse(status_code=404 if exc.code == "no_genome" else 400, content={"detail": exc.as_dict()})
    _audit(
        conn,
        principal,
        perm,
        action="lookup",
        tool="GET /api/profiles/{p}/genome/variant",
        resource_type="genome_position",
        resource_ids=out.get("cited_by_findings") or [],
        args_summary=query[:120],
        result_kind=out.get("withheld") or "ok",
        rows_out=len(out.get("calls") or out.get("records") or []),
    )
    return out | {"notice": NOTICE}


class ConsentIn(BaseModel):
    ui: bool | None = None
    agents: bool | None = None


@router.post("/profiles/{p}/genome/consent")
def genome_consent(
    p: str,
    body: ConsentIn,
    conn: Connection = Depends(get_conn),
    principal: Principal = Depends(get_principal),
    settings: Settings = Depends(get_settings),
) -> dict:
    """Turn the sensitive-findings switches on or off. Only the owner, or a guardian of a minor (whose
    sensitive findings stay withheld by the minor rule anyway); never a token; needs a fresh step-up."""
    if principal.kind == "token":
        raise AccessDenied("forbidden", "Consent changes only in the app, never with a token.")
    perm = profile_access(conn, principal, p)
    aom = age_of_majority(conn, perm.profile.household_id, settings.age_of_majority)
    minor = perm.profile.is_minor(date.today(), aom)
    if not (perm.role == "owner" or (perm.role == "guardian" and minor)):
        raise AccessDenied("forbidden", "Only the profile's owner changes its genome consent.")
    sessions.require_step_up(conn, principal)
    set_genome_switches(conn, perm.profile.id, ui=body.ui, agents=body.agents)
    _audit(
        conn,
        principal,
        perm,
        action="consent_change",
        tool="POST /api/profiles/{p}/genome/consent",
        resource_type="profile",
        resource_ids=[str(perm.profile.id)],
        args_summary=f"ui={body.ui} agents={body.agents}",
    )
    ui = perm.profile.genome_sensitive_ui if body.ui is None else body.ui
    agents = perm.profile.genome_sensitive_agents if body.agents is None else body.agents
    return {"ui": ui, "agents": agents}
