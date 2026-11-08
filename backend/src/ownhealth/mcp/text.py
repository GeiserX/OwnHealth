"""The compact markdown text block that rides beside structuredContent (mcp.md 3): the same facts,
one per line, flags first. Untrusted text appears only inside an `<untrusted_text>` tag whose body
cannot close the tag, so a model reading the text block sees the same marker as the JSON."""

from __future__ import annotations

from ownhealth.mcp.envelope import is_untrusted

FIRST_KEYS = ("flags",)
SKIP_KEYS = {"trust"}


def tag(node: dict) -> str:
    body = str(node.get("text", "")).replace("&", "&amp;").replace("<", "&lt;").replace(">", "&gt;")
    return f'<untrusted_text origin="{node.get("origin")}">{body}</untrusted_text>'


def fmt(value) -> str:
    if value is None:
        return "null"
    if is_untrusted(value):
        return tag(value)
    if isinstance(value, dict):
        inner = " ".join(f"{k}={fmt(v)}" for k, v in value.items() if v is not None and k not in SKIP_KEYS)
        return "{" + inner + "}"
    if isinstance(value, list):
        return "[" + ", ".join(fmt(v) for v in value) + "]"
    if isinstance(value, bool):
        return "true" if value else "false"
    return str(value).replace("\n", " ")


def _fact_line(d: dict) -> str:
    src = d.get("src")
    body = " ".join(f"{k}={fmt(v)}" for k, v in d.items() if k != "src" and v is not None and v != [])
    cite = ",".join(src) if isinstance(src, list) else src
    return f"- {body} [{cite}]"


def _lines(node, path: str, out: list[str]) -> None:
    if isinstance(node, dict):
        if node.get("src"):
            out.append(_fact_line(node))
            return
        if is_untrusted(node):
            out.append(f"- {path}: {tag(node)}")
            return
        keys = [k for k in FIRST_KEYS if k in node] + [k for k in node if k not in FIRST_KEYS]
        for k in keys:
            v = node[k]
            if isinstance(v, dict | list):
                if v:
                    out.append(f"{k}:" if not path else f"{path}.{k}:")
                    _lines(v, k, out)
                elif isinstance(v, list):
                    out.append(f"{k}: none")
            elif v is not None:
                out.append(f"{k}: {fmt(v)}")
    elif isinstance(node, list):
        for item in node:
            flat = isinstance(item, dict) and not any(isinstance(v, list) and v for v in item.values())
            if flat and not item.get("src") and not is_untrusted(item):
                out.append("- " + " ".join(f"{k}={fmt(v)}" for k, v in item.items() if v is not None and v != []))
            elif isinstance(item, dict | list):
                _lines(item, path, out)
            else:
                out.append(f"- {fmt(item)}")


def render(env: dict, tool: str) -> str:
    out = [f"OwnHealth {tool} · profile {env.get('profile')} · as of {env.get('as_of')} · lang {env.get('lang')}"]
    _lines(env.get("data") or {}, "", out)
    w = env.get("withheld") or {}
    if w.get("sensitive") or w.get("minor_rule") or w.get("by_scope"):
        out.append(
            f"withheld: sensitive={w.get('sensitive', 0)} minor_rule={w.get('minor_rule', 0)}"
            f" by_scope={','.join(w.get('by_scope') or []) or 'none'}"
        )
    for key in ("limitations", "not_assessed", "evidence"):
        items = env.get(key) or []
        if items:
            out.append(f"{key}:")
            _lines(items, key, out)
    if env.get("db_versions"):
        out.append("db_versions: " + fmt(env["db_versions"]))
    if env.get("next_cursor"):
        out.append(f"next_cursor: {env['next_cursor']} (more results; pass it as cursor)")
    out.append(env.get("notice") or "")
    return "\n".join(out)
