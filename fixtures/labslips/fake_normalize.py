"""A thin stand-in for lane A's `normalize.normalize_row`, used only while that is a contract stub.

It knows the analytes of `catalog.py` (names in es/en/de/fr, the units the slips print) and parses
values and ranges by locale. Anything it does not know fails a check, so a wrong row can only reach
the record through a missing check here, which the eval's false auto-accept count would show.
"""

from __future__ import annotations

import re
import sys
import unicodedata
from decimal import Decimal, InvalidOperation
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from catalog import ANALYTES, TEXT_RESULT, UNIT_UCUM  # noqa: E402

from ownhealth.core.acceptance import Check  # noqa: E402
from ownhealth.core.observations import ObservationIn  # noqa: E402
from ownhealth.normalize import Normalized  # noqa: E402

VERSION = "fake-normalize-1"


def _fold(s: str) -> str:
    s = unicodedata.normalize("NFKD", s or "")
    return " ".join("".join(c for c in s if not unicodedata.combining(c)).lower().replace("µ", "u").split())


_BY_NAME: dict[tuple[str, str], str] = {}
for _key, (_g, _l, _names, _units, _u, _d, _r, _spec) in ANALYTES.items():
    for _n in _names.values():
        _BY_NAME[(_fold(_n), "urine" if _spec == "urine" else "other")] = _key

_BY_NAME[(_fold("Glucosa en orina"), "urine")] = "urine_glucose"
_BY_NAME[(_fold("Proteínas en orina"), "urine")] = "urine_protein"

TEXT_CODES = {_fold(v): "negative" for v in TEXT_RESULT.values()}


def parse_number(s: str, lang: str) -> tuple[Decimal | None, str | None, str | None]:
    """(value, comparator, reason)."""
    s = s.strip()
    m = re.match(r"^([<>≤≥]=?)\s*(.*)$", s)
    cmp_ = None
    if m:
        cmp_ = {"≤": "<=", "≥": ">="}.get(m.group(1), m.group(1))
        s = m.group(2)
    if re.fullmatch(r"\d{1,3}( \d{3})+", s):
        return None, cmp_, "suspect_separator"  # "250 000": thousands with spaces
    if lang in ("es", "de", "fr"):
        if re.fullmatch(r"\d{1,3}(\.\d{3})+", s):
            s = s.replace(".", "")
        s = s.replace(",", ".")
    else:
        if re.fullmatch(r"\d{1,3}(,\d{3})+", s):
            s = s.replace(",", "")
    try:
        return Decimal(s), cmp_, None
    except InvalidOperation:
        return None, cmp_, "unparsed"


def normalize_row(
    conn,
    *,
    profile_id,
    printed_name,
    printed_value,
    printed_unit,
    printed_range,
    printed_flag,
    effective_at,
    lang,
    specimen=None,
    provider=None,
    document_id=None,  # the real normalizer skips duplicates within one source; the stand-in has no check
) -> Normalized:
    checks: list[Check] = []
    key = _BY_NAME.get((_fold(printed_name), "urine" if specimen == "urine" else "other"))
    group = ANALYTES[key][0] if key else "uncoded:" + re.sub(r"[^a-z0-9]+", "_", _fold(printed_name)).strip("_")
    loinc = ANALYTES[key][1] if key else None
    checks.append(Check(name="loinc", ok=loinc is not None, reason=None if loinc else "loinc_unknown"))
    value_num = cmp_ = value_text = value_code = None
    folded = _fold(printed_value)
    if folded in TEXT_CODES:
        value_text, value_code = printed_value, TEXT_CODES[folded]
    else:
        value_num, cmp_, why = parse_number(printed_value, lang)
        if why:
            checks.append(
                Check(
                    name="value",
                    ok=False,
                    reason="suspect_separator" if why == "suspect_separator" else "unit_unconvertible",
                    detail=f"value {why}",
                )
            )
    ucum = None
    if key and printed_unit:
        unit_known = {_fold(u) for u in ANALYTES[key][3].values()}
        if _fold(printed_unit) in unit_known:
            ucum = ANALYTES[key][4] or UNIT_UCUM.get(printed_unit)
        elif printed_unit == "/µL" and key == "platelets":
            ucum = "/uL"
        checks.append(Check(name="unit", ok=ucum is not None, reason=None if ucum else "unit_dimension_mismatch"))
    elif key and ANALYTES[key][5] is not None:
        checks.append(Check(name="unit", ok=False, reason="unit_dimension_mismatch", detail="no unit printed"))
    low = high = None
    if printed_range and value_code is None:
        m = re.fullmatch(r"\s*([\d.,]+)\s*-\s*([\d.,]+)\s*", printed_range)
        upper = re.fullmatch(r"\s*<\s*([\d.,]+)\s*", printed_range)
        if m:
            low, _, _ = parse_number(m.group(1), lang)
            high, _, _ = parse_number(m.group(2), lang)
        elif upper:
            high, _, _ = parse_number(upper.group(1), lang)
        elif not re.search(r"\d", printed_range) or ":" in printed_range:
            pass  # sex-conditioned or text ranges: kept as printed
        else:
            checks.append(Check(name="range", ok=False, reason="range_parse"))
    obs = ObservationIn(
        profile_id=profile_id,
        printed_name=printed_name,
        printed_value=printed_value,
        printed_unit=printed_unit,
        printed_ref_range=printed_range,
        printed_flag=printed_flag,
        value_num=value_num,
        value_cmp=cmp_,
        value_text=value_text,
        value_code=value_code,
        ref_low=low,
        ref_high=high,
        specimen=specimen,
        loinc=loinc,
        alias_group=group,
        unit_ucum=ucum,
        value_canonical=value_num,
        unit_canonical=ucum,
        derivation_version=VERSION,
        effective_at=effective_at,
    )
    return Normalized(observation=obs, checks=checks)
