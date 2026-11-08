"""The original file, page renders and words of a document holding a sensitive value need read:sensitive:
otherwise they would show the values every other read path hides."""

from factories import obs

from ownhealth.core.documents import create_document, document_is_sensitive
from ownhealth.core.observations import commit_observation
from ownhealth.core.provenance import SrcIn

PDF = b"%PDF-1.4\n% synthetic LabNorte slip\n%%EOF\n"


def _doc_with(engine, blobs, fam, who, sensitivity):
    pid = fam.profiles[who]
    with engine.begin() as c:
        ref = create_document(
            c,
            household_id=fam.household_id,
            profile_id=pid,
            blob_sha256=blobs.put_bytes(PDF + str(sensitivity).encode()),
            mime="application/pdf",
            size_bytes=len(PDF),
            filename="labnorte.pdf",
            pages=1,
            doc_type="lab_report",
        )
        doc_id = getattr(ref, "id", ref)
        o = obs(pid, printed_name="VIH Ac", document_id=doc_id, sensitivity=sensitivity)
        commit_observation(c, o, src=SrcIn(kind="manual", profile_id=pid, actor_kind="system"), accepted_by="import")
    return doc_id


def test_caregiver_cannot_fetch_a_sensitive_document(client_for, engine, committed_family):
    fam = committed_family
    alex = client_for(fam.users["alex"])  # caregiver on carmen: read:labs, no read:sensitive
    blobs = alex.app.state.runtime.blobs
    sensitive = _doc_with(engine, blobs, fam, "carmen", ["sexual_health"])
    plain = _doc_with(engine, blobs, fam, "carmen", [])
    with engine.connect() as c:
        assert document_is_sensitive(c, sensitive) and not document_is_sensitive(c, plain)
    r = alex.get(f"/api/documents/{sensitive}/file")
    assert r.status_code == 403 and r.json()["detail"]["code"] == "insufficient_scope", r.text
    assert alex.get(f"/api/documents/{sensitive}/pages/1.png").status_code == 403
    assert alex.get(f"/api/documents/{plain}/file").status_code == 200
