"""Documents (one row per stored file per profile) and the upload dispatcher.

The same bytes uploaded twice for one profile are one document with two `document_source` rows.
`dispatch_upload` sniffs a stored blob and enqueues the import job of the lane that handles it.
"""

from __future__ import annotations

import json
import zipfile
from dataclasses import dataclass
from typing import Literal
from uuid import UUID

from sqlalchemy import Connection, text

from ownhealth.blobs import BlobStore, sha256_hex_to_bytes
from ownhealth.jobs import enqueue

UploadKind = Literal[
    "pdf", "image", "apple_health_zip", "fhir_json", "fhir_ndjson", "genome_facts", "vcf", "tbi", "unknown"
]

# Job kind per upload kind. Handlers: import.pdf (lane B), import.fhir_bundle and
# import.apple_health (lane C), import.genome_facts (lane D).
JOB_KINDS: dict[str, str] = {
    "pdf": "import.pdf",
    "apple_health_zip": "import.apple_health",
    "fhir_json": "import.fhir_bundle",
    "fhir_ndjson": "import.fhir_bundle",
    "genome_facts": "import.genome_facts",
}

MIME = {
    "pdf": "application/pdf",
    "fhir_json": "application/fhir+json",
    "fhir_ndjson": "application/fhir+ndjson",
    "apple_health_zip": "application/zip",
    "genome_facts": "application/json",
    "vcf": "application/gzip",
    "tbi": "application/octet-stream",
}


class UploadError(ValueError):
    pass


def sniff(filename: str, head: bytes, blob_path=None) -> UploadKind:
    """Classify an upload from its name and first bytes (and, for zips, its member list)."""
    name = (filename or "").lower()
    if head.startswith(b"%PDF-"):
        return "pdf"
    if head.startswith((b"\x89PNG", b"\xff\xd8\xff")) or name.endswith((".heic", ".heif")):
        return "image"
    if head.startswith(b"PK\x03\x04"):
        if blob_path is not None:
            with zipfile.ZipFile(blob_path) as z:
                if any(n.endswith("export.xml") for n in z.namelist()):
                    return "apple_health_zip"
        return "unknown"
    if name.endswith(".tbi"):
        return "tbi"
    if name.endswith((".vcf.gz", ".vcf.bgz")) or head.startswith(b"\x1f\x8b"):
        return "vcf"
    stripped = head.lstrip()
    if stripped.startswith(b"{"):
        if b"ownhealth.genome-facts/" in head:
            return "genome_facts"
        if name.endswith(".ndjson"):
            return "fhir_ndjson"
        if b'"resourceType"' in head:
            return "fhir_json"
    return "unknown"


@dataclass(frozen=True)
class DocumentRef:
    id: UUID
    created: bool


def create_document(
    conn: Connection,
    *,
    household_id: UUID,
    profile_id: UUID | None,
    blob_sha256: str,
    mime: str,
    size_bytes: int,
    filename: str | None,
    channel: str = "upload",
    source_ref: str | None = None,
    pages: int | None = None,
    doc_type: str | None = None,
) -> DocumentRef:
    """Insert a document or return the existing one for the same profile and bytes."""
    sha = sha256_hex_to_bytes(blob_sha256)
    existing = conn.execute(
        text(
            "select id from document where coalesce(profile_id, household_id) = coalesce(:p, :h) and blob_sha256 = :b"
        ),
        {"p": profile_id, "h": household_id, "b": sha},
    ).scalar()
    created = existing is None
    doc_id = (
        existing
        or conn.execute(
            text(
                "insert into document (household_id, profile_id, blob_sha256, filename, mime, size_bytes, pages,"
                " doc_type) values (:h, :p, :b, :f, :m, :s, :pg, :dt) returning id"
            ),
            {
                "h": household_id,
                "p": profile_id,
                "b": sha,
                "f": (filename or "")[:255] or None,
                "m": mime,
                "s": size_bytes,
                "pg": pages,
                "dt": doc_type,
            },
        ).scalar_one()
    )
    conn.execute(
        text("insert into document_source (document_id, channel, source_ref) values (:d, :c, :r)"),
        {"d": doc_id, "c": channel, "r": source_ref},
    )
    return DocumentRef(doc_id, created)


@dataclass(frozen=True)
class StoredFile:
    filename: str
    sha256: str
    size: int


def dispatch_upload(
    conn: Connection,
    blobs: BlobStore,
    *,
    household_id: UUID,
    profile_id: UUID,
    files: list[StoredFile],
    actor_user: UUID | None = None,
) -> dict:
    """Create documents and enqueue import jobs for files already in the blob store.

    Returns {"items": [{filename, kind, document_id?, job_id?}]}. A genome-facts file takes the
    .vcf.gz and .tbi uploaded with it; those alone are refused.
    """
    kinds: list[tuple[StoredFile, UploadKind]] = []
    for f in files:
        path = blobs.path(f.sha256)
        with open(path, "rb") as fh:
            head = fh.read(4096)
        kinds.append((f, sniff(f.filename, head, path)))
    has_facts = any(k == "genome_facts" for _, k in kinds)
    items = []
    for f, kind in kinds:
        item: dict = {"filename": f.filename, "kind": kind, "sha256": f.sha256}
        if kind == "unknown":
            raise UploadError(f"{f.filename}: not a PDF, image, Apple Health export, FHIR bundle or genome facts file")
        if kind in ("vcf", "tbi"):
            if not has_facts:
                raise UploadError(f"{f.filename}: upload it together with its genome-facts.json")
            item["attached_to"] = "genome_facts"
            items.append(item)
            continue
        if kind in ("pdf", "image"):
            mime = MIME.get(kind) or ("image/png" if f.filename.lower().endswith(".png") else "image/jpeg")
            ref = create_document(
                conn,
                household_id=household_id,
                profile_id=profile_id,
                blob_sha256=f.sha256,
                mime=mime,
                size_bytes=f.size,
                filename=f.filename,
            )
            item["document_id"] = str(ref.id)
            item["duplicate"] = not ref.created
            if ref.created:  # images too: the PDF handler files them as pending_document items
                item["job_id"] = enqueue(
                    conn,
                    JOB_KINDS["pdf"],
                    {"document_id": str(ref.id), "profile_id": str(profile_id), "household_id": str(household_id)},
                    dedupe_key=f"import.pdf:{ref.id}",
                )
            items.append(item)
            continue
        payload: dict = {
            "profile_id": str(profile_id),
            "household_id": str(household_id),
            "blob_sha256": f.sha256,
            "filename": f.filename,
        }
        if kind == "fhir_ndjson":
            payload["format"] = "ndjson"
        if kind == "genome_facts":
            payload["files"] = {g.filename: g.sha256 for g, k in kinds if k in ("vcf", "tbi")}
        item["job_id"] = enqueue(
            conn, JOB_KINDS[kind], payload, dedupe_key=f"{JOB_KINDS[kind]}:{profile_id}:{f.sha256}"
        )
        items.append(item)
    return {"items": json.loads(json.dumps(items, default=str))}
