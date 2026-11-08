"""Ops fixtures: a PostgreSQL 17 client for pg_dump and pg_restore, and one demo-seeded database.

When the box has no pg_dump 17 or newer on PATH, the tools run inside the shared test container
through `bin/pg-in-container` (OWNHEALTH_PG_BINDIR). CI installs postgresql-client-17 instead.
"""

from __future__ import annotations

import os
import re
import shutil
import subprocess
from pathlib import Path

import pytest

from ownhealth.blobs import BlobStore
from ownhealth.config import Settings
from ownhealth.db import make_engine
from ownhealth.db.migrate import migrate
from ownhealth.demo import seed_demo

BIN = Path(__file__).parent / "bin"


def _client_major() -> int:
    exe = shutil.which("pg_dump")
    if not exe:
        return 0
    out = subprocess.run([exe, "--version"], capture_output=True, text=True).stdout
    m = re.search(r"(\d+)\.", out)
    return int(m.group(1)) if m else 0


@pytest.fixture(scope="session", autouse=True)
def pg_client():
    if os.environ.get("OWNHEALTH_PG_BINDIR") or _client_major() >= 17:
        yield
        return
    os.environ["OWNHEALTH_PG_BINDIR"] = str(BIN)
    yield
    os.environ.pop("OWNHEALTH_PG_BINDIR", None)


def ops_settings(url: str, data_dir: Path) -> Settings:
    return Settings(database_url=url, data_dir=data_dir, run_jobs=False, run_migrations=False)


@pytest.fixture
def fresh(make_database, tmp_path) -> Settings:
    """An empty database (no migrations) and an empty blob directory: a fresh stack."""
    return ops_settings(make_database(), tmp_path / "fresh")


@pytest.fixture(scope="session")
def seeded(make_database, tmp_path_factory) -> Settings:
    """A migrated database with the demo Rivera family and its LabNorte PDFs in a blob store."""
    settings = ops_settings(make_database(), tmp_path_factory.mktemp("live"))
    eng = make_engine(settings.database_url)
    try:
        migrate(eng)
        assert seed_demo(eng, settings, BlobStore(settings.blobs_path))
    finally:
        eng.dispose()
    return settings


@pytest.fixture(scope="session")
def archive(seeded, tmp_path_factory) -> Path:
    from ownhealth import ops

    return ops.backup(seeded, tmp_path_factory.mktemp("backup") / "staging")
