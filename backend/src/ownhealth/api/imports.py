"""Uploads (foundation): PDF, image, Apple Health export.zip, FHIR bundle, OwnGenome output.

Files stream into the blob store; the dispatcher creates documents and enqueues the import job
of the lane that handles each kind. Progress is the job state.
"""

from __future__ import annotations

from fastapi import APIRouter, Depends, File, Form, HTTPException, Query, UploadFile
from sqlalchemy import Connection, text

from ownhealth.api._deps import get_blobs, get_conn, get_principal
from ownhealth.blobs import BlobStore
from ownhealth.core.access import Principal, accessible_profiles, require
from ownhealth.core.audit import AuditEvent, write_audit
from ownhealth.core.documents import StoredFile, UploadError, dispatch_upload

router = APIRouter(tags=["imports"])


@router.post("/imports")
def upload(
    profile: str = Form(...),
    files: list[UploadFile] = File(...),
    conn: Connection = Depends(get_conn),
    principal: Principal = Depends(get_principal),
    blobs: BlobStore = Depends(get_blobs),
) -> dict:
    perm = require(conn, principal, profile, action="import")
    stored = []
    for f in files:
        sha, size = blobs.put_stream(f.file)
        stored.append(StoredFile(filename=f.filename or "upload", sha256=sha, size=size))
    try:
        result = dispatch_upload(
            conn,
            blobs,
            household_id=perm.profile.household_id,
            profile_id=perm.profile.id,
            files=stored,
            actor_user=principal.user_id,
        )
    except UploadError as exc:
        raise HTTPException(415, detail={"code": "unsupported_file", "message": str(exc)}) from None
    write_audit(
        conn,
        AuditEvent(
            actor_kind=principal.actor_kind,
            actor_id=principal.user_id,
            via="ui",
            action="import",
            tool="POST /api/imports",
            profile_id=perm.profile.id,
            resource_type="upload",
            resource_ids=[i["sha256"] for i in result["items"]],
        ),
    )
    return result


@router.get("/jobs")
def jobs(
    ids: str | None = None,
    limit: int = Query(50, ge=1, le=200),
    conn: Connection = Depends(get_conn),
    principal: Principal = Depends(get_principal),
) -> dict:
    """Import jobs of the caller's profiles, newest first; `ids=1,2,3` to poll specific ones."""
    pids = [str(p.profile.id) for p in accessible_profiles(conn, principal)]
    params: dict = {"pids": pids, "limit": limit}
    where = "payload->>'profile_id' = any(:pids)"
    if ids:
        where += " and id = any(:ids)"
        params["ids"] = [int(i) for i in ids.split(",") if i.strip().isdigit()]
    rows = conn.execute(
        text(
            f"select id, kind, state, attempts, max_attempts, last_error, result, created_at, finished_at,"
            f" payload->>'profile_id' as profile_id, payload->>'filename' as filename,"
            f" payload->>'document_id' as document_id from job where {where} order by id desc limit :limit"
        ),
        params,
    ).mappings()
    return {"jobs": [dict(r) for r in rows]}
