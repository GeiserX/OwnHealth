"""The MCP response envelope (mcp.md 3, 9.1). `untrusted` is final and every lane uses it for free
text going out over MCP; the rest is lane F's contract."""

from __future__ import annotations

import base64
import hashlib
import hmac
import json
import math
from datetime import UTC, date, datetime
from decimal import Decimal
from typing import Literal
from uuid import UUID

Origin = Literal["pdf_text_layer", "ocr", "email_subject", "fhir_note", "agent_note", "person_note", "genome_facts"]
ORIGINS: tuple[str, ...] = (
    "pdf_text_layer",
    "ocr",
    "email_subject",
    "fhir_note",
    "agent_note",
    "person_note",
    "genome_facts",  # free text from an imported OwnGenome facts file (lane D)
)
NOTICE = "Information from your records, not a diagnosis."

# Every envelope carries these keys, in this order (mcp.md 3).
ENVELOPE_KEYS: tuple[str, ...] = (
    "profile",
    "as_of",
    "generated_at",
    "lang",
    "identity_mode",
    "data",
    "evidence",
    "limitations",
    "not_assessed",
    "withheld",
    "db_versions",
    "next_cursor",
    "budget",
    "notice",
)


def untrusted(text: str | None, origin: Origin) -> dict | None:
    """Wrap free text that did not come from our own templates. None stays None."""
    if text is None:
        return None
    if origin not in ORIGINS:
        raise ValueError(f"unknown untrusted_text origin {origin!r}")
    return {"trust": "untrusted_text", "origin": origin, "text": text}


def is_untrusted(value) -> bool:
    return isinstance(value, dict) and value.get("trust") == "untrusted_text" and "text" in value


class ToolError(Exception):
    """A tool execution error (`isError: true`) with a code, a one-line message and a fix hint."""

    def __init__(self, code: str, message: str, fix: str | None = None, **extra):
        super().__init__(message)
        self.code, self.message, self.fix, self.extra = code, message, fix, extra

    def as_dict(self) -> dict:
        return {"isError": True, "code": self.code, "message": self.message, "fix": self.fix, **self.extra}


def empty_withheld() -> dict:
    return {"sensitive": 0, "by_scope": [], "minor_rule": 0}


def envelope(*, profile: str, as_of: str, lang: str, identity_mode: str, data: dict, **fields) -> dict:
    """The common response shape. Unknown keyword fields are an error, so a typo cannot slip a
    field past the schema."""
    unknown = set(fields) - set(ENVELOPE_KEYS)
    if unknown:
        raise TypeError(f"unknown envelope fields {sorted(unknown)}")
    withheld = {**empty_withheld(), **(fields.get("withheld") or {})}
    withheld["by_scope"] = sorted(set(withheld["by_scope"]))
    env = {
        "profile": profile,
        "as_of": as_of,
        "generated_at": fields.get("generated_at") or datetime.now(UTC).strftime("%Y-%m-%dT%H:%M:%SZ"),
        "lang": lang,
        "identity_mode": identity_mode,
        "data": data,
        "evidence": list(fields.get("evidence") or []),
        "limitations": list(fields.get("limitations") or []),
        "not_assessed": list(fields.get("not_assessed") or []),
        "withheld": withheld,
        "db_versions": dict(fields.get("db_versions") or {}),
        "next_cursor": fields.get("next_cursor"),
        "budget": fields.get("budget") or {"tokens_est": 0, "truncated": False},
        "notice": fields.get("notice") or NOTICE,
    }
    return env


def jsonable(obj):
    """Plain JSON types: Decimal to int or float, dates to ISO strings, UUIDs to str."""
    if isinstance(obj, dict):
        return {str(k): jsonable(v) for k, v in obj.items()}
    if isinstance(obj, list | tuple):
        return [jsonable(v) for v in obj]
    if isinstance(obj, Decimal):
        return int(obj) if obj == obj.to_integral_value() else float(obj)
    if isinstance(obj, datetime):
        return obj.astimezone(UTC).strftime("%Y-%m-%dT%H:%M:%SZ") if obj.tzinfo else obj.isoformat()
    if isinstance(obj, date):
        return obj.isoformat()
    if isinstance(obj, UUID):
        return str(obj)
    if isinstance(obj, float) and not math.isfinite(obj):
        return None
    return obj


def dumps(obj) -> str:
    return json.dumps(obj, ensure_ascii=False, separators=(",", ":"), default=str)


def estimate_tokens(obj) -> int:
    """len(json)/3.5, the budget unit of mcp.md 3. A string is measured as it is."""
    s = obj if isinstance(obj, str) else dumps(obj)
    return math.ceil(len(s) / 3.5)


# ------------------------------------------------------------------- cursors ----


def _b64(raw: bytes) -> str:
    return base64.urlsafe_b64encode(raw).rstrip(b"=").decode()


def _unb64(s: str) -> bytes:
    return base64.urlsafe_b64decode(s + "=" * (-len(s) % 4))


def _mac(secret: str, body: bytes) -> bytes:
    return hmac.new(hashlib.sha256(b"ownhealth.mcp.cursor:" + secret.encode()).digest(), body, "sha256").digest()[:16]


def query_hash(*parts) -> str:
    """A short stable hash of the query a cursor belongs to (tool, token, profile, arguments)."""
    return hashlib.sha256(dumps(jsonable(list(parts))).encode()).hexdigest()[:24]


def encode_cursor(secret: str, query_hash: str, as_of: str, last_key) -> str:
    """Opaque, stateless, signed: base64url of {q, as_of, k} plus an HMAC (mcp.md 3)."""
    body = dumps({"q": query_hash, "as_of": as_of, "k": jsonable(last_key)}).encode()
    return _b64(body) + "." + _b64(_mac(secret, body))


class CursorError(ValueError):
    pass


def decode_cursor(secret: str, cursor: str, query_hash: str) -> dict:
    """{as_of, last_key} for a cursor this server signed for this exact query; CursorError otherwise."""
    try:
        body_s, mac_s = cursor.split(".", 1)
        body = _unb64(body_s)
        mac = _unb64(mac_s)
    except (ValueError, TypeError) as exc:
        raise CursorError("malformed cursor") from exc
    if not hmac.compare_digest(mac, _mac(secret, body)):
        raise CursorError("cursor signature does not match")
    data = json.loads(body)
    if data.get("q") != query_hash:
        raise CursorError("cursor belongs to a different query or token")
    return {"as_of": data.get("as_of"), "last_key": data.get("k")}


# ----------------------------------------------------------------- walkers ----


def walk(obj, fn, path=()):
    """Call fn(value, path) on every node, depth first."""
    fn(obj, path)
    if isinstance(obj, dict):
        for k, v in obj.items():
            walk(v, fn, (*path, k))
    elif isinstance(obj, list):
        for i, v in enumerate(obj):
            walk(v, fn, (*path, i))


def src_values(value) -> list[str]:
    if isinstance(value, str):
        return [value]
    if isinstance(value, list):
        return [v for v in value if isinstance(v, str)]
    return []


def collect_src_ids(obj) -> list[str]:
    """Every src id cited anywhere in obj, in first-seen order."""
    seen: dict[str, None] = {}

    def visit(node, _path):
        if isinstance(node, dict) and "src" in node:
            for s in src_values(node["src"]):
                seen.setdefault(s, None)

    walk(obj, visit)
    return list(seen)


def count_facts(obj) -> int:
    """A fact is one row with a src id (mcp.md 9.4)."""
    n = 0

    def visit(node, _path):
        nonlocal n
        if isinstance(node, dict) and node.get("src"):
            n += 1

    walk(obj, visit)
    return n


def wrap_plain(items, key: str, origin: Origin) -> None:
    """Wrap `item[key]` in place when another service handed it to us as a bare string. Record names
    (medications, conditions) are free text from imports or people; the contract says they go out
    wrapped, and this keeps that true even if the backing service forgets."""
    for item in items or []:
        if isinstance(item, dict) and isinstance(item.get(key), str):
            item[key] = untrusted(item[key], origin)
