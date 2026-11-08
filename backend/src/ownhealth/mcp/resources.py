"""MCP resources (mcp.md 7). Each read goes through the same rate bucket, permission, fact gate,
identity mode, egress budget and audit as a tool call; every result is `cacheScope: private`.

| URI                                   | Content                                   | Needs                      |
|---------------------------------------|-------------------------------------------|----------------------------|
| ownhealth://profiles                  | profiles this token may read              | read:summary on one        |
| ownhealth://profile/{id}/status       | get_health_status JSON                    | read:summary               |
| ownhealth://profile/{id}/status.md    | its markdown text block                   | read:summary               |
| ownhealth://source/{src_id}           | get_source without text                   | the fact's scope           |
| ownhealth://rules/{version}           | the rule pack in force                    | a valid token              |
| ownhealth://profile/{id}/access-log   | this profile's audit entries, 30 days     | owner or guardian          |
"""

from __future__ import annotations

import re
from datetime import UTC, datetime, timedelta

import mcp_types as types
from mcp.shared.exceptions import MCPError
from mcp_types import INVALID_PARAMS
from sqlalchemy import text

from ownhealth import runtime, state
from ownhealth.core.access import AccessDenied, Principal, accessible_profiles, require
from ownhealth.core.audit import AuditEvent, write_audit
from ownhealth.mcp import limits, redact
from ownhealth.mcp.envelope import ToolError, collect_src_ids, count_facts, dumps, jsonable, query_hash, untrusted
from ownhealth.mcp.tools import Call, db_now, finish, parse_as_of, tz, union_scopes
from ownhealth.mcp.tools.source import source_data
from ownhealth.mcp.tools.status import get_health_status

MINUTE, DAY = 60_000, 86_400_000
RESOURCE_NOT_FOUND = -32002


def resource_error(code: int, message: str) -> MCPError:
    return MCPError(code=code, message=message)


PROFILE_RE = re.compile(r"^ownhealth://profile/([a-z0-9_-]+)/(status|status\.md|access-log)$")
SOURCE_RE = re.compile(r"^ownhealth://source/(src_[a-z0-9_]+)$")
RULES_RE = re.compile(r"^ownhealth://rules/(.+)$")


def list_resources(conn, principal: Principal) -> list[types.Resource]:
    out = []
    if "read:summary" in union_scopes(conn, principal):  # the same gate _read applies
        out.append(
            types.Resource(
                uri="ownhealth://profiles",
                name="profiles",
                mime_type="application/json",
                description="Profiles this token may read: slug, relationship, age band, data freshness.",
            )
        )
    try:
        version = state.rules_version()
        out.append(
            types.Resource(
                uri=f"ownhealth://rules/{version}",
                name="rules",
                mime_type="application/json",
                description="The rule pack in force, to explain a flag by rule id.",
            )
        )
    except NotImplementedError:
        pass
    for perm in accessible_profiles(conn, principal):
        slug = perm.profile.slug
        if perm.has("read:summary"):
            out.append(
                types.Resource(
                    uri=f"ownhealth://profile/{slug}/status",
                    name=f"{slug} status",
                    mime_type="application/json",
                    description="The health status of this profile.",
                )
            )
            out.append(
                types.Resource(
                    uri=f"ownhealth://profile/{slug}/status.md",
                    name=f"{slug} status (text)",
                    mime_type="text/markdown",
                    description="The health status as markdown.",
                )
            )
        if perm.role in ("owner", "guardian"):
            out.append(
                types.Resource(
                    uri=f"ownhealth://profile/{slug}/access-log",
                    name=f"{slug} access log",
                    mime_type="application/json",
                    description="Who read this profile, 30 days.",
                )
            )
    return out


def list_templates() -> list[types.ResourceTemplate]:
    return [
        types.ResourceTemplate(
            uri_template="ownhealth://source/{src_id}",
            name="source",
            mime_type="application/json",
            description="Where a cited value came from (get_source without text).",
        ),
        types.ResourceTemplate(
            uri_template="ownhealth://profile/{id}/status",
            name="status",
            mime_type="application/json",
            description="The health status of one profile.",
        ),
        types.ResourceTemplate(
            uri_template="ownhealth://profile/{id}/status.md",
            name="status.md",
            mime_type="text/markdown",
            description="The health status as markdown.",
        ),
        types.ResourceTemplate(
            uri_template="ownhealth://profile/{id}/access-log",
            name="access-log",
            mime_type="application/json",
            description="This profile's audit entries, 30 days.",
        ),
    ]


def _call(conn, principal: Principal, perm, args: dict, tool: str, usage, lim) -> Call:
    as_of = parse_as_of(None)
    as_of = type(as_of)(day=as_of.day, at=db_now(conn), label=as_of.label)
    return Call(
        conn=conn,
        principal=principal,
        perm=perm,
        args=args,
        as_of=as_of,
        lang=principal.lang if principal.lang in ("es", "en") else "es",
        units=principal.units,
        mode=redact.stricter(principal.identity_mode, None),
        offset=0,
        inner_cursor=None,
        usage=usage,
        limits=lim,
        tool=tool,
    )


def _profiles(conn, principal: Principal) -> dict:
    today = limits.today(tz())
    items = []
    for perm in accessible_profiles(conn, principal):
        if not perm.has("read:summary"):
            continue
        p = perm.profile
        fresh = {}
        for label, cat, scope in (("labs", "laboratory", "read:labs"), ("vitals", "vital-signs", "read:vitals")):
            if perm.has(scope):
                fresh[label] = conn.execute(
                    text(
                        "select max(effective_at)::date from observation where profile_id = :p and category = :c"
                        " and status = 'current'"
                    ),
                    {"p": p.id, "c": cat},
                ).scalar()
        if perm.has("read:wearables"):
            fresh["wearables"] = conn.execute(
                text("select max(t)::date from wearable_rollup where profile_id = :p"), {"p": p.id}
            ).scalar()
        item = {
            "slug": p.slug,
            "relationship": p.relationship,
            "role": perm.role,
            "age_band": redact.age_band(p.age_on(today)),
            "scopes": sorted(perm.scopes),
            "data_freshness": fresh,
            "default": p.id == principal.default_profile,
        }
        if principal.identity_mode == "full":
            item["display_name"] = p.display_name
        items.append(item)
    return {"profiles": items}


def _access_log(conn, perm) -> dict:
    rows = conn.execute(
        text(
            "select ts, actor_kind, client, via, action, tool, result_kind, rows_out, withheld from audit_event"
            " where profile_id = :p and ts > :since order by id desc limit 200"
        ),
        {"p": perm.profile.id, "since": datetime.now(UTC) - timedelta(days=30)},
    ).mappings()
    entries = []
    for r in rows:
        e = dict(r)
        e["client"] = untrusted(e["client"], "person_note") if e["client"] else None
        entries.append(e)
    return {"entries": entries, "days": 30}


def read(uri: str, principal: Principal, meta: dict) -> types.ReadResourceResult:
    """Sync: run in a worker thread by the server."""
    rt = runtime.get()
    lim = limits.limits_of(principal)
    day = limits.today(tz())
    with rt.engine.begin() as conn:
        wait = limits.take_call(conn, principal.token_id, lim["rate_per_min"])
    if wait is not None:
        raise resource_error(INVALID_PARAMS, f"rate_limited: retry in {wait} s")
    try:
        with rt.engine.begin() as conn:
            return _read(conn, uri, principal, meta, lim, day)
    except (ToolError, AccessDenied) as exc:
        code = getattr(exc, "code", "unknown")
        with rt.engine.begin() as conn:
            limits.add_usage(conn, principal.token_id, day, calls=1)
            write_audit(
                conn,
                AuditEvent(
                    actor_kind="token",
                    actor_id=principal.user_id,
                    token_id=principal.token_id,
                    client=principal.token_label,
                    via="mcp",
                    action="resources/read",
                    resource_type=uri.split("/")[2] if uri.count("/") >= 2 else None,
                    resource_ids=[uri[:200]],
                    result_kind="denied",
                    protocol_version=meta.get("protocol_version"),
                    ip=meta.get("ip"),
                ),
            )
        # one answer for every refusal, so a uri never tells whether its target exists
        raise resource_error(RESOURCE_NOT_FOUND, f"Resource not available to this token ({code}).") from None


def _read(conn, uri, principal, meta, lim, day) -> types.ReadResourceResult:
    usage = limits.usage(conn, principal.token_id, day)
    mime, ttl, profile_id = "application/json", MINUTE, None
    body: str
    if uri == "ownhealth://profiles":
        if "read:summary" not in union_scopes(conn, principal):
            raise ToolError("scope_missing", "needs read:summary")
        content = jsonable(_profiles(conn, principal))
        body = dumps(content)
    elif m := RULES_RE.match(uri):
        if m.group(1) != state.rules_version():
            raise ToolError("unknown", "no such rule pack version")
        from ownhealth.state.pack import load_pack

        pack = load_pack()  # the files in force, local overrides included
        content = jsonable({"version": pack.version, "sha256": pack.sha256, "files": pack.files})
        body, ttl = dumps(content), DAY
    elif m := SOURCE_RE.match(uri):
        call = _call(conn, principal, None, {"src_id": m.group(1)}, "get_source", usage, lim)
        result = source_data(call, m.group(1), include_text=False)
        env, _, _ = finish(call, result, query_hash(uri, str(principal.token_id)), rt_secret())
        content, body, ttl, profile_id = env, dumps(env), DAY, call.perm.profile.id
    elif m := PROFILE_RE.match(uri):
        slug, what = m.groups()
        perm = require(conn, principal, slug)
        profile_id = perm.profile.id
        if what == "access-log":
            if perm.role not in ("owner", "guardian"):
                raise ToolError("scope_missing", "owner or guardian only")
            content = jsonable(_access_log(conn, perm))
            body = dumps(content)
        else:
            if not perm.has("read:summary"):
                raise ToolError("scope_missing", "needs read:summary")
            call = _call(conn, principal, perm, {"profile": slug}, "get_health_status", usage, lim)
            env, text_block, _ = finish(
                call, get_health_status(call), query_hash(uri, str(principal.token_id)), rt_secret()
            )
            content = env
            if what == "status.md":
                body, mime = text_block, "text/markdown"
            else:
                body = dumps(env)
    else:
        raise ToolError("unknown", "unknown resource")

    facts = count_facts(content)
    nbytes = len(body.encode())
    if usage.facts + facts > lim["egress_facts_per_day"] or usage.bytes + nbytes > lim["egress_bytes_per_day"]:
        raise ToolError("egress_budget_exhausted", "daily budget used up")
    limits.add_usage(conn, principal.token_id, day, calls=1, facts=facts, bytes=nbytes)
    write_audit(
        conn,
        AuditEvent(
            actor_kind="token",
            actor_id=principal.user_id,
            token_id=principal.token_id,
            client=principal.token_label,
            via="mcp",
            action="resources/read",
            resource_type=uri.split("/")[2],
            resource_ids=[uri[:200]],
            profile_id=profile_id,
            src_ids=collect_src_ids(content)[:500] or None,
            result_kind="ok",
            rows_out=facts,
            bytes_out=nbytes,
            protocol_version=meta.get("protocol_version"),
            ip=meta.get("ip"),
        ),
    )
    return types.ReadResourceResult(
        contents=[types.TextResourceContents(uri=uri, mime_type=mime, text=body)], ttl_ms=ttl, cache_scope="private"
    )


def rt_secret() -> str:
    return runtime.get().settings.secret_key
