"""Lane A routes: observations, trend, aliases (CONTRACTS.md section 5)."""

from datetime import UTC, datetime

from sqlalchemy import text

from ownhealth.core.access import Principal
from ownhealth.core.observations import commit_observation
from ownhealth.core.provenance import SrcIn
from ownhealth.normalize import normalize_row

ROWS = [
    ("2024-03-02", "Colesterol LDL", "118", "mg/dL", "< 130"),
    ("2025-01-20", "Colesterol LDL directo", "84", "mg/dL", "< 130"),
    ("2026-09-14", "Colesterol LDL", "3,4", "mmol/L", "< 3,36"),
    ("2026-09-14", "Ferritina", "120", "ng/mL", "30 - 400"),
]


def _seed(engine, fam):
    with engine.begin() as c:
        for day, name, value, unit, rng in ROWS:
            y, m, d = map(int, day.split("-"))
            n = normalize_row(
                c,
                profile_id=fam.profiles["alex"],
                printed_name=name,
                printed_value=value,
                printed_unit=unit,
                printed_range=rng,
                printed_flag=None,
                effective_at=datetime(y, m, d, 8, tzinfo=UTC),
                lang="es",
            )
            commit_observation(
                c,
                n.observation,
                src=SrcIn(kind="manual", actor_kind="user", label=f"LabNorte {day}"),
                accepted_by="person",
            )


def test_observations_as_reported_and_converted(app_for, committed_family, engine):
    _seed(engine, committed_family)
    client = app_for(committed_family.users["alex"])
    items = client.get("/api/profiles/alex/observations", params={"alias_group": "ldl_c"}).json()["items"]
    assert [(i["date"], i["value"], i["unit"]) for i in items] == [
        ("2026-09-14", 3.4, "mmol/L"),
        ("2025-01-20", 84.0, "mg/dL"),
        ("2024-03-02", 118.0, "mg/dL"),
    ]
    assert items[0]["printed"]["value"] == "3,4" and items[0]["src_label"] == "LabNorte 2026-09-14"
    assert items[0]["interpretation"] == "H" and items[0]["name"] == "Colesterol LDL"
    conv = client.get("/api/profiles/alex/observations", params={"alias_group": "ldl_c", "units": "conventional"})
    assert [round(i["value"], 1) for i in conv.json()["items"]][0] == 131.5
    si = client.get("/api/profiles/alex/observations", params={"q": "LDL cholesterol", "units": "si"}).json()["items"]
    assert len(si) == 3 and {i["unit"] for i in si} == {"mmol/L"}
    with engine.connect() as c:
        n = c.execute(
            text("select count(*) from audit_event where resource_type = 'observation' and profile_id = :p"),
            {"p": committed_family.profiles["alex"]},
        ).scalar()
    assert n == 3


def test_trend(app_for, committed_family, engine):
    _seed(engine, committed_family)
    client = app_for(committed_family.users["alex"])
    t = client.get("/api/profiles/alex/trend", params={"code": "LOINC:13457-7", "window": "P5Y", "lang": "en"}).json()
    assert t["alias_group"] == "ldl_c" and t["unit"] == "mg/dL"
    assert [p["value"] for p in t["points"]][:2] == [118.0, 84.0]
    assert round(t["points"][-1]["value"], 1) == 131.5 and t["points"][-1]["printed"] == "3,4 mmol/L"
    assert t["codes"] == ["LOINC:13457-7", "LOINC:18262-6", "LOINC:39469-2"]
    assert any("Direct assay" in x for x in t["limitations"])
    assert any("molar mass of cholesterol" in x for x in t["limitations"])
    es = client.get("/api/profiles/alex/trend", params={"code": "LOINC:13457-7", "window": "P5Y", "lang": "es"}).json()
    assert any("masa molar de cholesterol" in x for x in es["limitations"]), es["limitations"]
    assert not any("are " in x or "Labs " in x for x in es["limitations"]), es["limitations"]
    s = t["stats"]
    assert s["n"] == 3 and s["last_vs_previous_pct"] == round((t["points"][-1]["value"] - 84) / 84 * 100, 1)
    assert s["last_vs_previous_pct"] != round(s["last_vs_previous_pct"])  # one decimal, not a whole number
    assert s["rcv_pct"] and s["exceeds_rcv"] is (abs(s["last_vs_previous_pct"]) > s["rcv_pct"])
    statuses = [p["status"] for p in t["points"]]  # lane E's word as of each point's day
    assert statuses and all(x in ("ok", "watch", "out", "stale", "nodata") for x in statuses), statuses
    assert client.get("/api/profiles/alex/trend", params={"code": "ldl_c", "window": "P1Y"}).json()["stats"]["n"] == 1
    assert client.get("/api/profiles/alex/trend", params={"code": "ldl_c", "window": "bad"}).status_code == 422
    assert client.get("/api/profiles/alex/trend", params={"code": "zzz qqq"}).status_code == 404


def test_scopes_are_enforced(app_for, committed_family, engine):
    _seed(engine, committed_family)
    hh = committed_family.household_id
    summary_only = Principal(
        kind="token", user_id=committed_family.users["alex"], household_id=hh, scopes=frozenset({"read:summary"})
    )
    client = app_for(principal=summary_only)
    assert client.get("/api/profiles/alex/observations").status_code == 403
    assert client.get("/api/profiles/alex/trend", params={"code": "ldl_c"}).status_code == 403
    sam = app_for(committed_family.users["sam"])  # a viewer on Alex: read:summary only
    assert sam.get("/api/profiles/alex/observations").status_code == 403


def test_add_alias_relinks(app_for, committed_family, engine):
    with engine.begin() as c:
        n = normalize_row(
            c,
            profile_id=committed_family.profiles["alex"],
            printed_name="Coles LDL calc.",
            printed_value="97",
            printed_unit="mg/dL",
            printed_range="< 130",
            printed_flag=None,
            effective_at=datetime(2026, 9, 14, 8, tzinfo=UTC),
            lang="es",
        )
        commit_observation(c, n.observation, src=SrcIn(kind="manual", actor_kind="user"), accepted_by="person")
    client = app_for(committed_family.users["alex"])
    r = client.post(
        "/api/aliases",
        json={"profile": "alex", "alias": "Coles LDL calc.", "alias_group": "ldl_c", "lang": "es", "loinc": "13457-7"},
    )
    assert r.status_code == 200 and r.json()["rederived"] == 1
    items = client.get("/api/profiles/alex/observations", params={"alias_group": "ldl_c"}).json()["items"]
    assert [i["loinc"] for i in items] == ["13457-7"]
    bad = client.post("/api/aliases", json={"profile": "alex", "alias": "x", "alias_group": "nope"})
    assert bad.status_code == 422
    viewer = app_for(committed_family.users["sam"])
    assert (
        viewer.post("/api/aliases", json={"profile": "alex", "alias": "y", "alias_group": "ldl_c"}).status_code == 403
    )


def test_a_window_too_large_for_the_calendar_is_refused(app_for, committed_family, engine):
    _seed(engine, committed_family)
    client = app_for(committed_family.users["alex"])
    for window in ("P99999Y", "P9999999D"):
        r = client.get("/api/profiles/alex/trend", params={"code": "ldl_c", "window": window})
        assert r.status_code == 422 and r.json()["detail"]["code"] == "bad_input", (window, r.text)


def test_browser_reads_and_alias_additions_are_audited_as_ui(app_for, committed_family, engine):
    _seed(engine, committed_family)
    client = app_for(committed_family.users["alex"])
    assert client.get("/api/profiles/alex/observations").status_code == 200
    r = client.post("/api/aliases", json={"profile": "alex", "alias": "LDL directo", "alias_group": "ldl_c"})
    assert r.status_code == 200, r.text
    with engine.connect() as c:
        vias = dict(
            c.execute(
                text(
                    "select action, array_agg(distinct via) from audit_event where actor_id = :u"
                    " and action in ('read', 'alias_add') and resource_type in ('observation', 'analyte_alias')"
                    " group by action"
                ),
                {"u": committed_family.users["alex"]},
            ).all()
        )
    assert vias == {"read": ["ui"], "alias_add": ["ui"]}, vias
