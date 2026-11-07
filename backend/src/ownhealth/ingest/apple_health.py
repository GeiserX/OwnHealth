"""Lane C: Apple Health export.zip import (connectors.md 4). Job kind `import.apple_health`.
Streams export.xml; quantity samples go to wearable_sample and wearable_rollup, clinical records
(FHIR JSON inside the zip) go through `ingest.fhir`. Contract stub until lane C lands it."""

from __future__ import annotations

from uuid import UUID

from sqlalchemy import Connection

from ownhealth.blobs import BlobStore
from ownhealth.ingest import ImportSummary


def import_apple_health(conn: Connection, blobs: BlobStore, *, document_id: UUID) -> ImportSummary:
    raise NotImplementedError("lane C: ingest.apple_health.import_apple_health")


def rebuild_rollups(conn: Connection, *, profile_id: UUID, metric: str | None = None) -> int:
    raise NotImplementedError("lane C: ingest.apple_health.rebuild_rollups")
