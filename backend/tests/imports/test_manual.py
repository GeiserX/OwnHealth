"""Typed values: a typed LOINC drives the derived layer like a received code."""

from datetime import UTC, datetime

from sqlalchemy import text

from ownhealth.ingest.manual import add_manual_observation


def test_a_typed_loinc_sets_the_alias_group(conn, family):
    committed = add_manual_observation(
        conn,
        profile_id=family.profiles["alex"],
        user_id=family.users["alex"],
        name="VNDR-FERR home kit",
        value="85",
        unit="ng/mL",
        effective_at=datetime(2026, 6, 2, 9, tzinfo=UTC),
        loinc="2276-4",
    )
    row = (
        conn.execute(
            text("select loinc, alias_group, value_canonical, unit_canonical from observation where id = :i"),
            {"i": committed.id},
        )
        .mappings()
        .one()
    )
    assert row["loinc"] == "2276-4" and row["alias_group"] == "ferritin"
    assert row["value_canonical"] is not None and row["unit_canonical"] is not None
