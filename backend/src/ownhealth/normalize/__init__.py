"""Lane A: normalization (docs/design/ingestion.md section 8, architecture.md section 10).

Printed value, unit and range stay the truth layer; everything here is derived and recomputable.
Importers (lanes B and C) and `propose` (lane F) call these functions; nobody else parses units.

Tables (normalize/terminology, all reviewed files in the repo): a LOINC 2.83 subset (NOTICE.md, LOINC
License), the alias groups with canonical units and plausibility limits, aliases in es/en/de/fr,
refused panel names, specimen twins and a cited molar-mass table. `DERIVATION_VERSION` carries a
hash of every table, so a table edit changes it and `rederive` recomputes the derived layer.
"""

from __future__ import annotations

from dataclasses import dataclass, field
from datetime import date, datetime, timedelta
from decimal import Decimal, localcontext
from fractions import Fraction
from typing import Literal
from uuid import UUID

from sqlalchemy import Connection, text

from ownhealth.core.acceptance import Check
from ownhealth.core.observations import ObservationIn
from ownhealth.normalize import _resolve, _tables, _text, _units
from ownhealth.normalize._text import RefRange

NORMALIZER_VERSION = "0.1.0"
DERIVATION_VERSION = f"norm-{NORMALIZER_VERSION}+t{_tables.tables_hash()}"

Locale = Literal["es", "en", "de", "fr"]

__all__ = [
    "DERIVATION_VERSION",
    "AliasError",
    "Candidate",
    "Locale",
    "Normalized",
    "ParsedValue",
    "RefRange",
    "Resolution",
    "add_alias",
    "canonical_unit",
    "convert",
    "display_name",
    "group_units",
    "interpret",
    "normalize_row",
    "parse_range",
    "parse_value",
    "pick_range",
    "rederive",
    "resolve_analyte",
    "to_ucum",
]

Candidate = _resolve.Candidate


@dataclass(frozen=True)
class ParsedValue:
    printed: str
    value: Decimal | None  # None for text results
    comparator: str | None = None  # <, >, <=, >=
    text_code: str | None = None  # negative | positive | not_detected | trace
    reason: str | None = None  # a review code when the parse is not trusted (ambiguous_separator ...)


@dataclass(frozen=True)
class Resolution:
    alias_group: str  # always set: an uncoded row stays usable under its group
    loinc: str | None
    property: str | None  # MCnc, SCnc, ... of the resolved code
    method: str | None = None  # calculated | direct | ...
    refused: bool = False  # a panel or category name, never coded as an analyte
    reasons: list[str] = field(default_factory=list)  # unmapped name_partial unit_dimension_mismatch method_uncertain
    confidence: float = 0.0  # 0..1 for the chosen candidate
    candidates: list[Candidate] = field(default_factory=list)  # every group the name could be, best first


@dataclass(frozen=True)
class Normalized:
    observation: ObservationIn
    checks: list[Check]


# ------------------------------------------------------------------ values ----


def parse_value(printed: str, *, locale: Locale, unit: str | None = None) -> ParsedValue:
    v = _text.parse_value(printed, locale, to_ucum(unit) if unit else None)
    return ParsedValue(printed, v.value, v.comparator, v.text_code, v.reason)


def to_ucum(printed_unit: str) -> str | None:
    return _units.to_ucum(printed_unit)


def canonical_unit(alias_group: str) -> str | None:
    g = _tables.groups().get(alias_group)
    return g.canonical if g else None


def group_units(alias_group: str) -> dict[str, str] | None:
    """{canonical, si, conventional} for a group, for the `units` preference."""
    g = _tables.groups().get(alias_group)
    return {"canonical": g.canonical, "si": g.si, "conventional": g.conventional} if g else None


def display_name(alias_group: str, lang: str) -> str | None:
    g = _tables.groups().get(alias_group)
    if g is None:
        return None
    return g.name_es if lang == "es" else g.name_en


def _to_decimal(x: Fraction) -> Decimal:
    with localcontext() as ctx:
        ctx.prec = 12
        d = Decimal(x.numerator) / Decimal(x.denominator)
    return Decimal(format(d.normalize(), "f"))


def convert(value: Decimal, from_ucum: str, to_ucum: str, *, alias_group: str) -> Decimal | None:
    """None when the conversion is not possible (the row then gets `unit_unconvertible`)."""
    if from_ucum == to_ucum:
        return value
    m = _tables.molar().get(alias_group)
    out = _units.convert_fraction(
        Fraction(value),
        from_ucum,
        to_ucum,
        alias_group=alias_group,
        molar_mass=m.g_per_mol if m else None,
        valence=m.valence if m else None,
    )
    return None if out is None else _to_decimal(out)


def parse_range(printed: str, *, locale: Locale, unit: str | None = None) -> list[RefRange]:
    return _text.parse_range(printed, locale, to_ucum(unit) if unit else None)


def pick_range(ranges: list[RefRange], *, sex: str | None, age_years: int | None) -> RefRange | None:
    return _text.pick_range(ranges, sex, age_years)


def interpret(
    value: Decimal | None, text_code: str | None, ref: RefRange | None, *, comparator: str | None = None
) -> str | None:
    """HL7 ObservationInterpretation from the printed range only: L, N, H, A. None without a range."""
    return _text.interpret(value, text_code, ref, comparator)


# -------------------------------------------------------------- resolution ----


def resolve_analyte(
    conn: Connection | None,
    printed_name: str,
    *,
    lang: Locale,
    unit: str | None,
    specimen: str | None = None,
    method: str | None = None,
    provider: str | None = None,
    household_id: UUID | None = None,
    text_result: bool = False,
) -> Resolution:
    """Name + unit + specimen to an alias group and a LOINC code, with every candidate scored.

    `unit` is the unit as printed (or UCUM). `household_id` adds that household's own aliases.
    `text_result` (Negativo, trazas) picks the group's ordinal code. A match on only part of the
    name (a parenthesis dropped) carries reason name_partial and is never auto-accepted.
    """
    ucum = to_ucum(unit) if unit else None
    cands, refused, folded = _resolve.resolve(
        conn,
        printed_name,
        lang=lang,
        unit_ucum=ucum,
        specimen=specimen,
        method=method,
        household_id=household_id,
        text_result=text_result,
    )
    if refused:
        return Resolution(_resolve.unmapped_group(folded, provider), None, None, refused=True, reasons=["unmapped"])
    if not cands:
        return Resolution(_resolve.unmapped_group(folded, provider), None, None, reasons=["unmapped"])
    best = cands[0]
    reasons = []
    if best.partial:
        reasons.append("name_partial")
    if not best.unit_ok:
        reasons.append("unit_dimension_mismatch")
    if best.method == "ckd-epi":
        reasons.append("method_uncertain")  # eGFR with no formula year printed: never assume 2021
    return Resolution(
        best.alias_group,
        best.loinc,
        best.property,
        best.method,
        reasons=reasons,
        confidence=best.confidence,
        candidates=cands,
    )


# ----------------------------------------------------------------- a row ----


def _profile(conn: Connection, profile_id) -> dict:
    row = (
        conn.execute(
            text("select household_id, sex_at_birth, birth_date from profile where id = :p"), {"p": profile_id}
        )
        .mappings()
        .first()
    )
    return dict(row) if row else {"household_id": None, "sex_at_birth": None, "birth_date": None}


def _age(birth: date | None, on: date) -> int | None:
    if birth is None:
        return None
    return on.year - birth.year - ((on.month, on.day) < (birth.month, birth.day))


def _ok(name: str) -> Check:
    return Check(name=name, ok=True)


def _fail(name: str, reason: str, detail: str) -> Check:
    return Check(name=name, ok=False, reason=reason, detail=detail)


def derive(
    *,
    printed_name: str,
    printed_unit: str | None,
    value: Decimal | None,
    comparator: str | None,
    text_code: str | None,
    ref: RefRange | None,
    printed_flag: str | None,
    lang: str,
    specimen: str | None,
    method: str | None,
    provider: str | None,
    loinc: str | None,
    conn: Connection | None,
    household_id,
) -> tuple[dict, Resolution, list[Check]]:
    """The derived layer of one row from its truth and parsed layers: what `rederive` recomputes."""
    checks: list[Check] = []
    text_result = value is None and text_code is not None
    res = resolve_analyte(
        conn,
        printed_name,
        lang=lang,
        unit=printed_unit,
        specimen=specimen,
        method=method,
        provider=provider,
        household_id=household_id,
        text_result=text_result,
    )
    code = _tables.loinc_codes().get(loinc) if loinc else None
    if loinc and code is None:
        checks.append(_fail("loinc", "loinc_unknown", f"LOINC {loinc} is not in the shipped cut"))
    elif code is not None:  # a structured import brought its own code: keep it, check it like any other
        ucum0 = to_ucum(printed_unit) if printed_unit else None
        fits = _resolve.unit_fits(code.property, _resolve.effective_unit(code.alias_group, ucum0, code))
        res = Resolution(
            code.alias_group,
            code.code,
            code.property,
            code.method or None,
            reasons=[] if fits or code.scale not in ("Qn", "SemiQn") else ["unit_dimension_mismatch"],
            confidence=1.0,
            candidates=res.candidates,
        )
    c = _tables.loinc_codes().get(res.loinc or "")
    if res.refused:
        checks.append(_fail("loinc", "unmapped", "a panel or category name, not an analyte"))
    elif c is None:
        if not checks:  # loinc_unknown already says it
            checks.append(_fail("loinc", "unmapped", "no alias matches this name"))
    elif "name_partial" in res.reasons:
        checks.append(_fail("loinc", "unmapped", f"only part of the printed name matched {res.alias_group}"))
    elif c.status == "DEPRECATED":
        checks.append(_fail("loinc", "loinc_deprecated", f"LOINC {c.code} is deprecated"))
    elif not checks:
        checks.append(_ok("loinc"))
    ucum = _resolve.effective_unit(res.alias_group, to_ucum(printed_unit) if printed_unit else None, c)
    quantitative = c is None or c.scale in ("Qn", "SemiQn")
    if c is not None and quantitative and value is not None:
        if "unit_dimension_mismatch" in res.reasons or ucum is None:
            what = f"unit {printed_unit!r} ({_units.dims_name(_units.dims_of(ucum))})" if printed_unit else "no unit"
            checks.append(
                _fail(
                    "unit_dimension",
                    "unit_dimension_mismatch",
                    f"{what} does not fit LOINC {c.code} property {c.property}",
                )
            )
        else:
            checks.append(_ok("unit_dimension"))
    if c is not None and quantitative and text_result:
        checks.append(_fail("plausibility", "implausible", f"a text result on quantitative LOINC {c.code}"))
    canonical_u = canonical_unit(res.alias_group)
    canonical_v = None
    if value is not None and ucum and canonical_u:
        canonical_v = convert(value, ucum, canonical_u, alias_group=res.alias_group)
    if value is not None and canonical_u and quantitative:
        if canonical_v is None:
            checks.append(
                _fail(
                    "converts",
                    "unit_unconvertible",
                    f"{printed_unit or 'no unit'} does not convert to {canonical_u} for {res.alias_group}",
                )
            )
        else:
            checks.append(_ok("converts"))
    g = _tables.groups().get(res.alias_group)
    if canonical_v is not None and g and (g.plaus_low is not None or g.plaus_high is not None):
        checks.extend(_plausibility(canonical_v, g, canonical_u))
    interp = interpret(value, text_code, ref, comparator=comparator)
    flag = _text.printed_flag_code(printed_flag)
    if flag in ("HH", "LL") and interp == flag[0]:
        interp = flag  # critical only when the lab prints it
    if _text.flag_disagrees(flag, interp):
        checks.append(
            _fail("flag", "flag_mismatch", f"printed flag {printed_flag!r} but the printed range gives {interp}")
        )
    elif flag is not None and interp is not None:
        checks.append(_ok("flag"))
    derived = {
        "loinc": res.loinc,
        "alias_group": res.alias_group,
        "method": res.method,
        "unit_ucum": ucum,
        "value_canonical": canonical_v,
        "unit_canonical": canonical_u if canonical_v is not None else None,
        "interpretation": interp,
        "specimen": c.system if c else _resolve.specimen_code(specimen),
    }
    return derived, res, checks


def _plausibility(v: Decimal, g: _tables.Group, unit: str) -> list[Check]:
    def inside(x: Decimal) -> bool:
        return (g.plaus_low is None or x >= g.plaus_low) and (g.plaus_high is None or x <= g.plaus_high)

    if inside(v):
        return [_ok("plausibility")]
    out = [_fail("plausibility", "implausible", f"{v} {unit} is outside {g.plaus_low}-{g.plaus_high} {unit}")]
    for factor in (Decimal(1000), Decimal("0.001")):
        if inside(v * factor):
            out.append(
                _fail(
                    "factor_1000",
                    "suspect_separator",
                    f"{v * factor} {unit} would be plausible: a thousands or decimal separator misread?",
                )
            )
            break
    return out


def normalize_row(
    conn: Connection,
    *,
    profile_id,
    printed_name: str,
    printed_value: str,
    printed_unit: str | None,
    printed_range: str | None,
    printed_flag: str | None,
    effective_at,
    lang: Locale,
    specimen: str | None = None,
    provider: str | None = None,
    loinc: str | None = None,
    method: str | None = None,
    document_id: UUID | None = None,
) -> Normalized:
    """One printed lab row to an ObservationIn plus every validator result (dimension, plausibility,
    flag mismatch, range parse, duplicate, conflict). The caller adds grounding and identity checks and
    routes the row through `core.review.route_fact`. `loinc` is a code a structured source sent.
    `document_id` is the source the row comes from: rows of one source are never duplicates."""
    prof = _profile(conn, profile_id)
    pv = parse_value(printed_value, locale=lang, unit=printed_unit)
    checks: list[Check] = []
    if pv.reason:
        checks.append(_fail("value", pv.reason, f"the value {printed_value!r} reads two ways in a {lang} report"))
    if _text.looks_like_range(printed_value):
        checks.append(_fail("value_is_range", "value_is_range", f"the value {printed_value!r} looks like a range"))
    elif pv.value is None and pv.text_code is None:
        checks.append(_fail("value", "implausible", f"no number or known result in {printed_value!r}"))

    ranges = parse_range(printed_range, locale=lang, unit=printed_unit) if printed_range else []
    on = effective_at.date() if isinstance(effective_at, datetime) else effective_at
    ref = pick_range(ranges, sex=prof["sex_at_birth"], age_years=_age(prof["birth_date"], on)) if ranges else None
    if printed_range and printed_range.strip():
        if not ranges:
            checks.append(_fail("range", "range_parse", f"cannot read the range {printed_range!r}"))
        elif ref is None:
            checks.append(_fail("range", "range_parse", "no printed range applies to this profile's sex and age"))
        elif ref.low is not None and ref.high is not None and ref.low >= ref.high:
            checks.append(_fail("range", "range_parse", f"low {ref.low} is not below high {ref.high}"))
            ref = None
        else:
            checks.append(_ok("range"))

    derived, res, dchecks = derive(
        printed_name=printed_name,
        printed_unit=printed_unit,
        value=pv.value,
        comparator=pv.comparator,
        text_code=pv.text_code,
        ref=ref,
        printed_flag=printed_flag,
        lang=lang,
        specimen=specimen,
        method=method,
        provider=provider,
        loinc=loinc,
        conn=conn,
        household_id=prof["household_id"],
    )
    checks.extend(dchecks)
    checks.extend(_duplicates(conn, profile_id, derived, pv, effective_at, document_id))
    obs = ObservationIn(
        profile_id=profile_id,
        printed_name=printed_name,
        printed_value=printed_value,
        printed_unit=printed_unit,
        printed_ref_range=printed_range,
        printed_flag=printed_flag,
        value_num=pv.value,
        value_cmp=pv.comparator,
        value_text=printed_value if pv.text_code else None,
        value_code=pv.text_code,
        method=derived["method"],
        ref_low=ref.low if ref else None,
        ref_high=ref.high if ref else None,
        ref_applies=(ref.applies_to or None) if ref else None,
        specimen=derived["specimen"],
        loinc=derived["loinc"],
        alias_group=derived["alias_group"],
        unit_ucum=derived["unit_ucum"],
        value_canonical=derived["value_canonical"],
        unit_canonical=derived["unit_canonical"],
        interpretation=derived["interpretation"],
        derivation_version=DERIVATION_VERSION,
        effective_at=effective_at,
    )
    return Normalized(observation=obs, checks=checks)


def _duplicates(
    conn: Connection, profile_id, derived: dict, pv: ParsedValue, effective_at, document_id: UUID | None = None
) -> list[Check]:
    """One fact from two sources (architecture.md, dedup): same alias group and day, equal value, the
    same comparator and the same method (both empty counts as equal), from another source. A direct
    and a calculated LDL are two analytes, not a conflict; `<5` beside `5` is a conflict; rows of one
    source are never merged."""
    if not isinstance(effective_at, datetime):
        return []
    rows = (
        conn.execute(
            text(
                "select id, value_canonical, value_num, value_code, value_cmp, method, document_id from observation"
                " where profile_id = :p and alias_group = :g and status in ('current', 'provisional')"
                " and superseded_at is null and effective_at between :a and :b order by effective_at, id"
            ),
            {
                "p": profile_id,
                "g": derived["alias_group"],
                "a": effective_at - timedelta(days=1),
                "b": effective_at + timedelta(days=1),
            },
        )
        .mappings()
        .all()
    )
    rows = [r for r in rows if (r["method"] or None) == (derived["method"] or None)]
    if not rows:
        return []
    mine = derived["value_canonical"] if derived["value_canonical"] is not None else pv.value
    conflict = None
    for r in rows:
        theirs = r["value_canonical"] if r["value_canonical"] is not None else r["value_num"]
        same_value = (
            mine is not None
            and theirs is not None
            and abs(Decimal(mine) - Decimal(theirs)) <= abs(Decimal(theirs)) * Decimal("1e-6")
        ) or bool(pv.text_code and pv.text_code == r["value_code"])
        if same_value and (r["value_cmp"] or None) == (pv.comparator or None):
            if document_id is not None and r["document_id"] == document_id:
                continue  # the same source twice: never merged
            return [_fail("duplicate", "duplicate", f"the same value is already accepted as observation {r['id']}")]
        conflict = conflict or r["id"]
    if conflict is None:
        return []
    return [_fail("conflict", "conflict", f"another value for this analyte and day is accepted ({conflict})")]


# ---------------------------------------------------------------- aliases ----


class AliasError(ValueError):
    pass


def add_alias(
    conn: Connection,
    *,
    household_id: UUID,
    alias: str,
    alias_group: str,
    lang: str | None,
    added_by: UUID | None,
    unit: str | None = None,
    specimen: str | None = None,
    loinc: str | None = None,
) -> UUID:
    """A household's own alias ("add alias" from the review queue). The group must be a curated one."""
    if alias_group not in _tables.groups():
        raise AliasError(f"unknown alias_group {alias_group!r}")
    if loinc and (_tables.loinc_codes().get(loinc) is None or _tables.loinc_codes()[loinc].alias_group != alias_group):
        raise AliasError(f"LOINC {loinc} is not a code of {alias_group}")
    norm = _tables.fold(alias)
    if not norm or norm in _tables.panels():
        raise AliasError("a panel or category name cannot be an alias")
    return conn.execute(
        text(
            "insert into analyte_alias (household_id, alias, alias_norm, lang, alias_group, loinc, unit, specimen,"
            " added_by) values (:h, :a, :n, :l, :g, :c, :u, :s, :by)"
            " on conflict (coalesce(household_id::text, ''), alias_norm, coalesce(lang, ''), coalesce(unit, ''),"
            " coalesce(specimen, '')) do update set alias_group = excluded.alias_group, loinc = excluded.loinc,"
            " alias = excluded.alias, added_by = excluded.added_by returning id"
        ),
        {
            "h": household_id,
            "a": alias,
            "n": norm,
            "l": lang,
            "g": alias_group,
            "c": loinc,
            "u": unit,
            "s": specimen,
            "by": added_by,
        },
    ).scalar_one()


# ---------------------------------------------------------------- rederive ----

_DERIVED = ("loinc", "alias_group", "method", "unit_ucum", "value_canonical", "unit_canonical", "interpretation")


def rederive(conn: Connection, *, profile_id=None, since: date | None = None) -> int:
    """Recompute the derived layer (loinc, alias group, canonical, interpretation) after a table change.

    Observation rows are immutable, so a changed row is superseded by a copy with the new derived
    layer and the same source; truth and parsed columns never change. Only current rows are
    touched (a provisional row waits for its final report). One `rederive_diff` review item lists
    what changed, and each touched profile gets a `state.rebuild` job. Returns the changed count.
    """
    from ownhealth.core.observations import supersede_observation
    from ownhealth.core.review import ReviewItemIn, create_item
    from ownhealth.ingest.fhir.mapping import received_ucum
    from ownhealth.jobs import enqueue

    where = ["o.status = 'current'", "o.superseded_at is null", "o.category = 'laboratory'"]
    params: dict = {}
    if profile_id is not None:
        where.append("o.profile_id = :p")
        params["p"] = profile_id
    if since is not None:
        where.append("o.effective_at >= :since")
        params["since"] = datetime.combine(since, datetime.min.time()).astimezone()
    rows = (
        conn.execute(
            text(
                "select o.*, d.language as doc_lang, d.provider_name, p.household_id as household_id,"
                " p.sex_at_birth, p.birth_date, s.kind as src_kind from observation o"
                " join profile p on p.id = o.profile_id left join src s on s.id = o.src_id"
                f" left join document d on d.id = o.document_id where {' and '.join(where)}"
                " order by o.profile_id, o.effective_at, o.id for update of o"
            ),
            params,
        )
        .mappings()
        .all()
    )
    changes: dict[UUID, list[dict]] = {}
    households: dict[UUID, UUID] = {}
    for r in rows:
        lang = r["doc_lang"] if r["doc_lang"] in _tables.LANGS else "en"
        ref = None
        if r["ref_low"] is not None or r["ref_high"] is not None:
            ref = RefRange(r["ref_low"], r["ref_high"], r["printed_ref_range"] or "", dict(r["ref_applies"] or {}))
            if r["printed_ref_range"]:  # keep exclusive bounds and text codes as printed
                again = pick_range(
                    parse_range(r["printed_ref_range"], locale=lang, unit=r["printed_unit"]),
                    sex=r["sex_at_birth"],
                    age_years=_age(r["birth_date"], r["effective_at"].date()),
                )
                if again is not None and (again.low, again.high) == (r["ref_low"], r["ref_high"]):
                    ref = again
        elif r["printed_ref_range"]:
            ref = pick_range(
                parse_range(r["printed_ref_range"], locale=lang, unit=r["printed_unit"]),
                sex=r["sex_at_birth"],
                age_years=_age(r["birth_date"], r["effective_at"].date()),
            )
        # a structured source's LOINC and UCUM code are truth: keep them, whoever accepted the row
        received = r["accepted_by"] == "import" or r["src_kind"] == "fhir_import"
        keep_code = r["loinc"] if received and r["loinc"] in _tables.loinc_codes() else None
        keep_unit = received_ucum(r["fhir"], r["loinc"]) if r["src_kind"] == "fhir_import" else None
        derived, _res, _ = derive(
            printed_name=r["printed_name"],
            printed_unit=keep_unit or r["printed_unit"],
            value=r["value_num"],
            comparator=r["value_cmp"],
            text_code=r["value_code"],
            ref=ref,
            printed_flag=r["printed_flag"],
            lang=lang,
            specimen=r["specimen"],
            method=r["method"] if r["method"] in ("direct", "calculated") else None,
            provider=r["provider_name"],
            loinc=keep_code,
            conn=conn,
            household_id=r["household_id"],
        )
        if derived["loinc"] is None and r["loinc"] is not None:
            continue  # never lose a code a row already had
        diff = {k: (r[k], derived[k]) for k in _DERIVED if not _same(r[k], derived[k])}
        if not diff:
            continue
        new = ObservationIn(
            **{
                k: r[k]
                for k in ObservationIn.model_fields
                if k not in ("fhir", *_DERIVED, "derivation_version", "specimen", "ref_applies")
            },
            **{k: derived[k] for k in _DERIVED},
            specimen=r["specimen"],
            ref_applies=r["ref_applies"],
            derivation_version=DERIVATION_VERSION,
            fhir=r["fhir"] if received else None,
        )
        committed = supersede_observation(
            conn,
            r["id"],
            new,
            src=r["src_id"],
            accepted_by=r["accepted_by"],
            reason=f"rederive {DERIVATION_VERSION}",
            reviewed_by=r["reviewed_by"],
        )
        households[r["profile_id"]] = r["household_id"]
        changes.setdefault(r["profile_id"], []).append(
            {
                "old": str(r["id"]),
                "new": str(committed.id),
                "src": r["src_id"],
                "changed": {k: [_json(a), _json(b)] for k, (a, b) in diff.items()},
            }
        )
    for pid, items in changes.items():
        create_item(
            conn,
            ReviewItemIn(
                kind="rederive_diff",
                household_id=households[pid],
                profile_id=pid,
                payload={"derivation_version": DERIVATION_VERSION, "rows": items},
            ),
        )
        enqueue(conn, "state.rebuild", {"profile_id": str(pid)}, dedupe_key=f"state.rebuild:{pid}")
    return sum(len(v) for v in changes.values())


def _same(a, b) -> bool:
    if isinstance(a, Decimal | int | float) and isinstance(b, Decimal | int | float):
        return Decimal(a) == Decimal(b)
    return a == b


def _json(v):
    return str(v) if isinstance(v, Decimal) else v
