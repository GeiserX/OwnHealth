"""Make `auth_python_rows.json`: login rows written by OwnHealth 0.1.1 (Python), for the Rust
compatibility test (`auth_python_rows.rs`).

Run from the repository root with the test Postgres up (scripts/test-db.sh):

    backend/.venv/bin/python server/crates/ownhealth-api/tests/auth_python_rows.py

It sets up a fresh instance through the real 0.1.1 REST API with a software passkey (so py_webauthn
verifies and stores the credential), sets a password, makes a session and a PAT, and dumps every
row they wrote together with the plaintexts and the passkey's private key. Synthetic data only.
"""

from __future__ import annotations

import base64
import json
import os
import secrets
import sys
import tempfile
from pathlib import Path

HERE = Path(__file__).resolve().parent
ROOT = HERE.parents[3]
sys.path[:0] = [str(ROOT / "backend" / "src"), str(ROOT / "backend" / "tests")]

from cryptography.hazmat.primitives.asymmetric import ec  # noqa: E402
from fastapi.testclient import TestClient  # noqa: E402
from sqlalchemy import text  # noqa: E402

from ownhealth.app import create_app  # noqa: E402
from ownhealth.auth import password, sessions, tokens  # noqa: E402
from ownhealth.config import Settings  # noqa: E402
from ownhealth.db import make_engine  # noqa: E402
from ownhealth.db.migrate import migrate  # noqa: E402
from security.softkey import SoftKey  # noqa: E402

ADMIN = os.environ.get("OWNHEALTH_TEST_ADMIN_URL", "postgresql://postgres:postgres@127.0.0.1:55432/postgres")
TABLES = ["household", "app_user", "profile", "profile_grant", "passkey", "recovery_code", "user_session", "api_token"]
PASSWORD = "carmen-ortega-labnorte-2026"


def main() -> None:
    name = f"oh_test_{secrets.token_hex(6)}"
    admin = make_engine(ADMIN, isolation_level="AUTOCOMMIT")
    with admin.connect() as c:
        c.execute(text(f'create database "{name}"'))
    try:
        eng = make_engine(ADMIN.rsplit("/", 1)[0] + "/" + name)
        migrate(eng)
        with tempfile.TemporaryDirectory() as tmp:
            static = Path(tmp) / "static"
            static.mkdir()
            (static / "index.html").write_text("<!doctype html><title>OwnHealth</title>")
            settings = Settings(
                database_url=eng.url.render_as_string(hide_password=False),
                data_dir=Path(tmp) / "data",
                static_dir=static,
                dev_auth=False,
                run_jobs=False,
                run_migrations=False,
                secret_key="test-secret",
                base_url="https://testserver",
            )
            out = generate(eng, settings)
        eng.dispose()
    finally:
        with admin.connect() as c:
            c.execute(text(f'drop database if exists "{name}" with (force)'))
        admin.dispose()
    (HERE / "auth_python_rows.json").write_text(json.dumps(out, indent=2, sort_keys=True) + "\n")


def generate(eng, settings) -> dict:
    key = SoftKey()
    with TestClient(create_app(settings, eng), base_url="https://testserver") as client:
        opts = client.post(
            "/api/auth/setup/begin", json={"household_name": "Ortega", "display_name": "Carmen Ortega"}
        ).json()
        r = client.post("/api/auth/setup/finish", json={"credential": key.create(opts), "label": "laptop"})
        assert r.status_code == 200, r.text
        setup = r.json()
    uid = setup["user_id"]
    with eng.begin() as c:
        password.set_password(c, uid, PASSWORD)
        session = sessions.create_session(c, uid, ip=None, user_agent="python 0.1.1", method="passkey")
        pid = c.execute(text("select id from profile where user_id = :u"), {"u": uid}).scalar()
        token = tokens.create_token(
            c, user_id=uid, label="Claude Desktop", profile_ids=[pid], scopes=["read:summary", "read:labs"]
        )
    with eng.connect() as c:
        rows = {
            t: [r[0] for r in c.execute(text(f"select to_jsonb(t) from {t} t order by 1::text"))] for t in TABLES
        }
    nums = key.key.private_numbers()
    return {
        "generated_by": "OwnHealth 0.1.1 (Python), auth_python_rows.py",
        "rows": rows,
        "user_id": uid,
        "password": PASSWORD,
        "recovery_codes": setup["recovery_codes"],
        "session_secret": session,
        "token_plaintext": token.plaintext,
        "passkey": {
            "credential_id": base64.urlsafe_b64encode(key.cred_id).rstrip(b"=").decode(),
            "private_scalar_hex": nums.private_value.to_bytes(32, "big").hex(),
            "sign_count": key.count,
            "user_handle": base64.urlsafe_b64encode(key.user_handle or b"").rstrip(b"=").decode(),
        },
    }


if __name__ == "__main__":
    assert isinstance(ec.SECP256R1(), ec.EllipticCurve)
    main()
