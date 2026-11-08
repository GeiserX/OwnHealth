"""Job `import.fhir_bundle` (lane C): store the uploaded bundle as a document, then import it."""

from __future__ import annotations

from uuid import UUID

from ownhealth.core.documents import create_document
from ownhealth.ingest.fhir import import_fhir
from ownhealth.jobs import Job, JobContext, PermanentJobError, handler


def document_for(conn, blobs, payload: dict, *, mime: str, channel: str) -> UUID:
    """The document a structured-import job works on: given in the payload, or made from its blob."""
    # A malformed payload or a blob that is not stored cannot heal on a retry: fail it for good.
    try:
        if payload.get("document_id"):
            return UUID(str(payload["document_id"]))
        sha = payload.get("blob_sha256")
        if not sha or not blobs.exists(sha):
            raise PermanentJobError(f"{payload.get('filename')}: the uploaded file is not in the blob store")
        household_id, profile_id = UUID(str(payload["household_id"])), UUID(str(payload["profile_id"]))
    except (KeyError, ValueError) as exc:
        raise PermanentJobError(f"bad payload: {exc}") from None
    return create_document(
        conn,
        household_id=household_id,
        profile_id=profile_id,
        blob_sha256=sha,
        mime=mime,
        size_bytes=blobs.path(sha).stat().st_size,
        filename=payload.get("filename"),
        channel=channel,
        doc_type="other",
    ).id


@handler("import.fhir_bundle")
def run(ctx: JobContext, job: Job) -> dict:
    mime = "application/fhir+ndjson" if job.payload.get("format") == "ndjson" else "application/fhir+json"
    with ctx.engine.begin() as conn:
        doc = document_for(conn, ctx.blobs, job.payload, mime=mime, channel="fhir")
        try:
            return import_fhir(conn, ctx.blobs, document_id=doc).as_dict()
        except (ValueError, KeyError, TypeError) as exc:  # malformed JSON or resource: a retry cannot help
            raise PermanentJobError(f"not a readable FHIR file: {exc}") from exc
