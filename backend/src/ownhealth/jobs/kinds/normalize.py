"""Job `normalize.rederive` (lane A): re-derive one profile's observations, or every profile's, with the
tables in force. Rows whose derived layer changed are superseded; the printed layer never changes."""

from __future__ import annotations

from ownhealth import normalize
from ownhealth.jobs import Job, JobContext, handler


@handler("normalize.rederive")
def rederive(ctx: JobContext, job: Job) -> dict:
    with ctx.engine.begin() as conn:
        return {"changed": normalize.rederive(conn, profile_id=job.payload.get("profile_id"))}
