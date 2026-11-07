"""Lane B: lab PDF import (ingestion.md sections 5-7, 9). Job kind `import.pdf`.

Text layer first (pdfplumber words with bboxes), vendor templates in `templates/labs/`, generic table
rules otherwise; page renders via pypdfium2. Each row becomes a CandidateIn submitted with
`core.review.submit_candidate`. Contract stubs until lane B lands them.
"""

from __future__ import annotations

from dataclasses import dataclass
from uuid import UUID

from sqlalchemy import Connection

from ownhealth.blobs import BlobStore
from ownhealth.ingest import ImportSummary


@dataclass(frozen=True)
class Word:
    text: str
    bbox: tuple[float, float, float, float]  # x0, top, x1, bottom in PDF points, top-left origin


def import_pdf(conn: Connection, blobs: BlobStore, *, document_id: UUID) -> ImportSummary:
    raise NotImplementedError("lane B: ingest.pdf.import_pdf")


def page_words(conn: Connection, blobs: BlobStore, *, document_id: UUID, page_no: int) -> list[Word]:
    raise NotImplementedError("lane B: ingest.pdf.page_words")


def render_page_png(blobs: BlobStore, *, blob_sha256: str, page_no: int, scale: float = 2.0) -> bytes:
    raise NotImplementedError("lane B: ingest.pdf.render_page_png")


def ground_value(
    conn: Connection, blobs: BlobStore, *, document_id: UUID, page_no: int, name: str, value: str
) -> tuple[float, float, float, float] | None:
    """Find a printed value on the page word layer (decimal-separator tolerant). Used by `propose`."""
    raise NotImplementedError("lane B: ingest.pdf.ground_value")
