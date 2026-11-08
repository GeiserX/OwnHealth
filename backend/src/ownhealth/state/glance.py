"""The "Health at a glance" page (docs/mockup, screen 1): one card per body system, the medication
strip with PGx badges, changes since the last draw, gaps, genome highlights and the state hash.

Everything here is display data derived from the same evaluation as the hashed state body; status
words come only from the engine. Shape: `Dashboard` in ui/src/lib/api/types.ts.
"""

from __future__ import annotations

from datetime import UTC, date, datetime
from decimal import Decimal

from sqlalchemy import Connection, text

from ownhealth import normalize
from ownhealth.state.engine import Evaluation, Marker, med_drug
from ownhealth.state.pack import Pack, cpic_snapshot
from ownhealth.state.rendering import window_months

COUNTRIES = {
    "ES": ("Spain", "España"),
    "PT": ("Portugal", "Portugal"),
    "FR": ("France", "Francia"),
    "DE": ("Germany", "Alemania"),
    "IT": ("Italy", "Italia"),
    "GB": ("United Kingdom", "Reino Unido"),
    "US": ("United States", "Estados Unidos"),
}
NORMAL_PHENOTYPES = {"normal metabolizer", "normal function", "indeterminate", "no result"}


def _f(v) -> float | None:
    return None if v is None else float(v)


def _short(phenotype: str | None) -> str:
    words = [w for w in (phenotype or "").split() if w.lower() not in ("likely", "possible")]
    return "".join(w[0].upper() for w in words)


def actionable_phenotype(phenotype: str | None) -> bool:
    p = (phenotype or "").strip().casefold()
    return bool(p) and p not in NORMAL_PHENOTYPES and not p.endswith("negative")


def _value(m: Marker, spec) -> tuple[str, str]:
    if m.source == "wearables":
        v = m.wearable["value"] if m.wearable else None
        unit = spec.get("unit", "")
        if v is None:
            return "", unit
        if spec.get("format") == "hm":
            minutes = int(v)
            return f"{minutes // 60}:{minutes % 60:02d}", unit
        if spec.get("agg") == "sum30":
            return f"{int(v):,}".replace(",", " "), unit
        return format(v, "f"), unit
    p = m.latest
    return p.shown, p.unit or ""


def _band(m: Marker) -> list[float] | None:
    p = m.latest
    if not p or (p.ref_low is None and p.ref_high is None):
        return None
    low, high = p.ref_low, p.ref_high
    # the range is printed in the report's unit; the spark is in the canonical unit
    if p.canonical is not None and p.unit_ucum and p.canonical_unit and p.unit_ucum != p.canonical_unit:
        conv = [
            normalize.convert(b, p.unit_ucum, p.canonical_unit, alias_group=m.key) if b is not None else None
            for b in (low, high)
        ]
        if any(c is None and b is not None for c, b in zip(conv, (low, high), strict=True)):
            return None
        low, high = conv
    vals = [x.comparable() for x in m.points if x.comparable() is not None]
    low = low if low is not None else Decimal(0)
    high = high if high is not None else max([low * Decimal("1.33"), *vals])
    return [float(low), float(high)]


def _marker_row(m: Marker, spec, pack: Pack, lang: str, sources: dict) -> dict:
    value, unit = _value(m, spec)
    if m.source == "wearables":
        w = m.wearable
        name = spec.get(lang) or spec.get("en") or m.key
        label = sources.get(m.key)
        return {
            "alias_group": m.key,
            "name": name,
            "value": value,
            "unit": unit,
            "status": m.status,
            "stale": m.stale,
            "date": w["newest"].isoformat(),
            "src": None,
            "src_label": " · ".join(x for x in (label, w["newest"].isoformat(), "30 d") if x),
            "spark": [_f(x) for x in w["spark"]],
            "band": None,
            "rule": m.rule,
        }
    p = m.latest
    points = [x for x in m.points if x.comparable() is not None][-int(pack.dashboard.get("spark_points", 9)) :]
    return {
        "alias_group": m.key,
        "name": pack.name(m.key, lang),
        "value": value,
        "unit": unit,
        "status": m.status,
        "stale": m.stale,
        "date": p.date.isoformat(),
        "src": p.src,
        "src_label": p.src_label,
        "spark": [_f(x.comparable()) for x in points],
        "band": _band(m),
        "rule": m.rule,
    }


def _wearable_sources(conn: Connection, profile_id) -> dict[str, str]:
    rows = conn.execute(
        text(
            "select distinct on (r.metric) r.metric, coalesce(s.device, s.provider) from wearable_rollup r"
            " join wearable_source s on s.id = r.source_id where r.profile_id = :p order by r.metric, r.t desc"
        ),
        {"p": profile_id},
    ).all()
    return dict(rows)


def build_dashboard(
    conn: Connection, ev: Evaluation, body: dict, pack: Pack, *, lang: str, scopes: frozenset[str]
) -> dict:
    lang = lang if lang in ("en", "es") else "en"
    t = pack.text
    as_of = ev.as_of
    max_markers = int(pack.dashboard.get("max_markers_per_card", 3))
    sources = _wearable_sources(conn, ev.profile_id) if "read:wearables" in scopes else {}
    labs_ok, wear_ok = "read:labs" in scopes, "read:wearables" in scopes

    systems = []
    for s, sys_def in zip(ev.systems, pack.systems, strict=True):
        allowed = labs_ok if sys_def.source == "labs" else wear_ok
        specs = list(sys_def.markers)
        rows = []
        if allowed:
            for spec, m in zip(specs, s.markers, strict=True):
                if m.status != "nodata" and len(rows) < max_markers:
                    rows.append(_marker_row(m, spec if isinstance(spec, dict) else {}, pack, lang, sources))
        systems.append(
            {
                "key": s.key,
                "name": sys_def.names.get(lang) or sys_def.names.get("en") or s.key,
                "status": s.status if allowed else "nodata",
                "markers": rows,
                "newest": s.newest.isoformat() if allowed and s.newest else None,
                "rule": s.rule if allowed else sys_def.file,
                "rule_file": sys_def.file,
                "withheld": not allowed,
                **({} if allowed else {"needs": "read:labs" if sys_def.source == "labs" else "read:wearables"}),
            }
        )

    # changes since the previous draw, among the lab markers on the page
    lab_markers = [m for m in ev.markers.values() if m.source == "labs" and m.points]
    draw_dates = sorted({p.date for m in lab_markers for p in m.points}, reverse=True)
    latest_draw = draw_dates[0] if draw_dates else None
    prev_draw = draw_dates[1] if len(draw_dates) > 1 else None
    changes, not_this_time = [], []
    if labs_ok and latest_draw:
        min_pct = Decimal(str(pack.dashboard.get("changes_min_pct", 15)))
        for m in lab_markers:
            p = m.latest
            if p.date != latest_draw:
                if m.stale:
                    not_this_time.append(pack.name(m.key, lang))
                continue
            prev = [x for x in m.points[:-1] if x.comparable() is not None]
            if not prev or p.comparable() is None or prev[-1].comparable() == 0:
                continue
            a, b = prev[-1].comparable(), p.comparable()
            pct = (b - a) / a * 100
            if m.status in ("out", "watch") or abs(pct) >= min_pct:
                changes.append(
                    {
                        "name": pack.name(m.key, lang),
                        "from": prev[-1].shown,
                        "to": p.shown,
                        "unit": p.unit or "",
                        "direction": "up" if b > a else "down",
                        "status": m.status,
                        "src": p.src,
                    }
                )

    # gaps: stale series (grouped by the rule that set them) and never-measured analytes
    due = []
    if labs_ok:
        by_rule: dict[str, list[dict]] = {}
        for s in body.get("stale", []):
            by_rule.setdefault(s["rule"], []).append(s)
        for rule, items in by_rule.items():
            names = " + ".join(pack.name(i["group"], lang) for i in items)
            due.append(
                {
                    "title": t("due.retest", lang, name=names),
                    "why": t("due.stale", lang, rule=rule, window_months=window_months(pack, rule)),
                    "status": "stale",
                    "kind": "lab",
                    "phase": None,
                    "rule": rule,
                }
            )
        for n in body.get("never_measured", []):
            due.append(
                {
                    "title": pack.name(n["group"], lang),
                    "why": t("due.never", lang, rule=n["rule"]),
                    "status": "watch",
                    "kind": "lab",
                    "phase": None,
                    "rule": n["rule"],
                }
            )

    # medication strip with PGx badges (CPIC level A pairs with a non-normal phenotype)
    findings = ev.findings if "read:genome" in scopes else []
    snap = cpic_snapshot()
    medications = []
    if "read:records" in scopes:
        for med in ev.medications:
            key = med_drug(pack, med)
            badge = None
            for pair in (p for p in snap["pairs"] if p["drugname"] == key):
                f = next((f for f in findings if f["gene"] == pair["genesymbol"]), None)
                if f and actionable_phenotype(f["phenotype"]):
                    badge = {
                        "gene": f["gene"],
                        "label": f"{f['gene']} {_short(f['phenotype'])}",
                        "phenotype": f["phenotype"],
                        "level": pair["cpiclevel"],
                        "text": t("pgx.badge", lang, gene=f["gene"], phenotype=f["phenotype"], level=pair["cpiclevel"]),
                        "src": f["src"],
                    }
                    break
            medications.append(
                {
                    "name": med["name"],
                    "strength": med.get("strength"),
                    "since": med["started"].isoformat() if med["started"] else None,
                    "src": med["src_id"],
                    "pgx": badge,
                }
            )

    genome = None
    if "read:genome" in scopes and (findings or any(ev.genome_withheld.values())):
        pgx = [f for f in findings if f["category"] == "pgx" and actionable_phenotype(f["phenotype"])]
        carriers = [f for f in findings if f["category"] == "carrier"]
        chips = [" ".join(x for x in (f["gene"], f["diplotype"], _short(f["phenotype"])) if x) for f in pgx]
        chips += [
            " ".join(x for x in (f["gene"], f["rsid"]) if x)
            for f in findings
            if f["category"] == "risk_marker" and f["call_state"] == "alt"
        ]
        hidden = ev.genome_withheld.get("sensitive", 0) + ev.genome_withheld.get("minor_rule", 0)
        assay = ev.genome_assay or {}
        genome = {
            "summary": " · ".join(
                t(f"genome.summary.{k}", lang, n=n)
                for k, n in (("pgx", len(pgx)), ("carrier", len(carriers)), ("hidden", hidden))
            ),
            "chips": chips,
            "hidden": hidden,
            "source": assay.get("source_tool") or assay.get("pipeline"),
        }

    review_open = conn.execute(
        text("select count(*) from review_item where profile_id = :p and status = 'open'"), {"p": ev.profile_id}
    ).scalar_one()

    prof = ev.profile
    parts = []
    if prof.sex_at_birth in ("female", "male"):
        parts.append(t(f"sex.{prof.sex_at_birth}", lang))
    if prof.age_on(as_of) is not None:
        parts.append(str(prof.age_on(as_of)))
    if prof.country:
        names = COUNTRIES.get(prof.country)
        parts.append(names[1 if lang == "es" else 0] if names else prof.country)
    fresh = body.get("freshness", {})
    if labs_ok and fresh.get("labs"):
        parts.append(t("subtitle.lab", lang, n=(as_of - date.fromisoformat(fresh["labs"])).days))
    if wear_ok and fresh.get("wearables"):
        parts.append(t("subtitle.wearables", lang, n=(as_of - date.fromisoformat(fresh["wearables"])).days))

    return {
        "profile": ev.slug,
        "as_of": as_of.isoformat(),
        "subtitle": " · ".join(parts),
        "systems": systems,
        "medications": medications,
        "changes": changes,
        "changes_vs": prev_draw.isoformat() if prev_draw and changes else None,
        "not_measured_this_time": not_this_time,
        "due": due,
        "genome": genome,
        "review_open": int(review_open),
        "state_hash": body["state_hash"],
        "rules_version": body["rules"],
        "generated_at": datetime.now(UTC).replace(microsecond=0).isoformat().replace("+00:00", "Z"),
    }
