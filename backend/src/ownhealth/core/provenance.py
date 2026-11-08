"""Provenance: every fact points to one `src` row, and its id is the citation id over MCP.

A document value needs document, page and bbox; an import needs its `import_ref`.
`resolve_src` is what both `get_source` (MCP) and `GET /api/sources/{id}` (UI) use.
"""

from __future__ import annotations

from datetime import datetime
from typing import Literal
from uuid import UUID

from pydantic import BaseModel, model_validator
from sqlalchemy import Connection, text

from ownhealth.core.ids import is_src_id, new_src_id

SrcKind = Literal[
    "document_region", "fhir_import", "apple_health", "genome_facts", "manual", "device", "agent_proposal", "calculated"
]

# Tables whose rows carry src_id, in lookup order for resolve_src.
FACT_TABLES = (
    "observation",
    "condition",
    "medication_statement",
    "allergy",
    "immunization",
    "procedure",
    "encounter",
    "genome_finding",
    "note",
    "imaging_study",
    "genome_dataset",
)


class ProvenanceError(ValueError):
    pass


class SrcIn(BaseModel):
    kind: SrcKind
    profile_id: UUID | None = None
    document_id: UUID | None = None
    page_no: int | None = None
    bbox: tuple[float, float, float, float] | None = None
    extraction_run_id: UUID | None = None
    candidate_row_id: UUID | None = None
    import_ref: str | None = None
    label: str | None = None
    actor_kind: Literal["user", "token", "system"] = "system"
    actor_id: UUID | None = None
    id: str | None = None  # explicit id (fixtures); otherwise generated

    @model_validator(mode="after")
    def _required_fields(self) -> SrcIn:
        if self.kind == "document_region" and (self.document_id is None or self.page_no is None or self.bbox is None):
            raise ProvenanceError("a document_region source needs document_id, page_no and bbox")
        if self.kind in ("fhir_import", "apple_health", "genome_facts") and not self.import_ref:
            raise ProvenanceError(f"a {self.kind} source needs import_ref")
        if self.id is not None and not is_src_id(self.id):
            raise ProvenanceError(f"bad src id {self.id!r}")
        return self


def create_src(conn: Connection, src: SrcIn) -> str:
    sid = src.id or new_src_id()
    conn.execute(
        text(
            "insert into src (id, profile_id, kind, document_id, page_no, bbox, extraction_run_id,"
            " candidate_row_id, import_ref, label, actor_kind, actor_id) values (:id, :profile_id, :kind,"
            " :document_id, :page_no, :bbox, :extraction_run_id, :candidate_row_id, :import_ref, :label,"
            " :actor_kind, :actor_id)"
        ),
        {**src.model_dump(exclude={"id"}), "id": sid, "bbox": list(src.bbox) if src.bbox else None},
    )
    return sid


def ensure_src(conn: Connection, src: SrcIn | str, profile_id: UUID | None = None) -> str:
    """Accept an existing src id or create one from a SrcIn.

    With `profile_id` (the citing fact's profile), an existing src scoped to another profile is
    refused, so a fact never cites, and resolve_src never lists it under, someone else's source.
    """
    if isinstance(src, str):
        row = conn.execute(text("select profile_id from src where id = :id"), {"id": src}).first()
        if row is None:
            raise ProvenanceError(f"unknown src {src}")
        if profile_id is not None and row.profile_id is not None and row.profile_id != profile_id:
            raise ProvenanceError(f"src {src} belongs to another profile")
        return src
    if profile_id is not None and src.profile_id is not None and src.profile_id != profile_id:
        raise ProvenanceError("a new src belongs to the profile of the fact that cites it")
    return create_src(conn, src)


def region_label(conn: Connection, document_id: UUID, page_no: int, effective_at: datetime | None) -> str:
    """The provenance text of a document value: "LabNorte · 2026-09-14 · p1"."""
    provider = conn.execute(text("select provider_name from document where id = :d"), {"d": document_id}).scalar()
    parts = [provider or "Lab report"]
    if effective_at is not None:
        parts.append(effective_at.date().isoformat())
    return " · ".join([*parts, f"p{page_no}"])


def get_src(conn: Connection, src_id: str) -> dict | None:
    row = conn.execute(text("select * from src where id = :id"), {"id": src_id}).mappings().first()
    return dict(row) if row else None


def resolve_src(conn: Connection, src_id: str) -> dict | None:
    """The src row, its document summary, and the facts that cite it with their lifecycle.

    Returns None for an unknown id. Callers check the profile permission before returning
    anything, and answer `unknown` for a profile the caller cannot read, so ids leak nothing.
    """
    if not is_src_id(src_id):
        return None
    src = get_src(conn, src_id)
    if src is None:
        return None
    facts = []
    for table in FACT_TABLES:
        sql = f"select id, profile_id, status, status_reason from {table} where src_id = :s"
        for r in conn.execute(text(sql), {"s": src_id}).mappings():
            facts.append({"table": table, **dict(r)})
    document = None
    if src["document_id"]:
        document = (
            conn.execute(
                text(
                    "select id, profile_id, doc_type, mime, pages, provider_name, report_status, collected_at,"
                    " reported_at, encode(blob_sha256, 'hex') as sha256, status from document where id = :d"
                ),
                {"d": src["document_id"]},
            )
            .mappings()
            .first()
        )
    profile_id = src["profile_id"] or next((f["profile_id"] for f in facts), None)
    if profile_id is None and document is not None:
        profile_id = document["profile_id"]
    return {"src": src, "profile_id": profile_id, "facts": facts, "document": dict(document) if document else None}
