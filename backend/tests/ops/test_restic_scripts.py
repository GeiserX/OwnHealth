"""The restic recipe in deploy/restic/, run end to end against a local restic repository.

OWNHEALTH_RUN="" makes the scripts run their commands here instead of in the app container. Needs
restic on PATH (CI installs the version the image ships); skipped without it, except in CI.
"""

from __future__ import annotations

import json
import os
import shutil
import subprocess
import sys
from pathlib import Path

import pytest

from ownhealth import ops

RESTIC = shutil.which("restic")
DEPLOY = Path(__file__).resolve().parents[3] / "deploy" / "restic"
pytestmark = pytest.mark.skipif(RESTIC is None and not os.environ.get("CI"), reason="restic not on PATH")


@pytest.fixture(autouse=True)
def _restic_on_path_in_ci():
    if RESTIC is None:  # only reached in CI, where a missing restic is a broken runner, not a skip
        pytest.fail("restic is not on PATH: CI must install the version the image ships")


@pytest.fixture
def env_for(tmp_path):
    repo = tmp_path / "repo"

    def _env(settings) -> dict[str, str]:
        env = dict(os.environ)
        env.update(
            OWNHEALTH_RUN="",
            OWNHEALTH_DATABASE_URL=settings.database_url,
            OWNHEALTH_DATA_DIR=str(settings.data_dir),
            OWNHEALTH_BACKUP_STAGING=str(tmp_path / "staging"),
            OWNHEALTH_RESTORE_DIR=str(tmp_path / "restore"),
            RESTIC_REPOSITORY=str(repo),
            RESTIC_PASSWORD="test-only-password",
            RESTIC_CACHE_DIR=str(tmp_path / "cache"),
            PATH=str(Path(sys.executable).parent) + os.pathsep + env["PATH"],
        )
        return env

    return _env


def _run(script: str, env, *args) -> subprocess.CompletedProcess:
    return subprocess.run([str(DEPLOY / script), *args], env=env, capture_output=True, text=True, timeout=300)


def test_backup_then_restore_test_then_restore_into_a_fresh_stack(seeded, fresh, env_for, tmp_path):
    env = env_for(seeded)
    subprocess.run([RESTIC, "init", "--quiet"], env=env, check=True, capture_output=True)

    done = _run("backup.sh", env)
    assert done.returncode == 0, done.stderr
    snaps = json.loads(subprocess.run([RESTIC, "snapshots", "--json"], env=env, capture_output=True, check=True).stdout)
    assert len(snaps) == 1 and snaps[0]["tags"] == ["ownhealth"]
    assert not (tmp_path / "staging").exists()

    check = _run("restore-test.sh", env)
    assert check.returncode == 0, check.stdout + check.stderr
    report = json.loads(check.stdout)
    assert report["ok"] and len(report["profiles"]) == 4
    assert not (tmp_path / "restore").exists()

    restored = _run("restore.sh", env_for(fresh))
    assert restored.returncode == 0, restored.stdout + restored.stderr
    fresh_report = json.loads(restored.stdout)
    assert fresh_report["ok"]
    assert {p: r["actual"] for p, r in fresh_report["profiles"].items()} == {
        p: r["expected"] for p, r in report["profiles"].items()
    }


def test_restore_test_fails_when_the_latest_snapshot_does_not_match_its_manifest(seeded, env_for, tmp_path):
    env = env_for(seeded)
    subprocess.run([RESTIC, "init", "--quiet"], env=env, check=True, capture_output=True)
    staging = tmp_path / "staging"
    ops.backup(seeded, staging)
    m = json.loads((staging / "manifest.json").read_text())
    m["state_hash"][sorted(m["state_hash"])[0]] = "sha256:" + "0" * 64
    (staging / "manifest.json").write_text(json.dumps(m))
    tag = ["--host", "ownhealth", "--tag", "ownhealth", "--quiet"]
    subprocess.run([RESTIC, "backup", *tag, str(staging)], env=env, check=True, capture_output=True)

    check = _run("restore-test.sh", env)
    assert check.returncode == 1, check.stdout + check.stderr
    assert json.loads(check.stdout)["ok"] is False
    assert "FAILED" in check.stderr


def test_restore_refuses_a_stack_that_already_has_data(seeded, env_for):
    env = env_for(seeded)
    subprocess.run([RESTIC, "init", "--quiet"], env=env, check=True, capture_output=True)
    assert _run("backup.sh", env).returncode == 0
    done = _run("restore.sh", env)
    assert done.returncode != 0 and "not empty" in done.stderr
