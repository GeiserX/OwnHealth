"""Process-wide handles (settings, engine, blob store) for code that runs outside a request,
such as MCP tools and job handlers. `create_app` and the CLI call `init()`; everything else calls
`get()`.
"""

from __future__ import annotations

from dataclasses import dataclass

from sqlalchemy import Engine

from ownhealth.blobs import BlobStore
from ownhealth.config import Settings


@dataclass
class Runtime:
    settings: Settings
    engine: Engine
    blobs: BlobStore


_current: Runtime | None = None


def init(settings: Settings, engine: Engine | None = None) -> Runtime:
    global _current
    from ownhealth.db import get_engine

    _current = Runtime(settings=settings, engine=engine or get_engine(settings), blobs=BlobStore(settings.blobs_path))
    return _current


def get() -> Runtime:
    if _current is None:
        raise RuntimeError("ownhealth.runtime.init() has not run")
    return _current
