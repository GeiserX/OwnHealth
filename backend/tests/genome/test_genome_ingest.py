"""Import of genome-facts.json and its raw files: sha256 checks, nothing half-imported, re-runs."""

import copy
import json
import secrets

import pytest
from sqlalchemy import text

from ownhealth.core.documents import StoredFile, dispatch_upload
from ownhealth.genome import GenomeImportError, synthetic
from ownhealth.jobs import JobContext, load_handlers, run_one


def _count(conn, table, profile_id):
    return conn.execute(text(f"select count(*) from {table} where profile_id = :p"), {"p": profile_id}).scalar()


def test_import_stores_facts_coverage_and_raw_file_pointers(conn, family, blobs, alex_genome):
    _doc_id, facts, summary = alex_genome
    alex = family.profiles["alex"]
    assert summary.accepted == len(facts["facts"]) and summary.reasons == {}
    ds = (
        conn.execute(
            text(
                "select id, assay, owngenome_version, db_versions, encode(vcf_blob, 'hex') as vcf,"
                " encode(tbi_blob, 'hex') as tbi, encode(gvcf_blob, 'hex') as gvcf, src_id, document_id"
                " from genome_dataset where profile_id = :p"
            ),
            {"p": alex},
        )
        .mappings()
        .one()
    )
    files = {f["role"]: f for f in facts["files"]}
    assert ds["vcf"] == files["calls_vcf"]["sha256"] and ds["tbi"] == files["calls_vcf"]["index_sha256"]
    assert ds["gvcf"] == files["gvcf"]["sha256"]
    assert ds["db_versions"]["cpic"] == "2026-09" and ds["owngenome_version"] == "OwnGenome 1.4.2"
    assert blobs.exists(ds["vcf"]) and blobs.exists(ds["tbi"])
    n_cov = conn.execute(text("select count(*) from genome_coverage where dataset_id = :d"), {"d": ds["id"]}).scalar()
    assert n_cov == len(facts["coverage"])
    rows = (
        conn.execute(
            text(
                "select fact_id, gene, evidence, confidence, call_state, sensitive, adult_onset, carrier, src_id"
                " from genome_finding where profile_id = :p"
            ),
            {"p": alex},
        )
        .mappings()
        .all()
    )
    by_gene = {r["gene"]: r for r in rows}
    assert len(rows) == len(facts["facts"])
    assert by_gene["CYP2C19"]["call_state"] == "alt" and by_gene["CYP2C19"]["evidence"] == "E1"
    assert by_gene["DPYD"]["call_state"] == "ref"
    assert by_gene["APOE"]["sensitive"] and by_gene["APOE"]["adult_onset"]
    assert by_gene["CFTR"]["carrier"] and not by_gene["CFTR"]["sensitive"]
    assert all(r["src_id"].startswith("src_g_") for r in rows)
    # raw genome files never enter the database: no column holds VCF bytes
    big = conn.execute(
        text("select max(pg_column_size(fact)) from genome_finding where profile_id = :p"), {"p": alex}
    ).scalar()
    assert big < 4000


def test_src_resolves_to_the_facts_file(conn, family, alex_genome):
    from ownhealth.core.provenance import resolve_src

    sid = conn.execute(
        text("select src_id from genome_finding where gene = 'SLCO1B1' and profile_id = :p"),
        {"p": family.profiles["alex"]},
    ).scalar_one()
    resolved = resolve_src(conn, sid)
    assert resolved["src"]["kind"] == "genome_facts"
    assert resolved["facts"][0]["table"] == "genome_finding" and resolved["profile_id"] == family.profiles["alex"]
    assert "#f_" in resolved["src"]["import_ref"]


def test_reimporting_the_same_bytes_is_a_no_op(conn, family, blobs, alex_genome):
    doc_id, facts, _ = alex_genome
    from ownhealth.genome import import_genome_facts

    again = import_genome_facts(conn, blobs, document_id=doc_id)
    assert again.accepted == 0 and again.skipped == len(facts["facts"])
    assert _count(conn, "genome_dataset", family.profiles["alex"]) == 1


def test_a_vcf_whose_sha256_differs_is_refused_and_nothing_is_written(conn, family, blobs, alex_raw):
    facts, attachments = synthetic.with_files(synthetic.alex_facts(), blobs, alex_raw)
    stranger = blobs.put_bytes(b"\x1f\x8b not the file the facts describe")
    attachments["AR-01.vcf.gz"] = stranger
    with pytest.raises(GenomeImportError, match="matches no file listed"):
        synthetic.seed_genome(
            conn,
            blobs,
            household_id=family.household_id,
            profile_id=family.profiles["alex"],
            facts=facts,
            attachments=attachments,
        )
    assert _count(conn, "genome_finding", family.profiles["alex"]) == 0


def test_corrupted_blob_bytes_are_refused(conn, family, blobs, alex_raw):
    facts, attachments = synthetic.with_files(synthetic.alex_facts(), blobs, alex_raw)
    sha = facts["files"][0]["sha256"]
    blobs.path(sha).chmod(0o644)
    blobs.path(sha).write_bytes(b"tampered")
    with pytest.raises(GenomeImportError, match="do not match sha256"):
        synthetic.seed_genome(
            conn,
            blobs,
            household_id=family.household_id,
            profile_id=family.profiles["alex"],
            facts=facts,
            attachments=attachments,
        )


def test_missing_raw_files_import_the_facts_and_say_so(conn, family, blobs):
    facts = synthetic.alex_facts()
    facts["files"] = [
        {
            "role": "calls_vcf",
            "path": "vcf/AR-01.vcf.gz",
            "sha256": "c" * 64,
            "index_path": "vcf/AR-01.vcf.gz.tbi",
            "index_sha256": "d" * 64,
        }
    ]
    _, summary = synthetic.seed_genome(
        conn, blobs, household_id=family.household_id, profile_id=family.profiles["alex"], facts=facts
    )
    assert summary.reasons == {"raw_file_missing": 1}
    vcf = conn.execute(
        text("select vcf_blob from genome_dataset where profile_id = :p"), {"p": family.profiles["alex"]}
    )
    assert vcf.scalar() is None


def test_invalid_file_raises_with_the_reasons(conn, family, blobs):
    facts = synthetic.alex_facts()
    facts["schema"] = "ownhealth.genome-facts/v2"
    with pytest.raises(GenomeImportError, match="unsupported genome-facts version 'ownhealth.genome-facts/v2'"):
        synthetic.seed_genome(
            conn, blobs, household_id=family.household_id, profile_id=family.profiles["alex"], facts=facts
        )


def test_the_same_file_on_another_profile_is_refused(conn, family, blobs, alex_genome):
    doc_id, _facts, _ = alex_genome
    from ownhealth.genome import import_genome_facts

    conn.execute(text("update document set profile_id = :p where id = :d"), {"p": family.profiles["sam"], "d": doc_id})
    with pytest.raises(GenomeImportError, match="already imported for another profile"):
        import_genome_facts(conn, blobs, document_id=doc_id)


def test_rerun_supersedes_the_previous_file(conn, family, blobs, alex_genome):
    alex = family.profiles["alex"]
    newer = synthetic.alex_facts()
    newer["provenance"]["generated_utc"] = "2026-11-01T09:00:00Z"
    newer["facts"] = [f for f in newer["facts"] if f.get("gene") != "LCT"]
    _, summary = synthetic.seed_genome(conn, blobs, household_id=family.household_id, profile_id=alex, facts=newer)
    assert summary.accepted == len(newer["facts"])
    states = dict(
        conn.execute(
            text("select status, count(*) from genome_dataset where profile_id = :p group by status"), {"p": alex}
        ).all()
    )
    assert states == {"current": 1, "superseded": 1}
    current = conn.execute(
        text("select gene, supersedes from genome_finding where profile_id = :p and status = 'current'"), {"p": alex}
    ).all()
    assert "LCT" not in {g for g, _ in current}
    assert all(sup is not None for _, sup in current)  # each re-run fact points at its previous version


def test_fingerprint_mismatch_blocks_a_second_source(conn, family, blobs, alex_genome):
    other = synthetic.sam_facts()
    other["subject"]["sample_id"] = "AR-CHIP-2019"
    with pytest.raises(GenomeImportError, match="fingerprint mismatch"):
        synthetic.seed_genome(
            conn, blobs, household_id=family.household_id, profile_id=family.profiles["alex"], facts=other
        )
    same = copy.deepcopy(synthetic.alex_facts())
    same["subject"]["sample_id"] = "AR-CHIP-2019"
    same["assay"] = {"type": "wgs", "build": "GRCh38"}
    synthetic.seed_genome(conn, blobs, household_id=family.household_id, profile_id=family.profiles["alex"], facts=same)
    assert _count(conn, "genome_dataset", family.profiles["alex"]) == 2


def test_a_sample_already_on_another_profile_is_refused(conn, family, blobs, alex_genome):
    with pytest.raises(GenomeImportError, match="already imported for profile 'alex'"):
        synthetic.seed_genome(
            conn,
            blobs,
            household_id=family.household_id,
            profile_id=family.profiles["sam"],
            facts=synthetic.alex_facts(),
        )


def test_upload_job_end_to_end(engine, committed_family, blobs, settings, alex_raw):
    """dispatch_upload -> import.genome_facts job -> findings; a bad upload becomes a job_failed item."""
    fam = committed_family
    doc = synthetic.alex_facts()
    doc["provenance"]["commit"] = secrets.token_hex(4)  # committed rows outlive the test
    facts, _ = synthetic.with_files(doc, blobs, alex_raw)
    data = json.dumps(facts).encode()
    files = [StoredFile("AR-01_genome_facts.json", blobs.put_bytes(data), len(data))]
    for data_path, index_path in alex_raw.values():
        for p in (data_path, index_path):
            sha, size = blobs.put_file(p)
            files.append(StoredFile(p.name, sha, size))
    bad = json.dumps(synthetic.lucia_facts() | {"schema": "ownhealth.genome-facts/v9"}).encode()
    with engine.begin() as c:
        dispatch_upload(c, blobs, household_id=fam.household_id, profile_id=fam.profiles["alex"], files=files)
        dispatch_upload(
            c,
            blobs,
            household_id=fam.household_id,
            profile_id=fam.profiles["lucia"],
            files=[StoredFile("bad.json", blobs.put_bytes(bad), len(bad))],
        )
    load_handlers()
    ctx = JobContext(engine=engine, settings=settings, blobs=blobs)
    ran = [run_one(ctx, kinds=["import.genome_facts"]) for _ in range(2)]
    assert all(ran)
    with engine.connect() as c:
        n = c.execute(
            text("select count(*) from genome_finding where profile_id = :p"), {"p": fam.profiles["alex"]}
        ).scalar()
        assert n == len(facts["facts"])
        vcf = c.execute(
            text("select vcf_blob is not null from genome_dataset where profile_id = :p"), {"p": fam.profiles["alex"]}
        ).scalar()
        assert vcf
        lucia = fam.profiles["lucia"]
        assert c.execute(text("select count(*) from genome_dataset where profile_id = :p"), {"p": lucia}).scalar() == 0
        assert c.execute(text("select count(*) from document where profile_id = :p"), {"p": lucia}).scalar() == 0
        item = c.execute(
            text("select payload from review_item where kind = 'job_failed' and profile_id = :p"), {"p": lucia}
        ).scalar_one()
        assert "ownhealth.genome-facts/v9" in item["error"]


def _copy_of_facts_without_files(facts):
    """Someone holds only the shareable facts JSON (it names the raw files by sha256)."""
    copied = copy.deepcopy(facts)
    copied["provenance"]["generated_utc"] = "2026-11-02T09:00:00Z"  # other bytes than the original
    return copied


@pytest.mark.parametrize("where", ["other_household", "same_household"])
def test_a_facts_file_cannot_attach_raw_files_another_profile_uploaded(conn, family, blobs, alex_genome, where):
    """A sha256 on its own grants nothing: raw files already in the blob store are linked only when
    they were uploaded with this import or already belong to this profile."""
    from ownhealth.demo.family import create_rivera_family
    from ownhealth.genome import Gate, lookup_variant

    _doc, facts, _ = alex_genome
    copied = _copy_of_facts_without_files(facts)
    if where == "other_household":
        other = create_rivera_family(conn, f"Rivera {secrets.token_hex(3)}")
        household, profile = other.household_id, other.profiles["alex"]
    else:  # Sam's profile in Alex's household, under another sample id so the sample guard does not fire
        copied["subject"]["sample_id"] = "SR-COPY-1"
        copied["subject"].pop("fingerprint", None)
        household, profile = family.household_id, family.profiles["sam"]
    _, summary = synthetic.seed_genome(conn, blobs, household_id=household, profile_id=profile, facts=copied)
    assert summary.reasons.get("raw_file_missing") == 2
    blobs_row = conn.execute(
        text("select vcf_blob, gvcf_blob from genome_dataset where profile_id = :p and status = 'current'"),
        {"p": profile},
    ).one()
    assert blobs_row == (None, None)
    answer = lookup_variant(conn, blobs, profile, gate=Gate(include_sensitive=True, minor=False), rsid="rs4244285")
    assert all(c.get("basis") != "vcf" for c in answer["calls"])


def test_a_reimport_on_the_same_profile_keeps_its_raw_files(conn, family, blobs, alex_genome):
    _doc, facts, _ = alex_genome
    _, summary = synthetic.seed_genome(
        conn,
        blobs,
        household_id=family.household_id,
        profile_id=family.profiles["alex"],
        facts=_copy_of_facts_without_files(facts),
    )
    assert "raw_file_missing" not in summary.reasons
    vcf = conn.execute(
        text("select vcf_blob from genome_dataset where profile_id = :p and status = 'current'"),
        {"p": family.profiles["alex"]},
    ).scalar()
    assert vcf is not None
