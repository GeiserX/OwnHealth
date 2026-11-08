"""REST: records (list, add, correct, retract), manual observations, and the wearables views."""

from datetime import UTC, date, datetime

from sqlalchemy import text

from ownhealth.core.access import ALL_SCOPES, Principal


def test_records_add_correct_retract_with_history(app_for, committed_family, engine):
    fam = committed_family
    client = app_for(fam.users["alex"])
    r = client.post(
        "/api/profiles/lucia/records/allergy",
        json={"substance": "Peanut", "criticality": "high", "reaction_text": "Hives"},
    )
    assert r.status_code == 201, r.text
    first = r.json()
    assert first["title"] == "Peanut" and first["status"] == "current" and first["src"].startswith("src_")
    r = client.post(
        f"/api/profiles/lucia/records/allergy/{first['id']}/correct",
        json={"substance": "Peanut", "criticality": "low", "reason": "allergist re-test"},
    )
    assert r.status_code == 201, r.text
    second = r.json()
    assert second["supersedes"] == first["id"] and second["detail"]["criticality"] == "low"
    items = client.get("/api/profiles/lucia/records/allergy").json()["items"]
    assert [i["id"] for i in items] == [second["id"]]
    hist = client.get("/api/profiles/lucia/records/allergy?history=true").json()["items"]
    assert [(i["id"], i["status"]) for i in hist] == [(second["id"], "current"), (first["id"], "superseded")]
    assert hist[1]["status_reason"] == "allergist re-test"
    # retract the current version with a reason; nothing is deleted
    r = client.post(f"/api/profiles/lucia/records/allergy/{second['id']}/retract", json={"reason": "entered in error"})
    assert r.status_code == 200 and r.json()["status"] == "retracted"
    assert client.get("/api/profiles/lucia/records/allergy").json()["items"] == []
    # correcting a retracted version is a lifecycle conflict
    r = client.post(f"/api/profiles/lucia/records/allergy/{second['id']}/correct", json={"substance": "Peanut"})
    assert r.status_code == 409
    with engine.connect() as c:
        n = c.execute(text("select count(*) from allergy where profile_id = :p"), {"p": fam.profiles["lucia"]}).scalar()
        actions = c.execute(
            text("select action from audit_event where profile_id = :p and tool = 'records' order by id"),
            {"p": fam.profiles["lucia"]},
        ).scalars()
        assert n == 2 and list(actions) == ["write", "correct", "read", "read", "retract", "read"]


def test_record_validation_and_permissions(app_for, committed_family):
    fam = committed_family
    alex = app_for(fam.users["alex"])
    assert alex.post("/api/profiles/lucia/records/medication", json={"strength": "5 mg"}).status_code == 422
    assert alex.get("/api/profiles/lucia/records/vaccine").status_code == 422  # unknown kind
    # Alex is a viewer on Sam: reads the summary only, writes nothing
    assert alex.post("/api/profiles/sam/records/condition", json={"text": "Migraine"}).status_code == 403
    assert alex.get("/api/profiles/sam/records/condition").status_code == 403
    # Sam has no grant on Carmen
    sam = app_for(fam.users["sam"])
    r = sam.get("/api/profiles/carmen/records/condition")
    assert r.status_code == 404 and r.json()["detail"]["code"] == "profile_not_granted"
    # a personal access token cannot write over REST, whatever its scopes
    with_token = app_for(
        principal=Principal(
            kind="token", user_id=fam.users["alex"], household_id=fam.household_id, scopes=ALL_SCOPES, token_id=None
        )
    )
    r = with_token.post("/api/profiles/alex/records/condition", json={"text": "Migraine"})
    assert r.status_code == 403 and "propose" in r.json()["detail"]["message"]


def test_manual_observation_and_its_correction(app_for, committed_family, engine):
    fam = committed_family
    client = app_for(fam.users["alex"])
    body = {
        "name": "Peso",
        "value": "81,4",
        "unit": "kg",
        "effective_at": "2026-11-06T08:00:00+01:00",
        "category": "vital-signs",
        "note": "after breakfast",
    }
    r = client.post("/api/profiles/alex/manual-observations", json=body)
    assert r.status_code == 201, r.text
    first = r.json()
    r = client.post(
        "/api/profiles/alex/manual-observations",
        json={**body, "value": "80,4", "corrects": first["id"], "reason": "typo"},
    )
    assert r.status_code == 201, r.text
    assert r.json()["supersedes"] == first["id"]
    with engine.connect() as c:
        rows = c.execute(
            text(
                "select o.status, o.status_reason, o.accepted_by, o.source_kind, o.category, o.value_num,"
                " o.reviewed_by, s.kind, s.actor_id from observation o join src s on s.id = o.src_id"
                " where o.profile_id = :p order by o.recorded_at, o.supersedes nulls first"
            ),
            {"p": fam.profiles["alex"]},
        ).all()
        assert [(x.status, str(x.value_num)) for x in rows] == [("superseded", "81.4"), ("current", "80.4")]
        assert rows[0].status_reason == "typo"
        assert {(x.accepted_by, x.source_kind, x.category, x.kind) for x in rows} == {
            ("person", "self_reported", "vital-signs", "manual")
        }
        assert all(x.actor_id == fam.users["alex"] and x.reviewed_by == fam.users["alex"] for x in rows)
        note = c.execute(text("select body, attach_to from note where attach_to = :s"), {"s": first["src"]}).one()
        assert note.body == "after breakfast"
    # no offset, or someone else's observation, is refused
    assert (
        client.post(
            "/api/profiles/alex/manual-observations", json={**body, "effective_at": "2026-11-06T08:00:00"}
        ).status_code
        == 422
    )
    assert (
        client.post(
            "/api/profiles/lucia/manual-observations", json={**body, "corrects": first["id"], "reason": "x"}
        ).status_code
        == 404
    )


def test_wearables_summary_and_series(app_for, committed_family, engine, blobs, settings, tmp_path):
    from ownhealth.core.documents import create_document
    from ownhealth.ingest.apple_health import import_apple_health
    from ownhealth.ingest.apple_health.synthetic import ExportSpec, write_export

    fam = committed_family
    end = date(2026, 11, 7)
    path = write_export(
        tmp_path / "e.zip", ExportSpec(days=40, end=end, birth_date=date(2017, 6, 21), first_name="Lucía")
    )
    sha, size = blobs.put_file(path)
    with engine.begin() as c:
        doc = create_document(
            c,
            household_id=fam.household_id,
            profile_id=fam.profiles["lucia"],
            blob_sha256=sha,
            mime="application/zip",
            size_bytes=size,
            filename="export.zip",
        ).id
        import_apple_health(c, blobs, document_id=doc)
    client = app_for(fam.users["sam"])
    r = client.get("/api/profiles/lucia/wearables/summary", params={"as_of": "2026-11-07", "days": 30, "lang": "en"})
    assert r.status_code == 200, r.text
    body = r.json()
    metrics = {m["metric"]: m for m in body["metrics"]}
    assert list(metrics)[:4] == ["sleep_asleep", "hrv_sdnn", "hr_resting", "steps"]
    steps = metrics["steps"]
    assert steps["label"] == "Steps" and steps["source"] == "Apple Watch" and len(steps["series"]) == 30
    assert steps["series"][-1]["date"] == "2026-11-07" and steps["latest"] == steps["series"][-1]["value"]
    values = [pt["value"] for pt in steps["series"]]
    assert steps["avg_30d"] == round(sum(values) / 30, 1)
    assert metrics["hr_resting"]["coverage_pct"] == 100.0
    assert {d["device"] for d in body["devices"]} == {"Apple Watch", "iPhone"}
    es = client.get("/api/profiles/lucia/wearables/summary", params={"as_of": "2026-11-07"}).json()
    assert es["metrics"][0]["label"] == "Duración del sueño"
    r = client.get("/api/profiles/lucia/wearables/steps", params={"window": "P7D", "as_of": "2026-11-07"})
    pts = r.json()["points"]
    assert len(pts) == 7 and {p["device"] for p in pts} == {"Apple Watch"} and len(r.json()["priority"]) == 2
    hourly = client.get(
        "/api/profiles/lucia/wearables/steps", params={"window": "P1D", "resolution": "hour", "as_of": "2026-11-07"}
    ).json()["points"]
    assert len(hourly) == 16 and datetime.fromisoformat(hourly[0]["t"]) == datetime(2026, 11, 7, 6, tzinfo=UTC)
    assert (
        client.get("/api/profiles/lucia/wearables/steps", params={"window": "P1Y", "resolution": "hour"}).status_code
        == 422
    )
    # a viewer without read:wearables
    alex = app_for(fam.users["alex"])
    assert alex.get("/api/profiles/sam/wearables/summary").status_code == 403
    with engine.connect() as c:
        n = c.execute(
            text("select count(*) from audit_event where profile_id = :p and tool like 'wearables.%'"),
            {"p": fam.profiles["lucia"]},
        ).scalar()
        assert n == 4  # the refused hourly request read nothing


def test_manual_observation_refuses_a_blank_name_or_value(app_for, committed_family):
    client = app_for(committed_family.users["alex"])
    body = {"name": "Peso", "value": "81,4", "unit": "kg", "effective_at": "2026-11-06T08:00:00+01:00"}
    for blank in ({"name": "   "}, {"value": "  "}):
        r = client.post("/api/profiles/alex/manual-observations", json={**body, **blank})
        assert r.status_code == 422, (blank, r.text)


def test_a_wearables_window_too_large_for_the_calendar_is_capped(app_for, committed_family):
    client = app_for(committed_family.users["sam"])
    r = client.get("/api/profiles/lucia/wearables/steps", params={"window": "P9999Y", "as_of": "2026-11-07"})
    assert r.status_code == 200, r.text
    r = client.get("/api/profiles/lucia/wearables/steps", params={"window": "P30D", "as_of": "0001-01-02"})
    assert r.status_code == 422 and r.json()["detail"]["code"] == "bad_input", r.text
