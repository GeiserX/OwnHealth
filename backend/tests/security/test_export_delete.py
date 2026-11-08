"""Full export (zip of FHIR NDJSON + original files) and hard deletion of a profile and an account."""

import hashlib
import io
import json
import secrets
import zipfile

from factories import lab_document, manual_src, obs
from sqlalchemy import text

from ownhealth.blobs import BlobStore
from ownhealth.core import review
from ownhealth.core.audit import AuditEvent, write_audit
from ownhealth.core.observations import commit_observation, retract_observation
from ownhealth.core.review import CandidateIn
from ownhealth.demo.pdf import LabRow


def _rows():
    """Two lab rows; a random value makes the PDF bytes, and so the blob, unique to this test."""
    return [
        LabRow("Colesterol LDL", "97", "mg/dL", "< 130"),
        LabRow("Vitamina D", str(secrets.randbelow(10**6)), "ng/mL", "30 - 100", "L"),
    ]


def _seed(engine, family, blobs: BlobStore, slug="alex", rows=None):
    """A LabNorte report with one auto-accepted row, one row in review, a manual value later retracted."""
    with engine.begin() as c:
        doc_id, boxes = lab_document(c, blobs, family, slug=slug, rows=rows or _rows())
        run = review.create_extraction_run(c, doc_id, extractor="rules:test", extractor_version="1", config_hash=b"t")
        pid = family.profiles[slug]
        cand = lambda b, conf, v: CandidateIn(  # noqa: E731
            page_no=1, bbox=b, cells={"value": {"text": v}}, row_conf=conf, observation=obs(pid, v)
        )
        ok = review.submit_candidate(c, run, cand(boxes[0], 0.99, "97"), allowance="template_text", identity="matched")
        queued = review.submit_candidate(c, run, cand(boxes[1], 0.5, "24"), allowance="rules_text", identity="matched")
        assert ok.state == "auto_accepted" and queued.state == "in_review"
        manual = commit_observation(
            c, obs(pid, "101", day="2026-10-01"), src=manual_src(family.users["alex"]), accepted_by="person"
        )
        retract_observation(c, manual.id, "typed the wrong value")
        write_audit(
            c, AuditEvent(actor_kind="user", actor_id=family.users["sam"], via="ui", action="read", profile_id=pid)
        )
        sha = c.execute(text("select encode(blob_sha256, 'hex') from document where id = :d"), {"d": doc_id}).scalar()
    return doc_id, sha, ok.src_id


def test_export_zip(client_for, committed_family, https_settings, engine):
    blobs = BlobStore(https_settings.blobs_path)
    doc_id, sha, src_id = _seed(engine, committed_family, blobs)
    with engine.begin() as c:  # Sam's value must not leak into Alex's export
        commit_observation(
            c,
            obs(committed_family.profiles["sam"], "55"),
            src=manual_src(committed_family.users["sam"]),
            accepted_by="person",
        )
    stale = client_for(committed_family.users["alex"], step_up=False)
    assert stale.post("/api/profiles/alex/export", json={"fmt": "zip"}).json()["detail"]["code"] == "step_up_required"

    r = client_for(committed_family.users["alex"]).post("/api/profiles/alex/export", json={"fmt": "zip"})
    assert r.status_code == 200, r.text
    assert r.headers["content-type"] == "application/zip"
    assert "ownhealth-alex-" in r.headers["content-disposition"]
    z = zipfile.ZipFile(io.BytesIO(r.content))
    assert z.testzip() is None
    manifest = json.loads(z.read("export-manifest.json"))
    assert manifest["format"] == "ownhealth.export/v1" and manifest["profile"]["slug"] == "alex"
    for f in manifest["files"]:  # every entry is listed with the right hash
        assert hashlib.sha256(z.read(f["path"])).hexdigest() == f["sha256"], f["path"]
    names = set(z.namelist())
    assert {
        "fhir/Patient.ndjson",
        "fhir/Observation.ndjson",
        "fhir/DocumentReference.ndjson",
        "fhir/Provenance.ndjson",
        "ownhealth/review_item.ndjson",
        "ownhealth/audit_event.ndjson",
    } <= names
    # the original upload, byte for byte
    original = next(n for n in names if n.startswith(f"files/{sha}/"))
    assert hashlib.sha256(z.read(original)).hexdigest() == sha
    observations = [json.loads(line) for line in z.read("fhir/Observation.ndjson").splitlines()]
    statuses = sorted(t["code"] for o in observations for t in o["meta"]["tag"])
    assert statuses == ["current", "retracted"]  # every lifecycle state, marked; nothing of Sam's
    with engine.connect() as c:
        alex_ids = {
            str(i)
            for i in c.execute(
                text("select id from observation where profile_id = :p"), {"p": committed_family.profiles["alex"]}
            ).scalars()
        }
    assert {o["id"] for o in observations} == alex_ids
    assert all(o["subject"]["reference"].startswith("Patient/") for o in observations)
    prov = [json.loads(line) for line in z.read("fhir/Provenance.ndjson").splitlines()]
    cited = next(p for p in prov if p["extension"][0]["valueString"] == src_id)
    assert cited["entity"][0]["what"]["reference"] == f"DocumentReference/{doc_id}"
    loc = json.loads(next(e["valueString"] for e in cited["extension"] if e["url"] == "urn:ownhealth:location"))
    assert loc["page"] == 1 and len(loc["bbox"]) == 4
    # only Alex's data, and nothing about access control or tokens
    assert "api_token" not in str(names) and "profile_grant" not in str(names)
    assert str(committed_family.profiles["sam"]) not in z.read("fhir/Observation.ndjson").decode()
    # audited, and the temporary zip is gone
    with engine.connect() as c:
        ev = c.execute(
            text("select via, bytes_out from audit_event where action = 'export' and profile_id = :p"),
            {"p": committed_family.profiles["alex"]},
        ).one()
        assert ev.via == "export" and ev.bytes_out == len(r.content)
    assert not list((https_settings.data_dir / "tmp").glob("export-*"))


def test_delete_profile(client_for, committed_family, https_settings, engine):
    f = committed_family
    blobs = BlobStore(https_settings.blobs_path)
    _, sha, _ = _seed(engine, f, blobs)
    assert blobs.exists(sha)
    with engine.begin() as c:  # Sam's own value, which deleting Alex must leave alone
        sams = commit_observation(c, obs(f.profiles["sam"], "55"), src=manual_src(f.users["sam"]), accepted_by="person")
    alex = client_for(f.users["alex"])
    assert alex.delete("/api/profiles/alex", params={"confirm": "Alex"}).status_code == 422
    assert client_for(f.users["sam"]).delete("/api/profiles/alex", params={"confirm": "alex"}).status_code == 403
    assert (
        client_for(f.users["alex"], step_up=False).delete("/api/profiles/alex", params={"confirm": "alex"}).status_code
        == 403
    )
    r = alex.delete("/api/profiles/alex", params={"confirm": "alex"})
    assert r.status_code == 200, r.text
    assert r.json()["rows"]["observation"] == 2 and r.json()["blobs"] == 1
    pid = f.profiles["alex"]
    with engine.connect() as c:
        for table in ("observation", "document", "src", "review_item", "profile"):
            col = "id" if table == "profile" else "profile_id"
            assert c.execute(text(f"select count(*) from {table} where {col} = :p"), {"p": pid}).scalar() == 0, table
        # the audit log keeps its events (ids only), and gains the purge
        actions = c.execute(text("select action from audit_event where profile_id = :p"), {"p": pid}).scalars().all()
        assert "read" in actions and "purge" in actions
    assert not blobs.exists(sha)
    # Sam's data is untouched
    assert {p["slug"] for p in client_for(f.users["sam"]).get("/api/profiles").json()} == {"sam", "lucia"}
    with engine.connect() as c:
        owner = c.execute(text("select profile_id from observation where id = :i"), {"i": sams.id}).scalar()
    assert owner == f.profiles["sam"]


def test_shared_blob_survives(client_for, committed_family, https_settings, engine):
    """The same file uploaded for two profiles is one blob: deleting one profile keeps it."""
    f = committed_family
    blobs = BlobStore(https_settings.blobs_path)
    rows = _rows()
    _, sha, _ = _seed(engine, f, blobs, slug="lucia", rows=rows)
    with engine.begin() as c:
        lab_document(c, blobs, f, slug="sam", rows=rows)  # same bytes, other profile
    assert client_for(f.users["alex"]).delete("/api/profiles/lucia", params={"confirm": "lucia"}).status_code == 200
    assert blobs.exists(sha)


def test_delete_account(client_for, committed_family, engine):
    f = committed_family
    sam = client_for(f.users["sam"])
    assert sam.delete("/api/account", params={"confirm": "Sam"}).status_code == 422
    # Sam is the only one on sam: closing the login would strand it
    r = sam.delete("/api/account", params={"confirm": "Sam Rivera"})
    assert r.status_code == 409 and r.json()["detail"]["profiles"] == ["sam"]
    r = sam.delete("/api/account", params={"confirm": "Sam Rivera", "delete_profile": "true"})
    assert r.status_code == 200, r.text
    assert sam.get("/api/me").status_code == 401  # the session is gone
    with engine.connect() as c:
        row = c.execute(
            text("select display_name, disabled_at from app_user where id = :u"), {"u": f.users["sam"]}
        ).one()
        assert row.display_name == "Deleted user" and row.disabled_at is not None
        assert (
            c.execute(text("select count(*) from profile_grant where user_id = :u"), {"u": f.users["sam"]}).scalar()
            == 0
        )
        assert c.execute(text("select count(*) from profile where id = :p"), {"p": f.profiles["sam"]}).scalar() == 0
    # Lucía still has Alex as guardian
    assert "lucia" in {p["slug"] for p in client_for(f.users["alex"]).get("/api/profiles").json()}


def test_delete_account_checks_before_deleting(client_for, committed_family, engine):
    """With delete_profile=true and another profile only Sam manages, nothing is deleted at all."""
    f = committed_family
    sam = client_for(f.users["sam"])
    assert (
        sam.post("/api/household/profiles", json={"display_name": "Mateo Rivera", "role": "guardian"}).status_code
        == 201
    )
    r = sam.delete("/api/account", params={"confirm": "Sam Rivera", "delete_profile": "true"})
    assert r.status_code == 409 and r.json()["detail"]["profiles"] == ["mateo"]
    with engine.connect() as c:
        assert c.execute(text("select count(*) from profile where id = :p"), {"p": f.profiles["sam"]}).scalar() == 1


def test_a_missing_blob_is_listed_in_the_manifest(conn, family, blobs):
    from ownhealth.export import export_profile

    doc_id, _ = lab_document(conn, blobs, family, rows=_rows())
    sha = conn.execute(text("select encode(blob_sha256, 'hex') from document where id = :d"), {"d": doc_id}).scalar()
    blobs.path(sha).unlink()  # the row survives, the file is gone
    out = io.BytesIO()
    manifest = export_profile(conn, blobs, family.profiles["alex"], out)
    z = zipfile.ZipFile(out)
    assert z.testzip() is None and not any(n.startswith(f"files/{sha}/") for n in z.namelist())
    assert [m["source_blob"] for m in manifest["missing_blobs"]] == [sha]
    assert json.loads(z.read("export-manifest.json"))["missing_blobs"][0]["path"].startswith(f"files/{sha}/")
