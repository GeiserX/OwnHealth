"""Per-token identity mode, language and units are applied after state_hash (mcp.md 4, 9.6)."""

from __future__ import annotations

from datetime import UTC, date, datetime

import pytest
from conftest import manual
from mcp_checks import sc
from sqlalchemy import text

from ownhealth.core.access import ALL_SCOPES
from ownhealth.core.provenance import ensure_src
from ownhealth.mcp import redact

pytestmark = pytest.mark.anyio


async def test_same_state_hash_for_every_language_unit_and_identity_mode(world, make_token, connect, fakes):
    hashes, texts = set(), set()
    today = datetime.now(UTC).date().isoformat()  # one as_of for all three calls
    for lang, units, mode in [
        ("en", "si", "full"),
        ("es", "as_reported", "pseudonymous"),
        ("es", "conventional", "minimal"),
    ]:
        async with connect(make_token(scopes=ALL_SCOPES, lang=lang, units=units, identity_mode=mode)) as c:
            env = sc(await c.call_tool("get_health_status", {"profile": "alex", "as_of": today}))
        assert env["lang"] == lang and env["identity_mode"] == mode
        hashes.add(env["data"]["state_hash"])
        texts.add(str(env["data"]["flags"][0].get("text")))
    assert len(hashes) == 1 and len(texts) == 2  # en and es text, one hash


async def test_pseudonymous_and_minimal_redaction(world, make_token, connect, fakes, engine):
    body = "Alex Rivera asked: call me at +34 600 123 456 or alex.rivera@example.com, DNI 12345678Z."
    with engine.begin() as conn:
        sid = ensure_src(conn, manual(world.pid("alex")))
        conn.execute(
            text(
                "insert into note (profile_id, kind, author_kind, body, src_id) values"
                " (:p, 'journal', 'person', :b, :s)"
            ),
            {"p": world.pid("alex"), "b": body, "s": sid},
        )
    async with connect(make_token(scopes=ALL_SCOPES, identity_mode="pseudonymous")) as c:
        env = sc(await c.call_tool("get_health_status", {"profile": "alex"}))
        note = next(n for n in env["data"]["notes"] if n["src"] == sid)["text"]["text"]
        assert "Alex" not in note and "Rivera" not in note and "600" not in note and "@" not in note
        assert "12345678Z" not in note and "[name]" in note and "[phone]" in note and "[email]" in note
        rows = sc(await c.call_tool("search_observations", {"profile": "alex", "query": "Vitamina D"}))
        assert rows["data"]["results"][0]["lab"] == "lab" and rows["data"]["results"][0]["date"] == "2026-09-14"
    as_of = datetime.now(UTC).date()  # fixture rows are recorded today, so an earlier as_of hides them
    async with connect(make_token(scopes=ALL_SCOPES, identity_mode="minimal")) as c:
        rows = sc(
            await c.call_tool(
                "search_observations", {"profile": "alex", "query": "Vitamina D", "as_of": as_of.isoformat()}
            )
        )
        r = rows["data"]["results"][0]
        assert r["date"] == f"{(as_of - date(2026, 9, 14)).days} days before as_of"
        assert r["name"] is None and r["lab"] == "lab"  # free text dropped
        assert r["value"] == 24 and r["unit"] == "ng/mL"  # structured values survive every mode
    async with connect(make_token(scopes=ALL_SCOPES, identity_mode="full")) as c:
        env = sc(await c.call_tool("get_health_status", {"profile": "alex"}))
        assert any(n["text"]["text"] == body for n in env["data"]["notes"])


def test_detectors_leave_dates_and_values_alone():
    assert redact.scrub("LDL 97 mg/dL on 2026-09-14", []) == "LDL 97 mg/dL on 2026-09-14"
    assert redact.scrub("IBAN ES91 2100 0418 4502 0005 1332", []) == "IBAN [iban]"
    assert redact.stricter("full", "minimal") == "minimal" and redact.stricter("minimal", "full") == "minimal"
    assert redact.age_band(8) == "0-17" and redact.age_band(47) == "40-49"
