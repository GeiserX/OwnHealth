"""Uploading a file again after its document was retracted brings the document and its values back."""

from __future__ import annotations

from uuid import UUID

import pytest
from sqlalchemy import text

from ownhealth import jobs
from ownhealth.core.documents import StoredFile, dispatch_upload

pytestmark = pytest.mark.usefixtures("fake_normalizer")


def _upload(engine, blobs, fam, pdf):
    sha = blobs.put_bytes(pdf)
    with engine.begin() as c:
        (item,) = dispatch_upload(
            c,
            blobs,
            household_id=fam.household_id,
            profile_id=fam.profiles["alex"],
            files=[StoredFile("LabNorte_2026-09-14.pdf", sha, len(pdf))],
        )["items"]
    return item


def _drain(engine, settings, blobs):
    ctx = jobs.JobContext(engine=engine, settings=settings, blobs=blobs)
    for _ in range(20):
        if jobs.run_one(ctx, kinds=["import.pdf"]) is None:
            break


def _state(engine, doc):
    with engine.connect() as c:
        status = c.execute(text("select status from document where id = :d"), {"d": doc}).scalar()
        current = c.execute(
            text("select count(*) from observation where document_id = :d and status in ('current', 'provisional')"),
            {"d": doc},
        ).scalar()
    return status, current


def test_reupload_after_retract_brings_the_document_back(app_for, engine, settings, blobs, committed_family, slips):
    jobs.load_handlers()
    fam = committed_family
    pdf, _ = slips["es-labnorte-3p"]
    first = _upload(engine, blobs, fam, pdf)
    _drain(engine, settings, blobs)
    doc = UUID(first["document_id"])
    status, imported = _state(engine, doc)
    assert status == "active" and imported > 0

    r = app_for(fam.users["alex"]).post(f"/api/documents/{doc}/retract", json={"reason": "uploaded twice"})
    assert r.status_code == 200, r.text
    assert len(r.json()["retracted_observations"]) == imported
    assert _state(engine, doc) == ("retracted", 0)

    again = _upload(engine, blobs, fam, pdf)
    assert again["document_id"] == str(doc) and again["job_id"] and not again["duplicate"]
    _drain(engine, settings, blobs)
    assert _state(engine, doc) == ("active", imported)
