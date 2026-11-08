"""Job `import.apple_health` (lane C): store the export.zip as a document, then import it."""

from __future__ import annotations

import zipfile

from ownhealth.ingest.apple_health import import_apple_health
from ownhealth.jobs import Job, JobContext, PermanentJobError, handler
from ownhealth.jobs.kinds.fhir import document_for


@handler("import.apple_health")
def run(ctx: JobContext, job: Job) -> dict:
    with ctx.engine.begin() as conn:
        doc = document_for(conn, ctx.blobs, job.payload, mime="application/zip", channel="apple_health")
        try:
            return import_apple_health(conn, ctx.blobs, document_id=doc).as_dict()
        except (zipfile.BadZipFile, SyntaxError, ValueError, KeyError) as exc:  # ParseError is a SyntaxError
            raise PermanentJobError(f"not a readable Apple Health export: {exc}") from exc
