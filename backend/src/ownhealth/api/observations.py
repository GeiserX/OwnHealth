"""Lane A routes: observations, one analyte's trend, and user-added aliases (CONTRACTS.md section 5).

Values come back as printed by default (`units=as_reported`); `si` and `conventional` convert with
the normalizer and fall back to the printed value when a unit does not convert. No status words here:
those come from lane E.
"""

from __future__ import annotations

import re
from datetime import UTC, date, datetime, timedelta
from decimal import Decimal
from typing import Literal

from fastapi import APIRouter, Depends, HTTPException, Query
from pydantic import BaseModel
from sqlalchemy import Connection, text

from ownhealth import normalize
from ownhealth.api._deps import get_conn, get_principal, profile_access
from ownhealth.core.access import AccessDenied, Permission, Principal, accessible_profiles
from ownhealth.core.audit import AuditEvent, hash_arg, write_audit
from ownhealth.core.observations import list_observations
from ownhealth.core.records import list_records
from ownhealth.normalize import _tables
from ownhealth.state.engine import _rules_for, rcv_percent
from ownhealth.state.pack import load_pack

router = APIRouter(tags=["observations"])

Units = Literal["si", "conventional", "as_reported"]
CATEGORY_SCOPE = {
    "laboratory": "read:labs",
    "vital-signs": "read:vitals",
    "activity": "read:wearables",
    "survey": "read:records",
    "exam": "read:records",
}
_WINDOW = re.compile(r"^P(\d{1,7})([DMY])$")


def pretty_unit(ucum: str | None) -> str | None:
    """A UCUM code as a person reads it: 10*3/uL -> 10³/µL, u[IU]/mL -> µIU/mL, {INR} -> ''."""
    if ucum is None:
        return None
    s = ucum
    for a, b in (("10*12", "10¹²"), ("10*9", "10⁹"), ("10*6", "10⁶"), ("10*3", "10³")):
        s = s.replace(a, b)
    s = re.sub(r"(?<![A-Za-z])u(?=g|L|mol|\[IU\]|kat|U)", "µ", s)
    s = s.replace("[IU]", "IU").replace("[arb'U]", "U").replace("/{1.73_m2}", "/1.73 m²").replace("mm[Hg]", "mmHg")
    s = s.replace("/[HPF]", "/HPF").replace("(24.h)", "24 h")
    s = re.sub(r"\{[^}]*\}|\[pH\]", "", s)
    return s


def _shown(row: dict, units: str) -> tuple[Decimal | None, str | None, str | None]:
    """(value, unit as shown, UCUM of that unit) for one observation in the requested units."""
    value, printed = row["value_num"], row["printed_unit"]
    if units == "as_reported" or value is None:
        return value, printed, row["unit_ucum"]
    g = normalize.group_units(row["alias_group"])
    target = g[units] if g else None
    if target and row["unit_ucum"]:
        if target == row["unit_ucum"]:
            return value, printed, target
        conv = normalize.convert(value, row["unit_ucum"], target, alias_group=row["alias_group"])
        if conv is not None:
            return conv, pretty_unit(target), target
    return value, printed, row["unit_ucum"]


def _status(conn: Connection, profile_id, group: str, day: date) -> str | None:
    """Lane E's status word for this marker as of the point's day: the UI colours points with it and
    never computes one itself."""
    from ownhealth.state import status_word

    try:
        return status_word(conn, profile_id, group, as_of=day)
    except NotImplementedError:
        return None


def _labels(conn: Connection, rows: list[dict]) -> tuple[dict, dict]:
    src_ids = list({r["src_id"] for r in rows})
    docs = list({r["document_id"] for r in rows if r["document_id"]})
    src = {}
    if src_ids:
        src = dict(conn.execute(text("select id, label from src where id = any(:s)"), {"s": src_ids}).all())
    labs = {}
    if docs:
        labs = dict(conn.execute(text("select id, provider_name from document where id = any(:d)"), {"d": docs}).all())
    return src, labs


def _allowed(perm: Permission, category: str | None) -> list[str]:
    if category:
        scope = CATEGORY_SCOPE.get(category)
        if scope is None:
            raise HTTPException(422, {"code": "bad_input", "message": f"Unknown category {category!r}."})
        if not perm.has(scope):
            raise AccessDenied("insufficient_scope", f"This needs {scope}.")
        return [category]
    cats = [c for c, s in CATEGORY_SCOPE.items() if perm.has(s)]
    if not cats:
        raise AccessDenied("insufficient_scope", "This needs read:labs, read:vitals or read:wearables.")
    return cats


def _audit(conn: Connection, principal: Principal, perm: Permission, action: str, rows: list[dict], args: str | None):
    write_audit(
        conn,
        AuditEvent(
            actor_kind=principal.actor_kind,
            actor_id=principal.user_id,
            token_id=principal.token_id,
            via="api" if principal.kind == "token" else "ui",
            action=action,
            profile_id=perm.profile.id,
            resource_type="observation",
            src_ids=sorted({r["src_id"] for r in rows}) or None,
            args_summary=hash_arg(args) if args else None,
            rows_out=len(rows),
        ),
    )


def _query_groups(conn: Connection, q: str, household_id) -> set[str]:
    res = normalize.resolve_analyte(conn, q, lang="es", unit=None, household_id=household_id)
    return {c.alias_group for c in res.candidates} | ({res.alias_group} if not res.reasons else set())


@router.get("/profiles/{p}/observations")
def observations(
    p: str,
    q: str | None = None,
    loinc: str | None = None,
    alias_group: str | None = None,
    category: str | None = None,
    from_: date | None = Query(None, alias="from"),
    to: date | None = None,
    units: Units = "as_reported",
    lang: str = "es",
    conn: Connection = Depends(get_conn),
    principal: Principal = Depends(get_principal),
) -> dict:
    perm = profile_access(conn, principal, p)
    cats = _allowed(perm, category)
    rows = list_observations(
        conn,
        perm.profile.id,
        alias_group=alias_group,
        loinc=loinc.removeprefix("LOINC:") if loinc else None,
        category=category,
        since=datetime.combine(from_, datetime.min.time(), UTC) if from_ else None,
        until=datetime.combine(to, datetime.max.time(), UTC) if to else None,
        include_sensitive=perm.has("read:sensitive"),
    )
    rows = [r for r in rows if r["category"] in cats]
    if q:
        groups = _query_groups(conn, q, perm.profile.household_id)
        fq = _tables.fold(q)
        rows = [r for r in rows if r["alias_group"] in groups or fq in _tables.fold(r["printed_name"])]
    src, labs = _labels(conn, rows)
    items = []
    for r in rows:
        value, unit, _ = _shown(r, units)
        items.append(
            {
                "id": r["id"],
                "src": r["src_id"],
                "src_label": src.get(r["src_id"]),
                "name": normalize.display_name(r["alias_group"], lang) or r["printed_name"],
                "printed": {
                    "name": r["printed_name"],
                    "value": r["printed_value"],
                    "unit": r["printed_unit"],
                    "range": r["printed_ref_range"],
                    "flag": r["printed_flag"],
                },
                "value": float(value) if value is not None else None,
                "unit": unit,
                "loinc": r["loinc"],
                "alias_group": r["alias_group"],
                "interpretation": r["interpretation"],
                "method": r["method"],
                "date": r["effective_at"].date().isoformat(),
                "lab": labs.get(r["document_id"]),
                "accepted_by": r["accepted_by"],
                "status": r["status"],
            }
        )
    _audit(conn, principal, perm, "search" if q else "read", rows, q)
    return {"items": items}


def _group_for_code(conn: Connection, profile_id, code: str, household_id) -> str | None:
    c = code.removeprefix("LOINC:")
    if (lc := _tables.loinc_codes().get(c)) is not None:
        return lc.alias_group
    if code in _tables.groups():
        return code
    seen = conn.execute(
        text("select alias_group from observation where profile_id = :p and (loinc = :c or alias_group = :g) limit 1"),
        {"p": profile_id, "c": c, "g": code},
    ).scalar()
    if seen:
        return seen
    res = normalize.resolve_analyte(conn, code, lang="es", unit=None, household_id=household_id)
    return None if res.reasons and "unmapped" in res.reasons else res.alias_group


def _window_start(window: str, today: date) -> date | None:
    if window == "all":
        return None
    m = _WINDOW.match(window)
    if not m:
        raise HTTPException(422, {"code": "bad_input", "message": "window is P<n>D, P<n>M, P<n>Y or all."})
    n, unit = int(m.group(1)), m.group(2)
    days = {"D": 1, "M": 30, "Y": 365}[unit] * n
    try:
        return today - timedelta(days=days)
    except OverflowError:  # before year 1: the calendar has no such day
        raise HTTPException(422, {"code": "bad_input", "message": "window reaches before year 1."}) from None


def _num(x: Decimal | None) -> float | None:
    return float(x) if x is not None else None


@router.get("/profiles/{p}/trend")
def trend(
    p: str,
    code: str,
    window: str = "P3Y",
    units: Units = "as_reported",
    lang: str = "es",
    conn: Connection = Depends(get_conn),
    principal: Principal = Depends(get_principal),
) -> dict:
    perm = profile_access(conn, principal, p)
    cats = _allowed(perm, None)
    group = _group_for_code(conn, perm.profile.id, code, perm.profile.household_id)
    if group is None:
        raise HTTPException(404, {"code": "unknown", "message": f"No analyte matches {code!r}."})
    start = _window_start(window, date.today())
    rows = list_observations(
        conn,
        perm.profile.id,
        alias_group=group,
        since=datetime.combine(start, datetime.min.time(), UTC) if start else None,
        include_sensitive=perm.has("read:sensitive"),
    )
    rows = sorted((r for r in rows if r["category"] in cats), key=lambda r: (r["effective_at"], str(r["id"])))
    g = normalize.group_units(group)
    printed_ucums = {r["unit_ucum"] for r in rows if r["value_num"] is not None}
    if units == "as_reported":
        target = next(iter(printed_ucums)) if len(printed_ucums) == 1 else (g["canonical"] if g else None)
    else:
        target = g[units] if g else None
    src, labs = _labels(conn, rows)
    points, skipped, converted = [], 0, set()
    statuses: dict[date, str | None] = {}
    for r in rows:
        if r["value_num"] is None:
            continue
        ucum = r["unit_ucum"]
        if target and ucum and ucum != target:
            value, lo, hi = (
                normalize.convert(v, ucum, target, alias_group=group) if v is not None else None
                for v in (r["value_num"], r["ref_low"], r["ref_high"])
            )
            converted.add(ucum)
        elif target is None or ucum == target:
            value, lo, hi = r["value_num"], r["ref_low"], r["ref_high"]
        else:
            value = lo = hi = None
        if value is None:
            skipped += 1
            continue
        day = r["effective_at"].date()
        if day not in statuses:
            statuses[day] = _status(conn, perm.profile.id, group, day)
        points.append(
            {
                "date": r["effective_at"].date().isoformat(),
                "value": float(value),
                "unit": pretty_unit(target) if target else r["printed_unit"],
                "printed": " ".join(x for x in (r["printed_value"], r["printed_unit"]) if x),
                "range": r["printed_ref_range"],
                "ref_low": _num(lo),
                "ref_high": _num(hi),
                "flag": r["interpretation"],
                "method": r["method"],
                "loinc": r["loinc"],
                "lab": labs.get(r["document_id"]),
                "src": r["src_id"],
                "src_label": src.get(r["src_id"]),
                "status": statuses[day],
            }
        )
    codes = sorted({f"LOINC:{r['loinc']}" for r in rows if r["loinc"]})
    es = lang == "es"
    limitations = []
    methods = {r["method"] for r in rows if r["method"]}
    if len(codes) > 1 or len(methods) > 1:
        for c in codes:
            lc = _tables.loinc_codes().get(c.removeprefix("LOINC:"))
            dates = [pt["date"] for pt in points if pt["loinc"] == c.removeprefix("LOINC:")]
            if lc and dates:
                n = len(dates)
                limitations.append(
                    (f"{n} punto es" if n == 1 else f"{n} puntos son") + f" {lc.long_name} ({c})."
                    if es
                    else (f"{n} point is" if n == 1 else f"{n} points are") + f" {lc.long_name} ({c})."
                )
    if len({r["printed_ref_range"] for r in rows if r["printed_ref_range"]}) > 1:
        limitations.append(
            "Los laboratorios usan rangos de referencia distintos." if es else "Labs use different reference ranges."
        )
    for u in sorted(converted):
        m = _tables.molar().get(group)
        if es:
            how = f" con la masa molar de {m.species} ({float(m.g_per_mol)} g/mol)" if m and m.g_per_mol else ""
            limitations.append(
                f"Los valores impresos en {pretty_unit(u)} se muestran convertidos a {pretty_unit(target)}{how}."
            )
        else:
            how = f" with the molar mass of {m.species} ({float(m.g_per_mol)} g/mol)" if m and m.g_per_mol else ""
            limitations.append(f"Values printed in {pretty_unit(u)} are shown converted to {pretty_unit(target)}{how}.")
    if skipped:
        limitations.append(
            (
                f"{skipped} valor en una unidad que no se convierte a {pretty_unit(target)} no se dibuja."
                if skipped == 1
                else f"{skipped} valores en una unidad que no se convierte a {pretty_unit(target)} no se dibujan."
            )
            if es
            else (
                f"1 value in a unit that does not convert to {pretty_unit(target)} is not drawn."
                if skipped == 1
                else f"{skipped} values in a unit that does not convert to {pretty_unit(target)} are not drawn."
            )
        )
    annotations = []
    if perm.has("read:records"):
        for m in list_records(conn, "medication", perm.profile.id, include_sensitive=perm.has("read:sensitive")):
            if m["started"] and (start is None or (m["stopped"] or date.today()) >= start):
                label = " ".join(x for x in (m["name"], m["strength"]) if x)
                annotations.append(
                    {
                        "date": m["started"].isoformat(),
                        "end": m["stopped"].isoformat() if m["stopped"] else None,
                        "label": label,
                        "src": m["src_id"],
                    }
                )
        annotations.sort(key=lambda a: a["date"])
    last_vs_prev = rcv_pct = exceeds_rcv = None
    rcv_rules = _rules_for(load_pack(), "rcv", group)
    if rcv_rules:
        rcv_pct = float(rcv_percent(rcv_rules[0]))
    if len(points) >= 2 and points[-2]["value"]:
        change = (points[-1]["value"] - points[-2]["value"]) / points[-2]["value"] * 100
        last_vs_prev = round(change, 1)
        if rcv_pct is not None:
            exceeds_rcv = abs(change) > rcv_pct
    options = []
    for u in (g["canonical"], g["si"], g["conventional"]) if g else ():
        if pretty_unit(u) not in options:
            options.append(pretty_unit(u))
    _audit(conn, principal, perm, "read", rows, None)
    return {
        "name": normalize.display_name(group, lang) or (rows[0]["printed_name"] if rows else group),
        "alias_group": group,
        "codes": codes,
        "unit": pretty_unit(target) if target else (rows[0]["printed_unit"] if rows else ""),
        "unit_options": options,
        "points": points,
        "annotations": annotations,
        "stats": {
            "n": len(points),
            "last_vs_previous_pct": last_vs_prev,
            "rcv_pct": rcv_pct,
            "exceeds_rcv": exceeds_rcv,
        },
        "limitations": limitations,
    }


class AliasIn(BaseModel):
    profile: str
    alias: str
    alias_group: str
    lang: Literal["es", "en", "de", "fr"] | None = None
    loinc: str | None = None
    unit: str | None = None
    specimen: str | None = None


@router.post("/aliases")
def add_alias(
    body: AliasIn, conn: Connection = Depends(get_conn), principal: Principal = Depends(get_principal)
) -> dict:
    """Add a household alias from the review queue, then re-derive the profiles this person reviews."""
    perm = profile_access(conn, principal, body.profile, action="review")
    try:
        alias_id = normalize.add_alias(
            conn,
            household_id=perm.profile.household_id,
            alias=body.alias,
            alias_group=body.alias_group,
            lang=body.lang,
            added_by=principal.user_id,
            unit=body.unit,
            specimen=body.specimen,
            loinc=body.loinc,
        )
    except normalize.AliasError as exc:
        raise HTTPException(422, {"code": "bad_input", "message": str(exc)}) from exc
    changed = 0
    for other in accessible_profiles(conn, principal):
        if other.profile.household_id == perm.profile.household_id and other.can("review"):
            changed += normalize.rederive(conn, profile_id=other.profile.id)
    write_audit(
        conn,
        AuditEvent(
            actor_kind=principal.actor_kind,
            actor_id=principal.user_id,
            token_id=principal.token_id,
            via="api" if principal.kind == "token" else "ui",
            action="alias_add",
            profile_id=perm.profile.id,
            resource_type="analyte_alias",
            resource_ids=[str(alias_id)],
            args_summary=hash_arg(body.alias),
        ),
    )
    return {"id": alias_id, "alias_group": body.alias_group, "rederived": changed}
