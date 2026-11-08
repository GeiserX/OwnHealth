"""Shared fixtures for every lane.

The test Postgres is the shared container `ownhealth-testpg` (scripts/test-db.sh). Each pytest
session creates its own database `oh_test_<random>`, applies every migration, and drops it at the
end, so parallel lanes never touch each other's data. Never drop other databases.

Fixtures:
  engine   session-wide Engine on the session database (migrated)
  conn     a Connection inside a transaction that is rolled back after the test
  family   the Rivera household created on `conn` (rolled back with it)
  settings Settings for create_app (dev_auth on, jobs off, blobs in tmp)
  blobs    a BlobStore in tmp
  app_for  build a FastAPI app acting as a given user: client = app_for(user_id)
  committed_family  the Rivera household committed for real (for API, job and multi-connection tests)
"""

from __future__ import annotations

import os
import secrets
from collections.abc import Iterator
from pathlib import Path

import pytest
from fastapi.testclient import TestClient
from sqlalchemy import Engine, text

from ownhealth.blobs import BlobStore
from ownhealth.config import Settings
from ownhealth.db import make_engine
from ownhealth.db.migrate import migrate
from ownhealth.demo.family import Family, create_rivera_family

ADMIN_URL = os.environ.get("OWNHEALTH_TEST_ADMIN_URL", "postgresql://postgres:postgres@127.0.0.1:55432/postgres")


def _url_for(db: str) -> str:
    return ADMIN_URL.rsplit("/", 1)[0] + "/" + db


@pytest.fixture(scope="session")
def admin_engine() -> Iterator[Engine]:
    eng = make_engine(ADMIN_URL, isolation_level="AUTOCOMMIT")
    try:
        with eng.connect() as c:
            c.execute(text("select 1"))
    except Exception as exc:  # noqa: BLE001
        pytest.exit(f"test Postgres not reachable at {ADMIN_URL}: run scripts/test-db.sh ({exc})", returncode=2)
    yield eng
    eng.dispose()


@pytest.fixture(scope="session")
def make_database(admin_engine: Engine):
    """Create extra throwaway databases (dropped at session end). Returns their URL."""
    created: list[str] = []

    def _make() -> str:
        name = f"oh_test_{secrets.token_hex(6)}"
        with admin_engine.connect() as c:
            c.execute(text(f'create database "{name}"'))
        created.append(name)
        return _url_for(name)

    yield _make
    with admin_engine.connect() as c:
        for name in created:
            c.execute(text(f'drop database if exists "{name}" with (force)'))


@pytest.fixture(scope="session")
def db_url(make_database) -> str:
    return make_database()


@pytest.fixture(scope="session")
def engine(db_url: str) -> Iterator[Engine]:
    eng = make_engine(db_url)
    migrate(eng)
    yield eng
    eng.dispose()


@pytest.fixture
def conn(engine: Engine):
    with engine.connect() as c:
        tx = c.begin()
        try:
            yield c
        finally:
            tx.rollback()


@pytest.fixture
def family(conn) -> Family:
    return create_rivera_family(conn, f"Rivera {secrets.token_hex(3)}")


@pytest.fixture
def committed_family(engine: Engine) -> Family:
    with engine.begin() as c:
        return create_rivera_family(c, f"Rivera {secrets.token_hex(3)}")


@pytest.fixture
def blobs(tmp_path: Path) -> BlobStore:
    return BlobStore(tmp_path / "blobs")


@pytest.fixture
def settings(db_url: str, tmp_path: Path) -> Settings:
    static = tmp_path / "static"
    static.mkdir()
    (static / "index.html").write_text("<!doctype html><title>OwnHealth</title>")
    return Settings(
        database_url=db_url,
        data_dir=tmp_path / "data",
        static_dir=static,
        dev_auth=False,
        run_jobs=False,
        run_migrations=False,
        secret_key="test-secret",
    )


@pytest.fixture
def app_for(settings: Settings, engine: Engine):
    """app_for(user_id) -> TestClient whose requests act as that user's browser session."""
    from ownhealth.api._deps import get_principal
    from ownhealth.app import create_app
    from ownhealth.core.access import ALL_SCOPES, Principal

    clients: list[TestClient] = []

    def _make(user_id=None, principal: Principal | None = None) -> TestClient:
        app = create_app(settings, engine)
        if principal is None and user_id is not None:
            with engine.connect() as c:
                hh = c.execute(text("select household_id from app_user where id = :u"), {"u": user_id}).scalar_one()
            principal = Principal(kind="session", user_id=user_id, household_id=hh, scopes=ALL_SCOPES)
        if principal is not None:
            app.dependency_overrides[get_principal] = lambda: principal
        client = TestClient(app)
        client.__enter__()
        clients.append(client)
        return client

    yield _make
    for c in clients:
        c.__exit__(None, None, None)
