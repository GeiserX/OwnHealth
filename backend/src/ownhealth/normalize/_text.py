"""Printed values, ranges and flags (ingestion.md 8.1, 8.4, 8.5). The locale decides separators."""

from __future__ import annotations

import re
from dataclasses import dataclass, field
from decimal import Decimal, InvalidOperation

from ownhealth.normalize._tables import fold

# UCUM count-per-volume codes: 10*3/uL, /uL, /mm3 (8.1: only these read 1.200 as 1200)
_COUNT = re.compile(r"^(?:10\*\d+)?/(?:u|m|n)?L$|^/mm3$|^10\*\d+/(?:u|m|n)?L$")

_COMPARATORS = [  # folded prefix -> comparator, longest first where they overlap
    ("<=", "<="),
    (">=", ">="),
    ("=<", "<="),
    ("=>", ">="),
    ("≤", "<="),
    ("≥", ">="),
    ("<", "<"),
    (">", ">"),
    ("inferior a", "<"),
    ("menor de", "<"),
    ("menor que", "<"),
    ("superior a", ">"),
    ("mayor de", ">"),
    ("mayor que", ">"),
    ("kleiner als", "<"),
    ("grosser als", ">"),
    ("inferieur a", "<"),
    ("superieur a", ">"),
    ("less than", "<"),
    ("greater than", ">"),
]
TEXT_CODES = {
    **dict.fromkeys(["negativo", "negativa", "negative", "negativ", "negatif", "neg", "-"], "negative"),
    **dict.fromkeys(
        ["positivo", "positiva", "positive", "positiv", "positif", "pos", "+", "++", "+++", "++++"], "positive"
    ),
    **dict.fromkeys(
        [
            "indetectable",
            "no detectado",
            "no detectable",
            "not detected",
            "undetectable",
            "nicht nachweisbar",
            "non detecte",
            "absence",
            "absent",
            "ausencia",
            "ausente",
        ],
        "not_detected",
    ),
    **dict.fromkeys(["trazas", "indicios", "trace", "traces", "spuren"], "trace"),
}
_NEGATIVE_CODES = {"negative", "not_detected"}
_FLAG_TAIL = re.compile(r"\s*(?:\*+|[↑↓]|\([hl+-]\)|\b[hl]\b)\s*$")


@dataclass(frozen=True)
class RefRange:
    low: Decimal | None
    high: Decimal | None
    text: str
    applies_to: dict = field(default_factory=dict)  # {sex, age_min, age_max, pregnancy}
    low_inclusive: bool = True  # "> 90" is exclusive, "90 - 120" inclusive
    high_inclusive: bool = True  # "< 130" is exclusive
    text_code: str | None = None  # expected text result: "Negativo" -> negative


def is_count_unit(ucum: str | None) -> bool:
    return bool(ucum and _COUNT.match(ucum))


def parse_number(token: str, locale: str, *, count_unit: bool = False) -> tuple[Decimal | None, str | None]:
    """One printed number with the locale's separators -> (value, reason). Reason is a review code."""
    t = re.sub(r"[\s' ]", "", token)
    if not re.fullmatch(r"[+-]?(?:\d[\d.,]*|[.,]\d+)", t):
        return None, None
    sign = ""
    if t[0] in "+-":
        sign, t = t[0], t[1:]
    reason = None
    if "." in t and "," in t:
        dec = "." if t.rfind(".") > t.rfind(",") else ","
        grp = "," if dec == "." else "."
        head, tail = t.rsplit(dec, 1)
        if not _grouped(head, grp):
            return None, "suspect_separator"
        t = head.replace(grp, "") + "." + tail
    elif "." in t or "," in t:
        sep = "," if "," in t else "."
        decimal_sep = "." if locale == "en" else ","
        if t.count(sep) > 1:
            if not _grouped(t, sep):
                return None, "suspect_separator"
            t = t.replace(sep, "")  # 1.234.567: only a grouping reading exists
        elif sep == decimal_sep or not _grouped(t, sep):
            t = t.replace(sep, ".")
        elif count_unit or locale == "en":
            t = t.replace(sep, "")  # 6.800 /µL in an es report is 6800; 1,200 in an en report is 1200
        else:
            reason = "ambiguous_separator"  # 1.200 mg/dL in an es report: 1.2 or 1200?
            t = t.replace(sep, ".")
    try:
        return Decimal(sign + t), reason
    except InvalidOperation:
        return None, "suspect_separator"


def _grouped(s: str, sep: str) -> bool:
    parts = s.split(sep)
    return 1 <= len(parts[0]) <= 3 and all(len(p) == 3 for p in parts[1:]) and all(p.isdigit() for p in parts)


@dataclass(frozen=True)
class Value:
    value: Decimal | None
    comparator: str | None
    text_code: str | None
    reason: str | None


def parse_value(printed: str, locale: str, ucum: str | None) -> Value:
    f = fold(printed)
    f = _FLAG_TAIL.sub("", f).strip()
    code = TEXT_CODES.get(f.strip("() "))
    if code:
        return Value(None, None, code, None)
    cmp = None
    for word, c in _COMPARATORS:
        if f.startswith(word):
            cmp, f = c, f[len(word) :].strip()
            break
    num, reason = parse_number(f, locale, count_unit=is_count_unit(ucum))
    return Value(num, cmp if num is not None else None, None, reason)


def looks_like_range(printed: str) -> bool:
    return bool(re.fullmatch(r"\s*[+-]?\d[\d.,]*\s*(?:-|–|—|a|to|bis)\s*\d[\d.,]*\s*", printed))


# ----------------------------------------------------------------- ranges ----

_SEX_WORDS = {
    "male": r"hombres?|varon(?:es)?|masculino|male|men|maenner|manner|mann|hommes?",
    "female": r"mujer(?:es)?|femenino|female|women|frauen|frau|femmes?",
}
_PREG = (
    r"embarazo|embarazada|gestacion|pregnan\w*|schwanger\w*|grossesse|enceinte"
    r"|\d\s*(?:er|o|º|ª|st|nd|rd)?\s*trimest\w*"
)
_ADULT = r"adultos|adults?|erwachsene|adultes"
_CHILD = r"ninos|children|kinder|enfants"
_LABEL = re.compile(
    rf"\b(?:(?P<male>{_SEX_WORDS['male']})|(?P<female>{_SEX_WORDS['female']})|(?P<preg>{_PREG})|"
    rf"(?P<adult>{_ADULT})|(?P<child>{_CHILD}))\b\s*[:=]?"
)
_YEARS = r"(?:anos|years?|y|jahre|ans)\b"
_AGE_SPAN = re.compile(rf"(\d+)\s*(?:-|a|to|bis)\s*(\d+)\s*{_YEARS}")
_AGE_OPEN = re.compile(rf"(>=|<=|>|<|mayores de|menores de|over|under|ab|plus de|moins de)\s*(\d+)\s*{_YEARS}")
_NUM = r"[+-]?\d[\d.,]*"
_SPAN = re.compile(rf"(?P<lo>{_NUM})\s*(?:-|–|—|\ba\b|\bto\b|\bbis\b)\s*(?P<hi>\d[\d.,]*)")
_UPPER = re.compile(
    r"(?P<op><=|=<|≤|<|hasta|up to|bis|jusqu'?a|inferior a|menor de|menor que|kleiner als|inferieur a)"
    rf"\s*(?P<hi>{_NUM})"
)
_LOWER = re.compile(rf"(?P<op>>=|=>|≥|>|superior a|mayor de|mayor que|grosser als|superieur a|desde)\s*(?P<lo>{_NUM})")


def parse_range(printed: str, locale: str, ucum: str | None = None) -> list[RefRange]:
    """A printed reference range into one range per condition ("Mujeres: 12-16 Hombres: 13-17")."""
    if not printed or not printed.strip():
        return []
    count = is_count_unit(ucum)
    out: list[RefRange] = []
    single = len(re.split(r"[;\n|]", printed.strip())) == 1
    for chunk in re.split(r"[;\n|]", fold(printed)):
        labels = list(_LABEL.finditer(chunk))
        pieces: list[tuple[dict, str]] = []
        if not labels:
            pieces.append(({}, chunk))
        else:
            if chunk[: labels[0].start()].strip(" ,"):
                pieces.append(({}, chunk[: labels[0].start()]))
            for i, m in enumerate(labels):
                end = labels[i + 1].start() if i + 1 < len(labels) else len(chunk)
                pieces.append((_label_applies(m), chunk[m.end() : end]))
        for applies, text in pieces:
            r = _one_range(text, applies, locale, count)
            if r is not None:
                out.append(r)
    if single and len(out) == 1 and not out[0].applies_to:
        out[0] = RefRange(
            out[0].low, out[0].high, printed.strip(), {}, out[0].low_inclusive, out[0].high_inclusive, out[0].text_code
        )
    return out


def _label_applies(m: re.Match) -> dict:
    if m.group("male"):
        return {"sex": "male"}
    if m.group("female"):
        return {"sex": "female"}
    if m.group("preg"):
        return {"pregnancy": True}
    if m.group("adult"):
        return {"age_min": 18}
    return {"age_max": 17}


def _one_range(text: str, applies: dict, locale: str, count: bool) -> RefRange | None:
    applies = dict(applies)
    t = text.strip(" ,:")
    if m := _AGE_SPAN.search(t):
        applies.update(age_min=int(m.group(1)), age_max=int(m.group(2)))
        t = (t[: m.start()] + t[m.end() :]).strip(" ,:")
    elif m := _AGE_OPEN.search(t):
        n = int(m.group(2))
        op = m.group(1)
        if op in (">", ">=", "mayores de", "over", "ab", "plus de"):
            applies["age_min"] = n + (1 if op in (">", "mayores de", "over", "plus de") else 0)
        else:
            applies["age_max"] = n - (1 if op in ("<", "menores de", "under", "moins de") else 0)
        t = (t[: m.start()] + t[m.end() :]).strip(" ,:")
    if not t:
        return None
    label = text.strip(" ,:")
    if m := _SPAN.search(t):
        lo, _ = parse_number(m.group("lo"), locale, count_unit=count)
        hi, _ = parse_number(m.group("hi"), locale, count_unit=count)
        if lo is not None and hi is not None:
            return RefRange(lo, hi, label, applies)
    if m := _UPPER.search(t):
        hi, _ = parse_number(m.group("hi"), locale, count_unit=count)
        if hi is not None:
            return RefRange(None, hi, label, applies, high_inclusive=m.group("op") not in ("<",))
    if m := _LOWER.search(t):
        lo, _ = parse_number(m.group("lo"), locale, count_unit=count)
        if lo is not None:
            return RefRange(lo, None, label, applies, low_inclusive=m.group("op") not in (">",))
    code = TEXT_CODES.get(t.strip("() "))
    if code:
        return RefRange(None, None, label, applies, text_code=code)
    return None


def pick_range(ranges: list[RefRange], sex: str | None, age_years: int | None) -> RefRange | None:
    """The range that applies to this person; None when none does or two equally specific disagree."""
    fits = []
    for r in ranges:
        a = r.applies_to
        if a.get("pregnancy"):
            continue  # we do not know a pregnancy stage in v0.1: never pick a trimester range
        if "sex" in a and a["sex"] != sex:
            continue
        if ("age_min" in a or "age_max" in a) and age_years is None:
            continue
        if "age_min" in a and age_years < a["age_min"]:
            continue
        if "age_max" in a and age_years > a["age_max"]:
            continue
        fits.append(r)
    if not fits:
        return None
    best = max(len(r.applies_to) for r in fits)
    top = [r for r in fits if len(r.applies_to) == best]
    if len({(r.low, r.high, r.text_code) for r in top}) > 1:
        return None
    return top[0]


def interpret(value: Decimal | None, text_code: str | None, ref: RefRange | None, comparator: str | None) -> str | None:
    """L, N, H or A from the printed range only (HL7 ObservationInterpretation). None without a range."""
    if ref is None:
        return None
    if ref.text_code is not None:
        if text_code is None:
            return None
        same = text_code == ref.text_code or {text_code, ref.text_code} <= _NEGATIVE_CODES
        return "N" if same else "A"
    if value is None or (ref.low is None and ref.high is None):
        return None
    lo, hi = ref.low, ref.high
    below = lo is not None and (value < lo or (value == lo and not ref.low_inclusive))
    above = hi is not None and (value > hi or (value == hi and not ref.high_inclusive))
    if comparator in ("<", "<="):  # the true value is at most `value`
        if below or (lo is not None and value == lo and comparator == "<"):
            return "L"
        if lo is None and not above:
            return "N"
        return None
    if comparator in (">", ">="):  # the true value is at least `value`
        if above or (hi is not None and value == hi and comparator == ">"):
            return "H"
        if hi is None and not below:
            return "N"
        return None
    return "L" if below else "H" if above else "N"


_FLAG_WORDS = {
    **dict.fromkeys(["h", "high", "alto", "alta", "hoch", "haut", "eleve", "elevado", "elevada", "↑", "+", ">"], "H"),
    **dict.fromkeys(["l", "low", "bajo", "baja", "niedrig", "bas", "↓", "<", "-", "disminuido"], "L"),
    **dict.fromkeys(["hh", ">>", "↑↑", "critico alto", "critical high"], "HH"),
    **dict.fromkeys(["ll", "<<", "↓↓", "critico bajo", "critical low"], "LL"),
    **dict.fromkeys(["*", "**", "a", "abnormal", "anormal", "anomalo", "pathologisch", "patologico"], "A"),
    **dict.fromkeys(["n", "normal"], "N"),
}


def printed_flag_code(printed: str | None) -> str | None:
    if not printed or not printed.strip():
        return None
    return _FLAG_WORDS.get(fold(printed).strip("() ") or printed.strip())


def flag_disagrees(printed: str | None, computed: str | None) -> bool:
    """A printed flag that contradicts the range: a strong sign of a mis-read range or column shift."""
    if printed is None or computed is None:
        return False
    if printed in ("H", "HH"):
        return computed != "H"
    if printed in ("L", "LL"):
        return computed != "L"
    if printed == "A":
        return computed == "N"
    return printed == "N" and computed != "N"
