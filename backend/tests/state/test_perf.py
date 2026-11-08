"""Budget: the status of a profile with 5 years of labs (40 analytes x 60 monthly draws, 2400 values)
builds and renders in under 300 ms at p95."""

import time
from datetime import UTC, date, datetime
from decimal import Decimal

import pytest

from ownhealth import state
from ownhealth.core.access import ProfileIn, create_household, create_profile
from ownhealth.core.observations import ObservationIn, commit_observation
from ownhealth.core.provenance import SrcIn, create_src
from ownhealth.state.pack import load_pack

AS_OF = date(2026, 11, 7)
RUNS = 20
BUDGET_MS = 300


@pytest.fixture(scope="module")
def five_years(seeded):
    groups = [(g, str(a["loinc"][0]) if a.get("loinc") else None) for g, a in load_pack().analytes.items()]
    groups += [(f"extra_{i:02d}", None) for i in range(40 - len(groups))]
    assert len(groups) == 40
    with seeded.engine.begin() as conn:
        h = create_household(conn, "Ortega (lane E perf)")
        pid = create_profile(conn, ProfileIn(household_id=h, slug="perf", display_name="Carmen Ortega",
                                             relationship="self", birth_date=date(1955, 1, 30),
                                             sex_at_birth="female", country="ES"))  # fmt: skip
        for m in range(60):
            y, mo = divmod(2021 * 12 + 10 + m, 12)  # 2021-11 .. 2026-10
            day = datetime(y, mo + 1, 3, 8, 0, tzinfo=UTC)
            src = create_src(conn, SrcIn(kind="manual", profile_id=pid, label=f"LabNorte · {day.date()}"))
            for i, (g, loinc) in enumerate(groups):
                v = Decimal(50 + (m * 7 + i * 13) % 40)  # deterministic, crosses 80 now and then
                commit_observation(conn, ObservationIn(
                    profile_id=pid, printed_name=g, printed_value=str(v), printed_unit="U/L", printed_ref_range="40-80",
                    value_num=v, ref_low=Decimal(40), ref_high=Decimal(80), loinc=loinc, alias_group=g,
                    unit_ucum="U/L", value_canonical=v, unit_canonical="U/L", interpretation="H" if v > 80 else "N",
                    derivation_version="perf", effective_at=day,
                ), src=src, accepted_by="auto")  # fmt: skip
    return pid


def test_status_for_five_years_of_labs_is_under_300ms_p95(seeded, fake_genome, five_years):
    times = []
    with seeded.engine.connect() as conn:
        body = state.build_state(conn, five_years, as_of=AS_OF)  # warm the pack and the plan cache
        assert body["flags"], "the generated profile must exercise the rules"
        for _ in range(RUNS):
            t = time.perf_counter()
            body = state.build_state(conn, five_years, as_of=AS_OF)
            state.render(body, lang="es", identity_mode="pseudonymous")
            times.append((time.perf_counter() - t) * 1000)
        conn.rollback()
    p95 = sorted(times)[int(RUNS * 0.95) - 1]
    print(f"\nstatus p95 {p95:.1f} ms, median {sorted(times)[RUNS // 2]:.1f} ms over {RUNS} runs")
    assert p95 < BUDGET_MS
