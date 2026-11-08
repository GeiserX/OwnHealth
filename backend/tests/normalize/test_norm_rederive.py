"""rederive: a table change re-links history without touching the truth layer (architecture.md 13)."""

from datetime import UTC, datetime

from sqlalchemy import text

from ownhealth.core.observations import commit_observation
from ownhealth.core.provenance import SrcIn
from ownhealth.normalize import DERIVATION_VERSION, add_alias, normalize_row, rederive


def _commit(conn, family, name, value="97", unit="mg/dL", rng="< 130"):
    n = normalize_row(
        conn,
        profile_id=family.profiles["alex"],
        printed_name=name,
        printed_value=value,
        printed_unit=unit,
        printed_range=rng,
        printed_flag=None,
        effective_at=datetime(2026, 9, 14, 8, tzinfo=UTC),
        lang="es",
    )
    return commit_observation(conn, n.observation, src=SrcIn(kind="manual", actor_kind="user"), accepted_by="person")


def test_new_alias_relinks_history_once(conn, family):
    old = _commit(conn, family, "Coles LDL calc.")
    keep = _commit(conn, family, "Ferritina", "120", "ng/mL", "30 - 400")
    row = conn.execute(text("select alias_group, loinc from observation where id = :i"), {"i": old.id}).one()
    assert row.loinc is None and row.alias_group == "coles_ldl_calc"
    assert rederive(conn, profile_id=family.profiles["alex"]) == 0  # nothing changed yet

    add_alias(
        conn,
        household_id=family.household_id,
        alias="Coles LDL calc.",
        alias_group="ldl_c",
        lang="es",
        added_by=None,
        loinc="13457-7",
    )
    assert rederive(conn, profile_id=family.profiles["alex"]) == 1

    before = conn.execute(text("select * from observation where id = :i"), {"i": old.id}).mappings().one()
    after = conn.execute(text("select * from observation where supersedes = :i"), {"i": old.id}).mappings().one()
    assert before["status"] == "superseded" and after["status"] == "current"
    assert (after["alias_group"], after["loinc"], after["value_canonical"]) == ("ldl_c", "13457-7", 97)
    for col in (
        "printed_name",
        "printed_value",
        "printed_unit",
        "printed_ref_range",
        "value_num",
        "src_id",
        "accepted_by",
        "effective_at",
    ):
        assert after[col] == before[col], col  # truth layer, parsed layer and provenance carried over
    assert after["derivation_version"] == DERIVATION_VERSION
    assert conn.execute(text("select status from observation where id = :i"), {"i": keep.id}).scalar() == "current"

    item = conn.execute(
        text("select payload from review_item where kind = 'rederive_diff' and profile_id = :p"),
        {"p": family.profiles["alex"]},
    ).scalar_one()
    assert item["rows"][0]["changed"]["loinc"] == [None, "13457-7"]
    assert (
        conn.execute(
            text("select count(*) from job where kind = 'state.rebuild' and payload->>'profile_id' = :p"),
            {"p": str(family.profiles["alex"])},
        ).scalar()
        == 1
    )
    assert rederive(conn, profile_id=family.profiles["alex"]) == 0  # idempotent


def test_rederive_fills_a_stale_derived_layer(conn, family):
    """A row written with an older normalizer (no canonical value) gets one."""
    from factories import obs

    o = obs(family.profiles["alex"], value="3.4").model_copy(
        update={
            "printed_value": "3,4",
            "printed_unit": "mmol/L",
            "unit_ucum": None,
            "value_canonical": None,
            "unit_canonical": None,
            "loinc": None,
            "printed_ref_range": None,
            "ref_high": None,
        }
    )
    c = commit_observation(conn, o, src=SrcIn(kind="manual", actor_kind="user"), accepted_by="person")
    assert rederive(conn, profile_id=family.profiles["alex"]) == 1
    new = conn.execute(text("select * from observation where supersedes = :i"), {"i": c.id}).mappings().one()
    assert (new["loinc"], new["unit_ucum"], new["unit_canonical"]) == ("39469-2", "mmol/L", "mg/dL")
    assert round(float(new["value_canonical"]), 1) == 131.5
