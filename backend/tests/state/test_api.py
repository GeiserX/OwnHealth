"""REST: status, dashboard, rules and drug-check, with scopes, the 404 for an unknown drug and audit rows."""

import pytest
from fastapi.testclient import TestClient
from sqlalchemy import text

from ownhealth.api._deps import get_principal
from ownhealth.app import create_app
from ownhealth.config import Settings
from ownhealth.core.access import ALL_SCOPES, Principal


@pytest.fixture
def client_as(seeded, fake_genome, tmp_path):
    static = tmp_path / "static"
    static.mkdir()
    (static / "index.html").write_text("<!doctype html>")
    settings = Settings(database_url=str(seeded.engine.url), data_dir=tmp_path / "data", static_dir=static,
                        dev_auth=False, run_jobs=False, run_migrations=False, secret_key="test-secret")  # fmt: skip
    clients = []

    def _make(user: str, scopes=ALL_SCOPES) -> TestClient:
        app = create_app(settings, seeded.engine)
        p = Principal(kind="session", user_id=seeded.family.users[user], household_id=seeded.family.household_id,
                      scopes=scopes)  # fmt: skip
        app.dependency_overrides[get_principal] = lambda: p
        c = TestClient(app)
        c.__enter__()
        clients.append(c)
        return c

    yield _make
    for c in clients:
        c.__exit__(None, None, None)


def _audits(seeded, slug, tool):
    with seeded.engine.connect() as c:
        return c.execute(text("select resource_ids, args_summary, src_ids from audit_event where profile_id = :p"
                              " and tool = :t order by id"), {"p": seeded.pid(slug), "t": tool}).all()  # fmt: skip


def test_status_is_the_rendered_state_and_is_audited(client_as, seeded):
    r = client_as("alex").get("/api/profiles/alex/status", params={"as_of": "2026-11-07", "lang": "en"})
    assert r.status_code == 200, r.text
    body = r.json()
    assert body["state_hash"].startswith("sha256:") and body["data"]["flags"]
    es = client_as("alex").get("/api/profiles/alex/status", params={"as_of": "2026-11-07", "lang": "es"}).json()
    assert es["state_hash"] == body["state_hash"] and es["data"]["flags"][0]["text"] != body["data"]["flags"][0]["text"]
    rows = _audits(seeded, "alex", "GET /api/profiles/{p}/status")
    assert rows and rows[-1].resource_ids == [body["state_hash"]]


def test_dashboard_and_rules(client_as):
    d = client_as("alex").get("/api/profiles/alex/dashboard", params={"as_of": "2026-11-07", "lang": "en"})
    assert d.status_code == 200, d.text
    assert {s["key"]: s["status"] for s in d.json()["systems"]}["cardiometabolic"] == "out"
    rules = client_as("alex").get("/api/rules").json()
    assert rules["version"].startswith("ownhealth-rules ") and rules["cpic"]["license"].startswith("CC0")
    assert all(r["cite"].startswith("https://") for r in rules["rules"])


def test_drug_check_owner_and_unknown_drug(client_as, seeded):
    c = client_as("alex")
    r = c.get("/api/profiles/alex/drug-check", params={"drug": "clopidogrel"})
    assert r.status_code == 200, r.text
    assert r.json()["guidance"] and r.json()["pgx"][0]["gene"] == "CYP2C19"
    rows = _audits(seeded, "alex", "GET /api/profiles/{p}/drug-check")
    assert rows[-1].args_summary and "clopidogrel" not in str(rows[-1].args_summary)  # hashed, not the name
    assert "src_g07" in rows[-1].src_ids
    u = c.get("/api/profiles/alex/drug-check", params={"drug": "notadrugzz"})
    assert u.status_code == 404 and u.json()["detail"]["code"] == "drug_unknown"


def test_a_viewer_gets_the_summary_but_no_drug_check(client_as):
    sam = client_as("sam")
    assert sam.get("/api/profiles/alex/drug-check", params={"drug": "clopidogrel"}).status_code == 403
    d = sam.get("/api/profiles/alex/dashboard", params={"as_of": "2026-11-07"})
    assert d.status_code == 200, d.text
    assert all(card["withheld"] for card in d.json()["systems"])  # read:summary only: no lab values
    s = sam.get("/api/profiles/alex/status", params={"as_of": "2026-11-07"}).json()
    assert s["withheld"]["by_scope"]


def test_no_grant_no_status(client_as):
    assert client_as("sam").get("/api/profiles/carmen/status").status_code in (403, 404)
