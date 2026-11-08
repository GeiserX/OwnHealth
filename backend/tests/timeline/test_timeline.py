"""Timeline API (lane H): draws grouped, records dated, lifecycle and scopes respected, audited."""

from __future__ import annotations

from datetime import date

from factories import manual_src, obs
from sqlalchemy import text

from ownhealth.core.access import Principal
from ownhealth.core.observations import commit_observation
from ownhealth.core.records import ConditionIn, ImmunizationIn, MedicationIn, add_record, retract_record


def _seed(engine, fam):
    alex = fam.profiles["alex"]
    with engine.begin() as c:
        # One draw: two values sharing a report_key, one flagged by the lab.
        for value, group, flag in (("97", "ldl_c", None), ("24", "vitamin_d", "L")):
            commit_observation(
                c,
                obs(alex, value, group=group, interpretation=flag, report_key="LN-1"),
                src=manual_src(),
                accepted_by="person",
            )
        commit_observation(c, obs(alex, "118", day="2026-03-02"), src=manual_src(), accepted_by="person")
        add_record(
            c,
            MedicationIn(
                profile_id=alex,
                name="Atorvastatin",
                strength="20 mg",
                started=date(2022, 6, 20),
                stopped=date(2025, 12, 1),
                reason_text="paused",
            ),
            src=manual_src(),
            accepted_by="person",
        )
        gone = add_record(
            c,
            MedicationIn(profile_id=alex, name="Wrongpril", started=date(2024, 1, 1)),
            src=manual_src(),
            accepted_by="person",
        )
        retract_record(c, "medication", gone.id, "entered in error")
        add_record(
            c,
            ConditionIn(
                profile_id=alex,
                text="Hypercholesterolaemia",
                code_system="icd10",
                code="E78.0",
                onset=date(2021, 11, 9),
            ),
            src=manual_src(),
            accepted_by="person",
        )
        add_record(
            c,
            ConditionIn(profile_id=alex, text="Hidden thing", onset=date(2023, 1, 1), sensitivity=["mental_health"]),
            src=manual_src(),
            accepted_by="person",
        )
        add_record(
            c,
            ImmunizationIn(profile_id=alex, vaccine="Td booster", occurred=date(2016, 5, 10)),
            src=manual_src(),
            accepted_by="person",
        )


def test_timeline_groups_draws_and_dates_records(app_for, committed_family, engine):
    _seed(engine, committed_family)
    out = app_for(committed_family.users["alex"]).get("/api/profiles/alex/timeline?lang=en").json()
    ev = out["events"]
    assert [e["date"] for e in ev] == sorted((e["date"] for e in ev), reverse=True)
    labs = [e for e in ev if e["type"] == "lab"]
    assert [(e["date"], e["title"]) for e in labs] == [
        ("2026-09-14", "Lab report · 2 results"),
        ("2026-03-02", "Lab report · 1 result"),
    ]
    assert labs[0]["detail"] == "1 flagged by the lab"
    meds = {(e["date"], e["title"]) for e in ev if e["type"] == "medication"}
    assert meds == {("2022-06-20", "Atorvastatin 20 mg started"), ("2025-12-01", "Atorvastatin 20 mg stopped")}
    assert {e["title"] for e in ev if e["type"] == "diagnosis"} == {"Hidden thing", "Hypercholesterolaemia"}
    assert [e["date"] for e in ev if e["type"] == "vaccine"] == ["2016-05-10"]
    assert all(e["src"] and e["src"].startswith("src_") for e in ev)
    es = app_for(committed_family.users["alex"]).get("/api/profiles/alex/timeline?lang=es&types=lab").json()
    assert es["events"][0]["title"] == "Analítica · 2 resultados"


def test_timeline_filters_scopes_and_access(app_for, committed_family, engine):
    _seed(engine, committed_family)
    fam = committed_family
    alex = app_for(fam.users["alex"])
    only = alex.get("/api/profiles/alex/timeline?types=vaccine,diagnosis&from=2020-01-01&to=2022-01-01").json()
    assert [(e["type"], e["date"]) for e in only["events"]] == [("diagnosis", "2021-11-09")]
    assert alex.get("/api/profiles/alex/timeline?types=bogus").status_code == 422

    # A token with labs only: no records, no sensitive rows, and the withheld types are named.
    token = Principal(
        kind="token", user_id=fam.users["alex"], household_id=fam.household_id, scopes=frozenset({"read:labs"})
    )
    out = app_for(principal=token).get("/api/profiles/alex/timeline").json()
    assert {e["type"] for e in out["events"]} == {"lab"}
    assert "medication" in out["withheld"]["by_scope"]
    records = Principal(
        kind="token", user_id=fam.users["alex"], household_id=fam.household_id, scopes=frozenset({"read:records"})
    )
    titles = {e["title"] for e in app_for(principal=records).get("/api/profiles/alex/timeline").json()["events"]}
    assert "Hypercholesterolaemia" in titles and "Hidden thing" not in titles

    # Sam has no grant on Carmen.
    assert app_for(fam.users["sam"]).get("/api/profiles/carmen/timeline").status_code == 404
    with engine.connect() as c:
        n = c.execute(
            text("select count(*) from audit_event where resource_type = 'timeline' and profile_id = :p"),
            {"p": fam.profiles["alex"]},
        ).scalar()
    assert n >= 3
