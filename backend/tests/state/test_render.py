"""Rendering happens after hashing: language, units, identity mode and scopes change the text, never
the state_hash (ROADMAP v0.1 exit criterion "Status")."""

import itertools

from ownhealth import state
from ownhealth.core.access import ALL_SCOPES
from ownhealth.mcp.envelope import is_untrusted

from .conftest import AS_OF


def test_same_hash_for_every_language_unit_and_identity_setting(sconn, seeded):
    body = state.build_state(sconn, seeded.pid("alex"), as_of=AS_OF)
    outs = [
        state.render(body, lang=lang, units=units, identity_mode=mode)
        for lang, units, mode in itertools.product(
            ("en", "es"), ("as_reported", "si", "conventional"), ("full", "pseudonymous", "minimal")
        )
    ]
    assert {o["state_hash"] for o in outs} == {body["state_hash"]}
    assert {o["rules_version"] for o in outs} == {"ownhealth-rules 0.1.0"}
    en, es = (state.render(body, lang=lang)["data"]["flags"][0]["text"] for lang in ("en", "es"))
    assert en == "Lp(a) 142 nmol/L (H, lab range < 75) on 2025-09-08."
    assert es == "Lp(a) 142 nmol/L (H, rango del laboratorio < 75) el 2025-09-08."
    # rendering did not touch the body
    assert state.state_hash(body) == body["state_hash"]


def test_rendered_flags_follow_mcp_5_1(sconn, seeded):
    out = state.render(state.build_state(sconn, seeded.pid("alex"), as_of=AS_OF), lang="en")
    f1, f2, f3 = out["data"]["flags"][:3]
    assert (f1["rank"], f1["kind"], f1["code"], f1["flag"], f1["range"], f1["src"]) == (
        1, "out_of_range", "LOINC:43583-4", "H", "< 75", "src_a07",
    )  # fmt: skip
    assert (f2["code"], f2["flag"], f2["src"]) == ("LOINC:62292-8", "L", "src_a12")
    assert (f3["kind"], f3["drug"], f3["gene"], f3["src"]) == (
        "gene_drug",
        "Atorvastatin",
        "SLCO1B1",
        ["src_g31", "src_m02"],
    )
    assert "Discuss with your prescriber" not in f3["text"] and "To discuss with your prescriber" in f3["text"]
    assert out["data"]["stale"][0]["window_months"] == 12
    assert out["data"]["never_measured"][0] == {
        "name": "Hepatitis C antibody",
        "rule": "infect.hcv.once_adult",
        "text": "Hepatitis C antibody: no result on record (rule infect.hcv.once_adult).",
    }
    assert out["notice"] == "Information from your records, not a diagnosis."
    assert out["limitations"][0] == "Flags use each lab's own printed range."


def test_free_text_goes_out_wrapped(sconn, seeded):
    out = state.render(state.build_state(sconn, seeded.pid("alex"), as_of=AS_OF), lang="en")
    note = out["data"]["notes"][0]
    assert is_untrusted(note["text"]) and note["text"]["origin"] == "person_note"
    assert note["text"]["text"] == "Recheck vitamin D in winter."


def test_minimal_identity_mode_drops_free_text_and_makes_dates_relative(sconn, seeded):
    out = state.render(state.build_state(sconn, seeded.pid("alex"), as_of=AS_OF), lang="en", identity_mode="minimal")
    assert out["data"]["notes"] == []
    assert out["data"]["flags"][0]["date"] == "425 days ago"
    assert out["data"]["flags"][0]["text"] == "Lp(a) 142 nmol/L (H, lab range < 75) 425 days ago."
    assert out["identity_mode"] == "minimal"


def test_scopes_leave_sections_out_and_name_them(sconn, seeded):
    body = state.build_state(sconn, seeded.pid("alex"), as_of=AS_OF)
    summary_only = state.render(body, lang="en", scopes=frozenset({"read:summary"}))
    assert summary_only["data"]["flags"] == [] and summary_only["data"]["medications"] == []
    assert summary_only["withheld"]["by_scope"] == ["genome", "labs", "records", "wearables"]
    no_genome = state.render(body, lang="en", scopes=ALL_SCOPES - {"read:genome"})
    assert "gene_drug" not in {f["kind"] for f in no_genome["data"]["flags"]}
    assert no_genome["withheld"]["by_scope"] == ["genome"]
    assert len(no_genome["data"]["flags"]) == 4 and no_genome["data"]["medications"]
    full = state.render(body, lang="en", scopes=ALL_SCOPES)
    assert full["withheld"]["by_scope"] == [] and len(full["data"]["flags"]) == 5
    assert summary_only["state_hash"] == no_genome["state_hash"] == full["state_hash"]
