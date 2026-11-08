"""Wearable markers read the day value the metric means: a night split into sleep stages is the day's
total, never the mean of its stages."""

from datetime import date, timedelta

from ownhealth.state.engine import _wearable_marker
from ownhealth.state.pack import load_pack


def _spec(metric: str) -> dict:
    return next(m for s in load_pack().systems for m in s.markers if isinstance(m, dict) and m["metric"] == metric)


def test_sleep_is_the_nightly_total_of_its_stages():
    as_of = date(2026, 11, 7)
    # three stages a night: 120 + 60 + 240 minutes asleep, mean 140
    days = [(as_of - timedelta(days=i), 140.0, 420.0) for i in range(30)][::-1]
    assert _wearable_marker(_spec("sleep_asleep"), days, as_of)["value"] == 420
