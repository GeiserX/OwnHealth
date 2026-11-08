"""Lane C fixtures: synthetic FHIR files and Apple Health exports stored as documents."""

from __future__ import annotations

from pathlib import Path

import pytest

from ownhealth.core.documents import create_document

FIXTURES = Path(__file__).resolve().parents[3] / "fixtures"


@pytest.fixture
def store_doc(blobs):
    """store_doc(conn, family, slug, data: bytes, filename, mime) -> document id."""

    def _store(conn, family, slug, data: bytes, filename: str, mime: str):
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


@pytest.fixture
def fhir_file():
    """fhir_file(name) -> the bytes of fixtures/fhir/<name>."""
    return lambda name: (FIXTURES / "fhir" / name).read_bytes()
