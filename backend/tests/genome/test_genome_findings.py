"""list_findings and pgx_for_drug: gating, minors, evidence, coverage first, untrusted text."""

import json
from datetime import date

import pytest

from ownhealth.core.access import Principal, effective_permission, set_genome_switches
from ownhealth.genome import Gate, gate_for, list_findings, pgx_for_drug, synthetic
from ownhealth.genome.findings import as_of_bound

CLOSED = Gate(include_sensitive=False, minor=False)
OPEN = Gate(include_sensitive=True, minor=False)
AS_OF = date(2026, 11, 7)


def _genes(found):
    return [f["gene"] for f in found.findings]


def test_sensitive_finding_is_counted_never_described(conn, family, alex_genome):
    found = list_findings(conn, family.profiles["alex"], gate=CLOSED, min_evidence="E3")
    assert "APOE" not in _genes(found) and found.withheld_sensitive == 1
    assert "APOE" not in json.dumps(found.findings) and "e3/e4" not in json.dumps(found.findings)
    opened = list_findings(conn, family.profiles["alex"], gate=OPEN, min_evidence="E3")
    assert "APOE" in _genes(opened) and opened.withheld_sensitive == 0


def test_minor_rule_withholds_whatever_the_switches(conn, family, blobs):
    lucia = family.profiles["lucia"]
    synthetic.seed_genome(
        conn, blobs, household_id=family.household_id, profile_id=lucia, facts=synthetic.lucia_facts()
    )
    for gate in (Gate(include_sensitive=True, minor=True), Gate(include_sensitive=False, minor=True)):
        found = list_findings(conn, lucia, gate=gate, min_evidence="E4")
        assert _genes(found) == ["TPMT"]  # PGx guides a child's prescriptions; it stays
        assert found.withheld_minor == 3  # carrier, APOE, PRS
    adult = list_findings(conn, lucia, gate=OPEN, min_evidence="E4")
    assert adult.withheld_minor == 0 and len(adult.findings) == 4


def test_gate_for_uses_the_right_switch(conn, family):
    alex = family.profiles["alex"]
    owner = Principal(kind="session", user_id=family.users["alex"], household_id=family.household_id)
    perm = effective_permission(conn, owner, alex)
    assert gate_for(perm, surface="ui", on=AS_OF) == CLOSED
    set_genome_switches(conn, alex, ui=True)
    perm = effective_permission(conn, owner, alex)
    assert gate_for(perm, surface="ui", on=AS_OF).include_sensitive
    assert not gate_for(perm, surface="agents", on=AS_OF).include_sensitive  # seeing is not consent to share
    set_genome_switches(conn, alex, agents=True)
    token = Principal(
        kind="token", user_id=family.users["alex"], household_id=family.household_id, scopes=frozenset({"read:genome"})
    )
    assert not gate_for(effective_permission(conn, token, alex), surface="agents", on=AS_OF).include_sensitive
    token2 = Principal(
        kind="token",
        user_id=family.users["alex"],
        household_id=family.household_id,
        scopes=frozenset({"read:genome", "read:sensitive"}),
    )
    assert gate_for(effective_permission(conn, token2, alex), surface="agents", on=AS_OF).include_sensitive
    guardian = Principal(kind="session", user_id=family.users["alex"], household_id=family.household_id)
    assert gate_for(effective_permission(conn, guardian, family.profiles["lucia"]), surface="ui", on=AS_OF).minor


def test_evidence_filter_and_hidden_by_default(conn, family, alex_genome):
    alex = family.profiles["alex"]
    e2 = list_findings(conn, alex, gate=CLOSED)
    assert all(f["evidence"] in ("E1", "E2") for f in e2.findings)
    assert "CAD" not in json.dumps(e2.findings)
    e3 = list_findings(conn, alex, gate=CLOSED, min_evidence="E3")
    assert any(f["category"] == "prs" for f in e3.findings) and "MTHFR" not in _genes(e3)
    e4 = list_findings(conn, alex, gate=CLOSED, min_evidence="E4")
    mthfr = next(f for f in e4.findings if f["gene"] == "MTHFR")
    assert (
        mthfr["evidence"] == "E4"
        and mthfr["hidden_by_default"]
        and mthfr["evidence_native"].startswith("not supported")
    )
    assert [f["evidence"] for f in e4.findings] == sorted(f["evidence"] for f in e4.findings)


def test_findings_carry_evidence_confidence_call_state_and_src(conn, family, alex_genome):
    found = list_findings(conn, family.profiles["alex"], gate=CLOSED, category="pgx")
    cyp = next(f for f in found.findings if f["gene"] == "CYP2C19")
    assert cyp["evidence"] == "E1" and cyp["evidence_native"] == "CPIC A"
    assert cyp["confidence"] == "high" and cyp["confidence_basis"] == "two callers agree"
    assert cyp["call_state"] == "alt" and cyp["diplotype"] == "*1/*2" and cyp["src"].startswith("src_g_")
    assert cyp["summary"] == {
        "trust": "untrusted_text",
        "origin": "genome_facts",
        "text": "CYP2C19 *1/*2: Intermediate Metabolizer.",
    }
    assert cyp["drug_guidance"][0]["recommendation"]["trust"] == "untrusted_text"
    assert found.db_versions["cpic"] == "2026-09" and found.assay["type"] == "wgs"
    assert {f["category"] for f in found.findings} == {"pgx"}


def test_not_assessed_is_listed_explicitly(conn, family, alex_genome):
    found = list_findings(conn, family.profiles["alex"], gate=CLOSED)
    assert {"gene": "FMR1", "panel": "carrier", "reason": "not_run"} in found.not_assessed
    assert {"panel": "repeat_expansions", "reason": "not_run"} in found.not_assessed
    assert {"gene": "SMN1", "panel": "carrier", "reason": "paralog"} in found.not_assessed


def test_gene_query_answers_coverage_first(conn, family, alex_genome):
    alex = family.profiles["alex"]
    pms2 = list_findings(conn, alex, gate=CLOSED, gene="PMS2")
    assert pms2.findings == [] and pms2.coverage[0]["state"] == "partial" and pms2.coverage[0]["reason"] == "paralog"
    fmr1 = list_findings(conn, alex, gate=CLOSED, gene="fmr1")
    assert fmr1.coverage[0]["state"] == "not_assessed" and fmr1.not_assessed[0]["gene"] == "FMR1"
    unknown = list_findings(conn, alex, gate=CLOSED, gene="NOTAGENE")
    assert unknown.not_assessed == [{"gene": "NOTAGENE", "panel": None, "reason": "not_in_coverage_map"}]
    apoe = list_findings(conn, alex, gate=CLOSED, gene="APOE")
    assert apoe.findings == [] and apoe.withheld_sensitive == 1 and apoe.not_assessed == []


def test_no_genome_on_file(conn, family):
    found = list_findings(conn, family.profiles["carmen"], gate=CLOSED)
    assert found.findings == [] and found.assay is None and "OwnGenome" in found.limitations[0]


def test_as_of_before_the_import_sees_nothing(conn, family, blobs, alex_raw):
    from datetime import UTC, datetime

    facts, att = synthetic.with_files(synthetic.alex_facts(), blobs, alex_raw)
    synthetic.seed_genome(
        conn,
        blobs,
        household_id=family.household_id,
        profile_id=family.profiles["alex"],
        facts=facts,
        attachments=att,
        imported_at=datetime(2026, 10, 2, 8, tzinfo=UTC),
    )
    assert list_findings(conn, family.profiles["alex"], gate=CLOSED, as_of=date(2026, 10, 1)).findings == []
    assert list_findings(conn, family.profiles["alex"], gate=CLOSED, as_of=date(2026, 10, 2)).findings
    assert as_of_bound(date(2026, 10, 2)).isoformat().startswith("2026-10-02T23:59:59")


def test_pgx_for_drug(conn, family, blobs, alex_genome):
    alex = family.profiles["alex"]
    clop = pgx_for_drug(conn, alex, cpic_drug_id="RxNorm:32968")
    cyp = next(e for e in clop if e["gene"] == "CYP2C19")
    assert cyp["status"] == "found" and cyp["phenotype"] == "Intermediate Metabolizer" and cyp["actionable"]
    assert [g["drug"] for g in cyp["guidance"]] == ["clopidogrel"] and cyp["call_state"] == "alt"
    assert cyp["guidance"][0]["recommendation"]["trust"] == "untrusted_text"
    fu = pgx_for_drug(conn, alex, cpic_drug_id="RxNorm:4492")
    dpyd = next(e for e in fu if e["gene"] == "DPYD")
    assert dpyd["call_state"] == "ref" and not dpyd["actionable"] and dpyd["activity_score"] == 2.0
    # an explicit gene list (from lane E's CPIC pairs): a gene with no finding says why
    some = pgx_for_drug(conn, alex, cpic_drug_id="RxNorm:2670", genes=["CYP2D6", "UGT1A1", "FMR1"])
    by_gene = {e["gene"].upper(): e for e in some}
    assert by_gene["CYP2D6"]["activity_score"] == 1.5
    assert by_gene["UGT1A1"]["status"] == "no_finding" and by_gene["FMR1"]["status"] == "not_assessed"
    sam = family.profiles["sam"]
    synthetic.seed_genome(conn, blobs, household_id=family.household_id, profile_id=sam, facts=synthetic.sam_facts())
    codeine = pgx_for_drug(conn, sam, cpic_drug_id="RxNorm:2670")
    d6 = next(e for e in codeine if e["gene"] == "CYP2D6")
    assert d6["diplotype"] == "*1/*1" and d6["call_state"] == "ref" and d6["confidence"] == "medium"


@pytest.mark.parametrize("bad", [{"min_evidence": "E5"}, {"category": "astrology"}])
def test_bad_filters(conn, family, bad):
    with pytest.raises(ValueError):
        list_findings(conn, family.profiles["alex"], gate=CLOSED, **bad)
