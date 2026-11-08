"""What the two compose files hand the app container, rendered by `docker compose config`.

- The app listens on 0.0.0.0. OWNHEALTH_HOST is its bind address (config.py), so the public name in
  the homelab .env must not reach it under that name: health.example.lan as a bind address crash-loops.
- The app connects as the non-owner role and does not migrate; the admin service migrates as the owner.

Run from backend/: `uv run pytest ../deploy/tests`. Skipped without docker compose.
"""

from __future__ import annotations

import json
import os
import shutil
import subprocess
from pathlib import Path

import pytest

from ownhealth.config import Settings

ROOT = Path(__file__).resolve().parents[2]
pytestmark = pytest.mark.skipif(shutil.which("docker") is None, reason="docker compose not available")

# The image's own ENV (Dockerfile); compose's env_file and environment override it.
IMAGE_ENV = {"OWNHEALTH_DATA_DIR": "/data", "OWNHEALTH_HOST": "0.0.0.0", "OWNHEALTH_PORT": "8080"}


def _render(compose: Path, env_file: Path, tmp_path: Path, extra_env: str = "", files: dict | None = None) -> dict:
    (tmp_path / "compose.yaml").write_text(compose.read_text())
    (tmp_path / ".env").write_text(env_file.read_text() + extra_env)
    for name, body in (files or {}).items():
        (tmp_path / name).write_text(body)
    done = subprocess.run(
        ["docker", "compose", "config", "--format", "json"],
        cwd=tmp_path,
        capture_output=True,
        text=True,
        timeout=60,
        env={"PATH": os.environ["PATH"], "HOME": os.environ.get("HOME", "/tmp")},
    )
    assert done.returncode == 0, done.stderr
    return json.loads(done.stdout)["services"]


CASES = {
    "homelab": (ROOT / "deploy" / "compose.homelab.yaml", ROOT / "deploy" / "env.homelab.example", ""),
    "quickstart": (
        ROOT / "compose.yaml",
        ROOT / ".env.example",
        "OWNHEALTH_DB_PASSWORD=x\nOWNHEALTH_APP_DB_PASSWORD=y\n",
    ),
}


@pytest.mark.parametrize("case", CASES)
def test_app_binds_all_interfaces(case, tmp_path, monkeypatch):
    compose, env_file, extra = CASES[case]
    app = _render(compose, env_file, tmp_path, extra)["app"]
    for key in [k for k in os.environ if k.startswith("OWNHEALTH_")]:
        monkeypatch.delenv(key)
    for key, value in {**IMAGE_ENV, **app["environment"]}.items():
        if key.startswith("OWNHEALTH_") and not key.endswith("_FILE"):
            monkeypatch.setenv(key, value or "")
    assert Settings.from_env().host == "0.0.0.0"


@pytest.mark.parametrize("case", CASES)
def test_app_runs_as_the_non_owner_role_and_admin_migrates_as_owner(case, tmp_path):
    compose, env_file, extra = CASES[case]
    services = _render(compose, env_file, tmp_path, extra)
    app, admin = services["app"], services["admin"]
    assert app["environment"]["OWNHEALTH_DATABASE_URL"].startswith("postgresql://ownhealth_app@")
    assert app["environment"]["OWNHEALTH_RUN_MIGRATIONS"] == "0"
    assert app["depends_on"]["admin"]["condition"] == "service_completed_successfully"
    assert admin["environment"]["OWNHEALTH_DATABASE_URL"].startswith("postgresql://ownhealth@")
    assert "app-role.sh" in " ".join(admin["command"])


def test_homelab_app_gets_no_backup_credentials(tmp_path):
    """restic.env holds the backup repository and its S3 keys, for the admin service only. The app is
    the network-facing process, so a compromise of it must not hand over the keys to the backups."""
    compose, env_file, _ = CASES["homelab"]
    restic_env = (ROOT / "deploy" / "restic.env.example").read_text()
    restic_env += "AWS_ACCESS_KEY_ID=synthetic-key\nAWS_SECRET_ACCESS_KEY=synthetic-secret\n"
    services = _render(compose, env_file, tmp_path, files={"restic.env": restic_env})
    app_env, admin_env = services["app"]["environment"], services["admin"]["environment"]
    assert sorted(k for k in app_env if k.startswith(("RESTIC_", "AWS_"))) == []
    assert admin_env["AWS_ACCESS_KEY_ID"] == "synthetic-key"
    assert admin_env["RESTIC_REPOSITORY"].startswith("s3:")
    assert app_env["OWNHEALTH_DEFAULT_LOCALE"] == "es"


def test_homelab_without_restic_env_still_renders(tmp_path):
    """An install made before restic.env existed keeps RESTIC_REPOSITORY in .env; the admin service
    must still get it, and compose must not fail on the missing file."""
    compose, env_file, _ = CASES["homelab"]
    services = _render(compose, env_file, tmp_path, "RESTIC_REPOSITORY=s3:https://s3.example.lan/old\n")
    assert services["admin"]["environment"]["RESTIC_REPOSITORY"] == "s3:https://s3.example.lan/old"
