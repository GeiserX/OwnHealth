"""Document vault (lane B): list and search documents, the original file, page renders and word
layers for the review split view, and the two corrections a person makes: assign the document to
another profile, or retract it.

Lab reports need `read:labs`, other documents `read:records`; page text and words also need
`read:documents_text`. A document on a profile the caller cannot read answers 404 like an unknown id.
Every read writes one audit row. Filenames and page text are untrusted and go out marked as such.
"""

from __future__ import annotations

from uuid import UUID

from fastapi import APIRouter, Depends, HTTPException, Query, Response
from pydantic import BaseModel
from sqlalchemy import Connection, text

from ownhealth.api._deps import get_blobs, get_conn, get_principal
from ownhealth.blobs import BlobStore
from ownhealth.core.access import AccessDenied, Permission, Principal, effective_permission, require
from ownhealth.core.audit import AuditEvent, write_audit
from ownhealth.core.observations import retract_observation
from ownhealth.core.review import dismiss
from ownhealth.ingest.pdf import assign_document, page_words, render_page_png

router = APIRouter(tags=["documents"])

_COLS = (
    "d.id, d.profile_id, d.filename, d.doc_type, d.provider_name, d.collected_at, d.pages, d.status, d.flags,"
    " d.mime, d.size_bytes, d.blob_sha256, d.created_at,"
    " (select count(*) from review_item r where r.document_id = d.id and r.status = 'open') as open_review"
)


_FTS_CFG = (
    "cast(case d.language when 'es' then 'spanish' when 'en' then 'english' when 'de' then 'german'"
    " when 'fr' then 'french' else 'simple' end as regconfig)"
)


def _untrusted(s: str | None, origin: str) -> dict | None:
    return {"trust": "untrusted_text", "origin": origin, "text": s} if s else None


def _scope_for(doc_type: str | None) -> str:
    return "read:labs" if doc_type == "lab_report" else "read:records"


def _summary(row, snippet: str | None = None) -> dict:
    return {
        "id": row["id"],
        "profile_id": row["profile_id"],
        "filename": _untrusted(row["filename"], "filename"),
        "doc_type": row["doc_type"],
        "provider_name": row["provider_name"],
        "collected_at": row["collected_at"],
        "pages": row["pages"],
        "status": row["status"],
        "flags": {k: v for k, v in (row["flags"] or {}).items() if k != "metadata_text"},
        "open_review": row["open_review"],
        "snippet": _untrusted(snippet, "pdf_text_layer"),
    }


def _audit(conn, principal: Principal, profile_id, tool: str, ids: list[str], action: str = "read") -> None:
    write_audit(
        conn,
        AuditEvent(
            actor_kind=principal.actor_kind,
            actor_id=principal.user_id,
            token_id=principal.token_id,
            via="ui" if principal.token_id is None else "api",
            action=action,
            tool=tool,
            profile_id=profile_id,
            resource_type="document",
            resource_ids=ids,
            rows_out=len(ids),
        ),
    )


def _doc(conn: Connection, principal: Principal, document_id: UUID, *, scope_extra: str | None = None):
    """The document row and the caller's permission on its profile; 404 when it is not visible."""
    row = conn.execute(text(f"select {_COLS} from document d where d.id = :d"), {"d": document_id}).mappings().first()
    unknown = HTTPException(404, detail={"code": "unknown", "message": "Unknown document."})
    if row is None or row["profile_id"] is None:
        raise unknown
    try:
        perm = effective_permission(conn, principal, row["profile_id"])
    except AccessDenied:
        raise unknown from None
    for scope in (_scope_for(row["doc_type"]), scope_extra):
        if scope and scope not in perm.scopes:
            raise AccessDenied("insufficient_scope", f"This needs {scope}.")
    return row, perm


@router.get("/profiles/{p}/documents")
def list_documents(
    p: str,
    q: str | None = Query(None, max_length=200),
    type: str | None = Query(None, alias="type"),
    limit: int = Query(100, ge=1, le=500),
    conn: Connection = Depends(get_conn),
    principal: Principal = Depends(get_principal),
) -> dict:
    perm: Permission = require(conn, principal, p)
    types_ok = []
    if "read:labs" in perm.scopes:
        types_ok.append("lab")
    if "read:records" in perm.scopes:
        types_ok.append("other")
    if not types_ok:
        raise AccessDenied("insufficient_scope", "This needs read:labs or read:records.")
    where = ["d.profile_id = :p"]
    params: dict = {"p": perm.profile.id, "limit": limit}
    if types_ok == ["lab"]:
        where.append("d.doc_type = 'lab_report'")
    elif types_ok == ["other"]:
        where.append("d.doc_type is distinct from 'lab_report'")
    if type:
        where.append("d.doc_type = :t")
        params["t"] = type
    with_text = bool(q) and "read:documents_text" in perm.scopes
    if q:
        params["q"] = q
        # page text is searchable only with read:documents_text; the filename always is
        where.append(
            f"(d.filename ilike '%' || :q || '%' or exists (select 1 from document_page pg where pg.document_id = d.id"
            f" and pg.fts @@ websearch_to_tsquery({_FTS_CFG}, :q)))"
            if with_text
            else "d.filename ilike '%' || :q || '%'"
        )
    snippet = (
        f", (select ts_headline({_FTS_CFG}, pg.text_content, websearch_to_tsquery({_FTS_CFG}, :q),"
        " 'MaxWords=18, MinWords=6') from document_page pg where pg.document_id = d.id order by pg.page_no limit 1)"
        " as snippet"
        if with_text
        else ""
    )
    rows = (
        conn.execute(
            text(
                f"select {_COLS}{snippet} from document d where {' and '.join(where)}"
                " order by d.collected_at desc nulls last, d.created_at desc limit :limit"
            ),
            params,
        )
        .mappings()
        .all()
    )
    _audit(
        conn,
        principal,
        perm.profile.id,
        "GET /api/profiles/{p}/documents",
        [str(r["id"]) for r in rows],
        action="search" if q else "read",
    )
    return {"items": [_summary(r, r.get("snippet") if with_text else None) for r in rows]}


@router.get("/documents/{document_id}")
def get_document(
    document_id: UUID, conn: Connection = Depends(get_conn), principal: Principal = Depends(get_principal)
) -> dict:
    row, perm = _doc(conn, principal, document_id)
    pages = (
        conn.execute(
            text(
                "select page_no, width_pt, height_pt, text_layer_ok, flags from document_page where document_id = :d"
                " order by page_no"
            ),
            {"d": document_id},
        )
        .mappings()
        .all()
    )
    expected = (
        conn.execute(
            text("select printed_name, alias_group, reason from expected_test where report_document_id = :d"),
            {"d": document_id},
        )
        .mappings()
        .all()
    )
    _audit(conn, principal, perm.profile.id, "GET /api/documents/{id}", [str(document_id)])
    return {
        **_summary(row),
        "mime": row["mime"],
        "size_bytes": row["size_bytes"],
        "pages_info": [dict(p) for p in pages],
        "expected_tests": [
            {
                "printed_name": _untrusted(e["printed_name"], "pdf_text_layer"),
                "alias_group": e["alias_group"],
                "reason": _untrusted(e["reason"], "pdf_text_layer"),
            }
            for e in expected
        ],
    }


_FILE_HEADERS = {"X-Content-Type-Options": "nosniff", "Content-Security-Policy": "sandbox", "Cache-Control": "private"}


@router.get("/documents/{document_id}/file")
def document_file(
    document_id: UUID,
    conn: Connection = Depends(get_conn),
    principal: Principal = Depends(get_principal),
    blobs: BlobStore = Depends(get_blobs),
) -> Response:
    row, perm = _doc(conn, principal, document_id)
    _audit(conn, principal, perm.profile.id, "GET /api/documents/{id}/file", [str(document_id)])
    return Response(
        blobs.get_bytes(bytes(row["blob_sha256"]).hex()),
        media_type=row["mime"],
        headers={**_FILE_HEADERS, "Content-Disposition": "inline"},
    )


@router.get("/documents/{document_id}/pages/{page_no}.png")
def page_png(
    document_id: UUID,
    page_no: int,
    scale: float = Query(2.0, ge=0.5, le=4.0),
    conn: Connection = Depends(get_conn),
    principal: Principal = Depends(get_principal),
    blobs: BlobStore = Depends(get_blobs),
) -> Response:
    row, perm = _doc(conn, principal, document_id)
    if row["mime"] != "application/pdf" or not 1 <= page_no <= (row["pages"] or 0):
        raise HTTPException(404, detail={"code": "unknown", "message": "No such page."})
    png = render_page_png(blobs, blob_sha256=bytes(row["blob_sha256"]).hex(), page_no=page_no, scale=scale)
    _audit(conn, principal, perm.profile.id, "GET /api/documents/{id}/pages/{n}.png", [str(document_id)])
    return Response(png, media_type="image/png", headers=_FILE_HEADERS)


@router.get("/documents/{document_id}/pages/{page_no}/words")
def page_word_layer(
    document_id: UUID,
    page_no: int,
    conn: Connection = Depends(get_conn),
    principal: Principal = Depends(get_principal),
    blobs: BlobStore = Depends(get_blobs),
) -> dict:
    row, perm = _doc(conn, principal, document_id, scope_extra="read:documents_text")
    page = (
        conn.execute(
            text("select width_pt, height_pt, flags from document_page where document_id = :d and page_no = :n"),
            {"d": document_id, "n": page_no},
        )
        .mappings()
        .first()
    )
    if page is None:
        raise HTTPException(404, detail={"code": "unknown", "message": "No such page."})
    words = page_words(conn, blobs, document_id=document_id, page_no=page_no)
    _audit(conn, principal, perm.profile.id, "GET /api/documents/{id}/pages/{n}/words", [str(document_id)])
    return {
        "trust": "untrusted_text",
        "origin": "pdf_text_layer",
        "page_no": page_no,
        "width_pt": page["width_pt"],
        "height_pt": page["height_pt"],
        "flags": page["flags"],
        "words": [{"text": w.text, "bbox": list(w.bbox), "bold": w.bold, "hidden": w.hidden} for w in words],
    }


class AssignIn(BaseModel):
    profile: str


@router.post("/documents/{document_id}/assign")
def assign(
    document_id: UUID,
    body: AssignIn,
    conn: Connection = Depends(get_conn),
    principal: Principal = Depends(get_principal),
) -> dict:
    row, perm = _doc(conn, principal, document_id)
    if not perm.can("import"):
        raise AccessDenied("forbidden", f"The role {perm.role} cannot import on this profile.")
    target = require(conn, principal, body.profile, action="import")
    try:
        job_id = assign_document(conn, document_id, target.profile.id, user_id=principal.user_id)
    except ValueError as exc:
        raise HTTPException(409, detail={"code": "conflict", "message": str(exc)}) from None
    _audit(conn, principal, target.profile.id, "POST /api/documents/{id}/assign", [str(document_id)], action="import")
    return {"document_id": document_id, "profile": target.profile.slug, "job_id": job_id}


class RetractIn(BaseModel):
    reason: str


@router.post("/documents/{document_id}/retract")
def retract(
    document_id: UUID,
    body: RetractIn,
    conn: Connection = Depends(get_conn),
    principal: Principal = Depends(get_principal),
) -> dict:
    """Soft delete with a reason: the document leaves the vault, its values are retracted, its open
    review items dismissed. The blob stays until an explicit purge."""
    row, perm = _doc(conn, principal, document_id)
    if not perm.can("write"):
        raise AccessDenied("forbidden", f"The role {perm.role} cannot change this profile.")
    reason = body.reason.strip()
    if not reason:
        raise HTTPException(422, detail={"code": "bad_input", "message": "A retraction needs a reason."})
    if row["status"] == "retracted":
        raise HTTPException(409, detail={"code": "conflict", "message": "The document is already retracted."})
    retracted = []
    for obs_id in conn.execute(
        text("select id from observation where document_id = :d and status in ('current', 'provisional')"),
        {"d": document_id},
    ).scalars():
        retract_observation(conn, obs_id, f"document retracted: {reason}")
        retracted.append(obs_id)
    for item_id in conn.execute(
        text("select id from review_item where document_id = :d and status = 'open'"), {"d": document_id}
    ).scalars():
        dismiss(conn, item_id, principal.user_id, "document retracted")
    conn.execute(
        text("update document set status = 'retracted', status_reason = :r where id = :d"),
        {"r": reason, "d": document_id},
    )
    _audit(conn, principal, perm.profile.id, "POST /api/documents/{id}/retract", [str(document_id)], action="delete")
    return {"document_id": document_id, "status": "retracted", "retracted_observations": retracted}
