"""Assertion helpers for lane F tests."""

from __future__ import annotations

import json
import re

from ownhealth.mcp.envelope import ENVELOPE_KEYS

TAG_RE = re.compile(r'<untrusted_text origin="[a-z_]+">.*?</untrusted_text>', re.S)


def sc(result) -> dict:
    """structuredContent of a successful call."""
    assert not result.is_error, result.structured_content
    return result.structured_content


def err(result) -> dict:
    assert result.is_error, result.structured_content
    return result.structured_content


def assert_envelope(env: dict) -> None:
    assert list(env) == list(ENVELOPE_KEYS), list(env)
    assert set(env["withheld"]) == {"sensitive", "by_scope", "minor_rule"}
    assert env["notice"]


def facts(obj):
    """Every dict that carries a src."""
    out = []

    def go(n):
        if isinstance(n, dict):
            if n.get("src"):
                out.append(n)
            for v in n.values():
                go(v)
        elif isinstance(n, list):
            for v in n:
                go(v)

    go(obj)
    return out


def marker_outside_wrappers(obj, marker: str) -> list[str]:
    """Paths where `marker` appears in a string that is not the text of an untrusted_text wrapper."""
    bad = []

    def go(n, path, inside):
        if isinstance(n, dict):
            wrapped = n.get("trust") == "untrusted_text"
            for k, v in n.items():
                go(v, f"{path}.{k}", wrapped and k == "text")
        elif isinstance(n, list):
            for i, v in enumerate(n):
                go(v, f"{path}[{i}]", False)
        elif isinstance(n, str) and marker in n and not inside:
            bad.append(path)

    go(obj, "$", False)
    return bad


def marker_outside_tags(text: str, marker: str) -> bool:
    return marker in TAG_RE.sub("", text)


def all_srcs(obj) -> set[str]:
    return {s for f in facts(obj) for s in ([f["src"]] if isinstance(f["src"], str) else f["src"])}


def dumps(obj) -> str:
    return json.dumps(obj, ensure_ascii=False)
