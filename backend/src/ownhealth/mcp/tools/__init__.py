"""The v0.1 tools and the one pipeline every call goes through (mcp.md 3, 5, 9).

    rate bucket -> visible? -> arguments -> profile permission -> tool -> prune hidden facts
    -> identity mode -> budget fit + cursor -> egress budget -> usage + audit -> CallToolResult

A tool module registers a `ToolSpec` with `@tool(...)`; its function gets a `Call` and returns a
`Result`. Tools never check permissions themselves beyond their own extra rules: the pipeline
resolves the profile through `core.access.require` and drops any cited fact the caller cannot read.
"""

from __future__ import annotations

import logging
import os
import re
from collections.abc import Callable
from dataclasses import dataclass, field
from datetime import UTC, date, datetime, time
from typing import Any

import jsonschema
import mcp_types as types
from sqlalchemy import Connection, text

from ownhealth import runtime
from ownhealth.core.access import AccessDenied, Permission, Principal, accessible_profiles, require
from ownhealth.core.audit import AuditEvent, hash_arg, write_audit
from ownhealth.mcp import gate as gate_mod
from ownhealth.mcp import limits, redact
from ownhealth.mcp.envelope import (
    CursorError,
    ToolError,
    collect_src_ids,
    count_facts,
    decode_cursor,
    dumps,
    encode_cursor,
    envelope,
    estimate_tokens,
    jsonable,
    query_hash,
)
from ownhealth.mcp.text import render as render_text

log = logging.getLogger("ownhealth.mcp")

DEFAULT_BUDGET = 4000
MAX_BUDGET = 15000

OUTPUT_SCHEMA: dict = {
    "type": "object",
    "required": ["profile", "as_of", "generated_at", "lang", "identity_mode", "data", "withheld", "budget", "notice"],
    "properties": {
        "profile": {"type": ["string", "null"]},
        "as_of": {"type": "string"},
        "generated_at": {"type": "string"},
        "lang": {"enum": ["es", "en"]},
        "identity_mode": {"enum": ["full", "pseudonymous", "minimal"]},
        "data": {"type": "object"},
        "evidence": {"type": "array"},
        "limitations": {"type": "array"},
        "not_assessed": {"type": "array"},
        "withheld": {
            "type": "object",
            "required": ["sensitive", "by_scope", "minor_rule"],
            "properties": {
                "sensitive": {"type": "integer"},
                "by_scope": {"type": "array", "items": {"type": "string"}},
                "minor_rule": {"type": "integer"},
            },
        },
        "db_versions": {"type": "object"},
        "next_cursor": {"type": ["string", "null"]},
        "budget": {
            "type": "object",
            "required": ["tokens_est", "truncated"],
            "properties": {"tokens_est": {"type": "integer"}, "truncated": {"type": "boolean"}},
        },
        "notice": {"type": "string"},
    },
}

DENIED_CODES = {
    "profile_not_granted",
    "scope_missing",
    "rate_limited",
    "egress_budget_exhausted",
    "genome_lookup_cap",
    "propose_cap",
    "unknown",
}


# ------------------------------------------------------------------ types ----


@dataclass(frozen=True)
class AsOf:
    day: date  # the date rules and status use
    at: datetime | None  # cutoff for lists; None is the current view
    label: str  # what the envelope shows

    @property
    def iso_at(self) -> str | None:
        return self.at.isoformat() if self.at else None


@dataclass
class Call:
    conn: Connection
    principal: Principal
    perm: Permission | None
    args: dict
    as_of: AsOf
    lang: str
    units: str
    mode: str
    offset: int
    inner_cursor: Any
    usage: limits.Usage
    limits: dict
    tool: str

    @property
    def settings(self):
        return runtime.get().settings

    @property
    def blobs(self):
        return runtime.get().blobs

    @property
    def profile_id(self):
        return self.perm.profile.id if self.perm else None

    def gate(self):
        return gate_mod.genome_gate(self.perm, self.as_of.day, self.settings.age_of_majority)


@dataclass
class Result:
    data: dict
    pageable: str | None = None  # key in data of the list the budget may cut and the cursor resumes
    budget: int = DEFAULT_BUDGET  # tokens for structuredContent plus the text block
    max_data_bytes: int | None = None  # get_health_status: data under 2 KB
    limitations: list = field(default_factory=list)
    not_assessed: list = field(default_factory=list)
    evidence: list = field(default_factory=list)
    db_versions: dict = field(default_factory=dict)
    withheld: dict | None = None
    notice: str | None = None
    counters: dict = field(default_factory=dict)  # extra usage: lookups, proposals
    inner_next_cursor: Any = None  # a cursor of the backing service, wrapped in ours
    finalize: Callable[[dict], dict] | None = None  # reorder after the cut (trend points ascending)
    profile_slug: str | None = None  # for tools that are not profile-bound (get_source)
    audit_profile_id: Any = None
    prune: bool = True


@dataclass(frozen=True)
class ToolSpec:
    name: str
    title: str
    description: str
    input_schema: dict
    fn: Callable[[Call], Result]
    any_scope: tuple[str, ...]  # listed and callable with any one of these
    all_scopes: tuple[str, ...] = ()  # and every one of these
    profile_bound: bool = True
    read_only: bool = True

    def visible(self, scopes: frozenset[str]) -> bool:
        return any(s in scopes for s in self.any_scope) and all(s in scopes for s in self.all_scopes)

    def as_mcp(self) -> types.Tool:
        return types.Tool(
            name=self.name,
            title=self.title,
            description=self.description,
            input_schema=self.input_schema,
            output_schema=OUTPUT_SCHEMA,
            annotations=types.ToolAnnotations(
                title=self.title,
                read_only_hint=self.read_only,
                destructive_hint=False,
                idempotent_hint=True,
                open_world_hint=False,
            ),
        )


REGISTRY: dict[str, ToolSpec] = {}


def tool(**kw):
    def deco(fn):
        REGISTRY[kw["name"]] = ToolSpec(fn=fn, **kw)
        return fn

    return deco


# ----------------------------------------------------------------- helpers ----


def tz() -> str | None:
    return os.environ.get("OWNHEALTH_TIMEZONE")


def parse_as_of(value: str | None) -> AsOf:
    if not value:
        d = limits.today(tz())
        return AsOf(day=d, at=None, label=d.isoformat())
    try:
        if "T" in value or " " in value:
            dt = datetime.fromisoformat(value.replace("Z", "+00:00"))
            if dt.tzinfo is None:
                dt = dt.replace(tzinfo=UTC)
            return AsOf(day=dt.date(), at=dt, label=value)
        d = date.fromisoformat(value)
    except ValueError as exc:
        raise ToolError("bad_input", f"as_of '{value}' is not an ISO date.", "Use YYYY-MM-DD.") from exc
    return AsOf(day=d, at=datetime.combine(d, time.max, UTC), label=value)


def db_now(conn: Connection) -> datetime:
    """The database's clock: `recorded_at` comes from it, so a cutoff must too."""
    return conn.execute(text("select now()")).scalar_one()


def union_scopes(conn: Connection, principal: Principal) -> frozenset[str]:
    """What this token can reach on at least one of its profiles; tools/list is filtered by it."""
    out: set[str] = set()
    for perm in accessible_profiles(conn, principal):
        out |= perm.scopes
    return frozenset(out)


SAFE_ARG = re.compile(r"^[A-Za-z0-9_:.\-*/>+]{1,64}$")
HASHED_ARGS = {"query", "drug", "payload", "text"}
OMITTED_ARGS = {"cursor"}


def args_summary(args: dict) -> str:
    """Codes and ids only; free text hashed (mcp.md 9.5)."""
    parts = []
    for k in sorted(args):
        v = args[k]
        if k in OMITTED_ARGS or v is None:
            continue
        if isinstance(v, bool | int | float):
            parts.append(f"{k}={v}")
        elif k in HASHED_ARGS or not isinstance(v, str):
            if isinstance(v, list) and all(isinstance(x, str) and SAFE_ARG.match(x) for x in v):
                parts.append(f"{k}={','.join(v)[:200]}")
            else:
                parts.append(f"{k}={hash_arg(dumps(v) if not isinstance(v, str) else v)}")
        elif SAFE_ARG.match(v):
            parts.append(f"{k}={v}")
        else:
            parts.append(f"{k}={hash_arg(v)}")
    return " ".join(parts)[:1000]


def household_names(conn: Connection, household_id) -> list[str]:
    rows = conn.execute(
        text(
            "select display_name from app_user where household_id = :h"
            " union select display_name from profile where household_id = :h"
        ),
        {"h": household_id},
    ).scalars()
    return redact.name_dictionary(list(rows))


def _error_result(err: ToolError) -> types.CallToolResult:
    body = {"code": err.code, "message": err.message, "fix": err.fix, **jsonable(err.extra)}
    return types.CallToolResult(
        content=[types.TextContent(type="text", text=dumps({"isError": True, **body}))],
        structured_content=body,
        is_error=True,
    )


def _audit(conn: Connection, principal: Principal, spec_name: str, meta: dict, **kw) -> None:
    write_audit(
        conn,
        AuditEvent(
            actor_kind="token",
            actor_id=principal.user_id,
            token_id=principal.token_id,
            client=principal.token_label,
            via="mcp",
            action=f"tools/call:{spec_name}",
            tool=spec_name,
            protocol_version=meta.get("protocol_version"),
            ip=meta.get("ip"),
            user_agent=(meta.get("user_agent") or "")[:200] or None,
            **kw,
        ),
    )


def _access_error(exc: AccessDenied) -> ToolError:
    if exc.code in ("profile_not_granted", "profile_required"):
        allowed = ", ".join(exc.allowed) if exc.allowed else "none"
        return ToolError("profile_not_granted", str(exc), f"Use one of: {allowed}.", allowed=exc.allowed)
    return ToolError("scope_missing", str(exc), "Ask the profile's owner for a token with that scope.")


# ---------------------------------------------------------------- pipeline ----


def execute(name: str, args: dict | None, principal: Principal, meta: dict) -> types.CallToolResult:
    """Run one tools/call. Sync: called in a worker thread by the MCP server."""
    rt = runtime.get()
    args = dict(args or {})
    spec = REGISTRY.get(name)
    lim = limits.limits_of(principal)
    day = limits.today(tz())

    with rt.engine.begin() as conn:
        wait = limits.take_call(conn, principal.token_id, lim["rate_per_min"])
    try:
        if wait is not None:
            raise ToolError("rate_limited", "Too many calls for this token.", f"Retry in {wait} s.", retry_after_s=wait)
        if spec is None:
            raise ToolError("bad_input", f"Unknown tool '{name}'.", "Call tools/list for the tools this token has.")
        with rt.engine.begin() as conn:
            return _run(conn, spec, args, principal, meta, lim, day)
    except ToolError as err:
        with rt.engine.begin() as conn:
            limits.add_usage(conn, principal.token_id, day, calls=1)
            _audit(
                conn,
                principal,
                name if spec else "unknown",
                meta,
                args_summary=args_summary(args),
                result_kind="denied" if err.code in DENIED_CODES else "error",
                rows_out=0,
                bytes_out=0,
            )
        return _error_result(err)


def _run(conn, spec: ToolSpec, args, principal, meta, lim, day) -> types.CallToolResult:
    if not spec.visible(union_scopes(conn, principal)):
        raise ToolError("scope_missing", f"This token cannot use {spec.name}.", "Use a tool from tools/list.")
    try:
        jsonschema.validate(args, spec.input_schema)
    except jsonschema.ValidationError as exc:
        where = ".".join(str(p) for p in exc.absolute_path) or "arguments"
        raise ToolError("bad_input", f"Invalid {where}: {exc.message[:200]}", "Check the tool's input schema.") from exc

    perm = None
    if spec.profile_bound:
        try:
            perm = require(conn, principal, args.get("profile"))
        except AccessDenied as exc:
            raise _access_error(exc) from exc
        if not spec.visible(perm.scopes):
            missing = [s for s in (*spec.all_scopes, *spec.any_scope) if s not in perm.scopes]
            raise ToolError(
                "scope_missing",
                f"This token cannot use {spec.name} on profile '{perm.profile.slug}'.",
                f"Needs {' and '.join(spec.all_scopes) or ' or '.join(spec.any_scope)}; missing {', '.join(missing)}.",
            )

    qhash = query_hash(
        spec.name,
        str(principal.token_id),
        str(perm.profile.id) if perm else None,
        {k: v for k, v in args.items() if k != "cursor"},
    )
    offset, inner = 0, None
    as_of = parse_as_of(args.get("as_of"))
    if as_of.at is None:  # pin "now", so every page of a paginated answer sees the same store
        as_of = AsOf(day=as_of.day, at=db_now(conn), label=as_of.label)
    secret = runtime.get().settings.secret_key
    if args.get("cursor"):
        try:
            cur = decode_cursor(secret, args["cursor"], qhash)
        except CursorError as exc:
            raise ToolError("bad_input", f"Bad cursor: {exc}.", "Repeat the call without cursor.") from exc
        key = cur["last_key"] or {}
        offset, inner = int(key.get("off", 0)), key.get("inner")
        if cur["as_of"]:
            as_of = AsOf(day=as_of.day, at=datetime.fromisoformat(cur["as_of"]), label=as_of.label)

    usage = limits.usage(conn, principal.token_id, day)
    mode = redact.stricter(principal.identity_mode, None)
    call = Call(
        conn=conn,
        principal=principal,
        perm=perm,
        args=args,
        as_of=as_of,
        lang=principal.lang if principal.lang in ("es", "en") else "es",
        units=principal.units,
        mode=mode,
        offset=offset,
        inner_cursor=inner,
        usage=usage,
        limits=lim,
        tool=spec.name,
    )
    try:
        result = spec.fn(call)
    except AccessDenied as exc:
        raise _access_error(exc) from exc
    except NotImplementedError as exc:
        log.warning("tool %s: backing service not available: %s", spec.name, exc)
        raise ToolError("not_available", "This part of OwnHealth is not available yet.", None) from exc
    except ToolError:
        raise
    except Exception as exc:
        log.exception("tool %s failed", spec.name)
        raise ToolError("internal_error", "OwnHealth could not answer this call.", "Try again or report it.") from exc

    env, body_text, tokens = finish(call, result, qhash, secret)
    facts = count_facts(env["data"])
    nbytes = len(dumps(env).encode()) + len(body_text.encode())
    if usage.facts + facts > lim["egress_facts_per_day"] or usage.bytes + nbytes > lim["egress_bytes_per_day"]:
        raise ToolError(
            "egress_budget_exhausted",
            "This token's daily budget is used up.",
            "It resets at 00:00 in the install's timezone; a person can raise it in Settings.",
            facts_today=usage.facts,
            bytes_today=usage.bytes,
        )
    limits.add_usage(conn, principal.token_id, day, calls=1, facts=facts, bytes=nbytes, **result.counters)
    _audit(
        conn,
        principal,
        spec.name,
        meta,
        profile_id=perm.profile.id if perm else result.audit_profile_id,
        args_summary=args_summary(args),
        result_kind="budget_cut" if env["budget"]["truncated"] else "ok",
        rows_out=facts,
        bytes_out=nbytes,
        src_ids=collect_src_ids(env["data"])[:500] or None,
        withheld=env["withheld"],
    )
    return types.CallToolResult(
        content=[types.TextContent(type="text", text=body_text)], structured_content=env, is_error=False
    )


def finish(call: Call, result: Result, qhash: str, secret: str) -> tuple[dict, str, int]:
    """Prune, redact, cut to the budget, sign the cursor, render the text block."""
    data = jsonable(result.data)
    withheld = gate_mod.Withheld()
    withheld.merge(result.withheld)
    if call.perm is not None and result.prune:
        hidden = gate_mod.hidden_reasons(call.conn, call.perm, call.gate(), collect_src_ids(data))
        data = gate_mod.prune(data, hidden, withheld)
    names = household_names(call.conn, call.principal.household_id) if call.mode != "full" else []
    data = redact.apply(data, call.mode, as_of=call.as_of.day, names=names)
    limitations = redact.apply(jsonable(result.limitations), call.mode, as_of=call.as_of.day, names=names)
    not_assessed = redact.apply(jsonable(result.not_assessed), call.mode, as_of=call.as_of.day, names=names)

    budget = min(result.budget, MAX_BUDGET)
    key = result.pageable
    items = list(data.get(key) or [])[call.offset :] if key else []  # the tool returns the whole list
    pin = call.as_of.at or db_now(call.conn)

    def build(n: int):
        d = dict(data)
        more = False
        cursor_key = None
        if key:
            d[key] = items[:n]
            more = n < len(items)
            if more:
                cursor_key = {"off": call.offset + n}
        if result.inner_next_cursor is not None and not more:
            more, cursor_key = True, {"inner": result.inner_next_cursor}
        if result.finalize:
            d = result.finalize(d)
        env = envelope(
            profile=call.perm.profile.slug if call.perm else result.profile_slug,
            as_of=call.as_of.label,
            lang=call.lang,
            identity_mode=call.mode,
            data=d,
            evidence=jsonable(result.evidence),
            limitations=limitations,
            not_assessed=not_assessed,
            withheld=withheld.as_dict(),
            db_versions=result.db_versions,
            next_cursor=encode_cursor(secret, qhash, pin.isoformat(), cursor_key) if cursor_key else None,
            budget={"tokens_est": 0, "truncated": bool(key and n < len(items))},
            notice=result.notice,
        )
        body_text = render_text(env, call.tool)
        tokens = estimate_tokens(env) + estimate_tokens(body_text)
        env["budget"]["tokens_est"] = tokens
        fits = tokens <= budget and (result.max_data_bytes is None or len(dumps(d).encode()) <= result.max_data_bytes)
        return env, body_text, tokens, fits

    env, body_text, tokens, fits = build(len(items))
    if fits or not items:
        return env, body_text, tokens
    lo, hi = 1, len(items) - 1  # always return at least one item, so paging moves forward
    best = build(1)
    while lo <= hi:
        mid = (lo + hi) // 2
        attempt = build(mid)
        if attempt[3]:
            best, lo = attempt, mid + 1
        else:
            hi = mid - 1
    return best[0], best[1], best[2]


# Tool modules register themselves on import.
from ownhealth.mcp.tools import drug, genome, observations, propose, source, status  # noqa: E402, F401
