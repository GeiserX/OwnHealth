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


@pytest.mark.parametrize("doc", ["00000000-0000-0000-0000-000000000000", "not-a-uuid"])
def test_dead_job_naming_a_missing_document_still_files_its_item(engine, ctx, committed_family, doc):
    pid = str(committed_family.profiles["alex"])
    with engine.begin() as c:
        jid = enqueue(c, "test.bad_input", {"profile_id": pid, "document_id": doc})
    run_one(ctx, kinds=["test.bad_input"])
    assert _state(engine, jid)["state"] == "dead"
    with engine.connect() as c:
        item = (
            c.execute(
                text("select document_id, payload from review_item where payload->>'job_id' = :j"), {"j": str(jid)}
            )
            .mappings()
            .one()
        )
    assert item["document_id"] is None and item["payload"]["payload_document_id"] == doc


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


def _expire(engine, jid):
    with engine.begin() as c:
        c.execute(text("update job set locked_until = now() - interval '1 second' where id = :i"), {"i": jid})


def test_late_worker_cannot_finish_a_job_another_worker_holds(engine):
    with engine.begin() as c:
        jid = enqueue(c, "test.lost_lease", {})
    first = claim(engine, "core", "w0", ["test.lost_lease"])
    _expire(engine, jid)
    assert jobs.reap_expired(engine) == 1
    with engine.begin() as c:  # skip the backoff the reaper set
        c.execute(text("update job set run_after = now() where id = :i"), {"i": jid})
    second = claim(engine, "core", "w1", ["test.lost_lease"])
    assert second.id == first.id == jid
    assert jobs.complete(engine, first, "w0", {"from": "w0"}) is False
    assert jobs.fail(engine, first, RuntimeError("late"), "w0") == "lost"
    with engine.connect() as c:
        row = c.execute(text("select state, locked_by from job where id = :i"), {"i": jid}).mappings().one()
    assert row["state"] == "running" and row["locked_by"] == "w1"
    assert jobs.complete(engine, second, "w1", {"from": "w1"}) is True
    assert _state(engine, jid)["state"] == "done"


@handler("test.slow")
def _slow(ctx: JobContext, job: Job) -> dict:
    import time

    time.sleep(0.6)
    with ctx.engine.connect() as c:
        return {
            "left": c.execute(
                text("select extract(epoch from locked_until - now()) from job where id = :i"), {"i": job.id}
            ).scalar()
        }


def test_run_one_renews_the_lease_while_the_handler_runs(engine, ctx, monkeypatch):
    monkeypatch.setattr(jobs, "LEASE_SECONDS", 1)
    monkeypatch.setattr(jobs, "HEARTBEAT_SECONDS", 0.2)
    with engine.begin() as c:
        jid = enqueue(c, "test.slow", {})
    run_one(ctx, kinds=["test.slow"])
    s = _state(engine, jid)
    # without renewal the 1 s lease would be about 0.4 s from expiry by the end of the handler
    assert s["state"] == "done" and float(s["result"]["left"]) > 0.7


def test_job_that_keeps_killing_its_worker_ends_dead_with_an_item(engine, committed_family):
    pid = str(committed_family.profiles["alex"])
    with engine.begin() as c:
        jid = enqueue(c, "test.crashloop", {"profile_id": pid}, max_attempts=3)
    for i in range(6):
        with engine.begin() as c:
            c.execute(text("update job set run_after = now() where id = :i"), {"i": jid})
        if claim(engine, "core", f"w{i}", ["test.crashloop"]) is None:
            break
        _expire(engine, jid)  # the worker was OOM-killed: nobody calls fail()
        jobs.reap_expired(engine)
    s = _state(engine, jid)
    assert s["state"] == "dead" and s["attempts"] == 3
    with engine.connect() as c:
        n = c.execute(
            text("select count(*) from review_item where kind = 'job_failed' and payload->>'job_id' = :j"),
            {"j": str(jid)},
        ).scalar()
    assert n == 1


@pytest.mark.parametrize(
    "payload",
    [{"profile_id": "not-a-uuid"}, {"household_id": "00000000-0000-0000-0000-000000000000"}, {"household_id": 7}],
)
def test_dead_job_with_bad_scope_ids_still_files_its_item(engine, ctx, payload):
    with engine.begin() as c:
        jid = enqueue(c, "test.bad_input", payload)
    run_one(ctx, kinds=["test.bad_input"])
    assert _state(engine, jid)["state"] == "dead"
    with engine.connect() as c:
        item = (
            c.execute(
                text("select household_id, profile_id from review_item where payload->>'job_id' = :j"),
                {"j": str(jid)},
            )
            .mappings()
            .one()
        )
    assert item["household_id"] is None and item["profile_id"] is None


def test_one_failing_item_does_not_stall_the_reaper(engine, monkeypatch):
    from ownhealth.core import review

    with engine.begin() as c:
        a = enqueue(c, "test.crashloop", {"n": "a"}, max_attempts=1)
        b = enqueue(c, "test.crashloop", {"n": "b"}, max_attempts=1)
    for jid in (a, b):
        with engine.begin() as c:
            c.execute(text("update job set run_after = now() where id = :i"), {"i": jid})
        claim(engine, "core", "w", ["test.crashloop"])
        _expire(engine, jid)
    real = review.create_item

    def flaky(conn, item):
        if item.payload["job_id"] == a:
            conn.execute(text("select 1/0"))  # a database error inside the insert path
        return real(conn, item)

    monkeypatch.setattr(review, "create_item", flaky)
    jobs.reap_expired(engine)
    assert _state(engine, a)["state"] == "dead" and _state(engine, b)["state"] == "dead"


def _job(kind, payload):
    return Job(id=0, queue="core", kind=kind, payload=payload, attempts=1, max_attempts=5)


@pytest.mark.parametrize(
    "payload",
    [
        {"household_id": "x", "profile_id": "y"},  # no blob_sha256
        {"blob_sha256": "ab" * 32, "household_id": "x", "profile_id": "y"},  # not in the store
        {"blob_sha256": "not-hex", "household_id": "x", "profile_id": "y"},
    ],
)
def test_a_structured_import_with_no_stored_blob_fails_for_good(ctx, payload):
    from ownhealth.jobs.kinds import fhir

    with pytest.raises(PermanentJobError):
        fhir.run(ctx, _job("import.fhir_bundle", payload))


@pytest.mark.parametrize("payload", [{"as_of": "2026-13-40"}, {"profile_id": "not-a-uuid"}])
def test_a_state_rebuild_with_a_malformed_payload_fails_for_good(ctx, payload):
    from ownhealth.jobs.kinds import state

    with pytest.raises(PermanentJobError):
        state.rebuild(ctx, _job("state.rebuild", payload))
