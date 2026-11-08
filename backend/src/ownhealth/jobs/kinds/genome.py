"""Job `import.genome_facts` (lane D): a genome-facts.json uploaded with its .vcf.gz and .tbi.

Payload (core.documents.dispatch_upload): profile_id, household_id, blob_sha256, filename and
files {filename: sha256} of the raw files uploaded with it. The document row and every fact are
written in one transaction; a bad file rolls all of it back and becomes a job_failed review item
with the reason, so nothing is half-imported.
"""

from __future__ import annotations

from uuid import UUID

from ownhealth.core.documents import create_document
from ownhealth.genome import GenomeImportError, import_genome_facts
from ownhealth.jobs import Job, JobContext, PermanentJobError, handler


@handler("import.genome_facts")
def import_genome_facts_job(ctx: JobContext, job: Job) -> dict:
    p = job.payload
    sha = p["blob_sha256"]
    if not ctx.blobs.exists(sha):
        raise PermanentJobError(f"{p.get('filename')}: the uploaded file is not in the blob store")
    try:
        with ctx.engine.begin() as conn:
            ref = create_document(
                conn,
                household_id=UUID(p["household_id"]),
                profile_id=UUID(p["profile_id"]),
                blob_sha256=sha,
                mime="application/json",
                size_bytes=ctx.blobs.path(sha).stat().st_size,
                filename=p.get("filename"),
                doc_type="genomics",
            )
            summary = import_genome_facts(conn, ctx.blobs, document_id=ref.id, attachments=p.get("files") or {})
    except GenomeImportError as exc:
        raise PermanentJobError(str(exc)) from exc
    return summary.as_dict()
