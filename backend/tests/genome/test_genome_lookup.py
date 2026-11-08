"""lookup_variant: call_state for every site, sensitive gating of raw lookups, regions, the 1,000-rsID
exit criterion of ROADMAP v0.1."""

from collections import Counter

import pytest

from ownhealth.genome import CALL_STATES, Gate, GenomeQueryError, load_ref_sites, lookup_variant, synthetic
from ownhealth.genome import lookup as lookup_mod
from ownhealth.genome.sensitive import spans_overlapping
from ownhealth.genome.synthetic import VcfRow, write_vcf

CLOSED = Gate(include_sensitive=False, minor=False)
OPEN = Gate(include_sensitive=True, minor=False)


def _look(conn, blobs, profile_id, gate=CLOSED, **q):
    return lookup_variant(conn, blobs, profile_id, gate=gate, **q)


def _state(out):
    assert len(out["calls"]) == 1, out
    return out["calls"][0]["call_state"]


def test_alt_call_from_the_vcf(conn, family, blobs, alex_genome):
    out = _look(conn, blobs, family.profiles["alex"], rsid="rs4244285")
    call = out["calls"][0]
    assert call["call_state"] == "alt" and call["genotype"] == "G/A" and call["zygosity"] == "het"
    assert call["basis"] == "vcf" and call["gq"] == 60 and call["src"].startswith("src_gd_")
    assert out["variant"]["gene"] == "CYP2C19" and out["variant"]["pos"] == 94781859
    assert len(out["cited_by_findings"]) == 1 and out["withheld"] is None


def test_ref_inside_a_passing_gvcf_block(conn, family, blobs, alex_genome):
    out = _look(conn, blobs, family.profiles["alex"], rsid="rs3918290")
    assert _state(out) == "ref" and out["calls"][0]["basis"] == "gvcf_block"
    assert out["calls"][0]["genotype"] == "C/C"


def test_unknown_states_by_position(conn, family, blobs, alex_genome):
    alex = family.profiles["alex"]
    low = _look(conn, blobs, alex, hgvs="NC_000003.12:g.2000100A>G")
    assert _state(low) == "not_covered" and low["calls"][0]["reason"] == "block_below_quality"
    none = _look(conn, blobs, alex, hgvs="NC_000003.12:g.5000000A>G")
    assert _state(none) == "not_covered" and none["calls"][0]["reason"] == "no_reference_block"
    failed = _look(conn, blobs, alex, hgvs="NC_000003.12:g.1000500A>G")
    assert _state(failed) == "no_call"
    assert lookup_mod.UNKNOWN_NOTE in none["limitations"]


def test_not_assessed_gene_answers_not_covered(conn, family, blobs, alex_genome):
    load_ref_sites(
        conn, [{"rsid": "rs999000001", "chrom": "chrX", "pos": 147912100, "ref": "C", "gene": "FMR1"}], source="test"
    )
    out = _look(conn, blobs, family.profiles["alex"], rsid="rs999000001")
    assert _state(out) == "not_covered" and out["calls"][0]["reason"] == "not_run"
    assert out["coverage"]["state"] == "not_assessed"


def test_sensitive_gene_lookup_is_gated(conn, family, blobs, alex_genome):
    alex = family.profiles["alex"]
    # a withheld finding never resolves an rsID (that would say it exists), so the curated index does
    sites = [("rs429358", "chr19", 44908684, "T"), ("rs113993960", "chr7", 117559590, "ATCT")]
    load_ref_sites(conn, [{"rsid": r, "chrom": c, "pos": p, "ref": ref} for r, c, p, ref in sites], source="t")
    closed = _look(conn, blobs, alex, rsid="rs429358")
    assert closed["withheld"] == "withheld_sensitive" and closed["calls"] == []
    assert "0/1" not in str(closed) and "T/C" not in str(closed)
    opened = _look(conn, blobs, alex, gate=OPEN, rsid="rs429358")
    assert _state(opened) == "alt" and opened["variant"]["gene"] == "APOE"
    # a position in APOE that no finding cites is gated by the gene list, not by the facts
    by_pos = _look(conn, blobs, alex, hgvs="NC_000019.10:g.44906000A>G")
    assert by_pos["withheld"] == "withheld_sensitive"
    # the minor rule wins over an open switch, and covers carrier sites too
    minor = Gate(include_sensitive=True, minor=True)
    assert _look(conn, blobs, alex, gate=minor, rsid="rs429358")["withheld"] == "withheld_minor"
    assert _look(conn, blobs, alex, gate=minor, rsid="rs113993960")["withheld"] == "withheld_minor"
    assert _state(_look(conn, blobs, alex, gate=minor, rsid="rs4244285")) == "alt"  # PGx stays visible


def test_minor_lookups_are_gated_by_the_carrier_panel_spans(conn, family, blobs, alex_genome):
    """A position in a carrier-panel gene that no finding cites is withheld for a minor, and a region
    over it is clipped: the fixed list gates, not the person's own findings (genomics.md 7)."""
    alex = family.profiles["alex"]
    minor = Gate(include_sensitive=True, minor=True)
    uncited = _look(conn, blobs, alex, gate=minor, hgvs="NC_000007.14:g.117560000A>G")
    assert uncited["withheld"] == "withheld_minor" and uncited["calls"] == []
    assert _look(conn, blobs, alex, gate=OPEN, hgvs="NC_000007.14:g.117560000A>G")["withheld"] is None
    region = _look(conn, blobs, alex, gate=minor, region="chr7:117559000-117560000")
    assert region["records"] == [] and [c["gene"] for c in region["clipped"]] == ["CFTR"]
    assert region["clipped"][0]["reason"] == "withheld_minor"


def test_a_withheld_answer_does_not_depend_on_the_persons_findings(conn, family, blobs, alex_genome):
    """Lucía's CFTR carrier finding is withheld by the minor rule. Asking for its rsID must answer the
    way it does for Sam, who has no such finding, and a region answer carries no count of hidden rows."""
    for slug, facts in (("lucia", synthetic.lucia_facts()), ("sam", synthetic.sam_facts())):
        synthetic.seed_genome(
            conn, blobs, household_id=family.household_id, profile_id=family.profiles[slug], facts=facts
        )
    minor = Gate(include_sensitive=False, minor=True)
    codes = []
    for slug in ("lucia", "sam"):
        with pytest.raises(GenomeQueryError) as e:
            _look(conn, blobs, family.profiles[slug], gate=minor, rsid="rs113993960")
        codes.append(e.value.code)
    assert codes == ["rsid_unknown", "rsid_unknown"]
    out = _look(conn, blobs, family.profiles["alex"], gate=minor, region="chr10:94781000-94782000")
    assert "withheld_records" not in out and [r["pos"] for r in out["records"]] == [94781859]


def test_region_clips_sensitive_genes_and_pages(conn, family, blobs, alex_genome):
    alex = family.profiles["alex"]
    closed = _look(conn, blobs, alex, region="chr19:44900000-44910000")
    assert closed["records"] == [] and closed["clipped"][0]["gene"] == "APOE"
    opened = _look(conn, blobs, alex, gate=OPEN, region="chr19:44900000-44910000")
    assert [r["pos"] for r in opened["records"]] == [44908684] and opened["clipped"] == []
    with pytest.raises(GenomeQueryError) as e:
        _look(conn, blobs, alex, region="chr1:1-1000001")
    assert e.value.code == "region_too_large"


def test_bad_queries(conn, family, blobs, alex_genome):
    alex = family.profiles["alex"]
    for kwargs, code in [
        ({"rsid": "rs123456789012"}, "rsid_unknown"),
        ({"rsid": "4244285"}, "bad_input"),
        ({"hgvs": "NM_000410.4:c.845G>A"}, "hgvs_unresolved"),
        ({"rsid": "rs1", "region": "chr1:1-2"}, "bad_input"),
        ({}, "bad_input"),
    ]:
        with pytest.raises(GenomeQueryError) as e:
            _look(conn, blobs, alex, **kwargs)
        assert e.value.code == code, kwargs
    with pytest.raises(GenomeQueryError) as e:
        _look(conn, blobs, family.profiles["carmen"], rsid="rs4244285")
    assert e.value.code == "no_genome"


def test_facts_only_source_answers_cited_sites(conn, family, blobs):
    synthetic.seed_genome(
        conn,
        blobs,
        household_id=family.household_id,
        profile_id=family.profiles["lucia"],
        facts=synthetic.lucia_facts(),
    )
    out = _look(conn, blobs, family.profiles["lucia"], gate=Gate(False, True), rsid="rs1142345")
    assert _state(out) == "alt" and out["calls"][0]["basis"] == "facts_file"


def test_a_call_state_outside_the_contract_raises(conn, family, blobs, alex_genome, monkeypatch):
    real = lookup_mod._from_record
    monkeypatch.setattr(lookup_mod, "_from_record", lambda rec, basis: real(rec, basis=basis) | {"call_state": "het"})
    with pytest.raises(RuntimeError, match="outside the contract"):
        _look(conn, blobs, family.profiles["alex"], rsid="rs4244285")


# --------------------------------------------- exit criterion: 1,000 rsIDs ----

N = 1000


def _sites() -> list[tuple[str, int]]:
    """N synthetic positions outside every sensitive gene (those are gated, tested above)."""
    out, j = [], 0
    while len(out) < N:
        chrom, pos = f"chr{j % 22 + 1}", 1_000_000 + j * 1000
        if not spans_overlapping(chrom, pos, pos):
            out.append((chrom, pos))
        j += 1
    return out


def _facts(sample, assay, fp):
    doc = synthetic.sam_facts()
    doc["subject"]["sample_id"] = sample
    doc["subject"]["fingerprint"]["genotypes"] = fp
    doc["assay"] = {"type": assay, "build": "GRCh38"}
    if assay == "chip":  # the chip confidence caps (importer rule 6)
        for f in doc["facts"]:
            f["confidence"]["level"] = "low" if f["gene"] == "CYP2D6" else "medium"
    return doc


@pytest.fixture
def thousand(conn, family, blobs, tmp_path):
    """1,000 curated rsIDs; a WGS source (calls + gVCF) on Carmen and a chip (calls + site list) on Sam."""
    sites = _sites()
    rows = [{"rsid": f"rs9{i:08d}", "chrom": c, "pos": p, "ref": "A", "alts": ["G"]} for i, (c, p) in enumerate(sites)]
    load_ref_sites(conn, rows, source="test")
    calls, gvcf, chip, probed = [], [], [], []
    for i, (c, p) in enumerate(sites):
        k = i % 10
        if k <= 2:
            calls.append(VcfRow(c, p, "A", "G", "0/1"))
        elif k == 3:
            calls.append(VcfRow(c, p, "A", "G", "./.", gq=0))
        elif k <= 7:
            gvcf.append(VcfRow(c, p - 10, "A", "<NON_REF>", "0/0", gq=60, end=p + 10))
        elif k == 8:
            gvcf.append(VcfRow(c, p - 10, "A", "<NON_REF>", "0/0", gq=5, end=p + 10))
        if k <= 5:
            gt = "0/1" if k <= 1 else ("0/0" if k <= 4 else "./.")
            chip.append(VcfRow(c, p, "A", "G", gt))
        if k <= 6:
            probed.append(VcfRow(c, p, "A", "G", "0/0"))
    wgs_raw = {
        "calls_vcf": write_vcf(tmp_path / "w.vcf.gz", calls),
        "gvcf": write_vcf(tmp_path / "w.g.vcf.gz", gvcf + calls),
    }
    chip_raw = {
        "calls_vcf": write_vcf(tmp_path / "c.vcf.gz", chip),
        "chip_sites": write_vcf(tmp_path / "s.vcf.gz", probed),
    }
    for slug, sample, assay, raw, fp in [
        ("carmen", "CO-01", "wgs", wgs_raw, "0" * 37),
        ("sam", "SR-CHIP", "chip", chip_raw, "1" * 37),
    ]:
        facts, att = synthetic.with_files(_facts(sample, assay, fp), blobs, raw)
        synthetic.seed_genome(
            conn,
            blobs,
            household_id=family.household_id,
            profile_id=family.profiles[slug],
            facts=facts,
            attachments=att,
        )
    return [r["rsid"] for r in rows]


def test_thousand_rsids_all_get_a_call_state(conn, family, blobs, thousand):
    expected = {
        "carmen": {"alt": 300, "no_call": 100, "ref": 400, "not_covered": 200},
        "sam": {"alt": 200, "ref": 300, "no_call": 200, "not_on_array": 300},
    }
    for slug, want in expected.items():
        states = Counter()
        for rsid in thousand:
            out = _look(conn, blobs, family.profiles[slug], rsid=rsid)
            assert out["calls"], rsid
            for call in out["calls"]:
                assert call["call_state"] in CALL_STATES
                states[call["call_state"]] += 1
        assert sum(states.values()) == N
        assert dict(states) == want, slug
