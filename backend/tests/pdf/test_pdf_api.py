"""Document vault REST API (CONTRACTS 5, lane B rows)."""

from __future__ import annotations

import pytest
from sqlalchemy import text

from ownhealth.auth.tokens import create_token, verify_token
from ownhealth.core.documents import create_document
from ownhealth.demo.family import create_rivera_family
from ownhealth.ingest.pdf import import_pdf

pytestmark = pytest.mark.usefixtures("fake_normalizer")


@pytest.fixture
def imported(engine, settings, committed_family, slips):
    """The LabNorte report imported for Alex, committed; returns (family, document id)."""
    from ownhealth.blobs import BlobStore

    blobs = BlobStore(settings.blobs_path)
    pdf, _ = slips["es-labnorte-3p"]
    with engine.begin() as c:
        doc = create_document(
            c,
            household_id=committed_family.household_id,
            profile_id=committed_family.profiles["alex"],
            blob_sha256=blobs.put_bytes(pdf),
            mime="application/pdf",
            size_bytes=len(pdf),
            filename="LabNorte_2026-09-14.pdf",
        ).id
        import_pdf(c, blobs, document_id=doc)
    return committed_family, doc


def _audits(engine, tool):
    with engine.connect() as c:
        return c.execute(text("select count(*) from audit_event where tool = :t"), {"t": tool}).scalar()


def test_list_search_and_detail(app_for, engine, imported):
    fam, doc = imported
    client = app_for(fam.users["alex"])
    before = _audits(engine, "GET /api/profiles/{p}/documents")
    items = client.get("/api/profiles/alex/documents").json()["items"]
    assert [i["id"] for i in items] == [str(doc)]
    item = items[0]
    assert item["filename"] == {"trust": "untrusted_text", "origin": "filename", "text": "LabNorte_2026-09-14.pdf"}
    assert (item["doc_type"], item["provider_name"], item["pages"], item["status"]) == (
        "lab_report",
        "LabNorte",
        3,
        "active",
    )
    assert "metadata_text" not in item["flags"]
    hit = client.get("/api/profiles/alex/documents", params={"q": "hemoglobina"}).json()["items"]
    assert len(hit) == 1 and hit[0]["snippet"]["trust"] == "untrusted_text" and "<b>" in hit[0]["snippet"]["text"]
    assert client.get("/api/profiles/alex/documents", params={"q": "colonoscopia"}).json()["items"] == []
    assert _audits(engine, "GET /api/profiles/{p}/documents") == before + 3

    detail = client.get(f"/api/documents/{doc}").json()
    assert [p["page_no"] for p in detail["pages_info"]] == [1, 2, 3] and detail["mime"] == "application/pdf"
    assert detail["expected_tests"][0]["reason"]["text"] == "Pendiente"


def test_file_page_png_and_words(app_for, imported):
    fam, doc = imported
    client = app_for(fam.users["alex"])
    f = client.get(f"/api/documents/{doc}/file")
    assert f.status_code == 200 and f.content.startswith(b"%PDF") and f.headers["content-security-policy"] == "sandbox"
    png = client.get(f"/api/documents/{doc}/pages/2.png", params={"scale": 1})
    assert png.status_code == 200 and png.content.startswith(b"\x89PNG")
    assert client.get(f"/api/documents/{doc}/pages/4.png").status_code == 404
    words = client.get(f"/api/documents/{doc}/pages/1/words").json()
    assert words["trust"] == "untrusted_text" and any(w["text"] == "LabNorte" for w in words["words"])


def test_tokens_need_their_scopes_and_other_households_see_nothing(app_for, engine, imported):
    fam, doc = imported
    with engine.begin() as c:
        labs_only = create_token(
            c,
            user_id=fam.users["alex"],
            label="labs",
            profile_ids=[fam.profiles["alex"]],
            scopes=["read:summary", "read:labs"],
        )
        p = verify_token(c, labs_only.plaintext)
        stranger = create_rivera_family(c, "Strangers")
    client = app_for(principal=p)
    assert client.get(f"/api/documents/{doc}").status_code == 200
    r = client.get(f"/api/documents/{doc}/pages/1/words")
    assert r.status_code == 403 and r.json()["detail"]["code"] == "insufficient_scope"
    # without read:documents_text a search matches filenames only, and returns no snippet
    assert client.get("/api/profiles/alex/documents", params={"q": "hemoglobina"}).json()["items"] == []
    other = app_for(stranger.users["alex"])
    for url in (f"/api/documents/{doc}", f"/api/documents/{doc}/file", f"/api/documents/{doc}/pages/1.png"):
        assert other.get(url).status_code == 404


def test_assign_and_retract(app_for, engine, imported):
    fam, doc = imported
    client = app_for(fam.users["alex"])
    r = client.post(f"/api/documents/{doc}/assign", json={"profile": "lucia"})
    assert r.status_code == 200 and r.json()["profile"] == "lucia" and r.json()["job_id"]
    with engine.connect() as c:
        assert (
            c.execute(text("select profile_id from document where id = :d"), {"d": doc}).scalar()
            == fam.profiles["lucia"]
        )
        assert (
            c.execute(
                text("select count(*) from observation where document_id = :d and status = 'current'"), {"d": doc}
            ).scalar()
            == 0
        )
    viewer = app_for(fam.users["sam"])  # Sam is guardian of Lucía: may write, may not retract without a reason
    assert viewer.post(f"/api/documents/{doc}/retract", json={"reason": " "}).status_code == 422
    r = client.post(f"/api/documents/{doc}/retract", json={"reason": "wrong patient"})
    assert r.status_code == 200 and r.json()["status"] == "retracted"
    assert client.post(f"/api/documents/{doc}/retract", json={"reason": "again"}).status_code == 409
