"""Retracting or moving a structured document withdraws everything it brought in: observations,
records (through src.document_id) and wearable samples with their rollups."""

from datetime import date

import pytest
from sqlalchemy import text

from ownhealth.core.documents import create_document
from ownhealth.core.records import active_medications, list_records
from ownhealth.ingest.apple_health import import_apple_health
from ownhealth.ingest.apple_health.synthetic import ExportSpec, write_export
from ownhealth.ingest.fhir import import_fhir
from ownhealth.ingest.pdf import assign_document
from ownhealth.ingest.retract import retract_document_facts

JSON = "application/fhir+json"
ZIP = "application/zip"


def _current_obs(conn, profile):
    return conn.execute(
        text("select count(*) from observation where profile_id = :p and status in ('current', 'provisional')"),
        {"p": profile},
    ).scalar()


def _wearables(conn, profile):
    n = conn.execute(text("select count(*) from wearable_sample where profile_id = :p"), {"p": profile}).scalar()
    r = conn.execute(text("select count(*) from wearable_rollup where profile_id = :p"), {"p": profile}).scalar()
    return n, r


def _job(conn, job_id):
    return conn.execute(text("select kind, payload from job where id = :j"), {"j": job_id}).one()


def _fhir_doc(conn, family, store_doc, fhir_file, blobs):
    p = family.profiles["alex"]
    doc = store_doc(conn, family, "alex", fhir_file("ips-alex.json"), "ips.json", JSON)
    import_fhir(conn, blobs, document_id=doc)
    assert active_medications(conn, p) and list_records(conn, "condition", p) and _current_obs(conn, p)
    return doc


def _apple_doc(conn, family, store_doc, blobs, tmp_path):
    data = write_export(tmp_path / "e.zip", ExportSpec(days=3, end=date(2026, 11, 7), birth_date=date(1979, 5, 14)))
    doc = store_doc(conn, family, "alex", data.read_bytes(), "e.zip", ZIP)
    import_apple_health(conn, blobs, document_id=doc)
    n, r = _wearables(conn, family.profiles["alex"])
    assert n > 0 and r > 0
    return doc


def test_retracting_a_fhir_document_withdraws_its_records(conn, family, blobs, store_doc, fhir_file):
    p = family.profiles["alex"]
    doc = _fhir_doc(conn, family, store_doc, fhir_file, blobs)
    out = retract_document_facts(conn, doc, "document retracted: wrong patient")
    assert active_medications(conn, p) == [] and list_records(conn, "condition", p) == []
    for kind in ("allergy", "immunization", "procedure"):
        assert list_records(conn, kind, p) == []
    assert _current_obs(conn, p) == 0 and len(out["observations"]) == 3
    assert set(out["records"]) >= {"condition", "medication_statement", "allergy"}
    # and the import can run again: its stored summary is gone
    assert "import" not in conn.execute(text("select flags from document where id = :d"), {"d": doc}).scalar()


def test_retracting_an_apple_export_deletes_its_samples_and_rollups(conn, family, blobs, store_doc, tmp_path):
    p = family.profiles["alex"]
    doc = _apple_doc(conn, family, store_doc, blobs, tmp_path)
    out = retract_document_facts(conn, doc, "document retracted: not mine")
    assert out["wearable_samples"] > 0
    assert _wearables(conn, p) == (0, 0)
    assert active_medications(conn, p) == []  # its clinical records go too


def test_another_exports_samples_stay(conn, family, blobs, store_doc, tmp_path):
    p = family.profiles["alex"]
    _apple_doc(conn, family, store_doc, blobs, tmp_path)
    before = _wearables(conn, p)
    unrelated = store_doc(conn, family, "alex", b"%PDF-1.4 other", "x.pdf", "application/pdf")
    assert retract_document_facts(conn, unrelated, "not mine")["wearable_samples"] == 0
    assert _wearables(conn, p) == before


def test_moving_a_fhir_document_reimports_it_on_the_new_profile(conn, family, blobs, store_doc, fhir_file):
    alex, sam = family.profiles["alex"], family.profiles["sam"]
    doc = _fhir_doc(conn, family, store_doc, fhir_file, blobs)
    job = assign_document(conn, doc, sam, user_id=family.users["alex"])
    assert list_records(conn, "condition", alex) == [] and active_medications(conn, alex) == []
    assert _current_obs(conn, alex) == 0
    assert _job(conn, job).kind == "import.fhir_bundle"
    again = import_fhir(conn, blobs, document_id=doc)  # what the job runs: a real import, not the old summary
    assert again.queued > 0 and again.reasons.get("identity_mismatch")  # Alex's file now waits on Sam's profile


def test_moving_an_apple_export_takes_its_samples_off_the_old_profile(conn, family, blobs, store_doc, tmp_path):
    alex, sam = family.profiles["alex"], family.profiles["sam"]
    doc = _apple_doc(conn, family, store_doc, blobs, tmp_path)
    job = assign_document(conn, doc, sam, user_id=family.users["alex"])
    assert _wearables(conn, alex) == (0, 0)
    assert _job(conn, job).kind == "import.apple_health"


def _ecgs(conn, profile):
    return conn.execute(
        text("select count(*) from document where profile_id = :p and doc_type = 'ecg' and status = 'active'"),
        {"p": profile},
    ).scalar()


def _ecg_export(conn, family, store_doc, blobs, tmp_path):
    spec = ExportSpec(days=2, end=date(2026, 11, 7), birth_date=date(1979, 5, 14), ecg_count=2)
    doc = store_doc(conn, family, "alex", write_export(tmp_path / "ecg.zip", spec).read_bytes(), "e.zip", ZIP)
    import_apple_health(conn, blobs, document_id=doc)
    assert _ecgs(conn, family.profiles["alex"]) == 2
    return doc


def test_retracting_an_apple_export_retracts_its_ecg_documents(conn, family, blobs, store_doc, tmp_path):
    alex = family.profiles["alex"]
    doc = _ecg_export(conn, family, store_doc, blobs, tmp_path)
    out = retract_document_facts(conn, doc, "document retracted: not mine")
    assert _ecgs(conn, alex) == 0 and len(out["documents"]) == 2
    import_apple_health(conn, blobs, document_id=doc)  # the same export again brings them back
    assert _ecgs(conn, alex) == 2


def test_moving_an_apple_export_takes_its_ecgs_off_the_old_profile(conn, family, blobs, store_doc, tmp_path):
    alex, sam = family.profiles["alex"], family.profiles["sam"]
    doc = _ecg_export(conn, family, store_doc, blobs, tmp_path)
    assign_document(conn, doc, sam, user_id=family.users["alex"])
    assert _ecgs(conn, alex) == 0


def test_an_ecg_the_person_uploaded_on_its_own_stays(conn, family, blobs, store_doc, tmp_path):
    """An ECG document that existed before the export is not the export's: it stays when the export goes."""
    import zipfile

    spec = ExportSpec(days=2, end=date(2026, 11, 7), birth_date=date(1979, 5, 14), ecg_count=1)
    path = write_export(tmp_path / "one.zip", spec)
    with zipfile.ZipFile(path) as z:
        name = next(n for n in z.namelist() if "electrocardiograms/" in n)
        data = z.read(name)
    own = store_doc(conn, family, "alex", data, "ecg.csv", "text/csv")
    doc = store_doc(conn, family, "alex", path.read_bytes(), "e.zip", ZIP)
    import_apple_health(conn, blobs, document_id=doc)
    retract_document_facts(conn, doc, "document retracted: not mine")
    status = conn.execute(text("select status from document where id = :d"), {"d": own}).scalar()
    assert status == "active"


def test_a_genome_facts_file_does_not_move(conn, family, blobs):
    data = b'{"schema": "ownhealth.genome-facts/v1"}'
    doc = create_document(
        conn,
        household_id=family.household_id,
        profile_id=family.profiles["alex"],
        blob_sha256=blobs.put_bytes(data),
        mime="application/json",
        size_bytes=len(data),
        filename="g.json",
        doc_type="genomics",
    ).id
    with pytest.raises(ValueError, match="cannot move"):
        assign_document(conn, doc, family.profiles["sam"], user_id=family.users["alex"])


def test_the_retract_endpoint_withdraws_fhir_records(app_for, committed_family, engine, blobs, fhir_file):
    fam = committed_family
    data = fhir_file("ips-alex.json")
    sha = blobs.put_bytes(data)
    with engine.begin() as c:
        doc = create_document(
            c,
            household_id=fam.household_id,
            profile_id=fam.profiles["alex"],
            blob_sha256=sha,
            mime=JSON,
            size_bytes=len(data),
            filename="ips.json",
        ).id
        import_fhir(c, blobs, document_id=doc)
    r = app_for(fam.users["alex"]).post(f"/api/documents/{doc}/retract", json={"reason": "wrong patient"})
    assert r.status_code == 200
    with engine.connect() as c:
        assert active_medications(c, fam.profiles["alex"]) == []
        assert list_records(c, "condition", fam.profiles["alex"]) == []
