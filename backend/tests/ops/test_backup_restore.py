"""Backup, restore and verify-restore (architecture.md 14; ROADMAP v0.1 exit criterion "Restore")."""

from __future__ import annotations

import json
import shutil

import pytest
from sqlalchemy import text

from ownhealth import ops
from ownhealth.db import make_engine
from ownhealth.db.migrate import pending


def test_backup_writes_dump_blobs_and_a_manifest_with_every_profile(archive, seeded):
    m = json.loads((archive / "manifest.json").read_text())
    assert m["schema"] == "ownhealth.backup/v1"
    assert (archive / "db.dump").stat().st_size > 0
    eng = make_engine(seeded.database_url)
    try:
        with eng.connect() as c:
            profiles = {str(p) for p in c.execute(text("select id from profile")).scalars()}
            observations = c.execute(text("select count(*) from observation")).scalar_one()
            snapshots = c.execute(text("select count(*) from state_snapshot")).scalar_one()
    finally:
        eng.dispose()
    assert set(m["state_hash"]) == profiles and len(profiles) == 4
    assert all(h.startswith("sha256:") for h in m["state_hash"].values())
    assert m["tables"]["observation"] == observations > 0
    assert m["blobs"]["count"] == len(m["blobs"]["sha256"]) > 0
    # build_state ran inside the backup's rolled-back transaction, so the live database is unchanged.
    assert m["tables"]["state_snapshot"] == snapshots


def test_backup_refuses_a_directory_that_is_not_empty(seeded, tmp_path):
    (tmp_path / "x").write_text("old")
    with pytest.raises(ops.OpsError, match="not empty"):
        ops.backup(seeded, tmp_path)


def test_restore_into_a_fresh_instance_gives_the_same_state_hash_for_every_profile(archive, fresh):
    report = ops.restore(fresh, archive)
    assert report["ok"], report
    assert len(report["profiles"]) == 4 and all(p["actual"] == p["expected"] for p in report["profiles"].values())
    m = json.loads((archive / "manifest.json").read_text())
    restored = sorted(p.name for p in (fresh.blobs_path / "sha256").rglob("*") if p.is_file())
    assert restored == sorted(m["blobs"]["sha256"])
    eng = make_engine(fresh.database_url)
    try:
        assert pending(eng) == []  # the restored schema is the app's schema; nothing to migrate
    finally:
        eng.dispose()


def test_restore_refuses_a_database_that_is_not_empty(archive, seeded):
    with pytest.raises(ops.OpsError, match="not empty"):
        ops.restore(seeded, archive)


def test_verify_restore_passes_and_drops_its_scratch_database(archive, seeded):
    report = ops.verify_restore(seeded, archive)
    assert report["ok"], report
    assert report["blobs"]["missing"] == [] and report["blobs"]["corrupt"] == []
    eng = make_engine(seeded.database_url)
    try:
        with eng.connect() as c:
            name = seeded.database_url.rsplit("/", 1)[1] + "_restore_check"
            assert c.execute(text("select 1 from pg_database where datname = :n"), {"n": name}).first() is None
    finally:
        eng.dispose()


def _copy(archive, tmp_path):
    dst = tmp_path / "copy"
    shutil.copytree(archive, dst)
    return dst


def test_verify_restore_fails_on_a_corrupt_blob(archive, seeded, tmp_path):
    bad = _copy(archive, tmp_path)
    victim = next(p for p in (bad / "blobs" / "sha256").rglob("*") if p.is_file())
    victim.chmod(0o644)  # blobs are stored read-only
    victim.write_bytes(victim.read_bytes() + b"x")
    report = ops.verify_restore(seeded, bad)
    assert not report["ok"] and report["blobs"]["corrupt"] == [victim.name]


def test_verify_restore_fails_on_a_missing_blob(archive, seeded, tmp_path):
    bad = _copy(archive, tmp_path)
    victim = next(p for p in (bad / "blobs" / "sha256").rglob("*") if p.is_file())
    victim.unlink()
    report = ops.verify_restore(seeded, bad)
    assert not report["ok"] and report["blobs"]["missing"] == [victim.name]


def test_verify_restore_fails_when_a_state_hash_differs(archive, seeded, tmp_path):
    bad = _copy(archive, tmp_path)
    m = json.loads((bad / "manifest.json").read_text())
    pid = sorted(m["state_hash"])[0]
    m["state_hash"][pid] = "sha256:" + "0" * 64
    (bad / "manifest.json").write_text(json.dumps(m))
    report = ops.verify_restore(seeded, bad)
    assert not report["ok"] and not report["profiles"][pid]["ok"]
    assert sum(not p["ok"] for p in report["profiles"].values()) == 1


def test_verify_restore_fails_when_a_row_count_differs(archive, seeded, tmp_path):
    bad = _copy(archive, tmp_path)
    m = json.loads((bad / "manifest.json").read_text())
    m["tables"]["observation"] += 1
    (bad / "manifest.json").write_text(json.dumps(m))
    report = ops.verify_restore(seeded, bad)
    assert not report["ok"] and "observation" in report["tables"]


def test_unknown_manifest_schema_is_refused(archive, seeded, tmp_path):
    bad = _copy(archive, tmp_path)
    (bad / "manifest.json").write_text(json.dumps({"schema": "other/v9"}))
    with pytest.raises(ops.OpsError, match="unknown backup schema"):
        ops.verify_restore(seeded, bad)


def test_cli_verify_restore_exits_non_zero_on_a_mismatch(archive, seeded, tmp_path, monkeypatch, capsys):
    from ownhealth.cli import main

    monkeypatch.setenv("OWNHEALTH_DATABASE_URL", seeded.database_url)
    monkeypatch.setenv("OWNHEALTH_DATA_DIR", str(seeded.data_dir))
    assert main(["verify-restore", str(archive)]) == 0
    bad = _copy(archive, tmp_path)
    m = json.loads((bad / "manifest.json").read_text())
    m["tables"]["profile"] += 1
    (bad / "manifest.json").write_text(json.dumps(m))
    assert main(["verify-restore", str(bad)]) == 1
    assert '"ok": false' in capsys.readouterr().out


def test_the_password_stays_out_of_the_pg_tools_argv():
    dsn, env = ops._pg_conn("postgresql+psycopg://oh:s3cr%2Ft@db:5432/ownhealth?sslmode=require")
    assert "s3cr" not in dsn and dsn.startswith("postgresql://oh@db:5432/ownhealth") and "sslmode=require" in dsn
    assert env["PGPASSWORD"] == "s3cr/t"
    assert ops._pg_conn("postgresql://oh@db/ownhealth") == ("postgresql://oh@db/ownhealth", None)


def test_backup_lists_the_blobs_rows_reference(archive):
    m = json.loads((archive / "manifest.json").read_text())
    assert m["blobs"]["referenced"] and set(m["blobs"]["referenced"]) <= set(m["blobs"]["sha256"])


def test_verify_restore_fails_on_a_referenced_blob_the_backup_never_had(archive, seeded, tmp_path):
    bad = _copy(archive, tmp_path)
    m = json.loads((bad / "manifest.json").read_text())
    victim = m["blobs"]["referenced"][0]
    m["blobs"]["sha256"].remove(victim)  # as if the live store had already lost it at backup time
    (bad / "manifest.json").write_text(json.dumps(m))
    next(p for p in (bad / "blobs" / "sha256").rglob(victim)).unlink()
    report = ops.verify_restore(seeded, bad)
    assert not report["ok"] and report["blobs"]["missing"] == [victim]


def test_restore_checks_blobs_before_it_loads_the_database(archive, fresh, tmp_path):
    bad = _copy(archive, tmp_path)
    victim = next(p for p in (bad / "blobs" / "sha256").rglob("*") if p.is_file())
    victim.chmod(0o644)
    victim.write_bytes(victim.read_bytes() + b"x")
    with pytest.raises(ops.OpsError, match="corrupt"):
        ops.restore(fresh, bad)
    eng = make_engine(fresh.database_url)
    try:
        with eng.connect() as c:  # still empty, so a fixed backup can be restored into it
            assert c.execute(text("select count(*) from pg_tables where schemaname = 'public'")).scalar_one() == 0
    finally:
        eng.dispose()
    assert not list(fresh.blobs_path.rglob("*.part"))


def test_backup_and_restore_never_put_the_password_on_a_command_line(seeded, fresh, tmp_path, monkeypatch):
    """The pg tools get the password through PGPASSWORD; argv is readable by every user of the host."""
    from sqlalchemy.engine import make_url

    password = make_url(seeded.database_url).password
    assert password  # the test database URL carries one, so the check can fail
    seen: list[list[str]] = []
    real_popen, real_run = ops.subprocess.Popen, ops.subprocess.run

    def popen(cmd, *a, **kw):
        seen.append(list(cmd))
        return real_popen(cmd, *a, **kw)

    def run(cmd, *a, **kw):
        seen.append(list(cmd))
        return real_run(cmd, *a, **kw)

    monkeypatch.setattr(ops.subprocess, "Popen", popen)
    monkeypatch.setattr(ops.subprocess, "run", run)
    archive = ops.backup(seeded, tmp_path / "b")
    assert ops.restore(fresh, archive)["ok"]
    # the URL form of the password (user:password@); the user name may spell the same word
    assert len(seen) >= 2 and not [c for c in seen if any(f":{password}@" in a for a in c)], seen
