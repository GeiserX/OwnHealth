"""Response budgets (~4k tokens default, 15k max, status under 2 KB), cursor pagination that walks
every row exactly once, and cursors that only work for their own query and token."""

from __future__ import annotations

import json
import math

import pytest
from conftest import add_obs
from mcp_checks import err, facts, sc

from ownhealth.core.access import ALL_SCOPES

pytestmark = pytest.mark.anyio
N_GROUPS, PER_GROUP = 100, 6  # 600 observations: the largest fixture in this suite


@pytest.fixture
def big(world, engine):
    carmen = world.pid("carmen")
    with engine.begin() as conn:
        for g in range(N_GROUPS):
            for i in range(PER_GROUP):
                add_obs(
                    conn,
                    carmen,
                    f"Analito número {g} de prueba",
                    str(50 + i),
                    "mg/dL",
                    f"analyte_{g:03d}",
                    f"{10000 + g}-{g % 10}",
                    f"20{18 + i}-0{1 + g % 9}-1{i % 9}",
                    low="40",
                    high="60",
                    flag="H" if i > 6 else "N",
                )
    return world


def measured(result) -> int:
    """Our own measure, not the server's: len(json)/3.5 over structuredContent plus the text block."""
    body = json.dumps(result.structured_content, ensure_ascii=False, separators=(",", ":"))
    return math.ceil(len(body) / 3.5) + math.ceil(len(result.content[0].text) / 3.5)


async def walk(c, args):
    pages, cursor = [], None
    while True:
        r = await c.call_tool("search_observations", {**args, **({"cursor": cursor} if cursor else {})})
        env = sc(r)
        pages.append((r, env))
        cursor = env["next_cursor"]
        if not cursor:
            return pages
        assert len(pages) < 100


async def test_full_detail_pages_stay_under_15k_and_cover_everything(big, make_token, connect, fakes):
    async with connect(make_token(profiles=("carmen",), user="alex")) as c:
        pages = await walk(c, {"profile": "carmen", "query": "Analito", "detail": "full"})
    assert len(pages) > 1
    seen = []
    for r, env in pages:
        assert measured(r) <= 15000, measured(r)
        assert env["budget"]["tokens_est"] <= 15000
        seen += [f["src"] for f in facts(env["data"])]
    assert len(seen) == len(set(seen)) == N_GROUPS * PER_GROUP
    assert [env["budget"]["truncated"] for _, env in pages] == [True] * (len(pages) - 1) + [False]


async def test_summary_default_stays_under_4k(big, make_token, connect, fakes):
    async with connect(make_token(profiles=("carmen",), user="alex")) as c:
        pages = await walk(c, {"profile": "carmen", "query": "Analito"})
    total = sum(len(env["data"]["results"]) for _, env in pages)
    assert total == N_GROUPS  # latest value per analyte
    for r, _ in pages:
        assert measured(r) <= 4000, measured(r)


async def test_trend_and_status_budgets(big, make_token, connect, fakes, monkeypatch):
    from ownhealth import state

    def many_flags(conn, profile_id, *, as_of):
        flags = [
            {
                "rank": i,
                "kind": "out_of_range",
                "code": f"LOINC:{i}-0",
                "value": "1",
                "unit": "mg/dL",
                "src": "src_none",
            }
            for i in range(1, 80)
        ]
        return {"rules": "ownhealth-rules 0.1.0", "flags": flags, "state_hash": "sha256:abc"}

    monkeypatch.setattr(state, "build_state", many_flags)
    monkeypatch.setattr(state, "render", lambda body, **kw: {**body, "rules_version": body["rules"]})
    async with connect(make_token(profiles=("carmen",), user="alex")) as c:
        first = sc(await c.call_tool("get_health_status", {"profile": "carmen"}))
        assert len(json.dumps(first, ensure_ascii=False, separators=(",", ":")).encode()) < 2048
        assert first["budget"]["truncated"] and first["next_cursor"]
        ranks = [f["rank"] for f in first["data"]["flags"]]
        second = sc(await c.call_tool("get_health_status", {"profile": "carmen", "cursor": first["next_cursor"]}))
        assert second["data"]["flags"][0]["rank"] == ranks[-1] + 1
        assert second["data"]["state_hash"] == first["data"]["state_hash"]
        trend = await c.call_tool("get_trend", {"profile": "carmen", "code": "10000-0", "window": "all"})
        assert measured(trend) <= 2000


async def test_cursor_is_bound_to_query_and_token(big, make_token, connect, fakes):
    t1, t2 = make_token(profiles=("carmen",)), make_token(profiles=("carmen",))
    args = {"profile": "carmen", "query": "Analito", "detail": "full"}
    async with connect(t1) as c:
        cursor = sc(await c.call_tool("search_observations", args))["next_cursor"]
        assert cursor
        e = err(await c.call_tool("search_observations", {**args, "query": "otro", "cursor": cursor}))
        assert e["code"] == "bad_input"
        body, mac = cursor.split(".")
        forged = body[:-2] + ("AA" if body[-2:] != "AA" else "BB") + "." + mac
        assert err(await c.call_tool("search_observations", {**args, "cursor": forged}))["code"] == "bad_input"
    async with connect(t2) as c:
        e = err(await c.call_tool("search_observations", {**args, "cursor": cursor}))
        assert e["code"] == "bad_input"


async def test_as_of_answers_what_the_store_knew(world, make_token, connect, fakes):
    args = {"profile": "alex", "loinc": "13457-7", "detail": "full"}
    async with connect(make_token(scopes=ALL_SCOPES)) as c:
        # every row was recorded today, so on 2024-01-01 the store knew none of them
        then = sc(await c.call_tool("search_observations", {**args, "as_of": "2024-01-01"}))
        later = sc(await c.call_tool("search_observations", {**args, "as_of": "2099-01-01"}))
    assert then["data"]["results"] == [] and then["data"]["not_measured"] is True and then["as_of"] == "2024-01-01"
    assert len(later["data"]["results"]) == 8


async def test_demo_alex_status_pages_stay_under_2kb_whole(engine, settings, connect):
    """ROADMAP v0.1: get_health_status under 2 KB of JSON for a profile with 5 years of labs (demo Alex)."""
    from sqlalchemy import text

    from ownhealth.auth.tokens import create_token
    from ownhealth.blobs import BlobStore
    from ownhealth.demo import DEMO_HOUSEHOLD, seed_demo

    assert seed_demo(engine, settings, BlobStore(settings.blobs_path))
    with engine.begin() as c:
        u, p = c.execute(
            text(
                "select u.id, p.id from app_user u join household h on h.id = u.household_id"
                " join profile p on p.household_id = h.id and p.slug = 'alex'"
                " where h.name = :n order by u.created_at limit 1"
            ),
            {"n": DEMO_HOUSEHOLD},
        ).one()
        tok = create_token(
            c, user_id=u, label="t", profile_ids=[p], scopes=sorted(ALL_SCOPES), lang="en", identity_mode="full"
        ).plaintext
    flags, cursor, pages = [], None, 0
    async with connect(tok) as cl:
        while True:
            args = {"profile": "alex", **({"cursor": cursor} if cursor else {})}
            env = sc(await cl.call_tool("get_health_status", args))
            size = len(json.dumps(env, ensure_ascii=False, separators=(",", ":")).encode())
            assert size < 2048, (pages, size)  # the whole structuredContent, not only data
            flags += env["data"]["flags"]
            pages += 1
            cursor = env.get("next_cursor")
            if not cursor:
                break
    assert [f["rank"] for f in flags] == list(range(1, len(flags) + 1)) and len(flags) >= 5
    assert all(set(f) <= {"rank", "kind", "rule", "code", "text", "src"} for f in flags)
