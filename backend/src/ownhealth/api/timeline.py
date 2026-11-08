"""Timeline (lane H): one dated list of what happened, built from the facts already in the store.

Lab draws (one event per report, not per value), medication starts and stops, diagnoses, vaccines,
visits, imaging studies and genome imports. Only current facts; sensitive rows only with
`read:sensitive`; each event type only with the scope that guards its facts. Every event cites a `src`.
"""

from __future__ import annotations

from datetime import date

from fastapi import APIRouter, Depends, HTTPException, Query
from sqlalchemy import Connection, text

from ownhealth.api._deps import get_conn, get_principal, profile_access
from ownhealth.core.access import Principal
from ownhealth.core.audit import AuditEvent, write_audit

router = APIRouter(tags=["timeline"])

TYPES = ("lab", "visit", "imaging", "diagnosis", "medication", "vaccine", "genome")
SCOPE = {
    "lab": "read:labs",
    "visit": "read:records",
    "imaging": "read:records",
    "diagnosis": "read:records",
    "medication": "read:records",
    "vaccine": "read:records",
    "genome": "read:genome",
}

_T = {
    "en": {
        "draw": "Lab report · {n} results",
        "draw1": "Lab report · 1 result",
        "flagged": "{n} flagged by the lab",
        "started": "{name} started",
        "stopped": "{name} stopped",
        "genome": "Genome facts imported",
        "visit": "Visit",
    },
    "es": {
        "draw": "Analítica · {n} resultados",
        "draw1": "Analítica · 1 resultado",
        "flagged": "{n} marcados por el laboratorio",
        "started": "Inicio de {name}",
        "stopped": "Fin de {name}",
        "genome": "Datos genómicos importados",
        "visit": "Consulta",
    },
}


def _live(a: str, include_sensitive: bool) -> str:
    """What is true now on a clinical table aliased `a`: current, not superseded, sensitive only if allowed."""
    sql = f"{a}.status = 'current' and {a}.superseded_at is null"
    return sql if include_sensitive else sql + f" and cardinality({a}.sensitivity) = 0"


def _event(
    day: date | None,
    type_: str,
    title: str,
    detail: str | None,
    src: str | None,
    label: str | None,
    provider: str | None = None,
) -> dict | None:
    if day is None:
        return None
    return {
        "date": day.isoformat(),
        "type": type_,
        "title": title,
        "detail": detail or None,
        "country": None,  # not recorded per fact in v0.1
        "flags": [],
        "src": src,
        "src_label": label,
        "provider": provider,
    }


def _labs(conn: Connection, pid, sens: bool, t: dict) -> list[dict | None]:
    # One event per draw: report_key when the importer set it, else the document, else the day.
    rows = conn.execute(
        text(
            "select min(o.effective_at)::date as day, count(*) as n,"
            " count(*) filter (where o.interpretation in ('L', 'H', 'LL', 'HH', 'A')) as flagged,"
            " min(d.provider_name) as provider, min(o.src_id) as src, min(s.label) as label"
            " from observation o left join document d on d.id = o.document_id join src s on s.id = o.src_id"
            f" where o.profile_id = :p and o.category = 'laboratory' and {_live('o', sens)}"
            " group by coalesce(o.report_key, o.document_id::text, o.effective_at::date::text)"
        ),
        {"p": pid},
    ).mappings()
    out = []
    for r in rows:
        parts = [r["provider"], t["flagged"].format(n=r["flagged"]) if r["flagged"] else None]
        detail = " · ".join(p for p in parts if p)
        out.append(
            _event(
                r["day"],
                "lab",
                t["draw1"] if r["n"] == 1 else t["draw"].format(n=r["n"]),
                detail,
                r["src"],
                r["label"],
                r["provider"],
            )
        )
    return out


def _records(conn: Connection, pid, sens: bool, t: dict, want: set[str]) -> list[dict | None]:
    out: list[dict | None] = []
    q = lambda sql: conn.execute(text(sql), {"p": pid}).mappings()  # noqa: E731
    if "medication" in want:
        for r in q(
            "select m.name, m.strength, m.started, m.stopped, m.reason_text, m.dose_text, m.src_id, s.label"
            f" from medication_statement m join src s on s.id = m.src_id where m.profile_id = :p"
            f" and {_live('m', sens)}"
        ):
            name = " ".join(x for x in (r["name"], r["strength"]) if x)
            out.append(
                _event(
                    r["started"], "medication", t["started"].format(name=name), r["dose_text"], r["src_id"], r["label"]
                )
            )
            out.append(
                _event(
                    r["stopped"],
                    "medication",
                    t["stopped"].format(name=name),
                    r["reason_text"],
                    r["src_id"],
                    r["label"],
                )
            )
    if "diagnosis" in want:
        for r in q(
            "select c.text, c.code_system, c.code, coalesce(c.onset, c.effective_at::date) as day,"
            " c.verification_status, c.src_id, s.label from condition c join src s on s.id = c.src_id"
            f" where c.profile_id = :p and {_live('c', sens)}"
        ):
            code = f"{r['code_system'].upper()} {r['code']}" if r["code_system"] and r["code"] else None
            detail = " · ".join(x for x in (code, r["verification_status"]) if x)
            out.append(_event(r["day"], "diagnosis", r["text"], detail, r["src_id"], r["label"]))
    if "vaccine" in want:
        for r in q(
            "select i.vaccine, coalesce(i.occurred, i.effective_at::date) as day, i.dose_number, i.lot,"
            " i.src_id, s.label from immunization i join src s on s.id = i.src_id"
            f" where i.profile_id = :p and {_live('i', sens)}"
        ):
            detail = " · ".join(x for x in (f"#{r['dose_number']}" if r["dose_number"] else None, r["lot"]) if x)
            out.append(_event(r["day"], "vaccine", r["vaccine"], detail, r["src_id"], r["label"]))
    if "visit" in want:
        for r in q(
            "select e.class, e.provider_name, e.reason_text, coalesce(e.period_start, e.effective_at)::date as day,"
            " e.src_id, s.label from encounter e join src s on s.id = e.src_id"
            f" where e.profile_id = :p and {_live('e', sens)}"
        ):
            title = " · ".join(x for x in (t["visit"], r["reason_text"]) if x)
            out.append(
                _event(r["day"], "visit", title, r["provider_name"], r["src_id"], r["label"], r["provider_name"])
            )
    if "imaging" in want:
        for r in q(
            "select i.modality, i.description, i.study_date, i.src_id, s.label"
            " from imaging_study i join src s on s.id = i.src_id"
            " where i.profile_id = :p and i.status = 'current'"
        ):
            title = r["description"] or r["modality"] or "Imaging study"
            out.append(_event(r["study_date"], "imaging", title, r["modality"], r["src_id"], r["label"]))
    return out


def _genome(conn: Connection, pid, t: dict) -> list[dict | None]:
    rows = conn.execute(
        text(
            "select g.imported_at::date as day, g.assay, g.build, g.owngenome_version, g.src_id, s.label"
            " from genome_dataset g join src s on s.id = g.src_id where g.profile_id = :p and g.status = 'current'"
        ),
        {"p": pid},
    ).mappings()
    return [
        _event(
            r["day"],
            "genome",
            t["genome"],
            f"OwnGenome {r['owngenome_version']} · {r['assay'].upper()} · {r['build']}",
            r["src_id"],
            r["label"],
            "OwnGenome",
        )
        for r in rows
    ]


@router.get("/profiles/{p}/timeline")
def timeline(
    p: str,
    types: str | None = None,
    from_: date | None = Query(None, alias="from"),
    to: date | None = None,
    lang: str = "en",
    conn: Connection = Depends(get_conn),
    principal: Principal = Depends(get_principal),
) -> dict:
    perm = profile_access(conn, principal, p)
    asked = set(TYPES) if not types else {x.strip() for x in types.split(",") if x.strip()}
    unknown = asked - set(TYPES)
    if unknown:
        raise HTTPException(422, detail={"code": "bad_input", "message": f"Unknown types: {sorted(unknown)}"})
    want = {k for k in asked if perm.has(SCOPE[k])}
    withheld = sorted(asked - want)
    t = _T.get(lang, _T["en"])
    sens = perm.has("read:sensitive")
    pid = perm.profile.id

    events: list[dict | None] = []
    if "lab" in want:
        events += _labs(conn, pid, sens, t)
    events += _records(conn, pid, sens, t, want)
    if "genome" in want:
        events += _genome(conn, pid, t)
    items = [
        e
        for e in events
        if e and (from_ is None or e["date"] >= from_.isoformat()) and (to is None or e["date"] <= to.isoformat())
    ]
    items.sort(key=lambda e: (e["date"], e["type"], e["title"]), reverse=True)

    write_audit(
        conn,
        AuditEvent(
            actor_kind=principal.actor_kind,
            actor_id=principal.user_id,
            token_id=principal.token_id,
            via="ui",
            action="read",
            tool="GET /api/profiles/{p}/timeline",
            profile_id=pid,
            resource_type="timeline",
            rows_out=len(items),
            withheld={"by_scope": withheld} if withheld else None,
        ),
    )
    return {"events": items, "withheld": {"by_scope": withheld}}
