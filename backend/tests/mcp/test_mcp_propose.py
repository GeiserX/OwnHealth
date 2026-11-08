"""propose: every kind lands in the review queue, never in the record; failed rows are queued and
marked; a person's accept is what writes."""

from __future__ import annotations

import pytest
from mcp_checks import err, sc
from sqlalchemy import text

from ownhealth.core import review
from ownhealth.core.access import ALL_SCOPES

pytestmark = pytest.mark.anyio
FACT_TABLES = ("observation", "note", "medication_statement", "condition", "genome_finding")


def counts(engine, profile_id) -> dict:
    with engine.connect() as conn:
        return {
            t: conn.execute(text(f"select count(*) from {t} where profile_id = :p"), {"p": profile_id}).scalar()
            for t in FACT_TABLES
        }


def item(engine, handle):
    with engine.connect() as conn:
        return (
            conn.execute(
                text("select * from review_item where replace(id::text, '-', '') like :h"), {"h": handle[5:] + "%"}
            )
            .mappings()
            .one()
        )


async def test_observations_are_checked_and_queued(world, make_token, connect, fakes, engine):
    before = counts(engine, world.pid("alex"))
    doc = f"doc_2026-09-14_{str(world.doc_id).replace('-', '')[:8]}"
    payload = {
        "doc_id": doc,
        "rows": [
            {"name": "Colesterol LDL", "value": 97, "unit": "mg/dL", "date": "2026-09-14", "page": 1},
            {"name": "Hierro", "value": "1,62", "unit": "mg/dL", "date": "2026-09-14", "page": 1},
            {"name": "Potasio", "value": 2.1, "unit": "mmol/L", "date": "2025-01-01", "page": 1},
        ],
    }
    async with connect(make_token(scopes=ALL_SCOPES)) as c:
        env = sc(
            await c.call_tool(
                "propose",
                {"profile": "alex", "kind": "observations", "payload": payload, "based_on": [world.src["vitd"], doc]},
            )
        )
    d = env["data"]
    assert d["status"] == "queued_for_review" and d["proposal_id"].startswith("prop_")
    assert [r["grounded"] for r in d["rows"]] == [True, False, False]
    assert [r["check"] for r in d["rows"]] == ["ok", "not_grounded", "not_grounded"]
    assert "date_in_document" in d["rows"][2]["failed"]
    assert d["rows"][0]["name"] == {"trust": "untrusted_text", "origin": "agent_note", "text": "Colesterol LDL"}
    assert d["review_url"].endswith(str(item(engine, d["proposal_id"])["id"]))
    assert counts(engine, world.pid("alex")) == before  # nothing written
    it = item(engine, d["proposal_id"])
    assert it["kind"] == "agent_proposal" and it["status"] == "open" and it["row_conf"] == 0.0
    assert set(it["reasons"]) == {"not_grounded", "date_invalid"}
    assert it["document_id"] == world.doc_id and world.doc_id in it["context_documents"]
    assert world.src["vitd"] in it["context_src_ids"] and len(it["payload"]["rows"]) == 3
    assert it["payload"]["token_label"] == "Claude Code · test"

    with engine.begin() as conn:  # a person accepts: only now does a fact appear
        created = review.accept(conn, it["id"], world.family.users["alex"])
        assert len(created) == 3
        row = conn.execute(text("select accepted_by, src_id from observation where id = :i"), {"i": created[0]}).one()
        assert row.accepted_by == "person"
        assert conn.execute(text("select kind from src where id = :s"), {"s": row.src_id}).scalar() == "document_region"


async def test_note_medication_and_finding_status(world, make_token, connect, fakes, engine):
    before = counts(engine, world.pid("alex"))
    async with connect(make_token(scopes=ALL_SCOPES)) as c:
        note = sc(
            await c.call_tool(
                "propose",
                {
                    "profile": "alex",
                    "kind": "note",
                    "payload": {"text": "Ask about vitamin D.", "attach_to": world.src["vitd"]},
                },
            )
        )
        assert note["data"]["checks"][0]["ok"] is True
        bad = sc(
            await c.call_tool(
                "propose",
                {"profile": "alex", "kind": "note", "payload": {"text": "x", "attach_to": world.src["sam_ldl"]}},
            )
        )
        assert bad["data"]["checks"][0]["ok"] is False  # another profile's src is not a target
        med = sc(
            await c.call_tool(
                "propose",
                {
                    "profile": "alex",
                    "kind": "medication",
                    "payload": {
                        "action": "stop",
                        "name": "Atorvastatin",
                        "date": "2026-11-01",
                        "reason": "muscle pain",
                    },
                },
            )
        )
        assert med["data"]["checks"][0]["ok"] is True and med["data"]["drug"]["cpic_drug_id"]
        new = sc(
            await c.call_tool(
                "propose",
                {
                    "profile": "alex",
                    "kind": "medication",
                    "payload": {"action": "start", "name": "Vitamina D3 1000 UI", "date": "2026-11-01"},
                },
            )
        )
        assert any("free text" in x for x in new["limitations"])
        fid = None
        with engine.connect() as conn:
            fid = conn.execute(
                text("select fact_id from genome_finding where src_id = :s"), {"s": world.src["slco1b1"]}
            ).scalar()
            apoe = conn.execute(
                text("select fact_id from genome_finding where src_id = :s"), {"s": world.src["apoe"]}
            ).scalar()
        fs = sc(
            await c.call_tool(
                "propose",
                {
                    "profile": "alex",
                    "kind": "finding_status",
                    "payload": {"finding_id": fid, "verification": "confirmed", "reason": "clinical lab confirmed"},
                },
            )
        )
        assert [x["ok"] for x in fs["data"]["checks"]] == [True, True]
        hidden = sc(
            await c.call_tool(
                "propose",
                {
                    "profile": "alex",
                    "kind": "finding_status",
                    "payload": {"finding_id": apoe, "verification": "confirmed", "reason": "x"},
                },
            )
        )
        assert hidden["data"]["checks"][0]["ok"] is False  # a sensitive finding behind the switch does not exist
        e = err(await c.call_tool("propose", {"profile": "alex", "kind": "note", "payload": {"text": "x" * 4001}}))
        assert e["code"] == "bad_input"
        e = err(
            await c.call_tool(
                "propose", {"profile": "alex", "kind": "finding_status", "payload": {"finding_id": fid, "reason": "x"}}
            )
        )
        assert e["code"] == "bad_input"
    assert counts(engine, world.pid("alex")) == before
    with engine.begin() as conn:
        note_item = item(engine, note["data"]["proposal_id"])
        [nid] = review.accept(conn, note_item["id"], world.family.users["alex"])
        assert conn.execute(text("select author_kind from note where id = :i"), {"i": nid}).scalar() == "agent"
        stop_item = item(engine, med["data"]["proposal_id"])
        review.accept(conn, stop_item["id"], world.family.users["alex"])
        assert (
            conn.execute(
                text("select med_status from medication_statement where profile_id = :p and status = 'current'"),
                {"p": world.pid("alex")},
            ).scalar()
            == "stopped"
        )


async def test_propose_needs_write_scope_and_row_cap(world, make_token, connect, fakes):
    async with connect(make_token(scopes={"read:summary", "read:labs"})) as c:
        assert "propose" not in {t.name for t in (await c.list_tools()).tools}
        e = err(await c.call_tool("propose", {"profile": "alex", "kind": "note", "payload": {"text": "x"}}))
        assert e["code"] == "scope_missing"
    async with connect(make_token(scopes={"write:propose"})) as c:  # no read scope on the profile
        e = err(await c.call_tool("propose", {"profile": "alex", "kind": "note", "payload": {"text": "x"}}))
        assert e["code"] == "scope_missing"
    rows = [{"name": "A", "value": 1, "unit": "g", "date": "2026-09-14", "page": 1}] * 201
    async with connect(make_token(scopes=ALL_SCOPES)) as c:
        e = err(
            await c.call_tool(
                "propose",
                {
                    "profile": "alex",
                    "kind": "observations",
                    "payload": {"doc_id": "doc_2026-09-14_00000000", "rows": rows},
                },
            )
        )
        assert e["code"] == "propose_cap"
