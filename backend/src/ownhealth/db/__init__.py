"""Database access: one SQLAlchemy Core engine (psycopg 3) and the migration runner."""

from __future__ import annotations

from functools import lru_cache

from sqlalchemy import Engine, create_engine

from ownhealth.config import Settings, sqlalchemy_url


def make_engine(url: str, **kwargs) -> Engine:
    """Engine for a postgresql:// URL. Services take a Connection from `engine.begin()`."""
    return create_engine(sqlalchemy_url(url), pool_pre_ping=True, future=True, **kwargs)


@lru_cache(maxsize=4)
def _cached_engine(url: str) -> Engine:
    return make_engine(url)


def get_engine(settings: Settings) -> Engine:
    return _cached_engine(settings.database_url)
