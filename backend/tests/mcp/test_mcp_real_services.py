"""The same contract against lanes D and E as they ship, without the fakes: every tool answers in the
envelope with a src on its facts, nothing crashes, and the genome and sensitive gates hold."""

from __future__ import annotations

import pytest
from conftest import set_switch
from mcp_checks import all_srcs, assert_envelope, dumps, err, facts, sc

from ownhealth.core.access import ALL_SCOPES

pytestmark = pytest.mark.anyio
NO_GENOME = ALL_SCOPES - {"read:genome", "read:genome_raw"}
CALLS = [
    ("get_health_status", {"profile": "alex"}),
    ("search_observations", {"profile": "alex", "query": "LDL", "detail": "full"}),
    ("get_trend", {"profile": "alex", "code": "13457-7", "window": "all"}),
    ("check_drug", {"profile": "alex", "drug": "simvastatin"}),
    ("get_genome_findings", {"profile": "alex", "detail": "full"}),
    ("get_source", {"src_id": "vitd"}),
]


async def test_every_tool_answers_in_the_envelope(world, make_token, connect):
    async with connect(make_token(scopes=ALL_SCOPES)) as c:
        for name, args in CALLS:
            if args.get("src_id") in world.src:
                args = {**args, "src_id": world.src[args["src_id"]]}
            r = await c.call_tool(name, args)
            env = sc(r)
            assert_envelope(env)
            assert all(f["src"] for f in facts(env["data"])), name
        e = err(await c.call_tool("lookup_variant", {"profile": "alex", "rsid": "rs1"}))
        assert e["code"] != "internal_error"


async def test_real_services_hold_the_genome_and_sensitive_gates(world, make_token, connect, engine):
    genome_srcs = {world.src[k] for k in ("slco1b1", "apoe", "hfe")}
    async with connect(make_token(scopes=NO_GENOME)) as c:
        for name, args in CALLS[:4]:
            env = sc(await c.call_tool(name, args))
            # drug.genes names the genes CPIC ties to the drug (knowledge, not Alex's result)
            assert not all_srcs(env) & genome_srcs and "Decreased function" not in dumps(env), name
    set_switch(engine, world.pid("alex"), True)
    async with connect(make_token(scopes=ALL_SCOPES - {"read:sensitive"})) as c:
        env = sc(await c.call_tool("get_genome_findings", {"profile": "alex", "detail": "full"}))
        assert world.src["apoe"] not in all_srcs(env) and "APOE" not in dumps(env)
    async with connect(make_token(scopes=ALL_SCOPES)) as c:
        env = sc(await c.call_tool("get_genome_findings", {"profile": "alex", "detail": "full"}))
        assert world.src["apoe"] in all_srcs(env)  # the positive control: the gate opens when it should


async def test_genome_gate_control_check_drug_shows_the_phenotype_with_read_genome(world, make_token, connect):
    async with connect(make_token(scopes=ALL_SCOPES)) as c:
        env = sc(await c.call_tool("check_drug", {"profile": "alex", "drug": "simvastatin"}))
    assert "Decreased function" in dumps(env) and world.src["slco1b1"] in all_srcs(env)


async def test_lookup_of_a_withheld_sensitive_site_is_counted_not_crashed(world, make_token, connect):
    async with connect(make_token(scopes=ALL_SCOPES)) as c:  # the agents switch is off: APOE stays back
        env = sc(await c.call_tool("lookup_variant", {"profile": "alex", "rsid": "rs429358"}))
    assert env["data"]["withheld"] == "withheld_sensitive" and env["data"]["calls"] == []
    assert env["withheld"]["sensitive"] >= 1
