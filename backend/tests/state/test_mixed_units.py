"""A series where one point could not be converted keeps its printed unit: never compare across units."""

from datetime import date
from decimal import Decimal
from uuid import uuid4

from ownhealth.state.engine import Point, _rcv, _trend


def _pt(day: str, value: str, canonical: str | None, unit="mg/dL", canonical_unit="mmol/L") -> Point:
    return Point(
        id=uuid4(),
        date=date.fromisoformat(day),
        value=Decimal(value),
        canonical=Decimal(canonical) if canonical else None,
        canonical_unit=canonical_unit,
        printed_value=value,
        unit=unit,
        ref=None,
        ref_low=None,
        ref_high=None,
        flag=None,
        loinc=None,
        method=None,
        src="src_x",
        src_label=None,
        derivation="test",
    )


TREND = {"id": "t", "points": 3, "direction": "up", "min_slope_per_year": "0.1"}
RCV = {"id": "r", "cv_i": "6", "cv_a": "2"}


def test_a_trend_never_mixes_canonical_and_printed_units():
    # 2.0 -> 2.1 mmol/L, then 90 mg/dL the normalizer could not convert: no 40-fold "rise"
    pts = [_pt("2024-01-01", "77", "2.0"), _pt("2025-01-01", "81", "2.1"), _pt("2026-01-01", "90", None)]
    assert _trend(TREND, pts) is None
    # same unit throughout still trends
    pts = [_pt("2024-01-01", "77", "2.0"), _pt("2025-01-01", "81", "2.1"), _pt("2026-01-01", "90", "2.33")]
    assert _trend(TREND, pts)["reason"] == "TREND_UP"


def test_rcv_compares_the_unit_comparable_is_in():
    assert _rcv(RCV, [_pt("2025-01-01", "77", "2.0"), _pt("2026-01-01", "90", None)]) is None
