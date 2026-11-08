"""Lane G fixtures: an app with the real auth stack (no principal override) on https://testserver."""

from __future__ import annotations

import dataclasses

import pytest
from fastapi.testclient import TestClient
from sqlalchemy import Engine

from ownhealth.auth.sessions import COOKIE, create_session
from ownhealth.db import make_engine
from ownhealth.db.migrate import migrate

BASE = "https://testserver"


@pytest.fixture
def https_settings(settings):
    return dataclasses.replace(settings, base_url=BASE)


@pytest.fixture
def client_for(https_settings, engine):
    """client_for(user_id=None, *, step_up=True, method="passkey", eng=None, settings=None) -> TestClient
    logged in through a real session cookie (or anonymous with user_id=None)."""
    from ownhealth.app import create_app

    clients: list[TestClient] = []

    def _make(user_id=None, *, step_up=True, method="passkey", eng: Engine | None = None, settings=None):
        eng = eng or engine
        s = settings or https_settings
        app = create_app(s, eng)
        client = TestClient(app, base_url=s.base_url if s.base_url.startswith("http") else BASE)
        client.__enter__()
        clients.append(client)
        if user_id is not None:
            with eng.begin() as c:
                secret = create_session(c, user_id, ip=None, user_agent="pytest", method=method, step_up=step_up)
            client.cookies.set(COOKIE, secret)
        return client

    yield _make
    for c in clients:
        c.__exit__(None, None, None)


@pytest.fixture
def fresh_engine(make_database):
    """An empty, migrated database: for first-run setup, which needs an instance with no user."""
    eng = make_engine(make_database())
    migrate(eng)
    yield eng
    eng.dispose()
