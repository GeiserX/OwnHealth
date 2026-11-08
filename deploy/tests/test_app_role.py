"""The app's database role cannot rewrite the audit log (security-privacy.md section 9).

deploy/db/app-role.sql runs as the table owner after the migrations. Connected as the role it creates,
INSERT and SELECT on audit_event work, and UPDATE, DELETE, TRUNCATE and ALTER TABLE ... DISABLE
TRIGGER are refused by privileges, not only by the trigger. The owner control shows the same ALTER
succeeds for the owner, so the test can fail.

Run from backend/: `uv run pytest ../deploy/tests`. Uses the shared test Postgres (scripts/test-db.sh):
its own database and role, both dropped at the end. psql 15 or newer on PATH, or the test container.
"""

from __future__ import annotations

import os
import re
import secrets
import shutil
import subprocess
from collections.abc import Iterator
from pathlib import Path

import pytest
from sqlalchemy import Engine, text
from sqlalchemy.exc import DBAPIError

from ownhealth.db import make_engine
from ownhealth.db.migrate import migrate

ADMIN_URL = os.environ.get("OWNHEALTH_TEST_ADMIN_URL", "postgresql://postgres:postgres@127.0.0.1:55432/postgres")
SQL = Path(__file__).resolve().parents[1] / "db" / "app-role.sql"
CONTAINER = "ownhealth-testpg"


def _url(db: str, user: str | None = None, password: str | None = None) -> str:
    base = ADMIN_URL.rsplit("/", 1)[0]
    if user:
        base = re.sub(r"://[^@]+@", f"://{user}:{password}@", base)
    return f"{base}/{db}"


def _host_psql() -> str | None:
    exe = shutil.which("psql")
    if not exe:
        return None
    out = subprocess.run([exe, "--version"], capture_output=True, text=True).stdout
    m = re.search(r"(\d+)\.", out)
    return exe if m and int(m.group(1)) >= 15 else None


def apply_role_sql(db: str, role: str, password: str) -> subprocess.CompletedProcess:
    env = {**os.environ, "OWNHEALTH_APP_ROLE_PASSWORD": password}
    args = ["-X", "-q", "-v", "ON_ERROR_STOP=1", "-v", f"app_role={role}", "-f", "-"]
    if exe := _host_psql():
        cmd = [exe, _url(db), *args]
    elif shutil.which("docker"):
        cmd = [
            "docker",
            "exec",
            "-i",
            "-e",
            "OWNHEALTH_APP_ROLE_PASSWORD",
            CONTAINER,
            "psql",
            "-U",
            "postgres",
            "-d",
            db,
            *args,
        ]
    else:
        pytest.skip("no psql 15+ and no docker")
    return subprocess.run(cmd, input=SQL.read_text(), env=env, capture_output=True, text=True, timeout=60)


@pytest.fixture(scope="module")
def migrated() -> Iterator[tuple[str, str, str]]:
    admin = make_engine(ADMIN_URL, isolation_level="AUTOCOMMIT")
    db = f"oh_test_{secrets.token_hex(6)}"
    role = f"oh_test_app_{secrets.token_hex(4)}"
    password = secrets.token_urlsafe(16)
    with admin.connect() as c:
        c.execute(text(f'create database "{db}"'))
    owner = make_engine(_url(db))
    try:
        migrate(owner)
        done = apply_role_sql(db, role, password)
        assert done.returncode == 0, done.stderr
        again = apply_role_sql(db, role, password)  # idempotent: runs after every migration
        assert again.returncode == 0, again.stderr
        yield db, role, password
    finally:
        owner.dispose()
        with admin.connect() as c:
            c.execute(text(f'drop database if exists "{db}" with (force)'))
            c.execute(text(f'drop role if exists "{role}"'))
        admin.dispose()


def _insert(conn) -> None:
    conn.execute(text("insert into audit_event (actor_kind, via, action) values ('system', 'job', 'test')"))


def _refused(engine: Engine, sql: str) -> str:
    with engine.connect() as conn, pytest.raises(DBAPIError) as exc:
        conn.execute(text(sql))
    return str(exc.value.orig)


def test_app_role_can_append_and_read_but_not_rewrite_the_audit_log(migrated):
    db, role, password = migrated
    app = make_engine(_url(db, role, password))
    try:
        with app.begin() as conn:
            _insert(conn)
            assert conn.execute(text("select count(*) from audit_event")).scalar_one() >= 1
            conn.execute(text("select count(*) from profile"))  # ordinary tables stay usable
        for sql in (
            "update audit_event set action = 'rewritten'",
            "delete from audit_event",
            "truncate audit_event",
        ):
            assert "permission denied" in _refused(app, sql), sql
        assert "must be owner" in _refused(app, "alter table audit_event disable trigger all")
        assert "must be owner" in _refused(app, "drop trigger audit_event_no_update on audit_event")
    finally:
        app.dispose()


def test_owner_control_the_same_alter_succeeds_for_the_table_owner(migrated):
    db, _, _ = migrated
    owner = make_engine(_url(db))
    try:
        with owner.connect() as conn:
            trans = conn.begin()
            conn.execute(text("alter table audit_event disable trigger all"))
            conn.execute(text("update audit_event set action = 'rewritten'"))
            trans.rollback()
    finally:
        owner.dispose()
