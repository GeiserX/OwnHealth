"""A PAT works only on /mcp. Over REST it would skip the review queue (accept its own proposal,
import, retract), the rate limit, the egress budget, the genome and page caps and the identity mode."""

import pytest
from sqlalchemy import text

from ownhealth.auth.tokens import create_token
from ownhealth.core.access import ALL_SCOPES

ROUTES = [
    ("get", "/api/review"),
    ("get", "/api/review/00000000-0000-0000-0000-000000000000"),
    ("post", "/api/review/00000000-0000-0000-0000-000000000000/accept"),
    ("post", "/api/review/00000000-0000-0000-0000-000000000000/reject"),
    ("post", "/api/review/00000000-0000-0000-0000-000000000000/reopen"),
    ("post", "/api/review/00000000-0000-0000-0000-000000000000/dismiss"),
    ("post", "/api/review/bulk-accept"),
    ("post", "/api/imports"),
    ("post", "/api/documents/00000000-0000-0000-0000-000000000000/assign"),
    ("post", "/api/documents/00000000-0000-0000-0000-000000000000/retract"),
    ("get", "/api/documents/00000000-0000-0000-0000-000000000000/file"),
    ("post", "/api/aliases"),
    ("get", "/api/profiles/alex/observations"),
    ("get", "/api/profiles/alex/status?identity_mode=full"),
    ("get", "/api/profiles/alex/genome/variant?rsid=rs429358"),
    ("get", "/api/sources/00000000-0000-0000-0000-000000000000"),
]


@pytest.fixture
def pat(engine, committed_family):
    with engine.begin() as c:
        tok = create_token(
            c,
            user_id=committed_family.users["alex"],
            label="agent",
            profile_ids=[committed_family.profiles["alex"]],
            scopes=sorted(ALL_SCOPES),
        )
    return tok.plaintext


@pytest.mark.parametrize(("method", "path"), ROUTES)
def test_pat_is_refused_on_rest(client_for, pat, engine, method, path):
    with engine.connect() as c:
        before = c.execute(text("select (select count(*) from job), (select count(*) from review_item)")).one()
    r = getattr(client_for(), method)(
        path, headers={"Authorization": f"Bearer {pat}"}, **({"json": {}} if method == "post" else {})
    )
    assert r.status_code == 403 and r.json()["detail"]["code"] == "forbidden", (path, r.status_code, r.text)
    with engine.connect() as c:
        after = c.execute(text("select (select count(*) from job), (select count(*) from review_item)")).one()
    assert after == before


def test_session_still_reaches_rest(client_for, committed_family):
    c = client_for(committed_family.users["alex"])
    assert c.get("/api/review").status_code == 200
    assert c.get("/api/profiles/alex/observations").status_code == 200
