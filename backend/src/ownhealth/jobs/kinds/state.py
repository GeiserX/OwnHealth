"""Job `state.rebuild` (lane E): rebuild the current state of one profile, or of every profile when the
payload names none, as of today. A rebuild with an unchanged hash writes nothing new."""

from __future__ import annotations

from datetime import date
from uuid import UUID

from sqlalchemy import text

from ownhealth import state
from ownhealth.core.access import today_utc
from ownhealth.jobs import Job, JobContext, handler


@handler("state.rebuild")
def rebuild(ctx: JobContext, job: Job) -> dict:
    as_of = date.fromisoformat(job.payload["as_of"]) if job.payload.get("as_of") else today_utc()
    with ctx.engine.begin() as conn:
        if job.payload.get("profile_id"):
            ids = [UUID(str(job.payload["profile_id"]))]
        else:
            ids = list(conn.execute(text("select id from profile where not deceased order by id")).scalars())
        hashes = {str(pid): state.build_state(conn, pid, as_of=as_of)["state_hash"] for pid in ids}
    return {"as_of": as_of.isoformat(), "profiles": len(hashes), "hashes": hashes}
