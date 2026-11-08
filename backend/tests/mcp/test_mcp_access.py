"""effective = token.scopes ∩ grant.categories − profile.locks, plus the genome, sensitive and minor
gates, on every tool. The lane fakes ignore the gates on purpose; the MCP layer must hold them."""

from __future__ import annotations

import pytest
from conftest import set_switch
from mcp_checks import all_srcs, dumps, err, sc

from ownhealth.core.access import ALL_SCOPES, set_locks

pytestmark = pytest.mark.anyio
NO_GENOME = ALL_SCOPES - {"read:genome", "read:genome_raw"}


async def test_ungranted_profile_lists_the_allowed_slugs(world, make_token, connect, fakes):
    async with connect(make_token(profiles=("alex", "lucia"))) as c:
        e = err(await c.call_tool("get_health_status", {"profile": "carmen"}))
        assert e["code"] == "profile_not_granted"
        assert e["fix"] == "Use one of: alex, lucia." and e["allowed"] == ["alex", "lucia"]
        e = err(await c.call_tool("get_health_status", {"profile": "sam"}))  # Alex is a viewer of Sam, not on token
        assert e["code"] == "profile_not_granted"
        assert sc(await c.call_tool("get_health_status", {}))["profile"] == "alex"  # default profile


async def test_locks_bind_everyone_but_the_owner(world, make_token, connect, fakes, engine):
    with engine.begin() as conn:
        set_locks(conn, world.pid("alex"), {"read:labs"})
    async with connect(make_token(profiles=("alex",))) as c:  # Alex owns Alex: locks do not bind him
        assert sc(await c.call_tool("search_observations", {"profile": "alex", "query": "ldl"}))["data"]["results"]
    with engine.begin() as conn:
        set_locks(conn, world.pid("lucia"), {"read:genome"})
    async with connect(make_token(profiles=("lucia",), user="sam")) as c:  # Sam is Lucía's guardian
        names = {t.name for t in (await c.list_tools()).tools}
        assert "get_genome_findings" not in names
        e = err(await c.call_tool("get_genome_findings", {"profile": "lucia"}))
        assert e["code"] == "scope_missing"


async def test_without_read_genome_no_genome_fact_appears_anywhere(world, make_token, connect, fakes):
    genome_srcs = {world.src[k] for k in ("slco1b1", "apoe", "hfe")}
    async with connect(make_token(scopes=NO_GENOME)) as c:
        names = {t.name for t in (await c.list_tools()).tools}
        assert not names & {"get_genome_findings", "lookup_variant"}
        status = sc(await c.call_tool("get_health_status", {"profile": "alex"}))
        assert not all_srcs(status) & genome_srcs
        assert not [f for f in status["data"]["flags"] if f["kind"] == "gene_drug"]
        assert "genome" in status["withheld"]["by_scope"] and "genome" not in status["data"]["data_freshness"]
        drug = sc(await c.call_tool("check_drug", {"profile": "alex", "drug": "atorvastatin"}))
        assert "pgx" not in drug["data"] and "guidance" not in drug["data"]
        assert "genome" in drug["withheld"]["by_scope"]
        assert not all_srcs(drug) & genome_srcs and "SLCO1B1" not in dumps(drug)
        e = err(await c.call_tool("get_source", {"src_id": world.src["slco1b1"]}))
        assert e["code"] == "unknown"  # same answer as an id that never existed
        e2 = err(await c.call_tool("get_source", {"src_id": "src_neverexisted"}))
        assert (e["code"], e["message"]) == (e2["code"], e2["message"])
    async with connect(make_token(scopes=ALL_SCOPES)) as c:
        status = sc(await c.call_tool("get_health_status", {"profile": "alex"}))
        assert [f for f in status["data"]["flags"] if f["kind"] == "gene_drug"]


async def test_sensitive_genome_needs_switch_and_both_scopes(world, make_token, connect, fakes, engine):
    apoe = world.src["apoe"]
    full = make_token(scopes=ALL_SCOPES)
    no_sensitive = make_token(scopes=ALL_SCOPES - {"read:sensitive"})
    async with connect(full) as c:  # switch off by default
        env = sc(await c.call_tool("get_genome_findings", {"profile": "alex"}))
        assert apoe not in all_srcs(env) and "APOE" not in dumps(env) and env["withheld"]["sensitive"] == 1
    set_switch(engine, world.pid("alex"), True)
    async with connect(no_sensitive) as c:
        env = sc(await c.call_tool("get_genome_findings", {"profile": "alex"}))
        assert apoe not in all_srcs(env) and env["withheld"]["sensitive"] == 1
    async with connect(full) as c:
        env = sc(await c.call_tool("get_genome_findings", {"profile": "alex"}))
        assert apoe in all_srcs(env) and env["withheld"]["sensitive"] == 0


async def test_minor_rule_withholds_carrier_but_shows_pgx(world, make_token, connect, fakes, engine):
    set_switch(engine, world.pid("lucia"), True)
    async with connect(make_token(scopes=ALL_SCOPES, profiles=("lucia",))) as c:
        env = sc(await c.call_tool("get_genome_findings", {"profile": "lucia"}))
    srcs = all_srcs(env)
    assert world.src["lucia_pgx"] in srcs and world.src["lucia_carrier"] not in srcs
    assert env["withheld"]["minor_rule"] == 1 and "CFTR" not in dumps(env)


async def test_sensitive_observations_need_read_sensitive(world, make_token, connect, fakes):
    async with connect(make_token(scopes=ALL_SCOPES - {"read:sensitive"})) as c:
        env = sc(await c.call_tool("search_observations", {"profile": "alex", "query": "PHQ-9", "detail": "full"}))
        assert env["data"]["results"] == [] and env["withheld"]["sensitive"] == 1
        e = err(await c.call_tool("get_source", {"src_id": world.src["phq9"]}))
        assert e["code"] == "unknown"
    async with connect(make_token(scopes=ALL_SCOPES)) as c:
        env = sc(await c.call_tool("search_observations", {"profile": "alex", "query": "PHQ-9"}))
        assert [r["src"] for r in env["data"]["results"]] == [world.src["phq9"]]


async def test_category_scopes_split_one_search(world, make_token, connect, fakes):
    async with connect(make_token(scopes={"read:summary", "read:vitals"})) as c:
        env = sc(await c.call_tool("search_observations", {"profile": "alex", "query": "LDL"}))
        assert env["data"]["results"] == [] and env["withheld"]["by_scope"] == ["labs"]
        env = sc(await c.call_tool("search_observations", {"profile": "alex", "query": "Peso"}))
        assert [r["src"] for r in env["data"]["results"]] == [world.src["weight"]]
        e = err(await c.call_tool("get_source", {"src_id": world.src["vitd"]}))
        assert e["code"] == "scope_missing"


async def test_another_profiles_src_is_unknown(world, make_token, connect, fakes):
    async with connect(make_token(profiles=("alex",))) as c:
        e = err(await c.call_tool("get_source", {"src_id": world.src["sam_ldl"]}))
    assert e["code"] == "unknown"
