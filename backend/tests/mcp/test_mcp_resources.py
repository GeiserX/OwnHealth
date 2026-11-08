"""Resources and prompts (mcp.md 6, 7): listed per token, read under the same gates as the tools,
every read audited, one refusal answer whatever the reason."""

from __future__ import annotations

import json

import pytest
from mcp.shared.exceptions import MCPError
from mcp_checks import sc
from sqlalchemy import text

from ownhealth.core.access import ALL_SCOPES

pytestmark = pytest.mark.anyio


def audited(engine, token_prefix: str) -> list[tuple[str, str]]:
    with engine.connect() as conn:
        rows = conn.execute(
            text(
                "select e.action, e.result_kind from audit_event e join api_token t on t.id = e.token_id"
                " where t.prefix = :p order by e.id"
            ),
            {"p": token_prefix},
        ).all()
    return [tuple(r) for r in rows]


async def test_resources_follow_the_token(world, make_token, connect, fakes):
    async with connect(make_token(profiles=("alex", "sam"))) as c:
        uris = {str(r.uri) for r in (await c.list_resources()).resources}
        assert "ownhealth://profiles" in uris and "ownhealth://profile/alex/access-log" in uris
        assert "ownhealth://profile/sam/status" in uris
        assert "ownhealth://profile/sam/access-log" not in uris  # Alex only views Sam
        templates = {t.uri_template for t in (await c.list_resource_templates()).resource_templates}
        assert "ownhealth://source/{src_id}" in templates
    async with connect(make_token(scopes={"read:labs"})) as c:
        uris = {str(r.uri) for r in (await c.list_resources()).resources}
        assert "ownhealth://profiles" not in uris and not any("/status" in u for u in uris)


async def test_profiles_hide_names_when_pseudonymous(world, make_token, connect, fakes):
    async with connect(make_token(profiles=("alex", "sam"))) as c:
        full = json.loads((await c.read_resource("ownhealth://profiles")).contents[0].text)
    assert {p["slug"] for p in full["profiles"]} == {"alex", "sam"}
    assert all(p.get("display_name") for p in full["profiles"])
    async with connect(make_token(identity_mode="pseudonymous")) as c:
        pseudo = json.loads((await c.read_resource("ownhealth://profiles")).contents[0].text)
    assert "display_name" not in json.dumps(pseudo)


async def test_status_resource_matches_the_tool(world, make_token, connect, fakes):
    async with connect(make_token()) as c:
        tool = sc(await c.call_tool("get_health_status", {"profile": "alex"}))
        res = await c.read_resource("ownhealth://profile/alex/status")
        assert res.contents[0].mime_type == "application/json"
        env = json.loads(res.contents[0].text)
        assert env["data"]["state_hash"] == tool["data"]["state_hash"]
        md = (await c.read_resource("ownhealth://profile/alex/status.md")).contents[0]
        assert md.mime_type == "text/markdown" and md.text.startswith("OwnHealth get_health_status")


async def test_source_and_rules(world, make_token, connect, fakes):
    async with connect(make_token()) as c:
        env = json.loads((await c.read_resource(f"ownhealth://source/{world.src['vitd']}")).contents[0].text)
        assert env["data"]["fact"]["table"] == "observation" and env["data"]["text"] is None  # never page text
        rules = [str(r.uri) for r in (await c.list_resources()).resources if "/rules/" in str(r.uri)]
        assert rules
        body = json.loads((await c.read_resource(rules[0])).contents[0].text)
        assert body["version"] and body["files"]  # the real pack, not an empty shell
        with pytest.raises(MCPError):
            await c.read_resource("ownhealth://rules/0.0.0-nope")


async def test_access_log_owner_only_and_reads_are_audited(world, make_token, connect, fakes, engine):
    tok = make_token(profiles=("alex", "sam"))
    async with connect(tok) as c:
        await c.call_tool("get_health_status", {"profile": "alex"})
        log = json.loads((await c.read_resource("ownhealth://profile/alex/access-log")).contents[0].text)
        assert any(e["tool"] == "get_health_status" for e in log["entries"])
        with pytest.raises(MCPError) as denied:
            await c.read_resource("ownhealth://profile/sam/access-log")
        with pytest.raises(MCPError) as missing:
            await c.read_resource("ownhealth://source/src_doesnotexist")
        assert denied.value.error.code == missing.value.error.code  # a uri never tells what exists
    reads = [r for r in audited(engine, tok[:15]) if r[0] == "resources/read"]
    assert reads == [("resources/read", "ok"), ("resources/read", "denied"), ("resources/read", "denied")]


async def test_prompts_filtered_and_rendered(world, make_token, connect, fakes):
    async with connect(make_token(scopes={"read:labs"})) as c:
        names = {p.name for p in (await c.list_prompts()).prompts}
        assert names == {"check-before-new-drug"}
        with pytest.raises(MCPError):
            await c.get_prompt("prepare-doctor-visit", {"profile": "alex"})
    async with connect(make_token(scopes=ALL_SCOPES)) as c:
        names = {p.name for p in (await c.list_prompts()).prompts}
        assert names == {"prepare-doctor-visit", "check-before-new-drug", "what-to-measure-next"}
        got = await c.get_prompt("check-before-new-drug", {"profile": "alex", "drug": "simvastatin", "lang": "es"})
        body = got.messages[0].content.text
        assert "simvastatin" in body and "check_drug" in body and "Coméntalo" in body
        with pytest.raises(MCPError):
            await c.get_prompt("check-before-new-drug", {"profile": "alex"})  # drug is required
        with pytest.raises(MCPError):
            await c.get_prompt("no-such-prompt", {})
