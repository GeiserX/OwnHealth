"""The YAML rule pack v1: loads, every rule cites, templates are bilingual, overrides change the id."""

import shutil

import pytest

from ownhealth.state import pack as pack_mod
from ownhealth.state import rules_version
from ownhealth.state.pack import PACK_DIR, RULE_KINDS, PackError, _read_dir, build_pack, cpic_snapshot, load_pack


def test_shipped_pack():
    p = load_pack()
    assert rules_version() == "ownhealth-rules 0.1.0" == p.version
    assert len(p.sha256) == 64
    assert {r["kind"] for r in p.rules} == set(RULE_KINDS)
    assert [s.key for s in p.systems] == [
        "cardiometabolic", "liver", "kidney", "thyroid", "blood", "inflammation", "vitamins", "sleep", "fitness",
    ]  # fmt: skip
    for r in p.rules:
        assert r["cite"].startswith("https://"), r["id"]
    # every analyte a system shows has a display name in both languages
    for s in p.systems:
        for m in s.markers:
            if isinstance(m, str):
                assert p.analytes[m]["en"] and p.analytes[m]["es"], m
    # the documented windows (architecture.md section 16)
    assert p.rule("recency.hba1c")["window_days"] == 183
    assert p.rule("recency.lipids")["window_days"] == 365
    assert p.rule("recency.thyroid")["window_days"] == 365


def test_every_drug_in_rules_resolves_to_the_cpic_table():
    p, snap = load_pack(), cpic_snapshot()
    known = {d["name"] for d in snap["drugs"] + snap["drugs_ids_only"]} | set(p.drugs)
    for r in p.rules_of("gene_drug") + p.rules_of("gene_lab"):
        for d in r.get("drugs", []):
            assert d in known and d in p.drugs, (r["id"], d)
    for gene, classes in p.phenoconversion["inhibitors"].items():
        for names in classes.values():
            assert all(n in p.drugs for n in names), gene
    for drug, labs in p.drug_labs.items():
        assert drug in p.drugs
        assert all(lab["lab"] in p.analytes for lab in labs), drug
        assert all(set(lab["reason"]) == {"en", "es"} for lab in labs), drug


def test_override_changes_version_and_hash(tmp_path, monkeypatch):
    shipped = load_pack()
    (tmp_path / "thyroid.yaml").write_text((PACK_DIR / "thyroid.yaml").read_text().replace("365", "180"))
    monkeypatch.setenv(pack_mod.OVERRIDE_ENV, str(tmp_path))
    local = load_pack()
    assert local.version == f"ownhealth-rules 0.1.0+local.{local.sha256[:8]}"
    assert local.sha256 != shipped.sha256
    assert local.rule("recency.thyroid")["window_days"] == 180


def test_a_rule_without_cite_is_refused(tmp_path):
    for f in PACK_DIR.glob("*.yaml"):
        shutil.copy(f, tmp_path / f.name)
    text = (tmp_path / "liver.yaml").read_text()
    (tmp_path / "liver.yaml").write_text(text.replace("cite: https://", "cite: http://", 1))
    with pytest.raises(PackError, match=r"rule recency\.liver: .*cite"):  # the first cite in liver.yaml
        build_pack(_read_dir(tmp_path), True)


def test_a_template_missing_spanish_is_refused(tmp_path):
    files = _read_dir(PACK_DIR)
    files["templates.yaml"]["templates"]["notice"] = {"en": "only English"}
    with pytest.raises(PackError, match="en and es"):
        build_pack(files, True)


def test_cpic_snapshot_is_cc0_and_complete():
    snap = cpic_snapshot()
    assert snap["license"].startswith("CC0-1.0")
    assert all(p["cpiclevel"] == "A" for p in snap["pairs"])
    names = {d["name"] for d in snap["drugs"]}
    for drug in ("clopidogrel", "codeine", "tramadol", "simvastatin", "warfarin", "fluorouracil", "capecitabine",
                 "azathioprine", "mercaptopurine", "thioguanine"):  # fmt: skip
        assert drug in names
    pairs = {(p["drugname"], p["genesymbol"]) for p in snap["pairs"]}
    for pair in [("clopidogrel", "CYP2C19"), ("codeine", "CYP2D6"), ("tramadol", "CYP2D6"), ("simvastatin", "SLCO1B1"),
                 ("warfarin", "CYP2C9"), ("warfarin", "VKORC1"), ("fluorouracil", "DPYD"), ("azathioprine", "TPMT"),
                 ("azathioprine", "NUDT15")]:  # fmt: skip
        assert pair in pairs


def test_a_count_of_one_reads_singular():
    p = load_pack()
    assert p.text("subtitle.wearables", "en", n=1) == "wearables synced 1 day ago"
    assert p.text("subtitle.wearables", "es", n=1) == "wearables sincronizados hace 1 día"
    assert p.text("subtitle.wearables", "en", n=3) == "wearables synced 3 days ago"
    assert p.text("genome.summary.carrier", "es", n=1) == "1 hallazgo de portador"
    assert p.text("genome.summary.carrier", "en", n=2) == "2 carrier findings"


def test_an_unquoted_decimal_in_a_pack_file_is_a_pack_error():
    files = _read_dir(PACK_DIR)
    files["kidney.yaml"]["rules"][0]["cv_i"] = 4.5  # what `cv_i: 4.5` loads as
    with pytest.raises(PackError, match=r"kidney\.yaml: .*quoted"):
        build_pack(files, True)
