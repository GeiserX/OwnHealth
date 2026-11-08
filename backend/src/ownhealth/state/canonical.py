"""Canonical JSON per RFC 8785 (JCS) for the values the state body holds.

Keys sorted by UTF-16 code units, no whitespace, minimal string escaping. Decimals, dates, datetimes
and UUIDs become strings (a Decimal keeps its printed precision: Decimal("0.90") stays "0.90").
Floats are refused: a float in the hash would make it depend on binary rounding.
"""

from __future__ import annotations

import json
from datetime import date, datetime
from decimal import Decimal
from uuid import UUID


def _str(s: str) -> str:
    return json.dumps(s, ensure_ascii=False)


def _enc(v, out: list[str]) -> None:
    if v is None:
        out.append("null")
    elif v is True:
        out.append("true")
    elif v is False:
        out.append("false")
    elif isinstance(v, int):
        out.append(str(v))
    elif isinstance(v, float):
        raise TypeError("floats never enter the state hash; use Decimal or str")
    elif isinstance(v, str):
        out.append(_str(v))
    elif isinstance(v, Decimal):
        out.append(_str(str(v)))
    elif isinstance(v, datetime | date):
        out.append(_str(v.isoformat()))
    elif isinstance(v, UUID):
        out.append(_str(str(v)))
    elif isinstance(v, dict):
        out.append("{")
        for i, k in enumerate(sorted(v, key=lambda k: str(k).encode("utf-16-be"))):
            if i:
                out.append(",")
            out.append(_str(str(k)))
            out.append(":")
            _enc(v[k], out)
        out.append("}")
    elif isinstance(v, list | tuple):
        out.append("[")
        for i, item in enumerate(v):
            if i:
                out.append(",")
            _enc(item, out)
        out.append("]")
    else:
        raise TypeError(f"cannot canonicalize {type(v).__name__}")


def canonical_json(obj) -> bytes:
    out: list[str] = []
    _enc(obj, out)
    return "".join(out).encode("utf-8")
