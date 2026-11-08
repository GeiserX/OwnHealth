import json

from sqlalchemy import text

from ownhealth.demo.pdf import LabRow, lab_report_pdf


def test_health_and_spa_fallback(app_for):
    client = app_for()
    assert client.get("/healthz").json()["status"] == "ok"
    assert client.get("/readyz").json() == {"status": "ready"}
    assert "OwnHealth" in client.get("/genome").text  # SPA fallback to index.html
    assert "OwnHealth" in client.get("/mcp-access").text  # a UI route that starts with "mcp"
    assert client.get("/api/nope").status_code == 404
    assert client.get("/api/me").status_code == 401  # no session, no dev auth


def test_me_profiles_and_roles(app_for, committed_family):
    client = app_for(committed_family.users["alex"])
    me = client.get("/api/me").json()
    assert [p["slug"] for p in me["profiles"]] == ["alex", "sam", "lucia", "carmen"]  # the mockup's order
    roles = {p["slug"]: p["role"] for p in me["profiles"]}
    assert roles == {"alex": "owner", "carmen": "caregiver", "lucia": "guardian", "sam": "viewer"}
    lucia = next(p for p in me["profiles"] if p["slug"] == "lucia")
    assert lucia["is_minor"] and lucia["initials"] == "LR"
    sam = app_for(committed_family.users["sam"]).get("/api/profiles").json()
    assert {p["slug"] for p in sam} == {"alex", "sam", "lucia"}


def test_upload_pdf_enqueues_import_and_review_routes(app_for, committed_family, engine):
    client = app_for(committed_family.users["alex"])
    pdf, _ = lab_report_pdf(
        lab="LabNorte",
        title="Informe",
        patient="RIVERA, ALEX",
        birth="14/05/1979",
        collected="14/09/2026",
        report_id="LN-API",
        pages=[[LabRow("Glucosa", "92", "mg/dL")]],
    )
    files = [("files", ("labnorte.pdf", pdf, "application/pdf"))]
    r = client.post("/api/imports", data={"profile": "alex"}, files=files)
    assert r.status_code == 200, r.text
    item = r.json()["items"][0]
    assert item["kind"] == "pdf" and item["job_id"] and not item["duplicate"]
    again = client.post(
        "/api/imports", data={"profile": "alex"}, files=[("files", ("copy.pdf", pdf, "application/pdf"))]
    ).json()["items"][0]
    assert again["duplicate"] and again["document_id"] == item["document_id"]
    jobs = client.get("/api/jobs", params={"ids": str(item["job_id"])}).json()["jobs"]
    assert jobs[0]["kind"] == "import.pdf" and jobs[0]["state"] == "queued"
    # viewers cannot import
    assert (
        client.post(
            "/api/imports", data={"profile": "sam"}, files=[("files", ("x.pdf", pdf, "application/pdf"))]
        ).status_code
        == 403
    )
    bad = client.post("/api/imports", data={"profile": "alex"}, files=[("files", ("x.txt", b"hello", "text/plain"))])
    assert bad.status_code == 415
    with engine.begin() as c:
        c.execute(text("update job set state = 'done' where id = :i"), {"i": item["job_id"]})


def test_review_api_and_source_lookup(app_for, committed_family, engine, blobs):
    from factories import lab_document, obs

    from ownhealth.core import review
    from ownhealth.core.acceptance import Check
    from ownhealth.core.review import CandidateIn

    with engine.begin() as c:
        doc_id, boxes = lab_document(c, blobs, committed_family)
        run = review.create_extraction_run(c, doc_id, extractor="rules:test", extractor_version="1", config_hash=b"x")
        res = review.submit_candidate(
            c,
            run,
            CandidateIn(
                page_no=1,
                bbox=boxes[0],
                cells={},
                row_conf=0.99,
                observation=obs(committed_family.profiles["alex"]),
                checks=[Check(name="loinc", ok=False, reason="loinc_unknown")],
            ),
            allowance="rules_text",
            identity="matched",
        )
    alex = app_for(committed_family.users["alex"])
    listed = alex.get("/api/review").json()
    assert any(i["id"] == str(res.review_item_id) for i in listed["items"]) and listed["open"] >= 1
    out = alex.post(f"/api/review/{res.review_item_id}/accept", json={"edits": None}).json()
    assert out["status"] == "accepted" and len(out["result_ids"]) == 1
    with engine.connect() as c:
        src_id = c.execute(text("select src_id from observation where id = :i"), {"i": out["result_ids"][0]}).scalar()
    src = alex.get(f"/api/sources/{src_id}").json()
    assert src["location"]["page"] == 1 and src["status"] == "current" and src["document"]["id"] == str(doc_id)
    # Sam is a viewer of Alex (read:summary only): a lab source needs read:labs, and he cannot review.
    sam = app_for(committed_family.users["sam"])
    denied = sam.get(f"/api/sources/{src_id}")
    assert denied.status_code == 403 and "document" not in denied.text
    assert alex.post(f"/api/review/{res.review_item_id}/accept", json={}).status_code == 409
    assert sam.get("/api/sources/src_doesnotexist").status_code == 404
    with engine.connect() as c:
        actions = (
            c.execute(text("select action from audit_event where actor_id = :u"), {"u": committed_family.users["alex"]})
            .scalars()
            .all()
        )
    assert "review_accept" in actions and "read" in actions


def test_mcp_mounted_at_mcp(app_for, committed_family, engine):
    from ownhealth.auth.tokens import create_token

    client = app_for()
    headers = {
        "Accept": "application/json, text/event-stream",
        "Content-Type": "application/json",
        "MCP-Protocol-Version": "2025-11-25",
    }
    init = {
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {"protocolVersion": "2025-11-25", "capabilities": {}, "clientInfo": {"name": "test", "version": "0"}},
    }
    r = client.post("/mcp", content=json.dumps(init), headers=headers)
    assert r.status_code == 401 and "Bearer" in r.headers["www-authenticate"]  # a PAT is required
    with engine.begin() as c:
        tok = create_token(
            c,
            user_id=committed_family.users["alex"],
            label="test",
            profile_ids=[committed_family.profiles["alex"]],
        )
    headers["Authorization"] = f"Bearer {tok.plaintext}"
    r = client.post("/mcp", content=json.dumps(init), headers=headers)
    assert r.status_code == 200, r.text
    assert r.json()["result"]["serverInfo"]["name"] == "OwnHealth"
    r = client.post("/mcp", content=json.dumps({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}), headers=headers)
    assert "get_health_status" in [t["name"] for t in r.json()["result"]["tools"]]


def test_jobs_limit_is_checked_at_the_edge(app_for, committed_family):
    client = app_for(committed_family.users["alex"])
    for bad in (-1, 0, 201):
        assert client.get("/api/jobs", params={"limit": bad}).status_code == 422, bad
    assert client.get("/api/jobs", params={"limit": 200}).status_code == 200
