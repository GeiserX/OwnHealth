"""Postgres table job queue, run inside the app process.

    enqueue(conn, "import.pdf", {"document_id": ...}, dedupe_key=None)

Workers claim one job with `SELECT ... FOR UPDATE SKIP LOCKED`, run its handler, and mark it done.
A failure retries with backoff (2^attempts * 30 s); after max_attempts the job is `dead` and a
`job_failed` review item appears. Handlers must be idempotent: a job may run twice after a crash.

Handlers live in `ownhealth/jobs/kinds/<lane>.py` and register with the `handler` decorator.
Every module in that package is imported by `load_handlers()`.

    @handler("import.pdf")
    def import_pdf(ctx: JobContext, job: Job) -> dict | None: ...
"""

from __future__ import annotations

import asyncio
import contextlib
import importlib
import json
import logging
import os
import pkgutil
import socket
from collections.abc import Callable
from dataclasses import dataclass, field
from datetime import datetime
from typing import Any
from uuid import UUID

from sqlalchemy import Connection, Engine, text

from ownhealth.blobs import BlobStore
from ownhealth.config import Settings

log = logging.getLogger("ownhealth.jobs")

LEASE_SECONDS = 300
BACKOFF_BASE_SECONDS = 30


@dataclass(frozen=True)
class Job:
    id: int
    queue: str
    kind: str
    payload: dict
    attempts: int
    max_attempts: int
    dedupe_key: str | None = None


@dataclass
class JobContext:
    engine: Engine
    settings: Settings
    blobs: BlobStore
    worker_id: str = field(default_factory=lambda: f"{socket.gethostname()}:{os.getpid()}")

    def heartbeat(self, job: Job) -> None:
        """Long handlers call this about once a minute to keep their lease."""
        with self.engine.begin() as conn:
            conn.execute(
                text(
                    "update job set locked_until = now() + make_interval(secs => :s) where id = :id and locked_by = :w"
                ),
                {"s": LEASE_SECONDS, "id": job.id, "w": self.worker_id},
            )


Handler = Callable[[JobContext, Job], dict | None]
_HANDLERS: dict[str, Handler] = {}


class JobError(RuntimeError):
    pass


class PermanentJobError(JobError):
    """Raise from a handler when a retry cannot help (bad input): the job goes straight to dead."""


def handler(kind: str) -> Callable[[Handler], Handler]:
    def deco(fn: Handler) -> Handler:
        if kind in _HANDLERS and _HANDLERS[kind] is not fn:
            raise JobError(f"two handlers for job kind {kind}")
        _HANDLERS[kind] = fn
        return fn

    return deco


def handlers() -> dict[str, Handler]:
    return dict(_HANDLERS)


def load_handlers() -> dict[str, Handler]:
    """Import every module in ownhealth.jobs.kinds so their @handler decorators run."""
    from ownhealth.jobs import kinds

    for mod in pkgutil.iter_modules(kinds.__path__):
        importlib.import_module(f"{kinds.__name__}.{mod.name}")
    return handlers()


# --------------------------------------------------------------------- queue ----


def enqueue(
    conn: Connection,
    kind: str,
    payload: dict,
    *,
    queue: str = "core",
    dedupe_key: str | None = None,
    priority: int = 5,
    run_after: datetime | None = None,
    max_attempts: int = 5,
) -> int | None:
    """Add a job in the caller's transaction. Returns its id, or None when a queued or running job
    with the same dedupe_key exists already. Payloads carry ids only, never document content."""
    row = conn.execute(
        text(
            "insert into job (queue, kind, payload, dedupe_key, priority, run_after, max_attempts)"
            " values (:q, :k, cast(:p as jsonb), :d, :pr, coalesce(:ra, now()), :m)"
            " on conflict (dedupe_key) where state in ('queued', 'running') do nothing returning id"
        ),
        {
            "q": queue,
            "k": kind,
            "p": json.dumps(payload, default=str),
            "d": dedupe_key,
            "pr": priority,
            "ra": run_after,
            "m": max_attempts,
        },
    ).scalar()
    if row is not None:
        conn.execute(text("select pg_notify('ownhealth_job', :q)"), {"q": queue})
    return row


def claim(engine: Engine, queue: str, worker_id: str, kinds: list[str] | None = None) -> Job | None:
    """Claim the next ready job on `queue` (optionally only these kinds) with SKIP LOCKED."""
    with engine.begin() as conn:
        row = (
            conn.execute(
                text(
                    "with next as (select id from job where queue = :q and state = 'queued' and run_after <= now()"
                    " and (cast(:kinds as text[]) is null or kind = any(cast(:kinds as text[])))"
                    " order by priority, id for update skip locked limit 1)"
                    " update job set state = 'running', attempts = attempts + 1, locked_by = :w,"
                    " locked_until = now() + make_interval(secs => :s) from next where job.id = next.id"
                    " returning job.id, job.queue, job.kind, job.payload, job.attempts, job.max_attempts,"
                    " job.dedupe_key"
                ),
                {"q": queue, "w": worker_id, "s": LEASE_SECONDS, "kinds": kinds},
            )
            .mappings()
            .first()
        )
    return Job(**row) if row else None


def complete(engine: Engine, job: Job, result: dict | None = None) -> None:
    with engine.begin() as conn:
        conn.execute(
            text(
                "update job set state = 'done', finished_at = now(), locked_by = null, locked_until = null,"
                " result = cast(:r as jsonb), last_error = null where id = :id"
            ),
            {"id": job.id, "r": json.dumps(result, default=str) if result is not None else None},
        )


def _error_text(exc: BaseException) -> str:
    return f"{type(exc).__name__}: {exc}"[:500]


def fail(engine: Engine, job: Job, exc: BaseException) -> str:
    """Record a failure. Returns the new state: 'queued' (retry later) or 'dead'."""
    dead = job.attempts >= job.max_attempts or isinstance(exc, PermanentJobError)
    with engine.begin() as conn:
        if not dead:
            conn.execute(
                text(
                    "update job set state = 'queued', locked_by = null, locked_until = null, last_error = :e,"
                    " run_after = now() + make_interval(secs => :b) where id = :id"
                ),
                {"id": job.id, "e": _error_text(exc), "b": (2**job.attempts) * BACKOFF_BASE_SECONDS},
            )
            return "queued"
        conn.execute(
            text(
                "update job set state = 'dead', finished_at = now(), locked_by = null, locked_until = null,"
                " last_error = :e where id = :id"
            ),
            {"id": job.id, "e": _error_text(exc)},
        )
        _job_failed_item(conn, job, exc)
    return "dead"


def _job_failed_item(conn: Connection, job: Job, exc: BaseException) -> None:
    from ownhealth.core.review import ReviewItemIn, create_item

    p = job.payload
    profile_id = p.get("profile_id")
    household_id = p.get("household_id")
    document_id = p.get("document_id")
    # a payload may name a document that is gone or is not a uuid: keep it in the payload, never in the FK
    if document_id is not None:
        try:
            document_id = conn.execute(
                text("select id from document where id = :d"), {"d": str(UUID(str(document_id)))}
            ).scalar()
        except ValueError:
            document_id = None
    if household_id is None and profile_id:
        household_id = conn.execute(text("select household_id from profile where id = :p"), {"p": profile_id}).scalar()
    if household_id is None and document_id:
        household_id = conn.execute(
            text("select household_id from document where id = :d"), {"d": document_id}
        ).scalar()
    create_item(
        conn,
        ReviewItemIn(
            kind="job_failed",
            household_id=household_id,
            profile_id=profile_id,
            document_id=document_id,
            payload={
                "job_id": job.id,
                "kind": job.kind,
                "attempts": job.attempts,
                "error": _error_text(exc),
                "payload_document_id": p.get("document_id"),
            },
        ),
    )


def reap_expired(engine: Engine) -> int:
    """Move running jobs whose lease ran out back to queued (the worker died)."""
    with engine.begin() as conn:
        return conn.execute(
            text(
                "update job set state = 'queued', locked_by = null, locked_until = null"
                " where state = 'running' and locked_until < now()"
            )
        ).rowcount


def run_one(ctx: JobContext, queue: str = "core", kinds: list[str] | None = None) -> Job | None:
    """Claim and run one job. Returns the job it ran, or None when the queue was empty."""
    job = claim(ctx.engine, queue, ctx.worker_id, kinds)
    if job is None:
        return None
    fn = _HANDLERS.get(job.kind)
    try:
        if fn is None:
            raise PermanentJobError(f"no handler for job kind {job.kind}")
        result = fn(ctx, job)
    except Exception as exc:  # noqa: BLE001 - every failure is recorded, never silent
        state = fail(ctx.engine, job, exc)
        log.warning("job %s (%s) failed, now %s: %s", job.id, job.kind, state, _error_text(exc))
        return job
    complete(ctx.engine, job, result)
    return job


def drain(ctx: JobContext, queue: str = "core", limit: int = 1000, kinds: list[str] | None = None) -> int:
    """Run jobs until the queue has none ready. For tests and the CLI."""
    n = 0
    while n < limit and run_one(ctx, queue, kinds) is not None:
        n += 1
    return n


async def worker_loop(ctx: JobContext, queue: str, stop: asyncio.Event, poll_seconds: float = 2.0) -> None:
    last_reap = 0.0
    loop = asyncio.get_running_loop()
    while not stop.is_set():
        try:
            if loop.time() - last_reap > 60:
                await asyncio.to_thread(reap_expired, ctx.engine)
                last_reap = loop.time()
            ran = await asyncio.to_thread(run_one, ctx, queue)
        except Exception:  # noqa: BLE001 - database hiccup; keep the loop alive
            log.exception("job loop error")
            ran = None
        if ran is None:
            with contextlib.suppress(TimeoutError):
                await asyncio.wait_for(stop.wait(), timeout=poll_seconds)


def start_workers(ctx: JobContext, stop: asyncio.Event) -> list[asyncio.Task[Any]]:
    load_handlers()
    tasks = []
    for queue in ctx.settings.queues:
        for i in range(ctx.settings.job_workers):
            worker = JobContext(ctx.engine, ctx.settings, ctx.blobs, f"{ctx.worker_id}:{queue}:{i}")
            tasks.append(asyncio.create_task(worker_loop(worker, queue, stop, ctx.settings.job_poll_seconds)))
    return tasks
