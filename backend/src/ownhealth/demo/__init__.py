"""Demo seed: the synthetic Rivera family with enough data for every screen.

`uv run ownhealth demo-seed`, or OWNHEALTH_DEMO=1 at startup (seeds once).

Lanes add their own demo data with a hook module in `ownhealth/demo/hooks/<lane>.py`:

    ORDER = 300            # hooks run in ORDER, then module name; base data runs first
    def seed(conn, ctx: DemoContext) -> None: ...

Every hook runs in the same transaction as the base seed, so a broken hook leaves nothing behind.
"""

from __future__ import annotations

import importlib
import logging
import pkgutil
from dataclasses import dataclass, field
from datetime import date
from uuid import UUID

from sqlalchemy import Connection, Engine, text

from ownhealth.blobs import BlobStore
from ownhealth.config import Settings
from ownhealth.demo.family import AS_OF, Family, create_rivera_family

log = logging.getLogger("ownhealth.demo")

DEMO_HOUSEHOLD = "Rivera (demo)"


@dataclass
class DemoContext:
    settings: Settings
    blobs: BlobStore
    family: Family
    as_of: date = AS_OF
    documents: dict[str, UUID] = field(default_factory=dict)  # "alex:2026-09-14" -> lab report document id
    src_ids: dict[str, str] = field(default_factory=dict)  # named sources other hooks may cite


def hook_modules() -> list:
    from ownhealth.demo import hooks

    mods = [importlib.import_module(f"{hooks.__name__}.{m.name}") for m in pkgutil.iter_modules(hooks.__path__)]
    return sorted(mods, key=lambda m: (getattr(m, "ORDER", 500), m.__name__))


def seed_into(conn: Connection, ctx: DemoContext) -> None:
    from ownhealth.demo.base import seed_base

    seed_base(conn, ctx)
    for mod in hook_modules():
        log.info("demo hook %s", mod.__name__)
        mod.seed(conn, ctx)


def seed_demo(engine: Engine, settings: Settings, blobs: BlobStore, *, force: bool = False) -> bool:
    """Seed the demo household once. Returns False when it already exists (and not force)."""
    with engine.begin() as conn:
        conn.execute(text("select pg_advisory_xact_lock(4242)"))
        exists = conn.execute(text("select 1 from household where name = :n"), {"n": DEMO_HOUSEHOLD}).first()
        if exists and not force:
            return False
        family = create_rivera_family(conn, DEMO_HOUSEHOLD)
        seed_into(conn, DemoContext(settings=settings, blobs=blobs, family=family))
    return True
