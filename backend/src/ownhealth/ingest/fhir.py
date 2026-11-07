"""Lane C: FHIR R4 Bundle / IPS / NDJSON import (ingestion.md 3.3, architecture.md 9). Job kind
`import.fhir_bundle`. Codes arrive as received; provenance is `fhir_import` with an import_ref of
`<Bundle.id>/<resourceType>/<id>`. Contract stub until lane C lands it."""

from __future__ import annotations

from uuid import UUID

from sqlalchemy import Connection

from ownhealth.blobs import BlobStore
from ownhealth.ingest import ImportSummary


def import_fhir(conn: Connection, blobs: BlobStore, *, document_id: UUID) -> ImportSummary:
    raise NotImplementedError("lane C: ingest.fhir.import_fhir")
