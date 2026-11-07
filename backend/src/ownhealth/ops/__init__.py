"""Lane J: backup, restore, verify-restore and metrics (architecture.md 14-15). Contract stubs."""

from __future__ import annotations

from pathlib import Path

from ownhealth.config import Settings


def backup(settings: Settings, dest: Path) -> Path:
    raise NotImplementedError("lane J: ops.backup")


def restore(settings: Settings, archive: Path) -> None:
    raise NotImplementedError("lane J: ops.restore")


def verify_restore(settings: Settings, archive: Path) -> dict:
    """Restores into a scratch database and returns the state_hash per profile for comparison."""
    raise NotImplementedError("lane J: ops.verify_restore")
