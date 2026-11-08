"""Transport, protocol versions, the PAT door and the shape of tools/list."""

from __future__ import annotations

import json

import pytest
from mcp_checks import assert_envelope, sc
from sqlalchemy import text

from ownhealth.core.access import ALL_SCOPES
from ownhealth.mcp.server import INSTRUCTIONS
from ownhealth.mcp.tools import REGISTRY

pytestmark = pytest.mark.anyio

FORBIDDEN_WORDS = ("share", "export", "send", "email", "url", "webhook", "upload")


@pytest.mark.parametrize(
    "mode,expected", [("2026-07-28", "2026-07-28"), ("legacy", "2025-11-25"), ("auto", "2026-07-28")]
)
async def test_both_protocol_versions_on_one_endpoint(world, make_token, connect, fakes, engine, mode, expected):
    tok = make_token()
    async with connect(tok, mode=mode) as c:
        assert c.protocol_version == expected
        env = sc(await c.call_tool("get_health_status", {"profile": "alex"}))
        assert_envelope(env)
    with engine.connect() as conn:
        pv = conn.execute(
            text("select protocol_version from audit_event where tool = 'get_health_status' order by id desc limit 1")
        ).scalar()
    assert pv == expected


async def test_missing_bad_and_revoked_tokens_are_401(world, make_token, raw_http, engine):
    body = {"jsonrpc": "2.0", "id": 1, "method": "tools/list"}
    hdrs = {"Accept": "application/json, text/event-stream", "MCP-Protocol-Version": "2025-11-25"}
    tok = make_token()
    async with raw_http() as hc:
        r = await hc.post("/mcp", json=body, headers=hdrs)
        assert r.status_code == 401
        assert (
            'resource_metadata="http://127.0.0.1:8080/.well-known/oauth-protected-resource"'
            in r.headers["www-authenticate"]
        )
        meta = await hc.get("/.well-known/oauth-protected-resource")  # the document the 401 points at exists
        assert meta.status_code == 200 and meta.json()["resource"] == "http://127.0.0.1:8080/mcp"
        assert meta.json()["bearer_methods_supported"] == ["header"] and "authorization_servers" not in meta.json()
        r = await hc.post("/mcp", json=body, headers={**hdrs, "Authorization": "Bearer oh_pat_AAAAAAAA_nope"})
        assert r.status_code == 401 and 'error="invalid_token"' in r.headers["www-authenticate"]
        r = await hc.post("/mcp", json=body, headers={**hdrs, "Authorization": f"Bearer {tok}"})
        assert r.status_code != 401
        with engine.begin() as conn:
            conn.execute(text("update api_token set revoked_at = now() where prefix = :p"), {"p": tok[:15]})
        r = await hc.post("/mcp", json=body, headers={**hdrs, "Authorization": f"Bearer {tok}"})
        assert r.status_code == 401


async def test_origin_must_match_the_public_url(world, make_token, raw_http):
    tok = make_token()
    hdrs = {
        "Accept": "application/json, text/event-stream",
        "Authorization": f"Bearer {tok}",
        "MCP-Protocol-Version": "2025-11-25",
    }
    init = {
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {"protocolVersion": "2025-11-25", "capabilities": {}, "clientInfo": {"name": "t", "version": "0"}},
    }
    async with raw_http() as hc:
        r = await hc.post("/mcp", json=init, headers={**hdrs, "Origin": "https://evil.example"})
        assert r.status_code == 403
        r = await hc.post("/mcp", json=init, headers={**hdrs, "Origin": "http://127.0.0.1:8080"})
        assert r.status_code == 200, r.text


async def test_tool_list_is_sorted_annotated_and_has_no_outbound_tool(world, make_token, connect):
    async with connect(make_token(scopes=ALL_SCOPES)) as c:
        tools = (await c.list_tools()).tools
    names = [t.name for t in tools]
    assert names == sorted(names) == sorted(REGISTRY)
    assert "ping" not in names
    for t in tools:
        assert t.annotations.open_world_hint is False
        assert t.annotations.read_only_hint is (t.name != "propose")
        blob = (t.name + json.dumps(t.input_schema)).lower()
        assert not any(w in blob for w in FORBIDDEN_WORDS), t.name
        assert t.output_schema["type"] == "object"
    assert "untrusted_text" in INSTRUCTIONS


@pytest.mark.parametrize(
    "scopes,expected",
    [
        ({"read:summary"}, {"get_health_status", "get_source"}),
        (
            {"read:summary", "read:labs", "read:vitals"},
            {"get_health_status", "get_source", "search_observations", "get_trend", "check_drug"},
        ),
        ({"read:genome_raw"}, set()),  # lookups also need read:genome
        ({"read:genome", "read:genome_raw"}, {"get_genome_findings", "lookup_variant", "check_drug", "get_source"}),
        ({"read:records", "write:propose"}, {"check_drug", "get_source", "propose"}),
    ],
)
async def test_tools_list_follows_scopes(world, make_token, connect, scopes, expected):
    async with connect(make_token(scopes=scopes)) as c:
        names = {t.name for t in (await c.list_tools()).tools}
    assert names == expected


async def test_tools_list_follows_the_grant_not_only_the_token(world, make_token, connect):
    # Sam is only a viewer of Alex: every scope on the token, but the grant allows read:summary
    tok = make_token(scopes=ALL_SCOPES, profiles=("alex",), user="sam")
    async with connect(tok) as c:
        names = {t.name for t in (await c.list_tools()).tools}
        assert names == {"get_health_status", "get_source"}
        r = await c.call_tool("search_observations", {"profile": "alex", "query": "ldl"})
    assert r.is_error and r.structured_content["code"] == "scope_missing"
