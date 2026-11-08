"""Job `state.rebuild` (lane E): rebuild the current state of one profile, or of every profile when the
payload names none, as of today. A rebuild with an unchanged hash writes nothing new."""

from __future__ import annotations

from datetime import date
from uuid import UUID

from sqlalchemy import text

from ownhealth import state
from ownhealth.core.access import today_utc
from ownhealth.jobs import Job, JobContext, PermanentJobError, handler


@handler("state.rebuild")
def rebuild(ctx: JobContext, job: Job) -> dict:
    try:
        as_of = date.fromisoformat(str(job.payload["as_of"])) if job.payload.get("as_of") else today_utc()
        pid = UUID(str(job.payload["profile_id"])) if job.payload.get("profile_id") else None
    except ValueError as exc:  # a retry cannot fix a malformed payload
        raise PermanentJobError(f"bad payload: {exc}") from None
    with ctx.engine.begin() as conn:
        if pid is not None:
            ids = [pid]
        else:
            ids = list(conn.execute(text("select id from profile where not deceased order by id")).scalars())
        hashes = {str(pid): state.build_state(conn, pid, as_of=as_of)["state_hash"] for pid in ids}
    return {"as_of": as_of.isoformat(), "profiles": len(hashes), "hashes": hashes}
