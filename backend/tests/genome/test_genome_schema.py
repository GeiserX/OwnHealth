"""genome-facts/v1 validation: the schema, the version gate and the rules the schema cannot express."""

import copy
import json
from pathlib import Path

import pytest

from ownhealth.genome import CALL_STATES, synthetic, validate_facts
from ownhealth.genome.schema import schema_document

ROOT = Path(__file__).resolve().parents[3]


def test_synthetic_fixtures_are_valid():
    for doc in (synthetic.alex_facts(), synthetic.sam_facts(), synthetic.lucia_facts()):
        assert validate_facts(doc) == []


@pytest.mark.parametrize("version", ["ownhealth.genome-facts/v2", "ownhealth.genome-facts/v0"])
def test_version_mismatch_is_one_clear_error(version):
    doc = synthetic.alex_facts()
    doc["schema"] = version
    doc["facts"][0]["evidence"]["level"] = "E9"  # other problems are not reported on top
    errors = validate_facts(doc)
    assert len(errors) == 1
    assert version in errors[0] and "ownhealth.genome-facts/v1" in errors[0] and "Nothing was imported" in errors[0]


def test_not_a_facts_file():
    assert "must be" in validate_facts({"resourceType": "Bundle"})[0]
    assert "JSON object" in validate_facts([1, 2])[0]


def test_call_state_outside_the_five_is_rejected():
    doc = synthetic.alex_facts()
    doc["facts"][0]["variants"][0]["genotype"]["call_state"] = "unknown"
    errors = validate_facts(doc)
    assert any("call_state" in e and "unknown" in e for e in errors), errors
    enum = schema_document()["$defs"]["genotype"]["properties"]["call_state"]["enum"]
    assert tuple(enum) == CALL_STATES


def test_schema_errors_name_the_path():
    doc = synthetic.alex_facts()
    del doc["coverage"]
    doc["facts"][2]["confidence"]["level"] = "certain"
    errors = validate_facts(doc)
    assert any(e.startswith("$:") and "coverage" in e for e in errors)
    assert any(e.startswith("$.facts[2].confidence.level") for e in errors)


def test_importer_rules_beyond_the_schema():
    doc = synthetic.alex_facts()
    del doc["facts"][0]["variants"][0]["genotype"]  # a variant without a call_state
    doc["facts"][1]["summary"] = "line\x00with a NUL"
    doc["facts"][3]["id"] = doc["facts"][4]["id"]
    errors = validate_facts(doc)
    assert any("needs genotype.call_state" in e for e in errors)
    assert any("control characters" in e for e in errors)
    assert any("duplicate fact id" in e for e in errors)


def test_chip_pathogenic_must_be_unconfirmed():
    doc = synthetic.alex_facts()
    doc["assay"]["type"] = "chip"
    errors = validate_facts(doc)
    assert any("carrier fact from a chip must be 'unconfirmed'" in e for e in errors)
    carrier = next(f for f in doc["facts"] if f["category"] == "carrier")
    carrier["confidence"]["level"] = "unconfirmed"
    assert validate_facts(doc) == []


def test_raw_file_needs_its_index():
    doc = synthetic.alex_facts()
    doc["files"] = [{"role": "calls_vcf", "path": "vcf/a.vcf.gz", "sha256": "a" * 64}]
    assert any("tabix index" in e for e in validate_facts(doc))


def test_free_text_is_capped():
    doc = synthetic.alex_facts()
    doc["facts"][0]["summary"] = "x" * 2001
    assert any("summary" in e for e in validate_facts(doc))


def test_schema_copies_agree():
    package = json.loads((ROOT / "backend/src/ownhealth/genome/data/genome-facts.v1.json").read_text())
    assert json.loads((ROOT / "data/genome-facts/genome-facts.v1.json").read_text()) == package


@pytest.mark.parametrize(
    ("name", "build"),
    [
        ("alex-rivera", synthetic.alex_facts),
        ("sam-rivera", synthetic.sam_facts),
        ("lucia-rivera", synthetic.lucia_facts),
    ],
)
def test_fixture_files_match_the_generator(name, build):
    on_disk = json.loads((ROOT / f"fixtures/genome/{name}.genome-facts.json").read_text())
    assert on_disk == json.loads(json.dumps(build()))


def test_fact_ids_follow_the_rule():
    assert synthetic.fact_id("pgx", "CYP2C19", "CYP2C19") == synthetic.fact_id("pgx", "CYP2C19", "CYP2C19")
    ids = [f["id"] for f in synthetic.alex_facts()["facts"]]
    assert len(set(ids)) == len(ids)
    # the id holds no sample id: Lucía's CFTR carrier finding has the same id as Alex's
    alex = {f["gene"]: f["id"] for f in synthetic.alex_facts()["facts"] if f.get("gene")}
    lucia = {f["gene"]: f["id"] for f in synthetic.lucia_facts()["facts"] if f.get("gene")}
    assert alex["CFTR"] == lucia["CFTR"]


def test_validation_does_not_mutate():
    doc = synthetic.alex_facts()
    before = copy.deepcopy(doc)
    validate_facts(doc)
    assert doc == before
