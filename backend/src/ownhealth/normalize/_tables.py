"""The shipped terminology tables (normalize/terminology), loaded once.

loinc/loinc_subset.tsv  LOINC 2.83 subset (loinc/build_subset.py from loinc/codes.tsv), LOINC License
groups.tsv              one row per alias_group: canonical, SI and conventional units, limits, names
aliases/{es,en,de,fr}.tsv  names as labs print them -> alias_group (+ method)
aliases/panels.tsv      panel and category names, refused
aliases/specimen_twins.tsv  the same analyte in another specimen (creatinine in urine ...)
molar/molar_mass.tsv    molar mass and valence per alias_group, cited
"""

from __future__ import annotations

import hashlib
import re
import unicodedata
from dataclasses import dataclass
from decimal import Decimal
from fractions import Fraction
from functools import cache
from pathlib import Path

DATA = Path(__file__).resolve().parent / "terminology"
LANGS = ("es", "en", "de", "fr")


@dataclass(frozen=True)
class LoincCode:
    code: str
    alias_group: str
    method: str  # "" = method-agnostic
    long_name: str
    component: str
    property: str
    time: str
    system: str
    scale: str
    example_ucum: str
    status: str


@dataclass(frozen=True)
class Group:
    alias_group: str
    canonical: str
    si: str
    conventional: str
    plaus_low: Decimal | None
    plaus_high: Decimal | None
    name_en: str
    name_es: str


@dataclass(frozen=True)
class Alias:
    alias: str
    alias_group: str
    method: str
    lang: str
    source: str
    loinc: str = ""  # a household alias may name its code; its method then steers the pick


@dataclass(frozen=True)
class Molar:
    alias_group: str
    species: str
    g_per_mol: Fraction | None
    valence: int | None
    citation: str


def _rows(path: Path) -> list[dict[str, str]]:
    lines = [ln for ln in path.read_text(encoding="utf-8").splitlines() if ln and not ln.startswith("#")]
    header = lines[0].split("\t")
    out = []
    for ln in lines[1:]:
        cells = ln.split("\t")
        cells += [""] * (len(header) - len(cells))
        out.append(dict(zip(header, (c.strip() for c in cells), strict=False)))
    return out


def fold(text: str) -> str:
    """Matching key for names: lower case, no accents, µ as u, dashes and spaces folded."""
    s = unicodedata.normalize("NFKD", text.replace("µ", "u").replace("μ", "u").replace("ß", "ss"))
    s = "".join(c for c in s if not unicodedata.combining(c)).lower()
    s = s.replace("œ", "oe").replace("–", "-").replace("−", "-").replace(" ", " ")
    s = re.sub(r"\s*([/(),:])\s*", r"\1", s)
    s = re.sub(r"[\s_]+", " ", s).strip(" .:*")
    return s.replace("( ", "(").replace(" )", ")")


@cache
def loinc_codes() -> dict[str, LoincCode]:
    fields = [f for f in LoincCode.__dataclass_fields__ if f != "code"]
    rows = _rows(DATA / "loinc" / "loinc_subset.tsv")
    return {r["loinc"]: LoincCode(code=r["loinc"], **{f: r[f] for f in fields}) for r in rows}


@cache
def codes_by_group() -> dict[str, list[LoincCode]]:
    out: dict[str, list[LoincCode]] = {}
    for c in loinc_codes().values():
        out.setdefault(c.alias_group, []).append(c)
    return out


def _dec(s: str) -> Decimal | None:
    return Decimal(s) if s else None


@cache
def groups() -> dict[str, Group]:
    return {
        r["alias_group"]: Group(
            r["alias_group"],
            r["canonical"],
            r["si"],
            r["conventional"],
            _dec(r["plaus_low"]),
            _dec(r["plaus_high"]),
            r["name_en"],
            r["name_es"],
        )
        for r in _rows(DATA / "groups.tsv")
    }


@cache
def aliases() -> dict[str, list[Alias]]:
    """Folded alias -> every shipped alias row with that key, in file order (es, en, de, fr)."""
    out: dict[str, list[Alias]] = {}
    for lang in LANGS:
        for r in _rows(DATA / "aliases" / f"{lang}.tsv"):
            out.setdefault(fold(r["alias"]), []).append(
                Alias(r["alias"], r["alias_group"], r["method"], lang, r["source"] or "curated")
            )
    return out


@cache
def panels() -> frozenset[str]:
    return frozenset(fold(r["name"]) for r in _rows(DATA / "aliases" / "panels.tsv"))


@cache
def specimen_twins() -> dict[tuple[str, str], str]:
    return {(r["alias_group"], r["specimen"]): r["twin"] for r in _rows(DATA / "aliases" / "specimen_twins.tsv")}


@cache
def molar() -> dict[str, Molar]:
    out = {}
    for r in _rows(DATA / "molar" / "molar_mass.tsv"):
        out[r["alias_group"]] = Molar(
            r["alias_group"],
            r["species"],
            Fraction(r["g_per_mol"]) if r["g_per_mol"] else None,
            int(r["valence"]) if r["valence"] else None,
            r["citation"],
        )
    return out


@cache
def tables_hash() -> str:
    """Short hash over every shipped table: part of DERIVATION_VERSION, so a table edit re-derives."""
    h = hashlib.sha256()
    for p in sorted(DATA.rglob("*.tsv")):
        h.update(p.relative_to(DATA).as_posix().encode())
        h.update(p.read_bytes())
    return h.hexdigest()[:8]
