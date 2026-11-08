"""Lane J: backup, restore and verify-restore (architecture.md 14).

A backup is a plain directory. restic snapshots it (deploy/restic/), so restic does the dedup,
encryption and retention, and this module only has to make the directory consistent:

    <dest>/manifest.json    app version, migrations, row counts, blob list, state_hash per profile
    <dest>/db.dump          pg_dump -Fc, taken from the same database snapshot as the counts and hashes
    <dest>/blobs/sha256/..  the blob store, hard-linked when on the same disk (blobs never change)

`restore` loads a backup into an empty instance and then checks it the same way `verify_restore` does.
`verify_restore` loads it into a scratch database next to the live one and drops it afterwards.
Both report the expected and actual `state_hash` of every profile.

pg_dump and pg_restore come from PATH, or from the directory in OWNHEALTH_PG_BINDIR. They read and
write only stdin and stdout, so a wrapper that runs them in another container works too.
"""

from __future__ import annotations

import hashlib
import json
import os
import shutil
import subprocess
from datetime import UTC, datetime
from pathlib import Path

from sqlalchemy import Connection, Engine, text
from sqlalchemy.engine import URL, make_url

from ownhealth import __version__
from ownhealth.config import Settings
from ownhealth.db import make_engine

MANIFEST_SCHEMA = "ownhealth.backup/v1"


class OpsError(RuntimeError):
    """A backup or restore step failed; the message says which and why."""


def _pg_tool(name: str) -> str:
    bindir = os.environ.get("OWNHEALTH_PG_BINDIR")
    path = str(Path(bindir) / name) if bindir else shutil.which(name)
    if not path or not os.access(path, os.X_OK):
        raise OpsError(f"{name} not found: install the PostgreSQL 17 client or set OWNHEALTH_PG_BINDIR")
    return path


def _libpq_url(url: str) -> str:
    return url.replace("postgresql+psycopg://", "postgresql://", 1)


def _pg_conn(url: str) -> tuple[str, dict[str, str] | None]:
    """(the --dbname value without the password, the env that carries it): argv is world-readable."""
    u = make_url(_libpq_url(url))
    if u.password is None:
        return u.render_as_string(hide_password=False), None
    bare = URL.create(u.drivername, u.username, None, u.host, u.port, u.database, u.query)  # set() ignores None
    return bare.render_as_string(hide_password=False), {**os.environ, "PGPASSWORD": u.password}


def _with_db(url: str, db: str) -> str:
    base, _, rest = _libpq_url(url).rpartition("/")
    query = rest.partition("?")[2]
    return f"{base}/{db}" + (f"?{query}" if query else "")


def _sha256_file(path: Path) -> str:
    h = hashlib.sha256()
    with path.open("rb") as f:
        for chunk in iter(lambda: f.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()


def _blob_files(root: Path) -> dict[str, Path]:
    base = root / "sha256"
    return {p.name: p for p in sorted(base.rglob("*")) if p.is_file()} if base.is_dir() else {}


def _table_counts(conn: Connection) -> dict[str, int]:
    names = conn.execute(text("select tablename from pg_tables where schemaname = 'public' order by 1")).scalars()
    return {n: conn.execute(text(f'select count(*) from "{n}"')).scalar_one() for n in names}


def _state_hashes(conn: Connection, as_of: str) -> dict[str, str]:
    from ownhealth.state import build_state

    ids = conn.execute(text("select id from profile order by id")).scalars().all()
    return {str(pid): build_state(conn, pid, as_of=as_of)["state_hash"] for pid in ids}


def _referenced_blobs(conn: Connection) -> list[str]:
    """Every blob hash a document or genome dataset row points at, so verify can tell a blob the
    live store had already lost from one the backup lost."""
    cols = conn.execute(
        text(
            "select column_name from information_schema.columns where table_schema = 'public'"
            " and table_name = 'genome_dataset' and data_type = 'bytea' and column_name like '%\\_blob'"
        )
    ).scalars()
    parts = ["select blob_sha256 as h from document"] + [f'select "{c}" from genome_dataset' for c in cols]
    rows = conn.execute(text(f"select distinct encode(h, 'hex') from ({' union '.join(parts)}) x where h is not null"))
    return sorted(rows.scalars())


def _link_or_copy(src: str, dst: str) -> None:
    try:
        os.link(src, dst)
    except OSError:
        shutil.copy2(src, dst)


def backup(settings: Settings, dest: Path) -> Path:
    """Write a consistent backup directory at `dest` (must be missing or empty). Returns `dest`."""
    dest = Path(dest)
    if dest.exists() and any(dest.iterdir()):
        raise OpsError(f"{dest} is not empty")
    dest.mkdir(parents=True, exist_ok=True)
    engine = make_engine(settings.database_url, isolation_level="REPEATABLE READ")
    try:
        with engine.connect() as conn:
            tx = conn.begin()
            # pg_dump reads the snapshot this transaction exported, so the dump, the counts and the
            # state hashes all describe the same instant even while the app keeps writing.
            snapshot = conn.execute(text("select pg_export_snapshot()")).scalar_one()
            with (dest / "db.dump").open("wb") as out:
                dsn, env = _pg_conn(settings.database_url)
                cmd = [_pg_tool("pg_dump"), "-Fc", f"--snapshot={snapshot}", "--dbname", dsn]
                dump = subprocess.Popen(cmd, stdout=out, stderr=subprocess.PIPE, env=env)
                referenced = _referenced_blobs(conn)
                migrations = list(conn.execute(text("select version from schema_migrations order by 1")).scalars())
                tables = _table_counts(conn)
                as_of = datetime.now(UTC).date().isoformat()
                states = _state_hashes(conn, as_of)
                _, err = dump.communicate()
            tx.rollback()  # build_state may store snapshots; the backup leaves the database as it was
    finally:
        engine.dispose()
    if dump.returncode != 0:
        raise OpsError(f"pg_dump failed: {err.decode(errors='replace').strip()}")

    # Blobs are written before the rows that reference them commit, so every blob the snapshot
    # can see exists by now. Later blobs are harmless extras.
    src = settings.blobs_path / "sha256"
    if src.is_dir():
        shutil.copytree(src, dest / "blobs" / "sha256", copy_function=_link_or_copy)
    files = _blob_files(dest / "blobs")
    from ownhealth.state import rules_version

    manifest = {
        "schema": MANIFEST_SCHEMA,
        "created_at": datetime.now(UTC).isoformat(timespec="seconds"),
        "app_version": __version__,
        "migrations": migrations,
        "rules_version": rules_version(),
        "as_of": as_of,
        "tables": tables,
        "blobs": {
            "count": len(files),
            "bytes": sum(p.stat().st_size for p in files.values()),
            "sha256": list(files),
            "referenced": referenced,
        },
        "state_hash": states,
    }
    (dest / "manifest.json").write_text(json.dumps(manifest, indent=2, sort_keys=True) + "\n")
    return dest


def _read_manifest(archive: Path) -> dict:
    path = Path(archive) / "manifest.json"
    if not path.is_file():
        raise OpsError(f"{path} not found: point at the directory `ownhealth backup` wrote")
    manifest = json.loads(path.read_text())
    if manifest.get("schema") != MANIFEST_SCHEMA:
        raise OpsError(f"unknown backup schema {manifest.get('schema')!r}, expected {MANIFEST_SCHEMA}")
    return manifest


def _pg_restore(url: str, archive: Path) -> None:
    dsn, env = _pg_conn(url)
    with (Path(archive) / "db.dump").open("rb") as dump:
        done = subprocess.run(
            [
                _pg_tool("pg_restore"),
                "--no-owner",
                "--no-privileges",
                "--exit-on-error",
                "--single-transaction",
                "--dbname",
                dsn,
            ],
            stdin=dump,
            capture_output=True,
            env=env,
        )
    if done.returncode != 0:
        raise OpsError(f"pg_restore failed: {done.stderr.decode(errors='replace').strip()}")


def _check_blobs(manifest: dict, root: Path) -> dict:
    files = _blob_files(root)
    listed = set(manifest["blobs"]["sha256"])
    expected = listed | set(manifest["blobs"].get("referenced", ()))  # older manifests have no "referenced"
    missing = sorted(expected - set(files))
    corrupt = sorted(sha for sha, p in files.items() if sha in listed and _sha256_file(p) != sha)
    return {"checked": len(files), "missing": missing, "corrupt": corrupt}


def _compare(engine: Engine, manifest: dict, blob_root: Path) -> dict:
    """Row counts, blob hashes and state_hash per profile against the manifest."""
    with engine.begin() as conn:
        counts = _table_counts(conn)  # before build_state, which may add state_snapshot rows
        states = _state_hashes(conn, manifest["as_of"])
    tables = {
        name: {"expected": n, "actual": counts.get(name)}
        for name, n in manifest["tables"].items()
        if counts.get(name) != n
    }
    profiles = {
        pid: {"expected": h, "actual": states.get(pid), "ok": states.get(pid) == h}
        for pid, h in manifest["state_hash"].items()
    }
    for pid in states.keys() - manifest["state_hash"].keys():
        profiles[pid] = {"expected": None, "actual": states[pid], "ok": False}
    blobs = _check_blobs(manifest, blob_root)
    ok = not tables and all(p["ok"] for p in profiles.values()) and not blobs["missing"] and not blobs["corrupt"]
    return {"ok": ok, "as_of": manifest["as_of"], "tables": tables, "profiles": profiles, "blobs": blobs}


def restore(settings: Settings, archive: Path) -> dict:
    """Load a backup into the configured, empty database and blob directory, then check it.

    Returns the same report as `verify_restore`, computed on the restored instance.
    """
    manifest = _read_manifest(archive)
    engine = make_engine(settings.database_url)
    try:
        with engine.connect() as conn:
            tables = conn.execute(text("select count(*) from pg_tables where schemaname = 'public'")).scalar_one()
        if tables:
            raise OpsError(
                "the target database is not empty: restore needs a fresh instance (new volume, app not started)"
            )
        # Check every blob before the database is loaded: a corrupt one found after pg_restore would
        # leave a populated database that a second attempt refuses as "not empty".
        backup_blobs = _blob_files(Path(archive) / "blobs")
        for sha, path in backup_blobs.items():
            if _sha256_file(path) != sha:
                raise OpsError(f"blob {sha} in the backup is corrupt")
        _pg_restore(settings.database_url, archive)
        for sha, path in backup_blobs.items():
            target = settings.blobs_path / "sha256" / sha[:2] / sha[2:4] / sha
            if not target.exists():
                target.parent.mkdir(parents=True, exist_ok=True)
                part = target.with_name(f".{sha}.part")  # copied whole, then renamed: never a truncated blob
                shutil.copy2(path, part)
                os.replace(part, target)
        return _compare(engine, manifest, settings.blobs_path)
    finally:
        engine.dispose()


def verify_restore(settings: Settings, archive: Path) -> dict:
    """Restore into a scratch database `<db>_restore_check`, compare with the manifest, drop it.

    `ok` is true only when every row count, every blob hash and every profile's state_hash match.
    """
    manifest = _read_manifest(archive)
    live = _libpq_url(settings.database_url)
    scratch = live.rpartition("/")[2].partition("?")[0] + "_restore_check"
    admin = make_engine(live, isolation_level="AUTOCOMMIT")
    try:
        with admin.connect() as c:
            c.execute(text(f'drop database if exists "{scratch}" with (force)'))
            c.execute(text(f'create database "{scratch}"'))
        engine = make_engine(_with_db(live, scratch))
        try:
            _pg_restore(_with_db(live, scratch), archive)
            return _compare(engine, manifest, Path(archive) / "blobs")
        finally:
            engine.dispose()
            with admin.connect() as c:
                c.execute(text(f'drop database if exists "{scratch}" with (force)'))
    finally:
        admin.dispose()
