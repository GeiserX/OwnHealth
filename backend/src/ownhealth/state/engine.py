"""Evaluate the rule pack over one profile's facts at one date. Pure reads; no wall clock inside.

`evaluate` returns everything the status page and the MCP state need (markers with their series, flags,
gaps, records). `state.build_state` projects it to the hashed body; `state.dashboard` adds display data.

Inputs are the rows visible at `known_at` (None: the current view) with `effective_at` on or before the
end of the `as_of` day, minus rows tagged sensitive (they are counted, never evaluated). For a minor,
adult-onset, sensitive and carrier genome findings are not inputs (the genome gate).
"""

from __future__ import annotations

import re
from dataclasses import dataclass, field
from datetime import UTC, date, datetime, time, timedelta
from decimal import ROUND_HALF_EVEN, Decimal
from uuid import UUID

from sqlalchemy import Connection, text

from ownhealth import genome
from ownhealth.core import lifecycle
from ownhealth.core.access import age_of_majority, get_profile
from ownhealth.core.records import active_medications, list_records
from ownhealth.state.pack import Pack, System, dec

FLAGGED = ("H", "HH", "L", "LL", "A")
Z = Decimal("1.96")
SQRT2 = Decimal(2).sqrt()
DAYS_PER_YEAR = Decimal("365.25")
STATUS_RANK = {"out": 4, "watch": 3, "ok": 2, "stale": 1, "nodata": 0}


def day_end(as_of: date) -> datetime:
    """Inclusive end of the as_of day in UTC: effective_at < this."""
    return datetime.combine(as_of + timedelta(days=1), time(0), tzinfo=UTC)


def to_date(value) -> date:
    if isinstance(value, datetime):
        return value.astimezone(UTC).date() if value.tzinfo else value.date()
    if isinstance(value, date):
        return value
    return date.fromisoformat(str(value)[:10])


def num(v) -> str:
    """A Decimal as a plain string: no exponent, the precision it was stored with."""
    if v is None:
        return None
    d = v if isinstance(v, Decimal) else Decimal(str(v))
    return format(d, "f")


@dataclass
class Point:
    id: UUID
    date: date
    value: Decimal | None  # value_num, the printed number parsed
    canonical: Decimal | None
    canonical_unit: str | None
    printed_value: str
    unit: str | None
    ref: str | None
    ref_low: Decimal | None
    ref_high: Decimal | None
    flag: str | None  # lab interpretation, or the printed flag
    loinc: str | None
    method: str | None
    src: str
    src_label: str | None
    derivation: str
    unit_ucum: str | None = None  # the printed unit in UCUM, the unit ref_low and ref_high are in

    @property
    def shown(self) -> str:
        return num(self.value) if self.value is not None else self.printed_value

    def comparable(self) -> Decimal | None:
        return self.canonical if self.canonical is not None else self.value

    def comparable_unit(self) -> str | None:
        """The unit `comparable()` is in: two points compare only when these match."""
        return self.canonical_unit if self.canonical is not None else self.unit


@dataclass
class Marker:
    key: str  # alias group, or wearable metric
    system: str
    source: str  # labs | wearables
    status: str = "nodata"
    stale: bool = False
    rule: str | None = None  # the rule id that set the status
    window_days: int | None = None
    recency_rule: str | None = None
    points: list[Point] = field(default_factory=list)  # chronological (labs)
    wearable: dict | None = None  # value, unit, newest, spark, source (wearables)

    @property
    def latest(self) -> Point | None:
        return self.points[-1] if self.points else None

    @property
    def newest(self) -> date | None:
        if self.points:
            return self.points[-1].date
        return self.wearable["newest"] if self.wearable else None


@dataclass
class SystemState:
    key: str
    file: str
    status: str
    rule: str
    newest: date | None
    markers: list[Marker]


@dataclass
class Evaluation:
    profile_id: UUID
    slug: str
    as_of: date
    profile: object
    minor: bool
    markers: dict[str, Marker]
    flags: list[dict]
    stale: list[dict]
    withheld_sensitive: int
    series_all: dict[str, list[Point]]
    systems: list[SystemState] = field(default_factory=list)
    never_measured: list[dict] = field(default_factory=list)
    medications: list[dict] = field(default_factory=list)
    supplements: list[dict] = field(default_factory=list)
    conditions: list[dict] = field(default_factory=list)
    notes: list[dict] = field(default_factory=list)
    pending: list[dict] = field(default_factory=list)
    corrections: list[dict] = field(default_factory=list)
    freshness: dict = field(default_factory=dict)
    findings: list[dict] = field(default_factory=list)
    genome_withheld: dict = field(default_factory=dict)
    genome_assay: dict | None = None
    derivation: str = ""


# ------------------------------------------------------------------ loading ----


def _load_points(
    conn: Connection, profile_id: UUID, as_of: date, known_at: datetime | None, groups: set[str] | None, pack: Pack
):
    where = ["o.profile_id = :p", lifecycle.visible_sql("o"), "o.effective_at < :cut"]
    params: dict = {"p": profile_id, "as_of": known_at, "cut": day_end(as_of)}
    if groups is not None:
        codes = [c for g in groups for c in pack.analytes.get(g, {}).get("loinc", [])]
        where.append("(o.alias_group = any(:groups) or o.loinc = any(:codes))")
        params |= {"groups": list(groups), "codes": [str(c) for c in codes]}
    rows = conn.execute(
        text(
            "select o.id, o.alias_group, o.loinc, o.effective_at, o.recorded_at, o.value_num, o.value_canonical,"
            " o.unit_canonical, o.printed_value, o.printed_unit, o.unit_ucum, o.printed_ref_range, o.ref_low,"
            " o.ref_high,"
            " o.interpretation, o.printed_flag, o.method, o.src_id, o.derivation_version, o.sensitivity, s.label"
            " from observation o join src s on s.id = o.src_id"
            f" where {' and '.join(where)} order by o.effective_at, o.recorded_at, o.id"
        ),
        params,
    ).mappings()
    series: dict[str, list[Point]] = {}
    sensitive = 0
    for r in rows:
        if r["sensitivity"]:
            sensitive += 1
            continue
        group = r["alias_group"]
        if group not in pack.analytes and r["loinc"] in pack.loinc_to_group:
            group = pack.loinc_to_group[r["loinc"]]
        printed = (r["printed_flag"] or "").strip().upper() or None
        flag = r["interpretation"] or printed
        series.setdefault(group, []).append(
            Point(
                id=r["id"],
                date=to_date(r["effective_at"]),
                value=r["value_num"],
                canonical=r["value_canonical"],
                canonical_unit=r["unit_canonical"],
                printed_value=r["printed_value"],
                unit=r["printed_unit"],
                unit_ucum=r["unit_ucum"],
                ref=r["printed_ref_range"],
                ref_low=r["ref_low"],
                ref_high=r["ref_high"],
                flag=flag,
                loinc=r["loinc"],
                method=r["method"],
                src=r["src_id"],
                src_label=r["label"],
                derivation=r["derivation_version"],
            )
        )
    return series, sensitive


def _wearables(conn: Connection, profile_id: UUID, as_of: date, metrics: list[str]) -> dict[str, list[tuple]]:
    """Daily rollups of the last 90 days per metric, one source per metric (source_priority, else the
    source with the most days). Returns metric -> [(day, avg, sum)] chronological."""
    if not metrics:
        return {}
    rows = conn.execute(
        text(
            "select r.metric, r.source_id, r.t, r.v_avg, r.v_sum from wearable_rollup r"
            " where r.profile_id = :p and r.bucket = 'day' and r.metric = any(:m) and r.t < :cut and r.t >= :start"
            " order by r.metric, r.t"
        ),
        {"p": profile_id, "m": metrics, "cut": day_end(as_of), "start": day_end(as_of) - timedelta(days=90)},
    ).all()
    prio = {
        m: ids
        for m, ids in conn.execute(
            text("select metric, source_ids from source_priority where profile_id = :p"), {"p": profile_id}
        ).all()
    }
    by_metric: dict[str, dict[int, list[tuple]]] = {}
    for metric, source_id, t, v_avg, v_sum in rows:
        by_metric.setdefault(metric, {}).setdefault(source_id, []).append((to_date(t), v_avg, v_sum))
    out: dict[str, list[tuple]] = {}
    for metric, sources in by_metric.items():
        chosen = next((s for s in prio.get(metric, []) if s in sources), None)
        if chosen is None:
            chosen = min(sources, key=lambda s: (-len(sources[s]), s))
        out[metric] = sources[chosen]
    return out


def _round(v: float | None, places: int = 1) -> Decimal | None:
    if v is None:
        return None
    q = Decimal(1).scaleb(-places)
    return Decimal(str(v)).quantize(q, rounding=ROUND_HALF_EVEN)


def _wearable_marker(spec: dict, days: list[tuple], as_of: date) -> dict | None:
    if not days:
        return None
    newest = days[-1][0]
    since = as_of - timedelta(days=29)
    last30 = [d for d in days if d[0] >= since]
    agg = spec.get("agg", "avg30")
    if agg == "latest":
        value = days[-1][1]
    else:
        vals = [(d[2] if agg == "sum30" else d[1]) for d in last30 if (d[2] if agg == "sum30" else d[1]) is not None]
        value = sum(vals) / len(vals) if vals else None
    # sparkline: six 15-day buckets over the 90 days (latest values for `latest`)
    spark = []
    if agg == "latest":
        spark = [_round(d[1]) for d in days[-6:]]
    else:
        start = as_of - timedelta(days=89)
        for b in range(6):
            lo, hi = start + timedelta(days=15 * b), start + timedelta(days=15 * (b + 1))
            vals = [(d[2] if agg == "sum30" else d[1]) for d in days if lo <= d[0] < hi]
            vals = [v for v in vals if v is not None]
            if vals:
                spark.append(_round(sum(vals) / len(vals)))
    places = 0 if spec.get("metric") in ("steps", "hr_resting", "hrv_rmssd", "sleep_asleep") else 1
    return {"value": _round(value, places), "newest": newest, "spark": spark, "n_days": len(last30)}


# -------------------------------------------------------------------- rules ----


def _rcv_value(rule: dict) -> Decimal:
    """Reference change value in %, two-sided 95%: z * sqrt(2) * sqrt(CVa^2 + CVi^2) (Fraser)."""
    cvi, cva = dec(rule["cv_i"]), dec(rule["cv_a"])
    return Z * SQRT2 * (cvi * cvi + cva * cva).sqrt()


def rcv_percent(rule: dict) -> str:
    return num(_rcv_value(rule).quantize(Decimal("0.1"), rounding=ROUND_HALF_EVEN))


def _rcv(rule: dict, pts: list[Point]) -> dict | None:
    if len(pts) < 2:
        return None
    a, b = pts[-2], pts[-1]
    va, vb = a.comparable(), b.comparable()
    if va is None or vb is None or va == 0 or a.comparable_unit() != b.comparable_unit():
        return None
    rcv = _rcv_value(rule)
    change = (vb - va) / va * 100
    if abs(change) <= rcv:
        return None
    return {
        "kind": "rcv",
        "reason": "RCV_CHANGE",
        "rule": rule["id"],
        "severity": int(rule.get("severity", 1)),
        "value": b.shown,
        "prev": a.shown,
        "prev_date": a.date.isoformat(),
        "unit": b.unit,
        "change_pct": num(change.quantize(Decimal("0.1"), rounding=ROUND_HALF_EVEN)),
        "rcv_pct": rcv_percent(rule),
        "date": b.date.isoformat(),
        "src": [a.src, b.src],
    }


def _trend(rule: dict, pts: list[Point]) -> dict | None:
    n = int(rule.get("points", 3))
    usable = [p for p in pts if p.comparable() is not None]
    # one unit per series: a point the normalizer could not convert must not read as a jump
    tail = [p for p in usable if p.comparable_unit() == usable[-1].comparable_unit()][-n:] if usable else []
    if len(tail) < n or len({p.date for p in tail}) < n:
        return None
    vals = [p.comparable() for p in tail]
    up = rule.get("direction", "up") == "up"
    pairs = list(zip(vals, vals[1:], strict=False))
    if not all((b > a) if up else (b < a) for a, b in pairs):
        return None
    x0 = tail[0].date
    xs = [Decimal((p.date - x0).days) / DAYS_PER_YEAR for p in tail]
    mx, my = sum(xs) / n, sum(vals) / n
    sxx = sum((x - mx) ** 2 for x in xs)
    slope = sum((x - mx) * (y - my) for x, y in zip(xs, vals, strict=True)) / sxx
    if abs(slope) < dec(rule["min_slope_per_year"]):
        return None
    last = tail[-1]
    return {
        "kind": "trend",
        "reason": "TREND_UP" if up else "TREND_DOWN",
        "rule": rule["id"],
        "severity": int(rule.get("severity", 1)),
        "value": last.shown,
        "unit": last.unit,
        "n": n,
        "slope": num(slope.quantize(Decimal("0.01"), rounding=ROUND_HALF_EVEN)),
        "date": last.date.isoformat(),
        "src": [p.src for p in tail],
    }


def _lab_range(rule: dict, p: Point) -> dict | None:
    if p.flag not in FLAGGED:
        return None
    return {
        "kind": "lab_range",
        "reason": rule["reason"][p.flag],
        "rule": rule["id"],
        "severity": int(rule["severity"][p.flag]),
        "value": p.shown,
        "unit": p.unit,
        "ref": p.ref,
        "flag": p.flag,
        "loinc": p.loinc,
        "date": p.date.isoformat(),
        "src": p.src,
    }


def _rules_for(pack: Pack, kind: str, key: str) -> list[dict]:
    return [r for r in pack.rules_of(kind) if key in r.get("match", [])]


def _phen(s: str | None) -> str:
    return (s or "").strip().casefold()


def _finding(d: dict) -> dict:
    """Read a lane D finding (list_findings) into the fields the rules use."""
    value = d.get("value") if isinstance(d.get("value"), dict) else {}
    return {
        "id": d.get("id") or d.get("fact_id"),
        "category": d.get("category"),
        "gene": d.get("gene"),
        "diplotype": d.get("diplotype") or value.get("diplotype"),
        "phenotype": d.get("phenotype") or value.get("phenotype"),
        "activity_score": dec(d.get("activity_score") if d.get("activity_score") is not None else None),
        "call_state": d.get("call_state"),
        "rsid": d.get("rsid"),
        "evidence": d.get("evidence"),
        "src": d.get("src") or d.get("src_id"),
    }


def drug_key(pack: Pack, name: str | None) -> str | None:
    """A medication name or ingredient to the CPIC drug name key of drugs.yaml (no DB needed)."""
    if not name:
        return None
    import unicodedata

    def norm(s: str) -> str:
        s = unicodedata.normalize("NFKD", s.casefold())
        return "".join(c for c in s if not unicodedata.combining(c)).strip()

    n = norm(name)
    for key, aliases in pack.drugs.items():
        if n == norm(key) or n in {norm(a) for a in aliases}:
            return key
    # first word of "Atorvastatin 20 mg", "Omeprazol cinfa"
    head = n.split()[0] if n.split() else n
    if head != n:
        return drug_key(pack, head)
    return None


def med_drug(pack: Pack, med: dict) -> str | None:
    return drug_key(pack, med.get("ingredient")) or drug_key(pack, med.get("name"))


# ----------------------------------------------------------------- evaluate ----


def _list_findings(conn: Connection, profile_id: UUID, minor: bool, as_of: date):
    return genome.list_findings(
        conn, profile_id, gate=genome.Gate(include_sensitive=False, minor=minor), min_evidence="E2", as_of=as_of
    )


def evaluate(
    conn: Connection,
    profile_id: UUID,
    *,
    as_of: date,
    pack: Pack,
    known_at: datetime | None = None,
    only_group: str | None = None,
) -> Evaluation:
    profile = get_profile(conn, profile_id)
    if profile is None:
        raise LookupError(f"profile {profile_id} not found")
    minor = profile.is_minor(as_of, age_of_majority(conn, profile.household_id))
    age = profile.age_on(as_of)
    series, sensitive = _load_points(conn, profile_id, as_of, known_at, {only_group} if only_group else None, pack)

    flags: list[dict] = []
    stale: list[dict] = []
    markers: dict[str, Marker] = {}
    range_rule = pack.rules_of("lab_range")[0]
    systems_in_scope = [s for s in pack.systems if not only_group or (s.source == "labs" and only_group in s.markers)]
    if only_group and not systems_in_scope:
        systems_in_scope = [System("other", "", 99, {}, "labs", (only_group,))]

    wearable_specs = [m for s in systems_in_scope if s.source == "wearables" for m in s.markers]
    wdays = _wearables(conn, profile_id, as_of, [m["metric"] for m in wearable_specs]) if not only_group else {}

    for system in systems_in_scope:
        for spec in system.markers:
            key = spec["metric"] if isinstance(spec, dict) else spec
            m = Marker(key=key, system=system.key, source=system.source)
            rec = _rules_for(pack, "recency", key)
            if rec:
                m.window_days, m.recency_rule = int(rec[0]["window_days"]), rec[0]["id"]
            if system.source == "wearables":
                m.wearable = _wearable_marker(spec, wdays.get(key, []), as_of)
                if m.wearable:
                    m.status = "ok"
                    if m.window_days is not None and (as_of - m.wearable["newest"]).days > m.window_days:
                        m.status, m.stale, m.rule = "stale", True, m.recency_rule
                markers[key] = m
                continue
            m.points = series.get(key, [])
            p = m.latest
            if p is None:
                markers[key] = m
                continue
            m.status = "ok"
            if m.window_days is not None and (as_of - p.date).days > m.window_days:
                m.stale = True
                stale.append(
                    {
                        "group": key,
                        "last": p.date.isoformat(),
                        "window_days": m.window_days,
                        "rule": m.recency_rule,
                        "src": p.src,
                    }
                )
            found = _lab_range(range_rule, p)
            if found:
                m.status, m.rule = "out", found["rule"]
                flags.append({**found, "group": key})
            else:
                for rule in _rules_for(pack, "rcv", key):
                    f = _rcv(rule, m.points)
                    if f:
                        flags.append({**f, "group": key})
                        m.status, m.rule = "watch", f["rule"]
                        break
                if m.status != "watch":
                    for rule in _rules_for(pack, "trend", key):
                        f = _trend(rule, m.points)
                        if f:
                            flags.append({**f, "group": key})
                            m.status, m.rule = "watch", f["rule"]
                            break
            if m.status == "ok" and m.stale:
                m.status, m.rule = "stale", m.recency_rule
            markers[key] = m

    if only_group:
        return Evaluation(profile_id, profile.slug, as_of, profile, minor, markers, flags, stale, sensitive, series)

    systems = [_system_state(s, markers) for s in pack.systems]

    meds_all = active_medications(conn, profile_id, on=as_of, as_of=known_at, include_sensitive=False)
    meds_all = [m for m in meds_all if m["started"] is None or m["started"] <= as_of]
    medications = [m for m in meds_all if m["kind"] == "medication"]
    supplements = [m for m in meds_all if m["kind"] == "supplement"]

    findings_obj = _list_findings(conn, profile_id, minor, as_of)
    findings = [_finding(f) for f in findings_obj.findings]

    never: list[dict] = []
    seen_never: set[str] = set()

    def never_add(group: str, rule_id: str) -> None:
        if group not in seen_never and not series.get(group):
            seen_never.add(group)
            never.append({"group": group, "rule": rule_id})

    for rule in pack.rules_of("never_measured"):
        when = rule.get("when", {})
        if "min_age" in when and (age is None or age < int(when["min_age"])):
            continue
        if "max_age" in when and (age is None or age > int(when["max_age"])):
            continue
        if "sex" in when and profile.sex_at_birth != when["sex"]:
            continue
        if "taking" in when:
            words = [w.casefold() for w in when["taking"]]
            names = [f"{m['name']} {m.get('ingredient') or ''}".casefold() for m in meds_all]
            # whole words: "iron" must not fire on "Environ"
            if not any(re.search(rf"(?<!\w){re.escape(w)}(?!\w)", n) for w in words for n in names):
                continue
        for group in rule["match"]:
            never_add(group, rule["id"])

    med_by_drug: dict[str, dict] = {}
    for med in medications:
        k = med_drug(pack, med)
        if k:
            med_by_drug.setdefault(k, med)

    for rule in pack.rules_of("gene_drug"):
        wanted = {_phen(p) for p in rule["phenotypes"]}
        for f in findings:
            if f["gene"] != rule["gene"] or _phen(f["phenotype"]) not in wanted:
                continue
            for drug in rule["drugs"]:
                med = med_by_drug.get(drug)
                if med:
                    flags.append(
                        {
                            "kind": "gene_drug",
                            "reason": "GENE_DRUG",
                            "rule": rule["id"],
                            "severity": int(rule.get("severity", 2)),
                            "gene": f["gene"],
                            "phenotype": f["phenotype"],
                            "drug": drug,
                            "date": med["started"].isoformat() if med["started"] else None,
                            "src": [f["src"], med["src_id"]],
                        }
                    )

    for rule in pack.rules_of("gene_lab"):
        genes = rule["gene"] if isinstance(rule["gene"], list) else [rule["gene"]]
        hits = [
            f
            for f in findings
            if f["gene"] in genes
            and (not rule.get("categories") or f["category"] in rule["categories"])
            and (not rule.get("call_states") or f["call_state"] in rule["call_states"])
            and (not rule.get("phenotypes") or _phen(f["phenotype"]) in {_phen(p) for p in rule["phenotypes"]})
        ]
        if not hits or (rule.get("drugs") and not any(d in med_by_drug for d in rule["drugs"])):
            continue
        for lab in rule["labs"]:
            pts = series.get(lab, [])
            if not pts:
                never_add(lab, rule["id"])
            elif (
                rule.get("window_days")
                and (as_of - pts[-1].date).days > int(rule["window_days"])
                and not any(s["group"] == lab for s in stale)
            ):
                stale.append(
                    {
                        "group": lab,
                        "last": pts[-1].date.isoformat(),
                        "window_days": int(rule["window_days"]),
                        "rule": rule["id"],
                        "src": pts[-1].src,
                    }
                )

    order = {k: i for i, k in enumerate(pack.order)}
    flags.sort(
        key=lambda f: (
            -f["severity"],
            order.get(f["kind"], 99),
            -(date.fromisoformat(f["date"]).toordinal() if f.get("date") else 0),
            f.get("group") or f.get("gene") or "",
            f["rule"],
        )
    )
    stale.sort(key=lambda s: (s["last"], s["group"]))

    conditions = [
        c
        for c in list_records(conn, "condition", profile_id, as_of=known_at, include_sensitive=False)
        if c["clinical_status"] in ("active", "recurrence", "relapse")
        and c["verification_status"] not in ("refuted", "entered-in-error")
        and (c["onset"] is None or c["onset"] <= as_of)
    ]
    conditions.sort(key=lambda c: (c["onset"] or date.min, c["text"]))

    cut = day_end(as_of)
    note_rows = conn.execute(
        text(
            "select n.author_kind, n.body, n.effective_at, n.recorded_at, n.src_id from note n"
            f" where n.profile_id = :p and n.kind = 'priorities' and {lifecycle.visible_sql('n')}"
            " and coalesce(n.effective_at, n.recorded_at) < :cut and n.sensitivity = '{}'"
            " order by coalesce(n.effective_at, n.recorded_at) desc, n.id limit 3"
        ),
        {"p": profile_id, "as_of": known_at, "cut": cut},
    ).mappings()
    notes = [
        {
            "author_kind": r["author_kind"],
            "date": to_date(r["effective_at"] or r["recorded_at"]).isoformat(),
            "text": r["body"],
            "src": r["src_id"],
        }
        for r in note_rows
    ]

    know = "and o.recorded_at <= :known" if known_at else ""
    pending = [
        {"group": r["alias_group"], "date": to_date(r["effective_at"]).isoformat(), "src": r["src_id"]}
        for r in conn.execute(
            text(
                "select o.alias_group, o.effective_at, o.src_id from observation o where o.profile_id = :p"
                f" and o.status = 'provisional' and o.effective_at < :cut and o.sensitivity = '{{}}' {know}"
                " order by o.effective_at desc, o.id limit 3"
            ),
            {"p": profile_id, "cut": cut, "known": known_at},
        ).mappings()
    ]
    corr_know = "and o.superseded_at <= :known" if known_at else ""
    corrections = [
        {
            "group": r["alias_group"],
            "value": r["printed_value"],
            "unit": r["printed_unit"],
            "date": to_date(r["effective_at"]).isoformat(),
            "status": r["status"],
            "reason": r["status_reason"],
            "on": to_date(r["superseded_at"]).isoformat(),
            "src": r["src_id"],
        }
        for r in conn.execute(
            text(
                "select o.alias_group, o.printed_value, o.printed_unit, o.effective_at, o.status, o.status_reason,"
                " o.superseded_at, o.src_id from observation o where o.profile_id = :p"
                " and o.status in ('superseded', 'retracted') and o.superseded_at < :cut"
                f" and o.superseded_at >= :since and o.sensitivity = '{{}}' {corr_know}"
                " and coalesce(o.status_reason, '') not like 'rederive norm-%'"
                " order by o.superseded_at desc, o.id limit 3"
            ),
            {"p": profile_id, "cut": cut, "since": cut - timedelta(days=90), "known": known_at},
        ).mappings()
    ]

    lab_dates = [pts[-1].date for pts in series.values() if pts]
    wear_dates = [m.wearable["newest"] for m in markers.values() if m.wearable]
    genome_date = conn.execute(
        text(
            "select max(imported_at) from genome_dataset where profile_id = :p and status = 'current'"
            " and imported_at < :cut" + (" and imported_at <= :known" if known_at else "")
        ),
        {"p": profile_id, "cut": cut, "known": known_at},
    ).scalar()
    freshness = {
        "labs": max(lab_dates).isoformat() if lab_dates else None,
        "wearables": max(wear_dates).isoformat() if wear_dates else None,
        "genome": to_date(genome_date).isoformat() if genome_date else None,
    }
    derivation = ",".join(sorted({p.derivation for pts in series.values() for p in pts}))

    return Evaluation(
        profile_id=profile_id,
        slug=profile.slug,
        as_of=as_of,
        profile=profile,
        minor=minor,
        markers=markers,
        systems=systems,
        flags=flags,
        stale=stale,
        never_measured=never,
        medications=medications,
        supplements=supplements,
        conditions=conditions,
        notes=notes,
        pending=pending,
        corrections=corrections,
        freshness=freshness,
        findings=findings,
        genome_withheld={"sensitive": findings_obj.withheld_sensitive, "minor_rule": findings_obj.withheld_minor},
        genome_assay=findings_obj.assay,
        withheld_sensitive=sensitive,
        derivation=derivation,
        series_all=series,
    )


def _system_state(system: System, markers: dict[str, Marker]) -> SystemState:
    ms = [markers[m["metric"] if isinstance(m, dict) else m] for m in system.markers]
    with_data = [m for m in ms if m.status != "nodata"]
    newest = max((m.newest for m in with_data if m.newest), default=None)
    if not with_data:
        return SystemState(system.key, system.file, "nodata", system.file, None, ms)
    if any(m.status in ("out", "watch") for m in with_data):
        worst = max(
            (m for m in with_data if m.status in ("out", "watch")),
            key=lambda m: (STATUS_RANK[m.status], -ms.index(m)),
        )
        return SystemState(system.key, system.file, worst.status, worst.rule, newest, ms)
    if any(m.status == "ok" for m in with_data):
        return SystemState(system.key, system.file, "ok", system.file, newest, ms)
    return SystemState(system.key, system.file, "stale", with_data[0].rule or system.file, newest, ms)
