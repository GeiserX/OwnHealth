"""Per-token rate limit, daily egress budget, usage counters and the audit log (no payloads)."""

from __future__ import annotations

import pytest
from mcp_checks import err, sc
from sqlalchemy import text

from ownhealth.core.access import ALL_SCOPES
from ownhealth.mcp.limits import take_call

pytestmark = pytest.mark.anyio


def tok_row(engine, tok):
    with engine.connect() as conn:
        return conn.execute(text("select * from api_token where prefix = :p"), {"p": tok[:15]}).mappings().one()


def set_limit(engine, tok, **cols):
    with engine.begin() as conn:
        for k, v in cols.items():
            conn.execute(text(f"update api_token set {k} = :v where prefix = :p"), {"v": v, "p": tok[:15]})


async def test_rate_limit_per_token(world, make_token, connect, fakes, engine):
    slow, other = make_token(), make_token()
    set_limit(engine, slow, rate_per_min=2)  # burst min(20, 2)
    async with connect(slow) as c:
        for _ in range(2):
            sc(await c.call_tool("get_health_status", {"profile": "alex"}))
        e = err(await c.call_tool("get_health_status", {"profile": "alex"}))
        assert e["code"] == "rate_limited" and e["retry_after_s"] > 0
    async with connect(other) as c:  # the bucket is per token
        sc(await c.call_tool("get_health_status", {"profile": "alex"}))


def test_bucket_refills(engine, world, make_token):
    from datetime import UTC, datetime, timedelta

    tid = tok_row(engine, make_token())["id"]
    t0 = datetime(2026, 11, 7, 12, tzinfo=UTC)
    with engine.begin() as conn:
        results = [take_call(conn, tid, 60, now=t0) for _ in range(21)]
        assert results[:20] == [None] * 20 and results[20] is not None  # burst 20
        assert take_call(conn, tid, 60, now=t0 + timedelta(seconds=1)) is None  # 1 per second at 60/min
        assert take_call(conn, tid, 60, now=t0 + timedelta(seconds=1)) is not None


async def test_daily_egress_budget(world, make_token, connect, fakes, engine):
    tok = make_token()
    set_limit(engine, tok, egress_facts_per_day=10)
    async with connect(tok) as c:
        first = sc(await c.call_tool("search_observations", {"profile": "alex", "loinc": "13457-7", "detail": "full"}))
        assert len(first["data"]["results"]) == 8
        e = err(await c.call_tool("search_observations", {"profile": "alex", "loinc": "13457-7", "detail": "full"}))
        assert e["code"] == "egress_budget_exhausted" and e["facts_today"] == 8
    with engine.connect() as conn:
        used = conn.execute(
            text("select calls, facts from mcp_usage where token_id = :t"), {"t": tok_row(engine, tok)["id"]}
        ).one()
    assert used.calls == 2 and used.facts == 8  # the refused call delivered nothing


async def test_every_call_is_audited_without_payloads(world, make_token, connect, fakes, engine):
    tok = make_token()
    secret_query = "colesterol de mi padre"
    async with connect(tok) as c:
        sc(await c.call_tool("search_observations", {"profile": "alex", "query": "Colesterol LDL"}))
        await c.call_tool("search_observations", {"profile": "alex", "query": secret_query})
        await c.call_tool("get_health_status", {"profile": "carmen"})
        await c.call_tool("get_trend", {"profile": "alex", "code": "13457-7"})
    tid = tok_row(engine, tok)["id"]
    with engine.connect() as conn:
        rows = (
            conn.execute(text("select * from audit_event where token_id = :t order by id"), {"t": tid}).mappings().all()
        )
    assert [r["tool"] for r in rows] == ["search_observations", "search_observations", "get_health_status", "get_trend"]
    assert all(r["via"] == "mcp" and r["actor_kind"] == "token" and r["action"].startswith("tools/call:") for r in rows)
    assert [r["result_kind"] for r in rows] == ["ok", "ok", "denied", "ok"]
    assert rows[0]["src_ids"] == [world.src["ldl_latest"]] and rows[0]["rows_out"] == 1 and rows[0]["bytes_out"] > 0
    assert rows[0]["profile_id"] == world.pid("alex") and rows[0]["client"] == "Claude Code · test"
    assert "code=13457-7" in rows[3]["args_summary"]
    everything = str([dict(r) for r in rows])
    assert (
        secret_query not in everything
        and "Colesterol" not in everything
        and "97" not in str([r["args_summary"] for r in rows])
    )
    assert "query=h:" in rows[1]["args_summary"]


async def test_usage_route(world, make_token, connect, fakes, engine, app_for):
    tok = make_token()
    async with connect(tok) as c:
        sc(await c.call_tool("get_health_status", {"profile": "alex"}))
    tid = tok_row(engine, tok)["id"]
    alex = app_for(world.family.users["alex"])
    body = alex.get(f"/api/tokens/{tid}/usage").json()
    assert body["today"]["calls"] == 1 and body["limits"]["rate_per_min"] == 60 and body["history"]
    sam = app_for(world.family.users["sam"])
    assert sam.get(f"/api/tokens/{tid}/usage").status_code == 404


async def test_propose_daily_cap(world, make_token, connect, fakes, engine):
    tok = make_token(scopes=ALL_SCOPES)
    set_limit(engine, tok, proposals_per_day=1)
    async with connect(tok) as c:
        sc(await c.call_tool("propose", {"profile": "alex", "kind": "note", "payload": {"text": "first"}}))
        # the same payload again is the same proposal, not a new one
        again = sc(await c.call_tool("propose", {"profile": "alex", "kind": "note", "payload": {"text": "first"}}))
        assert again["data"]["duplicate_of_open_proposal"] is True
        e = err(await c.call_tool("propose", {"profile": "alex", "kind": "note", "payload": {"text": "second"}}))
        assert e["code"] == "propose_cap"
