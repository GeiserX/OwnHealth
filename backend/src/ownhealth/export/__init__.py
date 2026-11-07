"""Lane G: export. A UI action with a human click and step-up auth; never an MCP tool (mcp.md 9.2).
Contract stubs."""

from __future__ import annotations

from typing import Literal
from uuid import UUID

from sqlalchemy import Connection

from ownhealth.blobs import BlobStore

ExportFormat = Literal["fhir_bundle", "ips", "zip"]


def export_profile(conn: Connection, blobs: BlobStore, profile_id: UUID, *, fmt: ExportFormat) -> str:
    """Builds the export, stores it as a blob and returns its sha256."""
    raise NotImplementedError("lane G: export.export_profile")
