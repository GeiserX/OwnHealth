"""Small builders shared by tests of every lane. Synthetic values only."""

from __future__ import annotations

from datetime import UTC, datetime
from decimal import Decimal

from sqlalchemy import Connection

from ownhealth.blobs import BlobStore
from ownhealth.core.documents import create_document
from ownhealth.core.observations import ObservationIn
from ownhealth.core.provenance import SrcIn
from ownhealth.demo.pdf import LabRow, lab_report_pdf


def obs(profile_id, value="97", group="ldl_c", loinc="13457-7", day="2026-09-14", **kw) -> ObservationIn:
    y, m, d = map(int, day.split("-"))
    return ObservationIn(
        profile_id=profile_id,
        printed_name=kw.pop("printed_name", "Colesterol LDL"),
        printed_value=value,
        printed_unit="mg/dL",
        printed_ref_range="< 130",
        value_num=Decimal(value),
        ref_high=Decimal("130"),
        loinc=loinc,
        alias_group=group,
        unit_ucum="mg/dL",
        value_canonical=Decimal(value),
        unit_canonical="mg/dL",
        derivation_version="test",
        effective_at=datetime(y, m, d, 8, tzinfo=UTC),
        **kw,
    )


def manual_src(user_id=None) -> SrcIn:
    return SrcIn(kind="manual", actor_kind="user", actor_id=user_id)


def lab_document(conn: Connection, blobs: BlobStore, family, slug="alex", rows=None):
    """A one-page synthetic LabNorte PDF stored as a document. Returns (document_id, bboxes)."""
    rows = rows or [
        LabRow("Colesterol LDL", "97", "mg/dL", "< 130"),
        LabRow("Vitamina D", "24", "ng/mL", "30 - 100", "L"),
    ]
    pdf, boxes = lab_report_pdf(
        lab="LabNorte",
        title="Informe",
        patient="RIVERA, ALEX",
        birth="14/05/1979",
        collected="14/09/2026",
        report_id="LN-T",
        pages=[rows],
    )
    sha = blobs.put_bytes(pdf)
    ref = create_document(
        conn,
        household_id=family.household_id,
        profile_id=family.profiles[slug],
        blob_sha256=sha,
        mime="application/pdf",
        size_bytes=len(pdf),
        filename="labnorte.pdf",
        pages=1,
    )
    return ref.id, boxes[0]
