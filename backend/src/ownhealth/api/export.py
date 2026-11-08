"""Full export, from the UI only (never MCP): FHIR R4 NDJSON plus the original files in one zip.

Needs step-up and the right to manage the profile (owner, or guardian of an unclaimed profile).
The zip is built in a temporary file under the data directory and streamed; the file is deleted
once sent, so no copy of the export stays on the server.
"""

from __future__ import annotations

import os
import tempfile
from datetime import date
from typing import Literal

from fastapi import APIRouter, Depends, Request
from fastapi.responses import FileResponse
from pydantic import BaseModel
from sqlalchemy import Connection
from starlette.background import BackgroundTask

from ownhealth.api._deps import get_blobs, get_conn, get_settings
from ownhealth.auth.deps import StepUp, client_ip
from ownhealth.auth.household import require_manage
from ownhealth.blobs import BlobStore
from ownhealth.config import Settings
from ownhealth.core.access import Principal, require
from ownhealth.core.audit import AuditEvent, write_audit
from ownhealth.export import export_profile

router = APIRouter(tags=["export"])


class ExportIn(BaseModel):
    fmt: Literal["zip"] = "zip"


@router.post("/profiles/{p}/export")
def export(
    p: str,
    body: ExportIn,
    request: Request,
    conn: Connection = Depends(get_conn),
    principal: Principal = StepUp,
    blobs: BlobStore = Depends(get_blobs),
    settings: Settings = Depends(get_settings),
):
    perm = require_manage(require(conn, principal, p))
    tmp_dir = settings.data_dir / "tmp"
    tmp_dir.mkdir(parents=True, exist_ok=True)
    fd, path = tempfile.mkstemp(prefix="export-", suffix=".zip", dir=tmp_dir)
    try:
        with os.fdopen(fd, "wb") as out:
            manifest = export_profile(conn, blobs, perm.profile.id, out, fmt=body.fmt)
    except BaseException:
        os.unlink(path)
        raise
    size = os.path.getsize(path)
    write_audit(
        conn,
        AuditEvent(
            actor_kind="user",
            actor_id=principal.user_id,
            via="export",
            action="export",
            tool="POST /api/profiles/export",
            profile_id=perm.profile.id,
            resource_type="profile",
            resource_ids=[str(perm.profile.id)],
            rows_out=sum(v for v in manifest["counts"].values()),
            bytes_out=size,
            args_summary=f"fmt={body.fmt};files={len(manifest['files'])}",
            ip=client_ip(request),
        ),
    )
    return FileResponse(
        path,
        media_type="application/zip",
        filename=f"ownhealth-{perm.profile.slug}-{date.today().isoformat()}.zip",
        background=BackgroundTask(os.unlink, path),
        headers={"Cache-Control": "no-store"},
    )
