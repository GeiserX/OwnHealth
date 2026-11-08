"""Render a state body for a reader: localized text, unit preference, identity mode, scopes.

Rendering never touches the body or its hash. The output is the `get_health_status` data block of
mcp.md 5.1 plus the envelope fields only E knows (`rules_version`, `state_hash`, `withheld`,
`limitations`, `notice`); lane F wraps it in the common envelope.
"""

from __future__ import annotations

from datetime import date
from decimal import Decimal, InvalidOperation

from ownhealth.mcp.envelope import untrusted
from ownhealth.state.engine import rcv_percent
from ownhealth.state.pack import Pack, load_pack

# Which scope each section needs; a section whose scope is missing is left out and named in by_scope.
LAB_KINDS = ("lab_range", "rcv", "trend")
KIND_OF = {
    "LAB_HIGH": "lab_range",
    "LAB_LOW": "lab_range",
    "LAB_ABNORMAL": "lab_range",
    "RCV_CHANGE": "rcv",
    "TREND_UP": "trend",
    "TREND_DOWN": "trend",
    "GENE_DRUG": "gene_drug",
}


def window_months(pack: Pack, rule_id: str) -> int:
    return round(int((pack.rule(rule_id) or {}).get("window_days", 0)) / 30.44)


DOMAIN_SCOPE = {"labs": "read:labs", "records": "read:records", "genome": "read:genome", "wearables": "read:wearables"}


def _when(d: str | None, as_of: date, lang: str, mode: str, pack: Pack) -> str | None:
    if d is None:
        return None
    if mode != "minimal":
        return d
    n = (as_of - date.fromisoformat(d[:10])).days
    return pack.text("days_ago", lang, n=n)


def _convert(group: str | None, value: str | None, unit: str | None, units: str) -> dict | None:
    """The `canonical` block in the reader's unit preference, from the normalizer (lane A).
    None when no conversion applies or the normalizer cannot convert (the printed value stays)."""
    if units == "as_reported" or not group or value is None or not unit:
        return None
    try:
        from ownhealth import normalize

        if units != "si":
            return None
        target = normalize.canonical_unit(group)
        source = normalize.to_ucum(unit)
        if not target or not source or target == source:
            return None
        out = normalize.convert(Decimal(value), source, target, alias_group=group)
    except (NotImplementedError, InvalidOperation, ValueError, KeyError, TypeError):
        return None
    return {"value": format(out, "f"), "unit": target} if out is not None else None


def render_state(
    body: dict, *, lang: str, units: str = "as_reported", identity_mode: str = "pseudonymous", scopes=None
) -> dict:
    pack = load_pack()
    lang = lang if lang in ("en", "es") else "en"
    as_of = date.fromisoformat(body["as_of"])
    mode = identity_mode if identity_mode in ("full", "pseudonymous", "minimal") else "pseudonymous"

    def allowed(domain: str) -> bool:
        return scopes is None or DOMAIN_SCOPE[domain] in scopes

    by_scope: set[str] = set()

    def when(d):
        return _when(d, as_of, lang, mode, pack)

    def on(d):  # "on 2026-09-14", or "425 days ago" in minimal mode
        return pack.text("on_date", lang, date=d) if mode != "minimal" else when(d)

    flags = []
    for f in body.get("flags", []):
        f = {**f, "kind": KIND_OF[f["reason"]]}
        domain_ok = allowed("labs") if f["kind"] in LAB_KINDS else allowed("genome") and allowed("records")
        if not domain_ok:
            missing = ["labs"] if f["kind"] in LAB_KINDS else [d for d in ("genome", "records") if not allowed(d)]
            by_scope.update(missing)
            continue
        out = {"rank": len(flags) + 1, "kind": "out_of_range" if f["kind"] == "lab_range" else f["kind"]}
        out["rule"] = f["rule"]
        rule = pack.rule(f["rule"]) or {}
        if f["kind"] in LAB_KINDS:
            name = pack.name(f["group"], lang)
            loinc = f.get("loinc") or next(iter(pack.analytes.get(f["group"], {}).get("loinc", [])), None)
            out |= {
                "code": f"LOINC:{loinc}" if loinc else None,
                "name": name,
                "value": f["value"],
                "unit": f.get("unit"),
                "date": when(f["date"]),
            }
            if f["kind"] == "lab_range":
                out |= {"range": f.get("ref"), "flag": f["flag"]}
                text = pack.text(
                    f["reason"], lang, name=name, value=f["value"], unit=f.get("unit") or "", ref=f.get("ref") or "",
                    flag=f["flag"], when=on(f["date"]),
                )  # fmt: skip
            elif f["kind"] == "rcv":
                out |= {"previous": f["prev"], "change_pct": f["change_pct"], "rcv_pct": rcv_percent(rule)}
                text = pack.text(
                    "RCV_CHANGE", lang, name=name, prev=f["prev"], value=f["value"], unit=f.get("unit") or "",
                    change_pct=f["change_pct"], rcv_pct=rcv_percent(rule), prev_date=when(f["prev_date"]),
                )  # fmt: skip
            else:
                out |= {"points": int(rule.get("points", 3)), "slope_per_year": f["slope"]}
                text = pack.text(
                    f["reason"], lang, name=name, n=int(rule.get("points", 3)), slope=f["slope"], value=f["value"],
                    unit=f.get("unit") or "", when=on(f["date"]),
                )  # fmt: skip
            canonical = _convert(f["group"], f["value"], f.get("unit"), units)
            if canonical:
                out["canonical"] = canonical
        else:
            drug = f["drug"].capitalize()
            out |= {"drug": drug, "gene": f["gene"], "phenotype": f["phenotype"], "guideline": "CPIC"}
            text = pack.text("GENE_DRUG", lang, drug=drug, gene=f["gene"], phenotype=f["phenotype"])
        out["text"] = " ".join(text.split())
        out["src"] = f["src"]
        flags.append({k: v for k, v in out.items() if v is not None})

    labs_ok = allowed("labs")
    if not labs_ok and (body.get("stale") or body.get("never_measured") or body.get("pending")):
        by_scope.add("labs")
    stale = []
    for s in body.get("stale", []) if labs_ok else []:
        months = window_months(pack, s["rule"])
        name = pack.name(s["group"], lang)
        stale.append(
            {
                "name": name,
                "last": when(s["last"]),
                "window_months": months,
                "rule": s["rule"],
                "text": pack.text("STALE", lang, name=name, last=when(s["last"]), window_months=months),
                "src": s["src"],
            }
        )
    never = [
        {
            "name": pack.name(n["group"], lang),
            "rule": n["rule"],
            "text": pack.text("NEVER_MEASURED", lang, name=pack.name(n["group"], lang), rule=n["rule"]),
        }
        for n in body.get("never_measured", [])
        if labs_ok
    ]
    records_ok = allowed("records")
    if not records_ok and (body.get("medications") or body.get("supplements") or body.get("conditions")):
        by_scope.add("records")

    def meds(key):
        if not records_ok:
            return []
        return [
            {
                "name": " ".join(x for x in (m["name"], m.get("strength")) if x),
                "since": when(m.get("since")),
                "src": m["src"],
            }
            for m in body.get(key, [])
        ]

    conditions = (
        [
            {"name": c["name"], "verification": c["verification"], "since": when(c.get("since")), "src": c["src"]}
            for c in body.get("conditions", [])
        ]
        if records_ok
        else []
    )
    notes = (
        []
        if mode == "minimal"
        else [
            {
                "author_kind": n["author_kind"],
                "date": when(n["date"]),
                "text": untrusted(n["text"], "agent_note" if n["author_kind"] == "agent" else "person_note"),
                "src": n["src"],
            }
            for n in body.get("notes", [])
        ]
    )
    pending = [
        {"name": pack.name(p["group"], lang), "date": when(p["date"]), "src": p["src"]}
        for p in body.get("pending", [])
        if labs_ok
    ]
    corrections = []
    for c in body.get("corrections", []) if labs_ok else []:
        item = {
            "fact": f"{pack.name(c['group'], lang)} {c['value']} {c.get('unit') or ''}".strip()
            + f" ({when(c['date'])})",
            "status": c["status"],
            "on": when(c["on"]),
            "src": c["src"],
        }
        if c.get("reason") and mode != "minimal":
            item["reason"] = untrusted(c["reason"], "person_note")
        corrections.append(item)
    fresh = body.get("freshness", {})
    freshness = {}
    for domain in ("labs", "wearables", "genome"):
        if fresh.get(domain):
            if allowed(domain):
                freshness[domain] = when(fresh[domain])
            else:
                by_scope.add(domain)

    withheld = body.get("withheld", {})
    limitations = [pack.text(code, lang) for code in pack.limitations]
    if withheld.get("sensitive"):
        limitations.append(pack.text("SENSITIVE_EXCLUDED", lang, n=withheld["sensitive"]))
    return {
        "profile": body["profile"],
        "as_of": body["as_of"],
        "lang": lang,
        "identity_mode": mode,
        "rules_version": body["rules"],
        "state_hash": body["state_hash"],
        "data": {
            "flags": flags,
            "medications": meds("medications"),
            "supplements": meds("supplements"),
            "active_conditions": conditions,
            "never_measured": never,
            "stale": stale,
            "pending": pending,
            "corrections": corrections,
            "notes": notes,
            "data_freshness": freshness,
        },
        "withheld": {
            "sensitive": withheld.get("sensitive", 0),
            "by_scope": sorted(by_scope),
            "minor_rule": withheld.get("minor_rule", 0),
        },
        "limitations": limitations,
        "notice": pack.text("notice", lang),
    }
