"""The deterministic state for the synthetic Rivera family on 2026-11-07 (architecture.md section 16,
mockup screen 1): flags, gaps, the per-system status words, the hash and the snapshot."""

from datetime import UTC, date, datetime
from decimal import Decimal

from sqlalchemy import text

from ownhealth import state
from ownhealth.core.observations import (
    ObservationIn,
    commit_observation,
    retract_observation,
    supersede_observation,
)
from ownhealth.core.provenance import SrcIn
from ownhealth.core.records import MedicationIn, add_record
from ownhealth.state.canonical import canonical_json

from .conftest import AS_OF

MOCKUP_SYSTEMS = {
    "cardiometabolic": "out",
    "liver": "ok",
    "kidney": "ok",
    "thyroid": "stale",
    "blood": "ok",
    "inflammation": "watch",
    "vitamins": "out",
    "sleep": "ok",
    "fitness": "ok",
}


def _obs(pid, group, loinc, value, day, unit="ng/mL", ref="30 - 100", low="30", high="100", interp=None):
    y, m, d = map(int, day.split("-"))
    return ObservationIn(
        profile_id=pid, printed_name=group, printed_value=value, printed_unit=unit, printed_ref_range=ref,
        value_num=Decimal(value), ref_low=Decimal(low) if low else None, ref_high=Decimal(high) if high else None,
        loinc=loinc, alias_group=group, unit_ucum=unit, value_canonical=Decimal(value), unit_canonical=unit,
        interpretation=interp, derivation_version="test", effective_at=datetime(y, m, d, 8, tzinfo=UTC),
    )  # fmt: skip


def test_alex_matches_the_fixture_and_the_mockup(sconn, seeded):
    body = state.build_state(sconn, seeded.pid("alex"), as_of=AS_OF)
    assert body["schema"] == "ownhealth.state/v1" and body["profile"] == "alex" and body["as_of"] == "2026-11-07"
    assert body["systems"] == MOCKUP_SYSTEMS
    flags = [(f["reason"], f["rule"], f.get("group") or f.get("gene")) for f in body["flags"]]
    assert flags == [
        ("LAB_HIGH", "range.lab", "lpa"),
        ("LAB_LOW", "range.lab", "vitamin_d_25oh"),
        ("GENE_DRUG", "pgx.slco1b1.statin", "SLCO1B1"),
        ("RCV_CHANGE", "rcv.crp_hs", "crp_hs"),
        ("TREND_UP", "trend.hba1c.rise", "hba1c"),
    ]
    lpa, vitd, gene_drug = body["flags"][:3]
    assert (lpa["value"], lpa["ref"], lpa["date"], lpa["src"]) == ("142", "< 75", "2025-09-08", "src_a07")
    assert (vitd["value"], vitd["date"], vitd["src"]) == ("24", "2026-09-14", "src_a12")
    assert gene_drug["src"] == ["src_g31", "src_m02"] and gene_drug["drug"] == "atorvastatin"
    tsh = next(s for s in body["stale"] if s["group"] == "tsh")
    assert tsh == {"group": "tsh", "last": "2025-01-20", "rule": "recency.thyroid", "src": "src_a05"}
    assert {s["group"] for s in body["stale"]} == {"tsh", "ft4", "urine_acr"}
    assert body["never_measured"] == [{"group": "hcv_ab", "rule": "infect.hcv.once_adult"}]
    assert [(m["name"], m["since"], m["src"]) for m in body["medications"]] == [
        ("Atorvastatin", "2026-03-10", "src_m02"),
        ("Omeprazole", "2026-04-01", "src_m05"),
    ]
    assert body["notes"][0]["src"] == "src_n41" and body["notes"][0]["author_kind"] == "person"
    assert body["freshness"] == {"labs": "2026-09-14", "wearables": "2026-11-07"}


def test_every_fact_in_the_body_cites_a_src(sconn, seeded):
    body = state.build_state(sconn, seeded.pid("alex"), as_of=AS_OF)
    for section in ("flags", "stale", "medications", "conditions", "notes"):
        for item in body[section]:
            srcs = item["src"] if isinstance(item["src"], list) else [item["src"]]
            assert srcs and all(s.startswith("src_") for s in srcs), (section, item)


def test_body_fits_the_2kb_budget(sconn, seeded):
    for slug in ("alex", "sam", "lucia", "carmen"):
        body = state.build_state(sconn, seeded.pid(slug), as_of=AS_OF)
        assert len(canonical_json(body)) <= state.MAX_STATE_BYTES, slug


def test_other_profiles(sconn, seeded):
    sam = state.build_state(sconn, seeded.pid("sam"), as_of=AS_OF)
    assert {n["group"] for n in sam["never_measured"]} == {"hcv_ab", "lpa"}
    lucia = state.build_state(sconn, seeded.pid("lucia"), as_of=AS_OF)
    assert lucia["never_measured"] == []  # adult screening rules skip a child
    carmen = state.build_state(sconn, seeded.pid("carmen"), as_of=AS_OF)
    assert carmen["systems"]["kidney"] == "out" and carmen["systems"]["cardiometabolic"] == "out"
    assert [f["group"] for f in carmen["flags"]] == ["hba1c", "egfr"]


def test_two_runs_same_bytes_and_hash_under_any_session_timezone(sconn, seeded):
    pid = seeded.pid("alex")
    a = state.build_state(sconn, pid, as_of=AS_OF)
    sconn.execute(text("set local time zone 'Pacific/Kiritimati'"))
    b = state.build_state(sconn, pid, as_of=AS_OF)
    sconn.execute(text("set local time zone 'America/Los_Angeles'"))
    c = state.build_state(sconn, pid, as_of=AS_OF)
    assert canonical_json(a) == canonical_json(b) == canonical_json(c)
    assert a["state_hash"] == state.state_hash(a)


def test_snapshot_written_once_per_hash(sconn, seeded):
    pid = seeded.pid("alex")
    count = "select count(*), max(created_at) from state_snapshot where profile_id = :p"
    first = state.build_state(sconn, pid, as_of=AS_OF)
    n1, t1 = sconn.execute(text(count), {"p": pid}).one()
    state.build_state(sconn, pid, as_of=AS_OF)
    assert sconn.execute(text(count), {"p": pid}).one() == (n1, t1)
    stored = sconn.execute(
        text("select state_hash, body from state_snapshot where profile_id = :p and as_of = :a"),
        {"p": pid, "a": datetime(2026, 11, 7, tzinfo=UTC)},
    ).one()
    assert "sha256:" + stored.state_hash.hex() == first["state_hash"] == stored.body["state_hash"]


def test_new_data_changes_the_hash_and_known_at_reproduces_the_old_one(sconn, seeded):
    pid = seeded.pid("alex")
    before = state.build_state(sconn, pid, as_of=AS_OF)
    # inside one test transaction every insert has recorded_at = now(); the seed was committed earlier
    cut = sconn.execute(text("select now() - interval '1 microsecond'")).scalar_one()
    src = SrcIn(kind="manual", actor_kind="system")
    tsh = _obs(pid, "tsh", "3016-3", "2.0", "2026-11-02", unit="mUI/L", ref="0.40 - 4.00", low="0.40", high="4.00")
    commit_observation(sconn, tsh, src=src, accepted_by="person")
    after = state.build_state(sconn, pid, as_of=AS_OF)
    assert after["state_hash"] != before["state_hash"]
    assert "tsh" not in {s["group"] for s in after["stale"]}
    assert after["systems"]["thyroid"] == "ok"  # TSH fresh, free T4 still old: one fresh marker is enough
    # what the store knew before the import gives the old bytes back
    again = state.build_state(sconn, pid, as_of=AS_OF, known_at=cut)
    assert again["state_hash"] == before["state_hash"]
    now = sconn.execute(text("select clock_timestamp()")).scalar_one()  # the DB clock, not this host's
    assert state.build_state(sconn, pid, as_of=AS_OF, known_at=now)["state_hash"] == after["state_hash"]


def test_thyroid_card_turns_ok_when_any_marker_is_fresh(sconn, seeded):
    pid = seeded.pid("alex")
    for group, loinc, value in (("tsh", "3016-3", "2.0"), ("ft4", "3024-7", "1.2")):
        o = _obs(pid, group, loinc, value, "2026-11-02", unit="x", ref="0.4 - 4.0", low="0.4", high="4.0")
        commit_observation(sconn, o, src=SrcIn(kind="manual", actor_kind="system"), accepted_by="person")
    assert state.build_state(sconn, pid, as_of=AS_OF)["systems"]["thyroid"] == "ok"


def test_a_retraction_appears_in_corrections_and_leaves_the_flags(sconn, seeded):
    pid = seeded.pid("alex")
    oid = sconn.execute(
        text("select o.id from observation o where o.src_id = 'src_a12'")
    ).scalar_one()  # vitamin D 24 (L)
    retract_observation(sconn, oid, "entered for the wrong person")
    # the retraction is stamped now(), so build the state as of the DB's day, never before it
    today = sconn.execute(text("select (now() at time zone 'utc')::date")).scalar_one()
    body = state.build_state(sconn, pid, as_of=max(AS_OF, today))
    assert "vitamin_d_25oh" not in [f.get("group") for f in body["flags"]]
    assert body["corrections"][0]["src"] == "src_a12" and body["corrections"][0]["status"] == "retracted"


def test_a_rederive_supersession_is_not_a_correction(sconn, seeded):
    pid = seeded.pid("alex")
    row = sconn.execute(text("select * from observation where src_id = 'src_a12'")).mappings().one()
    copy = _obs(pid, row["alias_group"], row["loinc"], row["printed_value"], row["effective_at"].date().isoformat())
    supersede_observation(sconn, row["id"], copy, src=row["src_id"], accepted_by="auto", reason="rederive norm-9.9.9")
    today = sconn.execute(text("select (now() at time zone 'utc')::date")).scalar_one()
    body = state.build_state(sconn, pid, as_of=max(AS_OF, today))
    assert not [c for c in body["corrections"] if c["src"] == "src_a12"]


def test_sensitive_rows_are_counted_not_evaluated(sconn, seeded):
    pid = seeded.pid("sam")
    o = _obs(pid, "ferritin", "2276-4", "5", "2026-10-01", interp="L").model_copy(
        update={"sensitivity": ["reproductive"]}
    )
    commit_observation(sconn, o, src=SrcIn(kind="manual", actor_kind="system"), accepted_by="person")
    body = state.build_state(sconn, pid, as_of=AS_OF)
    assert body["flags"] == [] and body["withheld"]["sensitive"] == 1


def test_never_measured_ferritin_on_an_iron_supplement(sconn, seeded):
    pid = seeded.pid("carmen")
    add_record(
        sconn,
        MedicationIn(profile_id=pid, kind="supplement", name="Hierro (sulfato ferroso)", started=date(2026, 9, 1)),
        src=SrcIn(kind="manual", actor_kind="system"),
        accepted_by="person",
    )
    body = state.build_state(sconn, pid, as_of=AS_OF)
    assert {"group": "ferritin", "rule": "iron.ferritin.on_iron"} in body["never_measured"]
    assert body["supplements"][0]["name"] == "Hierro (sulfato ferroso)"


def test_gene_lab_lpa_never_measured(sconn, seeded):
    pid = seeded.pid("sam")
    seeded.findings[pid].append({"category": "risk_marker", "gene": "LPA", "call_state": "alt", "src": "src_g90"})
    try:
        body = state.build_state(sconn, pid, as_of=AS_OF)
    finally:
        seeded.findings[pid].pop()
    # the screening rule claims Lp(a) first; the gene_lab rule does not add a second entry
    assert [n for n in body["never_measured"] if n["group"] == "lpa"] == [
        {"group": "lpa", "rule": "lipid.lpa.once_adult"}
    ]


def test_status_word(sconn, seeded):
    pid = seeded.pid("alex")
    words = {g: state.status_word(sconn, pid, g, as_of=AS_OF) for g in
             ("vitamin_d_25oh", "tsh", "crp_hs", "ldl_c", "hba1c", "lpa", "hcv_ab")}  # fmt: skip
    assert words == {
        "vitamin_d_25oh": "out",
        "tsh": "stale",
        "crp_hs": "watch",
        "ldl_c": "ok",
        "hba1c": "watch",
        "lpa": "out",
        "hcv_ab": "nodata",
    }
    assert set(words.values()) <= set(state.STATUS_WORDS)


def test_as_of_in_the_past_ignores_later_values(sconn, seeded):
    body = state.build_state(sconn, seeded.pid("alex"), as_of=date(2025, 2, 1))
    vitd = next(f for f in body["flags"] if f.get("group") == "vitamin_d_25oh")
    assert (vitd["value"], vitd["date"]) == ("29", "2025-01-20")  # the 24 of 2026-09-14 is not visible yet
    assert body["freshness"]["labs"] == "2025-01-20"
    assert body["systems"]["thyroid"] == "ok"
    # the first atorvastatin course (2022-06-20 to 2025-12-01) was active then
    assert [m["src"] for m in body["medications"]] == ["src_m01"]
