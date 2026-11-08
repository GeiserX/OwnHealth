"""Job `state.rebuild`: rebuilds and stores the state; a second run with the same data writes nothing."""

from sqlalchemy import text

from ownhealth.blobs import BlobStore
from ownhealth.config import Settings
from ownhealth.jobs import Job, JobContext, load_handlers


def _count(seeded, pid):
    with seeded.engine.connect() as c:
        return c.execute(text("select count(*) from state_snapshot where profile_id = :p"), {"p": pid}).scalar_one()


def test_state_rebuild_job(seeded, fake_genome, tmp_path):
    handler = load_handlers()["state.rebuild"]
    ctx = JobContext(engine=seeded.engine, settings=Settings(database_url=str(seeded.engine.url), data_dir=tmp_path),
                     blobs=BlobStore(tmp_path / "blobs"))  # fmt: skip
    pid = seeded.pid("lucia")
    job = Job(id=0, queue="core", kind="state.rebuild", payload={"profile_id": str(pid), "as_of": "2026-06-30"},
              attempts=0, max_attempts=3)  # fmt: skip
    before = _count(seeded, pid)
    out = handler(ctx, job)
    assert out["profiles"] == 1 and out["hashes"][str(pid)].startswith("sha256:")
    assert _count(seeded, pid) == before + 1
    assert handler(ctx, job) == out  # idempotent: same hash
    assert _count(seeded, pid) == before + 1
    every = handler(ctx, Job(id=0, queue="core", kind="state.rebuild", payload={"as_of": "2026-06-30"}, attempts=0,
                             max_attempts=3))  # fmt: skip
    assert every["hashes"][str(pid)] == out["hashes"][str(pid)] and every["profiles"] >= 4
