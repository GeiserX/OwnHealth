"""UCUM for the units labs print: parse, map printed spellings to UCUM, dimensions, conversion.

A deliberately small UCUM subset (ingestion.md 8.3): the atoms and prefixes that appear on lab
reports, products and quotients, integer exponents, `10*N` factors and `{annotations}`. Every
unit becomes (factor to base units, dimension vector). Base units: g, m, s, mol, plus four
dimensions UCUM treats as arbitrary and we never cross: [IU], [arb'U], eq, [pH], and [HPF].

Traps this module exists to hold:
* `[IU]` and `U` are different things (U is µmol/min, an enzyme activity) and never convert.
* `eq` is kept apart from `mol`; only an analyte's valence converts mEq to mmol.
* Mass <-> substance needs the analyte's molar mass (data/molar), never a guess.
* `%` <-> `mmol/mol` for HbA1c is the NGSP/IFCC master equation, not a factor of 10.
"""

from __future__ import annotations

import re
import unicodedata
from dataclasses import dataclass
from fractions import Fraction

BASES = ("g", "m", "s", "mol", "IU", "ARB", "EQ", "PH", "HPF")
_ZERO = (0,) * len(BASES)


def _dim(**exps: int) -> tuple[int, ...]:
    return tuple(exps.get(b, 0) for b in BASES)


# canonical atom -> (factor to base, dimension, prefixable)
ATOMS: dict[str, tuple[Fraction, tuple[int, ...], bool]] = {
    "g": (Fraction(1), _dim(g=1), True),
    "m": (Fraction(1), _dim(m=1), True),
    "L": (Fraction(1, 1000), _dim(m=3), True),
    "s": (Fraction(1), _dim(s=1), True),
    "min": (Fraction(60), _dim(s=1), False),
    "h": (Fraction(3600), _dim(s=1), False),
    "d": (Fraction(86400), _dim(s=1), False),
    "mol": (Fraction(1), _dim(mol=1), True),
    "kat": (Fraction(1), _dim(mol=1, s=-1), True),
    "U": (Fraction(1, 60_000_000), _dim(mol=1, s=-1), True),  # 1 U = 1 µmol/min
    "[IU]": (Fraction(1), _dim(IU=1), True),
    "[arb'U]": (Fraction(1), _dim(ARB=1), True),
    "eq": (Fraction(1), _dim(EQ=1), True),
    "osm": (Fraction(1), _dim(mol=1), True),
    "[pH]": (Fraction(1), _dim(PH=1), False),
    "[HPF]": (Fraction(1), _dim(HPF=1), False),
    "%": (Fraction(1, 100), _ZERO, False),
    "mm[Hg]": (Fraction(133322387415, 1000000), _dim(g=1, m=-1, s=-2), False),
}
PREFIXES: dict[str, Fraction] = {
    "k": Fraction(1000),
    "d": Fraction(1, 10),
    "c": Fraction(1, 100),
    "m": Fraction(1, 1000),
    "u": Fraction(1, 10**6),
    "n": Fraction(1, 10**9),
    "p": Fraction(1, 10**12),
    "f": Fraction(1, 10**15),
}

# Lower-case spellings seen on reports -> canonical atom. Tried whole first, then prefix + atom.
_ATOM_LC: dict[str, str] = {
    "g": "g",
    "gr": "g",
    "m": "m",
    "l": "L",
    "s": "s",
    "seg": "s",
    "sec": "s",
    "sg": "s",
    "min": "min",
    "h": "h",
    "hr": "h",
    "hrs": "h",
    "hora": "h",
    "d": "d",
    "dia": "d",
    "day": "d",
    "mol": "mol",
    "kat": "kat",
    "u": "U",
    "iu": "[IU]",
    "ui": "[IU]",
    "ie": "[IU]",
    "[iu]": "[IU]",
    "[arb'u]": "[arb'U]",
    "eq": "eq",
    "osm": "osm",
    "[ph]": "[pH]",
    "[hpf]": "[HPF]",
    "hpf": "[HPF]",
    "campo": "[HPF]",
    "%": "%",
    "mmhg": "mm[Hg]",
    "mm[hg]": "mm[Hg]",
}
_PREFIXABLE_LC = {k for k, v in _ATOM_LC.items() if ATOMS[v][2] and len(k) <= 4}

# Whole printed spellings (folded, lower case, no spaces) that the grammar cannot read.
# French labs print cell counts as G/L (giga, 10^9) and T/L (tera, 10^12); lower case g/L is grams,
# so these are read before the unit is lower-cased.
CASED: dict[str, str] = {
    "G/L": "10*9/L",
    "G/l": "10*9/L",
    "Giga/L": "10*9/L",
    "Giga/l": "10*9/L",
    "T/L": "10*12/L",
    "T/l": "10*12/L",
    "Tera/L": "10*12/L",
    "Tera/l": "10*12/L",
}

SYNONYMS: dict[str, str] = {
    "mg%": "mg/dL",
    "g%": "g/dL",
    "mg/100ml": "mg/dL",
    "miles/ul": "10*3/uL",
    "mil/ul": "10*3/uL",
    "x1000/ul": "10*3/uL",
    "k/ul": "10*3/uL",
    "thou/ul": "10*3/uL",
    "mill/ul": "10*6/uL",
    "millones/ul": "10*6/uL",
    "m/ul": "10*6/uL",
    "cel/ul": "/uL",
    "cél/ul": "/uL",
    "celulas/ul": "/uL",
    "células/ul": "/uL",
    "cells/ul": "/uL",
    "/mm3": "/mm3",
    "cel/mm3": "/mm3",
    "/campo": "/[HPF]",
    "cel/campo": "/[HPF]",
    "xcampo": "/[HPF]",
    "porcampo": "/[HPF]",
    "/hpf": "/[HPF]",
    "/cga": "/[HPF]",
    "mm/1ah": "mm/h",
    "mm/1ªh": "mm/h",
    "mm/1h": "mm/h",
    "mm/1ahora": "mm/h",
    "mm/1ªhora": "mm/h",
    "mm/hr": "mm/h",
    "mm/hora": "mm/h",
    "ratio": "{ratio}",
    "indice": "{ratio}",
    "índice": "{ratio}",
    "inr": "{INR}",
    "ph": "[pH]",
    "unidadesdeph": "[pH]",
    "g/24h": "g/(24.h)",
    "g/24horas": "g/(24.h)",
    "mg/24h": "mg/(24.h)",
    "mg/24horas": "mg/(24.h)",
    "l/24h": "L/(24.h)",
    "ml/24h": "mL/(24.h)",
}
_TEN_POWER = re.compile(r"^(?:x|\*)?10(?:\^|e|\*|exp)?(\d{1,2})/(.+)$")
_PER_AREA = re.compile(r"/1[.,]73(?:m2|m\^2|sqm|msq)?$")
_CREAT_SUFFIX = re.compile(r"\s*(?:de\s+)?(?:creat\w*|crea)\.?\s*$", re.I)
_SUPERSCRIPTS = str.maketrans(
    {"⁰": "0", "¹": "1", "²": "2", "³": "3", "⁴": "4", "⁵": "5", "⁶": "6", "⁷": "7", "⁸": "8", "⁹": "9", "⁻": "-"}
)


@dataclass(frozen=True)
class Unit:
    ucum: str
    factor: Fraction  # multiply a value in this unit to get base units
    dims: tuple[int, ...]

    @property
    def dimensionless(self) -> bool:
        return self.dims == _ZERO


class UnitError(ValueError):
    pass


# --------------------------------------------------------------- strict UCUM ----

_TOKEN = re.compile(r"\{[^}]*\}|mm\[Hg\]|[kdcmunpf]?\[[^\]]*\]|\d+\*[-+]?\d+|[A-Za-z%']+|[-+]?\d+|[./()]")


def parse_ucum(code: str) -> Unit:
    """Parse a UCUM code from our subset. Annotations count as 1. Raises UnitError."""
    tokens = _TOKEN.findall(code)
    if "".join(tokens) != code:
        raise UnitError(f"cannot read UCUM {code!r}")
    pos = 0

    def term() -> tuple[Fraction, list[int]]:
        nonlocal pos
        f, d = Fraction(1), [0] * len(BASES)
        op = "."
        if pos < len(tokens) and tokens[pos] == "/":
            op, pos = "/", pos + 1
        while pos < len(tokens) and tokens[pos] != ")":
            cf, cd = component()
            if op == ".":
                f, d = f * cf, [a + b for a, b in zip(d, cd, strict=True)]
            else:
                f, d = f / cf, [a - b for a, b in zip(d, cd, strict=True)]
            if pos < len(tokens) and tokens[pos] in "./":
                op, pos = tokens[pos], pos + 1
            elif pos < len(tokens) and tokens[pos] != ")":
                raise UnitError(f"unexpected {tokens[pos]!r} in {code!r}")
        return f, d

    def component() -> tuple[Fraction, list[int]]:
        nonlocal pos
        tok = tokens[pos]
        pos += 1
        if tok == "(":
            f, d = term()
            if pos >= len(tokens) or tokens[pos] != ")":
                raise UnitError(f"unbalanced parentheses in {code!r}")
            pos += 1
            return f, d
        if tok.startswith("{"):
            return Fraction(1), [0] * len(BASES)
        if "*" in tok:
            base, exp = tok.split("*")
            return Fraction(int(base)) ** int(exp), [0] * len(BASES)
        if re.fullmatch(r"[-+]?\d+", tok):
            return Fraction(int(tok)), [0] * len(BASES)
        exp = 1
        if pos < len(tokens) and re.fullmatch(r"[-+]?\d+", tokens[pos]):
            exp = int(tokens[pos])
            pos += 1
        f, d = _atom(tok, code)
        if pos < len(tokens) and tokens[pos].startswith("{"):
            pos += 1  # an annotation glued to the atom: mg{FEU}
        return f**exp, [x * exp for x in d]

    f, d = term()
    if pos != len(tokens):
        raise UnitError(f"unexpected {tokens[pos]!r} in {code!r}")
    return Unit(code, f, tuple(d))


def _atom(tok: str, code: str) -> tuple[Fraction, list[int]]:
    if tok in ATOMS:
        f, d, _ = ATOMS[tok]
        return f, list(d)
    if tok[0] in PREFIXES and tok[1:] in ATOMS and ATOMS[tok[1:]][2]:
        f, d, _ = ATOMS[tok[1:]]
        return PREFIXES[tok[0]] * f, list(d)
    raise UnitError(f"unknown unit {tok!r} in {code!r}")


def dims_of(ucum: str | None) -> tuple[int, ...] | None:
    if not ucum:
        return None
    try:
        return parse_ucum(ucum).dims
    except UnitError:
        return None


def dims_name(dims: tuple[int, ...] | None) -> str:
    if dims is None:
        return "unknown"
    parts = [f"{b}^{e}" if e != 1 else b for b, e in zip(BASES, dims, strict=True) if e]
    return ".".join(parts) or "1"


# ---------------------------------------------------------- printed -> UCUM ----


def fold_unit(printed: str) -> str:
    s = unicodedata.normalize("NFKC", printed.translate(_SUPERSCRIPTS))
    s = s.replace("µ", "u").replace("μ", "u").replace("×", "x").replace("−", "-").replace("–", "-")
    s = _CREAT_SUFFIX.sub("", s) if "/" in s else s
    return re.sub(r"\s+", "", s)


def to_ucum(printed: str | None) -> str | None:
    """The UCUM code for a unit as printed, or None when we cannot read it."""
    if printed is None:
        return None
    s = fold_unit(printed)
    if not s or s in {"-", "--", "."}:
        return None
    if s in CASED:
        return CASED[s]
    low = s.lower()
    if low in SYNONYMS:
        return SYNONYMS[low]
    m = _TEN_POWER.match(low)
    if m:
        rest = _from_printed(m.group(2))
        return f"10*{int(m.group(1))}/{rest}" if rest else None
    try:
        parse_ucum(s)  # already valid UCUM, case and all: keep it
        return s
    except UnitError:
        pass
    return _from_printed(low)


def _from_printed(low: str) -> str | None:
    area = ""
    if _PER_AREA.search(low):
        low, area = _PER_AREA.sub("", low), "/{1.73_m2}"
    parts = re.split(r"([/.])", low)
    out: list[str] = []
    for i, part in enumerate(parts):
        if part in ("/", "."):
            out.append(part)
            continue
        if part == "":
            if i == 0:
                continue  # leading "/"
            return None
        atom = _atom_printed(part)
        if atom is None:
            return None
        out.append(atom)
    code = "".join(out) + area
    try:
        parse_ucum(code)
    except UnitError:
        return None
    return code


def _atom_printed(part: str) -> str | None:
    m = re.fullmatch(r"(\d+)?([a-z%'\[\]]+?)(-?\d)?", part)
    if not m:
        return None
    count, word, exp = m.groups()
    atom = _ATOM_LC.get(word)
    prefix = ""
    if atom is None:
        if word.startswith("mc") and word[2:] in _PREFIXABLE_LC:
            prefix, atom = "u", _ATOM_LC[word[2:]]
        elif word[0] in PREFIXES and word[1:] in _PREFIXABLE_LC:
            prefix, atom = word[0], _ATOM_LC[word[1:]]
        else:
            return None
    text = f"{prefix}{atom}{exp or ''}"
    return f"({count}.{text})" if count else text


# ------------------------------------------------------------- conversion ----

HBA1C_GROUP = "hba1c"
# NGSP/IFCC master equation: IFCC (mmol/mol) = 10.929 x (NGSP % - 2.15). https://ngsp.org/ifccngsp.asp
_NGSP_SLOPE = Fraction("10.929")
_NGSP_OFFSET = Fraction("2.15")


def convert_fraction(
    value: Fraction, src: str, dst: str, *, alias_group: str, molar_mass: Fraction | None, valence: int | None
) -> Fraction | None:
    """Exact conversion; None when the units do not convert for this analyte."""
    if alias_group == HBA1C_GROUP and {src, dst} == {"%", "mmol/mol"}:
        if src == "%":
            return _NGSP_SLOPE * (value - _NGSP_OFFSET)
        return value / _NGSP_SLOPE + _NGSP_OFFSET
    try:
        a, b = parse_ucum(src), parse_ucum(dst)
    except UnitError:
        return None
    base = value * a.factor
    diff = [x - y for x, y in zip(a.dims, b.dims, strict=True)]
    gi, mi, ei = BASES.index("g"), BASES.index("mol"), BASES.index("EQ")
    mass, eq, mol = diff[gi], diff[ei], diff[mi]
    if any(d for i, d in enumerate(diff) if i not in (gi, mi, ei)) or mass + eq + mol != 0:
        return None
    if mass:
        if molar_mass is None:
            return None
        base = base / molar_mass**mass
    if eq:
        if not valence:
            return None
        base = base / Fraction(valence) ** eq
    return base / b.factor
