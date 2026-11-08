"""Lane B fixtures: the synthetic lab slips (fixtures/labslips), a stand-in normalizer, stored PDFs."""

from __future__ import annotations

import sys
from pathlib import Path

import pytest

from ownhealth.core.documents import create_document

LABSLIPS = Path(__file__).resolve().parents[3] / "fixtures" / "labslips"
sys.path.insert(0, str(LABSLIPS))

import generate  # noqa: E402


@pytest.fixture(scope="session")
def slips() -> dict[str, tuple[bytes, dict]]:
    """Every layout, the LabNorte 3-page report and the amended pairs: name -> (pdf, truth)."""
    return {name: (pdf, truth) for name, pdf, truth in generate.all_slips()}


@pytest.fixture(scope="session")
def special() -> dict[str, tuple[bytes, dict]]:
    """Pages without a text layer and the injection fixtures."""
    return {name: (pdf, truth) for name, pdf, truth in generate.no_text_layer() + generate.injection_fixtures()}


@pytest.fixture
def fake_normalizer(monkeypatch):
    """Lane A's normalize_row replaced by the catalog-based stand-in: tests lane B's pipeline alone."""
    import fake_normalize

    from ownhealth import normalize

    monkeypatch.setattr(normalize, "normalize_row", fake_normalize.normalize_row)


@pytest.fixture
def store(conn, blobs, family):
    """store(pdf, slug="alex", mime=...) -> document id of a stored upload on that profile."""

    def _store(data: bytes, slug: str = "alex", mime: str = "application/pdf", filename: str = "report.pdf"):
        sha = blobs.put_bytes(data)
        return create_document(
            conn,
            household_id=family.household_id,
            profile_id=family.profiles[slug],
            blob_sha256=sha,
            mime=mime,
            size_bytes=len(data),
            filename=filename,
        ).id

    return _store
