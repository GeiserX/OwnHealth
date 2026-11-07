"""Forward-only migration runner.

Migrations are `ownhealth/db/migrations/NNNN_name.sql`, applied in numeric order, each in its own
transaction, recorded in `schema_migrations`. A Postgres advisory lock keeps two processes from
migrating at once. The runner refuses a database that has a migration this code does not know
(a schema newer than the code). Lanes own number ranges; see CONTRACTS.md.
"""

from __future__ import annotations

import hashlib
import re
from dataclasses import dataclass
from pathlib import Path

from sqlalchemy import Engine, text

MIGRATIONS_DIR = Path(__file__).resolve().parent / "migrations"
_NAME = re.compile(r"^(\d{4})_([a-z0-9_]+)\.sql$")
_LOCK_KEY = 0x0E7_4EA1  # arbitrary, constant


class MigrationError(RuntimeError):
    pass


@dataclass(frozen=True)
class Migration:
    version: int
    name: str
    path: Path

    @property
    def sql(self) -> str:
        return self.path.read_text()

    @property
    def checksum(self) -> str:
        return hashlib.sha256(self.path.read_bytes()).hexdigest()


def discover(directory: Path = MIGRATIONS_DIR) -> list[Migration]:
    found: dict[int, Migration] = {}
    for path in sorted(directory.glob("*.sql")):
        m = _NAME.match(path.name)
        if not m:
            raise MigrationError(f"bad migration file name: {path.name} (want NNNN_name.sql)")
        version = int(m.group(1))
        if version in found:
            raise MigrationError(f"two migrations share number {version:04d}")
        found[version] = Migration(version, m.group(2), path)
    return [found[v] for v in sorted(found)]


def applied_versions(engine: Engine) -> dict[int, str]:
    with engine.connect() as conn:
        exists = conn.execute(text("select to_regclass('schema_migrations') is not null")).scalar()
        if not exists:
            return {}
        rows = conn.execute(text("select version, checksum from schema_migrations")).all()
        return {r.version: r.checksum for r in rows}


def migrate(engine: Engine, directory: Path = MIGRATIONS_DIR) -> list[int]:
    """Apply pending migrations. Returns the versions applied in this call."""
    migrations = discover(directory)
    known = {m.version for m in migrations}
    applied_now: list[int] = []
    with engine.connect() as lock_conn:
        lock_conn.execute(text("select pg_advisory_lock(:k)"), {"k": _LOCK_KEY})
        try:
            with engine.begin() as conn:
                conn.execute(
                    text(
                        "create table if not exists schema_migrations ("
                        " version int primary key, name text not null, checksum text not null,"
                        " applied_at timestamptz not null default now())"
                    )
                )
            done = applied_versions(engine)
            unknown = sorted(set(done) - known)
            if unknown:
                raise MigrationError(
                    f"database has migrations this code does not know: {unknown}; "
                    "the schema is newer than the code, refusing to start"
                )
            for m in migrations:
                if m.version in done:
                    if done[m.version] != m.checksum:
                        raise MigrationError(
                            f"migration {m.version:04d}_{m.name} changed after it was applied; "
                            "migrations are forward-only, add a new file instead"
                        )
                    continue
                with engine.begin() as conn:
                    # Raw cursor without parameters: psycopg sends the file as one simple query,
                    # so '%' in plpgsql bodies is not taken for a placeholder.
                    with conn.connection.dbapi_connection.cursor() as cur:
                        cur.execute(m.sql)
                    conn.execute(
                        text("insert into schema_migrations (version, name, checksum) values (:v, :n, :c)"),
                        {"v": m.version, "n": m.name, "c": m.checksum},
                    )
                applied_now.append(m.version)
        finally:
            lock_conn.execute(text("select pg_advisory_unlock(:k)"), {"k": _LOCK_KEY})
            lock_conn.commit()
    return applied_now


def pending(engine: Engine, directory: Path = MIGRATIONS_DIR) -> list[int]:
    done = applied_versions(engine)
    return [m.version for m in discover(directory) if m.version not in done]
