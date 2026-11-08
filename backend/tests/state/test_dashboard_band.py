"""The dashboard reference band is drawn in the same (canonical) unit as the sparkline."""

from datetime import UTC, date, datetime

from ownhealth import normalize, state
from ownhealth.core.access import ALL_SCOPES
from ownhealth.core.observations import commit_observation
from ownhealth.core.provenance import SrcIn


def _add(conn, pid, v, u, rr, when):
    n = normalize.normalize_row(
        conn,
        profile_id=pid,
        printed_name="Glucosa",
        printed_value=v,
        printed_unit=u,
        printed_range=rr,
        printed_flag=None,
        effective_at=when,
        lang="es",
    )
    src = SrcIn(kind="manual", profile_id=pid, label="LabNorte", actor_kind="system")
    commit_observation(conn, n.observation, src=src, accepted_by="import")


def _rows(o, key):
    if isinstance(o, dict):
        if o.get("alias_group") == key and "band" in o:
            yield o
        for v in o.values():
            yield from _rows(v, key)
    elif isinstance(o, list):
        for v in o:
            yield from _rows(v, key)


def test_si_range_is_converted_to_the_spark_unit(conn, family):
    pid = family.profiles["alex"]
    _add(conn, pid, "92", "mg/dL", "70-110", datetime(2026, 3, 1, tzinfo=UTC))
    _add(conn, pid, "5,3", "mmol/L", "3,9-6,1", datetime(2026, 9, 1, tzinfo=UTC))
    d = state.dashboard(conn, pid, as_of=date(2026, 10, 1), lang="en", scopes=ALL_SCOPES)
    rows = list(_rows(d, "glucose"))
    assert rows
    for r in rows:
        assert r["spark"][0] == 92.0
        low, high = r["band"]
        assert 69 < low < 71 and 109 < high < 111, r["band"]
