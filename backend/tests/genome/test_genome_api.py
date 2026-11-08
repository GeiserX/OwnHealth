"""REST: the genome page, a variant lookup and the consent switches, with scopes, gating and audit."""

import secrets

import pytest
from sqlalchemy import text

from ownhealth.auth import sessions
from ownhealth.blobs import BlobStore
from ownhealth.core.access import Principal
from ownhealth.genome import synthetic


@pytest.fixture
def genome_family(engine, committed_family, settings, tmp_path):
    fam = committed_family
    blobs = BlobStore(settings.blobs_path)
    alex, lucia = synthetic.alex_facts(), synthetic.lucia_facts()
    for doc in (alex, lucia):  # committed rows outlive the test; the same bytes twice would be one file
        doc["provenance"]["commit"] = secrets.token_hex(4)
    facts, att = synthetic.with_files(alex, blobs, synthetic.alex_raw_files(tmp_path / "raw"))
    with engine.begin() as c:
        synthetic.seed_genome(
            c, blobs, household_id=fam.household_id, profile_id=fam.profiles["alex"], facts=facts, attachments=att
        )
        synthetic.seed_genome(c, blobs, household_id=fam.household_id, profile_id=fam.profiles["lucia"], facts=lucia)
    return fam


def _token(fam, *scopes):
    return Principal(kind="token", user_id=fam.users["alex"], household_id=fam.household_id, scopes=frozenset(scopes))


def _audits(engine, profile_id, action):
    with engine.connect() as c:
        return (
            c.execute(
                text(
                    "select tool, args_summary, resource_ids, withheld from audit_event"
                    " where profile_id = :p and action = :a"
                    " order by id"
                ),
                {"p": profile_id, "a": action},
            )
            .mappings()
            .all()
        )


def test_genome_page_for_the_owner(app_for, genome_family, engine):
    fam = genome_family
    r = app_for(fam.users["alex"]).get("/api/profiles/alex/genome")
    assert r.status_code == 200, r.text
    body = r.json()
    assert body["dataset"]["assay"] == "wgs" and body["dataset"]["source_tool"] == "OwnGenome 1.4.2"
    assert body["dataset"]["db_versions"]["clinvar"] == "2026-09-28" and body["dataset"]["raw_files"]["gvcf"]
    genes = {f["gene"] for f in body["findings"]}
    assert {"CYP2C19", "SLCO1B1", "DPYD", "TPMT", "VKORC1", "CYP2C9", "CYP2D6", "CFTR"} <= genes
    assert "APOE" not in genes and "MTHFR" not in genes
    assert body["locked"] == [{"category": "risk_marker", "gene": "APOE"}]  # the owner sees what is locked
    assert body["withheld"] == {"sensitive": 1, "minor_rule": 0}
    assert body["consent"] == {"ui": False, "agents": False, "can_change": True}
    tiles = {t["key"]: t for t in body["tiles"]}
    assert tiles["pgx"]["value"] == "7" and "1 hidden" in tiles["risk_marker"]["detail"]
    assert any(c["target"] == "FMR1" and c["state"] == "not_assessed" for c in body["coverage"])
    audit = _audits(engine, fam.profiles["alex"], "read")[-1]
    assert audit["tool"] == "GET /api/profiles/{p}/genome" and audit["withheld"] == {"sensitive": 1, "minor_rule": 0}
    assert all(i.startswith("f_") for i in audit["resource_ids"])


def test_viewer_without_read_genome_is_refused(app_for, genome_family):
    r = app_for(genome_family.users["sam"]).get("/api/profiles/alex/genome")
    assert r.status_code == 403 and r.json()["detail"]["code"] == "insufficient_scope"


def test_consent_needs_step_up_then_reveals_in_the_ui_only(app_for, genome_family, engine, monkeypatch):
    fam = genome_family
    client = app_for(fam.users["alex"])
    r = client.post("/api/profiles/alex/genome/consent", json={"ui": True})
    assert r.status_code == 403 and r.json()["detail"]["code"] == "step_up_required"
    monkeypatch.setattr(sessions, "require_step_up", lambda conn, principal, max_age_seconds=300: None)
    r = client.post("/api/profiles/alex/genome/consent", json={"ui": True})
    assert r.status_code == 200 and r.json() == {"ui": True, "agents": False}
    body = client.get("/api/profiles/alex/genome").json()
    assert "APOE" in {f["gene"] for f in body["findings"]} and body["locked"] == []
    # the UI switch is not consent to hand it to an agent
    agent = app_for(principal=_token(fam, "read:genome", "read:sensitive"))
    assert "APOE" not in {f["gene"] for f in agent.get("/api/profiles/alex/genome").json()["findings"]}
    assert agent.get("/api/profiles/alex/genome").json()["locked"] == []  # tokens never get the locked list
    assert agent.post("/api/profiles/alex/genome/consent", json={"agents": True}).status_code == 403
    assert _audits(engine, fam.profiles["alex"], "consent_change")[0]["args_summary"] == "ui=True agents=None"


def test_only_owner_or_guardian_of_a_minor_changes_consent(app_for, genome_family, monkeypatch):
    monkeypatch.setattr(sessions, "require_step_up", lambda conn, principal, max_age_seconds=300: None)
    fam = genome_family
    sam = app_for(fam.users["sam"])
    assert sam.post("/api/profiles/alex/genome/consent", json={"ui": True}).status_code == 403  # viewer
    assert sam.post("/api/profiles/lucia/genome/consent", json={"ui": True}).status_code == 200  # guardian, minor


def test_minor_page_withholds_by_the_minor_rule(app_for, genome_family, monkeypatch):
    monkeypatch.setattr(sessions, "require_step_up", lambda conn, principal, max_age_seconds=300: None)
    client = app_for(genome_family.users["alex"])
    client.post("/api/profiles/lucia/genome/consent", json={"ui": True, "agents": True})
    body = client.get("/api/profiles/lucia/genome?min_evidence=E4").json()
    assert [f["gene"] for f in body["findings"]] == ["TPMT"]
    assert body["withheld"] == {"sensitive": 0, "minor_rule": 3} and body["locked"] == [] and body["is_minor"]


def test_variant_lookup_scope_gating_and_audit(app_for, genome_family, engine):
    fam = genome_family
    no_raw = app_for(principal=_token(fam, "read:genome"))
    assert no_raw.get("/api/profiles/alex/genome/variant?rsid=rs4244285").status_code == 403
    raw = app_for(principal=_token(fam, "read:genome_raw"))
    r = raw.get("/api/profiles/alex/genome/variant?rsid=rs4244285")
    assert r.status_code == 200 and r.json()["calls"][0]["call_state"] == "alt"
    apoe = raw.get("/api/profiles/alex/genome/variant?rsid=rs429358").json()
    assert apoe["withheld"] == "withheld_sensitive" and apoe["calls"] == []
    bad = raw.get("/api/profiles/alex/genome/variant?rsid=rs123456789")
    assert bad.status_code == 400 and bad.json()["detail"]["code"] == "rsid_unknown"
    none = raw.get("/api/profiles/carmen/genome/variant?rsid=rs4244285")
    assert none.status_code in (403, 404)
    lookups = _audits(engine, fam.profiles["alex"], "lookup")
    assert [a["args_summary"] for a in lookups] == ["rs4244285", "rs429358", "rs123456789"]
    assert all("0/1" not in str(a) for a in lookups)  # never a genotype in the log
