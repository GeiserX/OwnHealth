"""Job `import.fhir_bundle` (lane C): store the uploaded bundle as a document, then import it."""

from __future__ import annotations

from uuid import UUID

from ownhealth.core.documents import create_document
from ownhealth.ingest.fhir import import_fhir
from ownhealth.jobs import Job, JobContext, PermanentJobError, handler


def document_for(conn, blobs, payload: dict, *, mime: str, channel: str) -> UUID:
    """The document a structured-import job works on: given in the payload, or made from its blob."""
    if payload.get("document_id"):
        return UUID(payload["document_id"])
    sha = payload["blob_sha256"]
    return create_document(
        conn,
        household_id=UUID(payload["household_id"]),
        profile_id=UUID(payload["profile_id"]),
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
