"""Job kind `import.pdf` (lane B): one stored PDF or image -> pages, guesses, candidate rows.

Payload: {document_id, profile_id, household_id, force?}. The whole import runs in one transaction,
so a crash leaves nothing behind and the retry starts clean; a document already imported is skipped
unless `force` is set (re-import after the document moved to another profile).
"""

from __future__ import annotations

from uuid import UUID

from ownhealth.ingest.pdf import DocumentNotFound, import_pdf
from ownhealth.jobs import Job, JobContext, PermanentJobError, handler


@handler("import.pdf")
def run(ctx: JobContext, job: Job) -> dict:
    try:
        document_id = UUID(str(job.payload["document_id"]))
    except (KeyError, ValueError) as exc:
        raise PermanentJobError(f"bad payload: {exc}") from None
    with ctx.engine.begin() as conn:
        try:
            summary = import_pdf(
                conn,
                ctx.blobs,
                document_id=document_id,
                force=bool(job.payload.get("force")),
                threshold=ctx.settings.auto_accept_threshold,
            )
        except DocumentNotFound as exc:
            raise PermanentJobError(str(exc)) from None
    return summary.as_dict()
