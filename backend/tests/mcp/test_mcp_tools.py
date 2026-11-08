"""Contract tests for the eight tools, through a real MCP client session."""

from __future__ import annotations

import pytest
from mcp_checks import assert_envelope, err, facts, sc
from sqlalchemy import text

from ownhealth.core.access import ALL_SCOPES
from ownhealth.core.observations import retract_observation
from ownhealth.mcp.envelope import ToolError

pytestmark = pytest.mark.anyio


async def call(c, name, args):
    r = await c.call_tool(name, args)
    if not r.is_error:
        assert_envelope(r.structured_content)
        assert r.content[0].type == "text" and r.content[0].text.startswith(f"OwnHealth {name}")
        for f in facts(r.structured_content["data"]):
            assert f["src"]
    return r


async def test_search_summary_full_draw_and_flags(world, make_token, connect, fakes):
    async with connect(make_token()) as c:
        env = sc(await call(c, "search_observations", {"profile": "alex", "query": "Colesterol LDL"}))
        [row] = env["data"]["results"]
        assert row["src"] == world.src["ldl_latest"] and row["value"] == 97 and row["unit"] == "mg/dL"
        assert row["name"] == {"trust": "untrusted_text", "origin": "pdf_text_layer", "text": "Colesterol LDL"}
        assert row["range"] == {"high": 130} and row["code"] == "LOINC:13457-7"
        assert row["lab"]["trust"] == "untrusted_text" and row["draw_id"].startswith("doc_2026-09-14_")
        assert env["data"]["resolved"]["codes"] == ["LOINC:13457-7", "LOINC:18262-6"]
        full = sc(await call(c, "search_observations", {"profile": "alex", "loinc": "13457-7", "detail": "full"}))
        assert len(full["data"]["results"]) == 8  # both codes are one analyte
        flagged = sc(
            await call(
                c,
                "search_observations",
                {"profile": "alex", "loinc": "LOINC:13457-7", "detail": "full", "flagged_only": True},
            )
        )
        assert [r["value"] for r in flagged["data"]["results"]] == [163]
        window = sc(
            await call(
                c,
                "search_observations",
                {"profile": "alex", "query": "ldl", "detail": "full", "from": "2025-01-01", "to": "2025-12-31"},
            )
        )
        assert {r["date"] for r in window["data"]["results"]} == {"2025-01-20", "2025-09-08"}
        draws = sc(await call(c, "search_observations", {"profile": "alex", "query": "LabNorte", "group_by": "draw"}))
        assert draws["data"]["results"] == [] and draws["data"]["not_measured"] is True
        by_draw = sc(
            await call(
                c,
                "search_observations",
                {"profile": "alex", "loinc": "13457-7", "group_by": "draw", "detail": "full", "from": "2026-09-01"},
            )
        )
        [draw] = by_draw["data"]["results"]
        assert draw["draw_id"].startswith("doc_") and draw["results"][0]["src"] == world.src["ldl_latest"]
        e = err(await call(c, "search_observations", {"profile": "alex"}))
        assert e["code"] == "bad_input"
        e = err(await call(c, "search_observations", {"profile": "alex", "query": "x", "category": "genome"}))
        assert e["code"] == "bad_input"


async def test_trend_keeps_both_codes_stats_and_annotations(world, make_token, connect, fakes):
    async with connect(make_token()) as c:
        env = sc(await call(c, "get_trend", {"profile": "alex", "code": "LOINC:13457-7", "window": "P6Y"}))
    d = env["data"]
    assert d["codes"] == ["LOINC:13457-7", "LOINC:18262-6"] and d["canonical_unit"] == "mg/dL", d
    dates = [p["date"] for p in d["points"]]
    assert dates == sorted(dates) and len(dates) == 8 and d["resolution"] == "raw"
    assert d["stats"]["n"] == 8 and d["stats"]["trend"] in ("increasing", "decreasing", "no significant trend")
    assert d["stats"]["last_vs_previous_pct"] == round((97 - 118) / 118 * 100)
    assert [a["event"] for a in d["annotations"]] == ["medication_started"]
    assert d["annotations"][0]["name"]["trust"] == "untrusted_text"
    assert any("mixes LOINC codes" in x for x in env["limitations"])
    async with connect(make_token(scopes={"read:labs"})) as c:  # no read:records: no medication events
        env = sc(await call(c, "get_trend", {"profile": "alex", "code": "13457-7", "window": "all"}))
        assert env["data"]["annotations"] == []
        nothing = sc(await call(c, "get_trend", {"profile": "alex", "code": "13457-7", "window": "P30D"}))
        assert nothing["data"]["points"] == [] and nothing["data"]["not_measured"] is True


async def test_get_source_document_region(world, make_token, connect, fakes):
    async with connect(make_token(scopes=ALL_SCOPES - {"read:documents_text"})) as c:
        env = sc(await call(c, "get_source", {"src_id": world.src["vitd"]}))
        d = env["data"]
        assert env["profile"] == "alex" and d["status"] == "current"
        assert d["location"]["page"] == 1 and len(d["location"]["bbox"]) == 4 and d["location"]["units"] == "pt"
        assert d["printed"]["value"] == {"trust": "untrusted_text", "origin": "pdf_text_layer", "text": "24"}
        assert d["derived"]["loinc"] == "62292-8" and d["document"]["id"].startswith("doc_2026-09-14_")
        assert (
            d["crop_url"].startswith(f"http://127.0.0.1:8080/import-review?doc={world.doc_id}&page=1")
            and d["text"] is None
        )
        with_text = sc(await call(c, "get_source", {"src_id": world.src["vitd"], "include_text": True}))
        assert with_text["data"]["text"] is None and with_text["withheld"]["by_scope"] == ["documents_text"]
    async with connect(make_token(scopes=ALL_SCOPES)) as c:
        env = sc(await call(c, "get_source", {"src_id": world.src["vitd"], "include_text": True}))
        assert env["data"]["text"] == {"trust": "untrusted_text", "origin": "pdf_text_layer", "text": "Colesterol LDL"}
        e = err(await call(c, "get_source", {"src_id": "bad id"}))
        assert e["code"] == "bad_input"


async def test_get_source_lifecycle(world, make_token, engine, connect, fakes):
    with engine.begin() as conn:
        oid = conn.execute(
            text("select id from observation where src_id = :s"), {"s": world.src["ldl_2025-01-20"]}
        ).scalar_one()
        retract_observation(conn, oid, "OCR misread, confirmed by the lab")
    async with connect(make_token()) as c:
        env = sc(await call(c, "get_source", {"src_id": world.src["ldl_2025-01-20"]}))
        assert env["data"]["status"] == "retracted"
        assert env["data"]["reason"]["trust"] == "untrusted_text"
        trend = sc(await call(c, "get_trend", {"profile": "alex", "code": "13457-7", "window": "all"}))
        assert world.src["ldl_2025-01-20"] not in {p["src"] for p in trend["data"]["points"]}


async def test_genome_findings_and_lookup(world, make_token, connect, fakes, engine):
    async with connect(make_token(scopes=ALL_SCOPES)) as c:
        env = sc(await call(c, "get_genome_findings", {"profile": "alex"}))
        assert {f["gene"] for f in env["data"]["findings"]} == {"SLCO1B1", "HFE"}
        assert all(
            f["call_state"] in ("ref", "alt", "no_call", "not_on_array", "not_covered") for f in env["data"]["findings"]
        )
        assert env["not_assessed"] and env["db_versions"] == {"clinvar": "2026-10-28"}
        assert any("Research-grade" in x for x in env["limitations"])
        pgx = sc(await call(c, "get_genome_findings", {"profile": "alex", "category": "pgx"}))
        assert [f["gene"] for f in pgx["data"]["findings"]] == ["SLCO1B1"]
        hit = sc(await call(c, "lookup_variant", {"profile": "alex", "rsid": "rs1800562"}))
        assert hit["data"]["calls"][0]["call_state"] == "alt" and hit["data"]["quota"] == {
            "used_today": 1,
            "daily_cap": 200,
        }
        assert (
            err(await call(c, "lookup_variant", {"profile": "alex", "rsid": "rs1", "region": "chr1:1-2"}))["code"]
            == "bad_input"
        )
        e = err(await call(c, "lookup_variant", {"profile": "alex", "region": "chr1:1-2000000"}))
        assert e["code"] == "bad_input"
        page1 = sc(await call(c, "lookup_variant", {"profile": "alex", "region": "chr6:1000-2000"}))
        assert page1["next_cursor"]
        page2 = sc(
            await call(
                c, "lookup_variant", {"profile": "alex", "region": "chr6:1000-2000", "cursor": page1["next_cursor"]}
            )
        )
        assert [x["pos"] for x in page2["data"]["calls"]] == [1002, 1003] and page2["next_cursor"] is None


async def test_lookup_daily_cap(world, make_token, connect, fakes, engine):
    tok = make_token(scopes=ALL_SCOPES)
    with engine.begin() as conn:
        conn.execute(text("update api_token set genome_raw_lookups_per_day = 2 where prefix = :p"), {"p": tok[:15]})
    async with connect(tok) as c:
        for _ in range(2):
            sc(await call(c, "lookup_variant", {"profile": "alex", "rsid": "rs1800562"}))
        e = err(await call(c, "lookup_variant", {"profile": "alex", "rsid": "rs1800562"}))
        assert e["code"] == "genome_lookup_cap" and e["daily_cap"] == 2
        sc(await call(c, "get_genome_findings", {"profile": "alex"}))  # other tools still work


async def test_check_drug(world, make_token, connect, fakes):
    async with connect(make_token(scopes=ALL_SCOPES)) as c:
        env = sc(await call(c, "check_drug", {"profile": "alex", "drug": "atorvastatin"}))
    d = env["data"]
    assert d["pgx"][0]["gene"] == "SLCO1B1" and d["current_medications_relevant"][0]["src"] == world.src["statin"]
    assert d["current_medications_relevant"][0]["name"]["trust"] == "untrusted_text"
    assert env["notice"].endswith("Discuss with your prescriber.")
    assert any("drug-drug" in str(x) for x in env["not_assessed"])
    async with connect(make_token(scopes={"read:labs"})) as c:
        env = sc(await call(c, "check_drug", {"profile": "alex", "drug": "atorvastatin"}))
    assert set(env["data"]) == {"drug", "relevant_labs", "not_assessed"} - {"not_assessed"}
    assert env["withheld"]["by_scope"] == ["genome", "records"]


async def test_not_available_lane_is_a_tool_error(world, make_token, connect, monkeypatch):
    from ownhealth import state

    def stub(*a, **k):
        raise NotImplementedError("lane E")

    monkeypatch.setattr(state, "build_state", stub)
    async with connect(make_token()) as c:
        e = err(await c.call_tool("get_health_status", {"profile": "alex"}))
    assert e["code"] == "not_available"


def test_tool_error_shape():
    assert ToolError("x", "m", "f").as_dict() == {"isError": True, "code": "x", "message": "m", "fix": "f"}


async def test_wearable_trend_uses_the_day_total_for_summed_metrics(world, make_token, connect, engine):
    pid = world.pid("alex")
    with engine.begin() as conn:
        sid = conn.execute(
            text(
                "insert into wearable_source (profile_id, provider, device)"
                " values (:p, 'apple_health', 'Watch') returning id"
            ),
            {"p": pid},
        ).scalar_one()
        conn.execute(
            text(
                "insert into wearable_rollup (profile_id, metric, source_id, bucket, t, n, v_min, v_max, v_avg, v_sum,"
                " v_p10, v_p50, v_p90, coverage) values (:p, 'sleep_asleep', :s, 'day', '2026-11-01', 3, 60, 240,"
                " 140, 420, 60, 120, 240, 1)"
            ),
            {"p": pid, "s": sid},
        )
    async with connect(make_token(scopes=ALL_SCOPES)) as c:
        env = sc(await call(c, "get_trend", {"profile": "alex", "code": "sleep_asleep", "window": "all"}))
    assert [p["value"] for p in env["data"]["points"]] == [420]
