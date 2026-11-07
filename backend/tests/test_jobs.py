from concurrent.futures import ThreadPoolExecutor

import pytest
from sqlalchemy import text

from ownhealth import jobs
from ownhealth.jobs import Job, JobContext, PermanentJobError, claim, enqueue, handler, run_one


@pytest.fixture
def ctx(engine, settings, blobs):
    return JobContext(engine, settings, blobs, worker_id="test-worker")


calls: list[dict] = []


@handler("test.ok")
def _ok(ctx: JobContext, job: Job) -> dict:
    calls.append(job.payload)
    return {"seen": job.payload["n"]}


@handler("test.flaky")
def _flaky(ctx: JobContext, job: Job) -> None:
    raise RuntimeError("temporary")


@handler("test.bad_input")
def _bad(ctx: JobContext, job: Job) -> None:
    raise PermanentJobError("unreadable file")


def _state(engine, job_id):
    with engine.connect() as c:
        return (
            c.execute(
                text("select state, attempts, last_error, result, run_after > now() as later from job where id = :i"),
                {"i": job_id},
            )
            .mappings()
            .one()
        )


def test_enqueue_dedupes_while_queued(engine):
    with engine.begin() as c:
        a = enqueue(c, "test.ok", {"n": 1}, dedupe_key="state.rebuild:x")
        b = enqueue(c, "test.ok", {"n": 2}, dedupe_key="state.rebuild:x")
    assert a is not None and b is None
    with engine.begin() as c:
        c.execute(text("update job set state = 'done' where id = :i"), {"i": a})
        assert enqueue(c, "test.ok", {"n": 3}, dedupe_key="state.rebuild:x") is not None
        c.execute(text("update job set state = 'done' where kind = 'test.ok'"))


def test_run_success_and_retry_with_backoff(engine, ctx):
    with engine.begin() as c:
        ok = enqueue(c, "test.ok", {"n": 7})
        flaky = enqueue(c, "test.flaky", {"n": 0}, max_attempts=2)
    assert run_one(ctx, kinds=["test.ok"]).id == ok
    s = _state(engine, ok)
    assert s["state"] == "done" and s["result"] == {"seen": 7}
    assert run_one(ctx, kinds=["test.flaky"]).id == flaky
    s = _state(engine, flaky)
    assert s["state"] == "queued" and s["attempts"] == 1 and s["later"] and "temporary" in s["last_error"]
    assert run_one(ctx, kinds=["test.flaky"]) is None  # backoff: not ready yet


def test_dead_job_creates_review_item(engine, ctx, committed_family):
    with engine.begin() as c:
        jid = enqueue(c, "test.bad_input", {"profile_id": str(committed_family.profiles["alex"])})
    run_one(ctx, kinds=["test.bad_input"])
    assert _state(engine, jid)["state"] == "dead"
    with engine.connect() as c:
        item = (
            c.execute(
                text("select kind, household_id, payload from review_item where payload->>'job_id' = :j"),
                {"j": str(jid)},
            )
            .mappings()
            .one()
        )
    assert item["kind"] == "job_failed" and item["household_id"] == committed_family.household_id


def test_skip_locked_gives_each_worker_its_own_job(engine):
    with engine.begin() as c:
        ids = {enqueue(c, "test.claim", {"n": i}) for i in range(2)}
    with engine.connect() as holder:
        holder.begin()
        first = holder.execute(
            text(
                "select id from job where kind = 'test.claim' and state = 'queued' order by id for update skip locked"
                " limit 1"
            )
        ).scalar_one()
        # Must skip the locked row, not wait for it: run in a thread so a regression fails instead of hanging.
        with ThreadPoolExecutor(1) as pool:
            fut = pool.submit(claim, engine, "core", "w2", ["test.claim"])
            try:
                other = fut.result(timeout=10)
            finally:
                holder.rollback()
        assert other is not None and other.id != first and {first, other.id} == ids


def test_unknown_kind_is_dead_not_silent(engine, ctx):
    with engine.begin() as c:
        jid = enqueue(c, "test.nobody_handles_this", {})
    run_one(ctx, kinds=["test.nobody_handles_this"])
    s = _state(engine, jid)
    assert s["state"] == "dead" and "no handler" in s["last_error"]


def test_reaper_requeues_expired_leases(engine):
    with engine.begin() as c:
        jid = enqueue(c, "test.lease", {})
        c.execute(
            text("update job set state = 'running', locked_until = now() - interval '1 minute' where id = :i"),
            {"i": jid},
        )
    assert jobs.reap_expired(engine) >= 1
    assert _state(engine, jid)["state"] == "queued"
