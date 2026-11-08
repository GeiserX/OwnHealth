"""check_drug: PGx + current medications + relevant labs + CYP2D6 phenoconversion, CPIC text verbatim."""

from datetime import date

import pytest

from ownhealth import state
from ownhealth.core.access import ALL_SCOPES
from ownhealth.core.provenance import SrcIn
from ownhealth.core.records import MedicationIn, add_record
from ownhealth.state.pack import cpic_snapshot

from .conftest import AS_OF

CLOPIDOGREL_IM_ACS = (
    "Avoid standard dose (75 mg) clopidogrel if possible. Use prasugrel or ticagrelor at standard dose if no "
    "contraindication."
)
CODEINE_NM = "Use codeine label recommended age- or weight-specific dosing."
CODEINE_PM = (
    "Avoid codeine use because of possibility of diminished analgesia. If opioid use is warranted, consider a "
    "non-tramadol opioid."
)


def _med(conn, pid, name, started=date(2026, 4, 1)):
    return add_record(
        conn,
        MedicationIn(profile_id=pid, name=name, ingredient=name.lower(), strength="20 mg", started=started),
        src=SrcIn(kind="manual", actor_kind="system", label=f"Rx · {name}"),
        accepted_by="person",
    ).src_id


def test_alex_clopidogrel(sconn, seeded):
    out = state.check_drug(sconn, seeded.pid("alex"), "Plavix", scopes=ALL_SCOPES, as_of=AS_OF)
    assert out["drug"]["name"] == "Clopidogrel" and out["drug"]["cpic_drug_id"] == "RxNorm:32968"
    assert out["drug"]["currently_taking"] is False
    assert out["pgx"] == [
        {"gene": "CYP2C19", "diplotype": "*1/*2", "phenotype": "Intermediate Metabolizer", "activity_score": None,
         "evidence": "E1", "confidence": "high", "call_state": "alt", "cpic_level": "A", "src": "src_g07"},
    ]  # fmt: skip
    acs = next(g for g in out["guidance"] if g["population"] == "CVI ACS PCI")
    assert acs["quote"] == CLOPIDOGREL_IM_ACS
    assert (acs["source"], acs["level"], acs["strength"], acs["license"]) == ("CPIC", "A", "Strong", "CC0-1.0")
    assert acs["url"].startswith("https://") and acs["based_on"] == ["src_g07"]
    nvi = next(g for g in out["guidance"] if g["population"] == "NVI")
    assert nvi["strength"] == "Moderate"
    # omeprazole is a moderate CYP2C19 inhibitor; CPIC has no conversion rule for CYP2C19
    assert out["phenoconversion"] == [
        {"gene": "CYP2C19", "by": "Omeprazole 20 mg", "inhibitor_strength": "moderate", "computed": False,
         "functional_phenotype": None, "activity_score": None, "activity_score_functional": None, "rule": None,
         "basis": out["phenoconversion"][0]["basis"], "src": "src_m05", "text": out["phenoconversion"][0]["text"]},
    ]  # fmt: skip
    assert "not computed" in out["phenoconversion"][0]["text"]
    rel = {m["name"]: m["relation"] for m in out["current_medications_relevant"]}
    assert rel == {"Atorvastatin 20 mg": "not_assessed", "Omeprazole 20 mg": "inhibitor:CYP2C19:moderate"}
    labs = {lab["alias_group"]: lab for lab in out["relevant_labs"]}
    assert list(labs) == ["platelets", "hemoglobin", "egfr", "alt"]
    assert (labs["platelets"]["value"], labs["platelets"]["status"], labs["platelets"]["date"]) == (
        "231",
        "ok",
        "2026-09-14",
    )
    assert all(lab["src"].startswith("src_") for lab in labs.values())
    assert any("drug-drug interactions" in n for n in out["not_assessed"])
    assert out["withheld"] == {"by_scope": []}
    assert out["notice"].startswith("Information from your records, not medical advice.")


def test_every_quote_is_verbatim_cpic_text(sconn, seeded):
    snap = {(r["id"], r["drugrecommendation"]) for r in cpic_snapshot()["recommendations"]}
    for drug in ("clopidogrel", "atorvastatin", "omeprazole", "simvastatin", "citalopram"):
        out = state.check_drug(sconn, seeded.pid("alex"), drug, scopes=ALL_SCOPES, as_of=AS_OF)
        assert out["guidance"], drug
        for g in out["guidance"]:
            assert (g["recommendation_id"], g["quote"]) in snap


def test_sam_codeine_with_a_strong_cyp2d6_inhibitor_is_phenoconverted(sconn, seeded):
    pid = seeded.pid("sam")
    src = _med(sconn, pid, "Paroxetine")
    out = state.check_drug(sconn, pid, "codeína", scopes=ALL_SCOPES, as_of=AS_OF)
    (pc,) = out["phenoconversion"]
    assert (pc["gene"], pc["inhibitor_strength"], pc["computed"]) == ("CYP2D6", "strong", True)
    assert (pc["activity_score"], pc["activity_score_functional"], pc["functional_phenotype"]) == (
        "2.0", "0.0", "Poor Metabolizer",
    )  # fmt: skip
    assert pc["src"] == src and pc["based_on"] == ["src_g27", src]
    by_kind = {g["applies_to"].split("(")[-1].split(",")[0].rstrip(")"): g for g in out["guidance"]}
    assert by_kind["genotype"]["quote"] == CODEINE_NM and by_kind["genotype"]["strength"] == "Strong"
    assert by_kind["functional"]["quote"] == CODEINE_PM
    assert by_kind["functional"]["based_on"] == ["src_g27", src]
    assert out["pgx"][0]["phenotype"] == "Normal Metabolizer"  # the genotype phenotype is kept beside it
    assert out["current_medications_relevant"][0]["relation"] == "inhibitor:CYP2D6:strong"


def test_moderate_inhibitor_halves_the_activity_score(sconn, seeded):
    pid = seeded.pid("sam")
    _med(sconn, pid, "Duloxetine")
    (pc,) = state.check_drug(sconn, pid, "tramadol", scopes=ALL_SCOPES, as_of=AS_OF)["phenoconversion"]
    assert (pc["activity_score_functional"], pc["functional_phenotype"]) == ("1.0", "Intermediate Metabolizer")


def test_no_inhibitor_no_phenoconversion(sconn, seeded):
    out = state.check_drug(sconn, seeded.pid("sam"), "codeine", scopes=ALL_SCOPES, as_of=AS_OF)
    assert out["phenoconversion"] == []
    assert [g["quote"] for g in out["guidance"]] == [CODEINE_NM]


def test_multi_gene_thiopurine_lookup(sconn, seeded):
    pid = seeded.pid("carmen")
    seeded.findings[pid] = [
        {"category": "pgx", "gene": "TPMT", "diplotype": "*1/*3A", "phenotype": "Intermediate Metabolizer",
         "evidence": "E1", "call_state": "alt", "src": "src_g61"},
    ]  # fmt: skip
    try:
        only_tpmt = state.check_drug(sconn, pid, "azatioprina", scopes=ALL_SCOPES, as_of=AS_OF)
        seeded.findings[pid].append(
            {
                "category": "pgx",
                "gene": "NUDT15",
                "phenotype": "Normal Metabolizer",
                "call_state": "ref",
                "evidence": "E1",
                "src": "src_g62",
            }  # fmt: skip
        )
        both = state.check_drug(sconn, pid, "azathioprine", scopes=ALL_SCOPES, as_of=AS_OF)
    finally:
        seeded.findings.pop(pid)
    (g1,) = only_tpmt["guidance"]
    assert g1["quote"].startswith("Based on TPMT, initiate therapy with reduced starting doses")
    assert "NUDT15: no genome finding on record" in only_tpmt["not_assessed"]
    (g2,) = both["guidance"]
    assert g2["quote"].startswith("Initiate therapy with reduced starting doses (30-80% of standard starting dose)")
    assert g2["based_on"] == ["src_g61", "src_g62"]


def test_warfarin_is_named_not_quoted(sconn, seeded):
    pid = seeded.pid("carmen")
    seeded.findings[pid] = [
        {"category": "pgx", "gene": "CYP2C9", "phenotype": "Intermediate Metabolizer", "activity_score": "1.0",
         "call_state": "alt", "evidence": "E1", "src": "src_g63"},
    ]  # fmt: skip
    try:
        out = state.check_drug(sconn, pid, "Aldocumar", scopes=ALL_SCOPES, as_of=AS_OF)
    finally:
        seeded.findings.pop(pid)
    assert out["guidance"] == []
    assert any("algorithm" in n for n in out["not_assessed"])
    assert [lab["alias_group"] for lab in out["relevant_labs"]] == ["inr", "hemoglobin", "platelets"]
    assert out["relevant_labs"][0]["status"] == "nodata"  # missing is explicit, never normal


def test_scopes_withhold_parts(sconn, seeded):
    pid = seeded.pid("alex")
    labs_only = state.check_drug(sconn, pid, "clopidogrel", scopes=frozenset({"read:labs"}), as_of=AS_OF)
    assert labs_only["pgx"] == [] and labs_only["guidance"] == [] and labs_only["current_medications_relevant"] == []
    assert labs_only["withheld"]["by_scope"] == ["genome", "records"]
    assert labs_only["relevant_labs"]
    genome_only = state.check_drug(sconn, pid, "clopidogrel", scopes=frozenset({"read:genome"}), as_of=AS_OF)
    assert genome_only["pgx"] and genome_only["guidance"] and genome_only["phenoconversion"] == []
    assert genome_only["relevant_labs"] == [] and genome_only["drug"]["currently_taking"] is None


def test_unknown_drug_is_an_error_never_no_interaction(sconn, seeded):
    with pytest.raises(state.UnknownDrug):
        state.check_drug(sconn, seeded.pid("alex"), "not-a-drug-xyz", scopes=ALL_SCOPES, as_of=AS_OF)


def test_resolve_drug(sconn):
    assert state.resolve_drug(sconn, "Omeprazol") == {"cpic_drug_id": "RxNorm:7646", "name": "omeprazole"}
    assert state.resolve_drug(sconn, "RxNorm:32968")["name"] == "clopidogrel"
    assert state.resolve_drug(sconn, "Atorvastatina Cinfa 20 mg")["name"] == "atorvastatin"
    assert state.resolve_drug(sconn, "fluconazol") == {"cpic_drug_id": None, "name": "fluconazole"}
    assert state.resolve_drug(sconn, "zzz") is None


def test_a_drug_without_a_level_a_pair(sconn, seeded):
    out = state.check_drug(sconn, seeded.pid("alex"), "fluoxetine", scopes=ALL_SCOPES, as_of=AS_OF)
    assert out["pgx"] == [] and out["guidance"] == []
    assert any("no CPIC level A gene pair" in n for n in out["not_assessed"])
