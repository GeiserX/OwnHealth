"""Analyte resolution from name + unit + specimen, never name alone (ingestion.md 8.2).

1. Fold the printed name; refuse panel and category names.
2. Find alias groups for the name: the shipped tables, then the household's own aliases, with the
   name as printed, without specimen words, without a parenthesis, and the parenthesis alone.
3. Move to the specimen twin (creatinine read in urine is urine_creatinine) and the unit twin
   (Neutrófilos in % is neutrophils_pct).
4. Inside the group, the unit's dimension picks the LOINC property (LDL in mg/dL is MCnc, in mmol/L
   SCnc), the specimen picks the system, and the method picks between calculated and direct.
"""

from __future__ import annotations

import re
from dataclasses import dataclass, replace

from sqlalchemy import Connection, text

from ownhealth.normalize import _tables as T
from ownhealth.normalize import _units as U

# LOINC property -> the unit dimensions it accepts (8.3). Ord / Nom scales carry no unit.
_D = U._dim
PROPERTY_DIMS: dict[str, tuple[tuple[int, ...], ...]] = {
    "MCnc": (_D(g=1, m=-3),),
    "EntMCnc": (_D(g=1, m=-3),),
    "SCnc": (_D(mol=1, m=-3), _D(EQ=1, m=-3)),
    "ACnc": (_D(IU=1, m=-3), _D(ARB=1, m=-3)),
    "CCnc": (_D(mol=1, s=-1, m=-3),),
    "NCnc": (_D(m=-3),),
    "MFr": (U._ZERO,),
    "NFr": (U._ZERO,),
    "VFr": (U._ZERO,),
    "SFr": (U._ZERO,),
    "MRto": (U._ZERO,),
    "Ratio": (U._ZERO, _D(g=1, mol=-1)),
    "RelTime": (U._ZERO,),
    "SpGrav": (U._ZERO,),
    "DistWidth": (U._ZERO,),
    "EntMass": (_D(g=1),),
    "EntMeanVol": (_D(m=3),),
    "Vol": (_D(m=3),),
    "ArVRat": (_D(m=3, s=-1),),
    "VRat": (_D(m=3, s=-1),),
    "Osmol": (_D(mol=1, g=-1),),
    "PPres": (_D(g=1, m=-1, s=-2),),
    "LsCnc": (_D(PH=1),),
    "Time": (_D(s=1),),
    "Sedimentation Rate": (_D(m=1, s=-1),),
    "Naric": (_D(HPF=-1),),
    "MRat": (_D(g=1, s=-1),),
}
UNITLESS_CANONICAL = {"{INR}", "{ratio}", "{Spec_grav}", "[pH]"}

_SPECIMEN_HINTS = {
    "Urine": r"\b(?:orina|urine|urin|urinaire|urinario|urinaria|harn|u-)\b|^u ",
    "Bld": r"\b(?:sangre total|whole blood|vollblut|sang total|capilar|capillary|kapillar)\b",
    "Ser/Plas": r"\b(?:suero|serum|plasma|serico|serica|serique|plasmatico|s-|p-)\b|^[sp] ",
}
_SPECIMEN_WORDS = re.compile(
    r"\b(?:en|in|im|de|dans|del|la|le|les)?\s*\(?\b(?:orina|urine|urin|suero|serum|plasma|sangre|blood|blut|sang)\b\)?"
    r"|^[spu]-\s*|^[spu] "
)
_SPECIMEN_FREE = {  # free-text specimen hints from importers
    "urine": "Urine",
    "orina": "Urine",
    "urin": "Urine",
    "ur": "Urine",
    "blood": "Bld",
    "bld": "Bld",
    "sangre": "Bld",
    "whole blood": "Bld",
    "blut": "Bld",
    "sang": "Bld",
    "serum": "Ser/Plas",
    "suero": "Ser/Plas",
    "plasma": "Ser/Plas",
    "ser/plas": "Ser/Plas",
    "ser": "Ser/Plas",
    "plas": "Ser/Plas",
    "ppp": "Ser/Plas",
    "serum or plasma": "Ser/Plas",
}
_SYSTEMS = {
    "Urine": {"Urine", "Urine sed", "Urine+Ser/Plas"},
    "Bld": {"Bld", "BldA", "RBC", "Ser/Plas/Bld"},
    "Ser/Plas": {"Ser/Plas", "Ser", "Plas", "PPP", "Ser/Plas/Bld", "Urine+Ser/Plas"},
}


@dataclass(frozen=True)
class Candidate:
    alias_group: str
    loinc: str | None
    property: str | None
    method: str | None
    confidence: float
    matched: str  # the alias row that matched, or "loinc" for a given code
    unit_ok: bool
    specimen_ok: bool
    partial: bool = False  # only part of the printed name matched: never auto-accepted


def specimen_code(hint: str | None) -> str | None:
    if not hint:
        return None
    return _SPECIMEN_FREE.get(T.fold(hint)) or (hint if hint in _SYSTEMS else None)


def specimen_from_name(folded: str) -> str | None:
    for code, pattern in _SPECIMEN_HINTS.items():
        if re.search(pattern, folded):
            return code
    return None


_SUBSTANCE_FRACTION = re.compile(r"mol/(?:[a-z]?)mol$")


def unit_fits(prop: str, ucum: str | None) -> bool:
    """The unit's dimension matches the LOINC property. mmol/mol is a substance fraction (SFr) only."""
    allowed = PROPERTY_DIMS.get(prop)
    if allowed is None:
        return True
    dims = U.dims_of(ucum)
    if dims is None or dims not in allowed:
        return False
    if dims == U._ZERO and prop in ("SFr", "MFr", "NFr", "VFr"):
        return (prop == "SFr") == bool(_SUBSTANCE_FRACTION.search(ucum or ""))
    return True


def system_fits(system: str, specimen: str | None) -> bool:
    return specimen is None or system in _SYSTEMS.get(specimen, {system})


_PRINTED_U = re.compile(r"(?<![\w\[])([kmu]?)U(?![\w\]])")


def unit_reading(ucum: str | None, code: T.LoincCode | None) -> str | None:
    """How the code reads a printed U or IU. Units never convert between [IU] and U (that stays in
    _units); this only decides what a label meant, from the code's property:
      * ACnc with an [arb'U] example (CA 125): U/mL is [arb'U]/mL;
      * ACnc with an [IU] example (TSH, FSH): mU/L is m[IU]/L, as the German "mU/l" means;
      * CCnc (enzymes): UI/L is U/L, the enzyme unit that was named the international unit.
    """
    if not ucum or code is None:
        return ucum
    if code.property == "ACnc" and "[arb'U]" in code.example_ucum:
        return _PRINTED_U.sub(r"\1[arb'U]", ucum)
    if code.property == "ACnc" and "[IU]" in code.example_ucum:
        return _PRINTED_U.sub(r"\1[IU]", ucum)
    if code.property == "CCnc":
        return ucum.replace("[IU]", "U")
    return ucum


_METHOD_WORDS = {
    "calculated": r"calculad[oa]|calculated|calc\.?|berechnet|calcule|friedewald|nih|martin",
    "direct": r"direct[oa]?|direkt|directe|homogeneo|homogeneous",
}
EXACT = 0.95  # a key at or above this is the printed name up to specimen and method words


def method_from_name(folded: str) -> str | None:
    for method, pattern in _METHOD_WORDS.items():
        if re.search(rf"\b(?:{pattern})(?:\b|$)", folded):
            return method
    return None


def _squash(s: str) -> str:
    return re.sub(r"\s+", " ", s).strip(" ,-()")


_METHOD_RE = re.compile(rf"\(?\b(?:{'|'.join(_METHOD_WORDS.values())})(?:\b|$)\)?")


def group_methods(group: str) -> set[str]:
    """The methods a group's LOINC codes tell apart (LDL: calculated, direct); empty for most groups."""
    return {c.method for c in T.codes_by_group().get(group, []) if c.method}


def name_keys(folded: str) -> list[tuple[str, float, str | None]]:
    """(key, confidence, stripped method). The name as printed; then without specimen or method words
    and with parentheses unwrapped (still exact); then, as partial matches that never auto-accept,
    without a parenthesis or the parenthesis alone. A key with a method word stripped names that
    method: it is exact only for a group whose codes have it (LDL direct), never for "Bilirubin,
    direkt", where the word names another analyte."""
    keys: list[tuple[str, float, str | None]] = [(folded, 1.0, None)]
    unwrapped = _squash(re.sub(r"[()]", " ", folded))
    keys.append((unwrapped, 0.98, None))
    no_spec = _squash(_SPECIMEN_WORDS.sub(" ", folded))
    keys.append((no_spec, EXACT, None))
    for base in (unwrapped, no_spec):
        stripped = _squash(_METHOD_RE.sub(" ", base))
        keys.append((stripped, EXACT, method_from_name(base) if stripped != base else None))
    for base, conf in ((folded, 0.9), (no_spec, 0.88)):  # partial: below EXACT
        outside = re.sub(r"\([^)]*\)", " ", base)
        keys.append((re.sub(r"\s+", " ", outside).strip(" ,-"), conf, None))
        for inner in re.findall(r"\(([^)]*)\)", base):
            keys.append((inner.strip(), conf - 0.05, None))
    seen, out = set(), []
    for k, c, m in keys:
        if k and k not in seen:
            seen.add(k)
            out.append((k, c, m))
    return out


def user_aliases(conn: Connection | None, household_id, key: str) -> list[T.Alias]:
    if conn is None or household_id is None:
        return []
    rows = conn.execute(
        text(
            "select alias, alias_group, coalesce(lang, '') as lang, coalesce(loinc, '') as loinc from analyte_alias"
            " where alias_norm = :k and (household_id = :h or household_id is null) order by created_at"
        ),
        {"k": key, "h": household_id},
    ).mappings()
    codes = T.loinc_codes()
    return [
        T.Alias(
            r["alias"],
            r["alias_group"],
            codes[r["loinc"]].method if r["loinc"] in codes else "",
            r["lang"],
            "user",
            r["loinc"],
        )
        for r in rows
    ]


def _twin(group: str, specimen: str | None, dims: tuple[int, ...] | None, text_result: bool = False) -> str:
    if specimen and (t := T.specimen_twins().get((group, specimen))):
        group = t
    if text_result and (t := T.specimen_twins().get((group, "text"))):
        group = t
    if dims is not None:
        if group.endswith("_abs") and dims == U._ZERO:
            twin = group[:-4] + "_pct"
        elif group.endswith("_pct") and dims == U._dim(m=-3):
            twin = group[:-4] + "_abs"
        else:
            twin = group
        group = twin if twin in T.groups() else group
    return group


def effective_unit(group: str, unit_ucum: str | None, code: T.LoincCode | None) -> str | None:
    """The unit as the code reads it: unitless groups need none, CA 125 reads U as [arb'U]."""
    if unit_ucum is None:
        g = T.groups().get(group)
        return g.canonical if g and g.canonical in UNITLESS_CANONICAL else None
    return unit_reading(unit_ucum, code)


def pick_code(group: str, unit_ucum: str | None, specimen: str | None, method: str | None, text_result: bool = False):
    """The best LOINC code of a group for this unit, specimen and method -> (code, unit_ok, specimen_ok)."""
    codes = T.codes_by_group().get(group, [])
    if not codes:
        return None, False, False

    def score(c: T.LoincCode) -> tuple:
        if text_result:  # Negativo, trazas: the presence code, never the quantitative one
            ok_unit = c.scale in ("Ord", "Nom")
        elif c.scale in ("Qn", "SemiQn"):
            ok_unit = unit_fits(c.property, effective_unit(group, unit_ucum, c))
        else:
            ok_unit = unit_ucum is None
        return (
            ok_unit,
            system_fits(c.system, specimen),
            c.status in ("ACTIVE", "TRIAL"),
            (c.method == method) if method else (c.method == ""),
            c.method == "",
        )

    best = max(codes, key=score)  # max keeps the first of equals: file order breaks ties
    s = score(best)
    return best, s[0], s[1]


def resolve(
    conn,
    printed_name: str,
    *,
    lang: str,
    unit_ucum: str | None,
    specimen: str | None,
    method: str | None,
    household_id=None,
    text_result: bool = False,
) -> tuple[list[Candidate], bool, str]:
    """Every candidate, best first; (candidates, refused, folded_name)."""
    folded = T.fold(printed_name)
    if folded in T.panels():
        return [], True, folded
    spec = specimen_code(specimen) or specimen_from_name(folded)
    dims = U.dims_of(unit_ucum)
    named_method = method_from_name(folded)
    found: dict[str, Candidate] = {}
    for key, name_conf, stripped in name_keys(folded):
        rows = [*user_aliases(conn, household_id, key), *T.aliases().get(key, [])]
        for a in rows:
            group = _twin(a.alias_group, spec, dims, text_result)
            valid = group_methods(group)
            m = next((x for x in (method, a.method, named_method) if x and x in valid), None)
            code, unit_ok, spec_ok = pick_code(group, unit_ucum, spec, m, text_result)
            conf = name_conf * (1.0 if a.lang in (lang, "") else 0.9) * (1.0 if a.source == "user" else 0.99)
            conf *= 1.0 if unit_ok else 0.5
            conf *= 1.0 if spec_ok else 0.7
            cand = Candidate(
                group,
                code.code if code else None,
                code.property if code else None,
                (m or (code.method if code else "")) or None,
                round(conf, 3),
                a.alias,
                unit_ok,
                spec_ok,
                name_conf < EXACT or (stripped is not None and stripped not in valid),
            )
            if group not in found or found[group].confidence < cand.confidence:
                found[group] = cand
        if found and name_conf >= EXACT and any(c.unit_ok and c.specimen_ok and not c.partial for c in found.values()):
            break
    for group, cand in list(found.items()):
        if cand.partial and _glossed(conn, household_id, folded, group, spec, dims, text_result):
            found[group] = replace(cand, partial=False, confidence=round(cand.confidence / 0.9 * EXACT, 3))
    cands = sorted(found.values(), key=lambda c: -c.confidence)
    return cands, False, folded


def _glossed(conn, household_id, folded: str, group: str, spec, dims, text_result: bool) -> bool:
    """ "GPT (ALT)": the name and its parenthesis are each a name of the same group, so the whole
    name is that group, not a partial match."""
    inners = re.findall(r"\(([^)]*)\)", folded)
    outside = _squash(re.sub(r"\([^)]*\)", " ", folded))
    parts = [p.strip() for p in (outside, *inners) if p.strip()]
    if len(parts) < 2:
        return False
    for part in parts:
        rows = [*user_aliases(conn, household_id, part), *T.aliases().get(part, [])]
        if not any(_twin(a.alias_group, spec, dims, text_result) == group for a in rows):
            return False
    return True


def unmapped_group(folded: str, provider: str | None) -> str:
    slug = re.sub(r"[^a-z0-9]+", "_", folded).strip("_") or "unnamed"
    if provider:
        slug += "@" + (re.sub(r"[^a-z0-9]+", "_", T.fold(provider)).strip("_") or "provider")
    return slug[:120]
