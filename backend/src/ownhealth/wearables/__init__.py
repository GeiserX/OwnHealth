"""Wearable samples, rollups and the source-priority rule (lane C; connectors.md 4, architecture.md 6.4).

Samples keep UTC (`t`) plus the original offset (`tz_offset_min`). Rollups are stored per source:
hourly buckets on the UTC hour, daily buckets on the local calendar date of the sample (stored as
that date at 00:00 UTC). Sleep counts on the date of waking, so sleep buckets use `t_end`.

Source priority applies at query time, never at ingest: every raw sample is kept, and a daily series
takes, for each day, the highest-priority source that has data that day. That is what stops an
iPhone and an Apple Watch both counting the same steps. A `source_priority` row overrides the
default order (Watch, then iPhone, then anything else).
"""

from __future__ import annotations

from dataclasses import dataclass
from datetime import UTC, date, datetime
from uuid import UUID

from sqlalchemy import Connection, text


@dataclass(frozen=True)
class Metric:
    key: str
    unit: str
    agg: str  # "sum": the day value is the total; "mean": the day value is the average
    daily: bool  # one reading per day or night: coverage is 1 when present
    label_en: str
    label_es: str


METRICS: dict[str, Metric] = {
    m.key: m
    for m in (
        Metric("steps", "count", "sum", False, "Steps", "Pasos"),
        Metric("hr", "/min", "mean", False, "Heart rate", "Frecuencia cardiaca"),
        Metric("hr_resting", "/min", "mean", True, "Resting heart rate", "FC en reposo"),
        Metric("hrv_sdnn", "ms", "mean", True, "HRV (SDNN)", "VFC (SDNN)"),
        Metric("hrv_rmssd", "ms", "mean", True, "HRV (RMSSD)", "VFC (RMSSD)"),
        Metric("vo2max", "mL/kg/min", "mean", True, "VO₂max estimate", "VO₂máx estimado"),
        Metric("spo2", "%", "mean", False, "Blood oxygen", "Oxígeno en sangre"),
        Metric("resp_rate", "/min", "mean", False, "Respiratory rate", "Frecuencia respiratoria"),
        Metric("weight", "kg", "mean", True, "Weight", "Peso"),
        Metric("bp_systolic", "mm[Hg]", "mean", True, "Blood pressure, systolic", "Presión arterial sistólica"),
        Metric("bp_diastolic", "mm[Hg]", "mean", True, "Blood pressure, diastolic", "Presión arterial diastólica"),
        Metric("active_energy", "kcal", "sum", False, "Active energy", "Energía activa"),
        Metric("distance", "km", "sum", False, "Walking + running distance", "Distancia caminando y corriendo"),
        Metric("sleep_asleep", "min", "sum", True, "Sleep duration", "Duración del sueño"),
        Metric("sleep_in_bed", "min", "sum", True, "Time in bed", "Tiempo en cama"),
    )
}
SLEEP_METRICS = ("sleep_asleep", "sleep_in_bed")
DAILY_METRICS = [k for k, m in METRICS.items() if m.daily]


def metric_info(key: str) -> Metric:
    """Known metric, or a neutral description for one another importer added."""
    return METRICS.get(key) or Metric(key, "", "mean", False, key, key)


# ------------------------------------------------------------------- sources ----


def source_id(
    conn: Connection, profile_id: UUID, provider: str, device: str | None, external_id: str | None = None
) -> int:
    """Get or create the wearable_source row for one device of one profile."""
    params = {"p": profile_id, "pr": provider, "d": device, "x": external_id}
    sid = conn.execute(
        text(
            "select id from wearable_source where profile_id = :p and provider = :pr"
            " and coalesce(device, '') = coalesce(cast(:d as text), '')"
            " and coalesce(external_id, '') = coalesce(cast(:x as text), '')"
        ),
        params,
    ).scalar()
    if sid is None:
        sid = conn.execute(
            text(
                "insert into wearable_source (profile_id, provider, device, external_id) values (:p, :pr, :d, :x)"
                " on conflict do nothing returning id"
            ),
            params,
        ).scalar()
        if sid is None:  # created by a concurrent import
            return source_id(conn, profile_id, provider, device, external_id)
    return int(sid)


def default_rank(device: str | None) -> int:
    d = (device or "").lower()
    if "watch" in d:
        return 0
    if "iphone" in d:
        return 1
    return 2


def priority(conn: Connection, profile_id: UUID, metric: str) -> list[int]:
    """Source ids for one metric, best first: the profile's source_priority row, then the default order."""
    sources = conn.execute(
        text("select id, device from wearable_source where profile_id = :p order by id"), {"p": profile_id}
    ).all()
    explicit = (
        conn.execute(
            text("select source_ids from source_priority where profile_id = :p and metric = :m"),
            {"p": profile_id, "m": metric},
        ).scalar()
        or []
    )
    rest = sorted((s for s in sources if s.id not in explicit), key=lambda s: (default_rank(s.device), s.id))
    return [*explicit, *(s.id for s in rest)]


def set_priority(conn: Connection, profile_id: UUID, metric: str, source_ids: list[int]) -> None:
    conn.execute(
        text(
            "insert into source_priority (profile_id, metric, source_ids) values (:p, :m, :s)"
            " on conflict (profile_id, metric) do update set source_ids = excluded.source_ids"
        ),
        {"p": profile_id, "m": metric, "s": source_ids},
    )


# ------------------------------------------------------------------- rollups ----

# The instant a sample counts at: sleep on waking, everything else at its start.
_AT = "(case when s.metric = any(cast(:sleep as text[])) then coalesce(s.t_end, s.t) else s.t end)"
_LOCAL_DAY = (
    f"(date_trunc('day', ({_AT} at time zone 'UTC') + make_interval(mins => coalesce(s.tz_offset_min, 0)))"
    " at time zone 'UTC')"
)
_HOUR = f"(date_trunc('hour', {_AT} at time zone 'UTC') at time zone 'UTC')"
_STATS = (
    "count(*), min(s.v), max(s.v), avg(s.v), sum(s.v), percentile_cont(0.1) within group (order by s.v),"
    " percentile_cont(0.5) within group (order by s.v), percentile_cont(0.9) within group (order by s.v)"
)


def recompute_rollups(
    conn: Connection, *, profile_id: UUID, metrics: list[str] | None = None, source_ids: list[int] | None = None
) -> int:
    """Rebuild the hour and day rollups of one profile from wearable_sample (all metrics, or these).
    Returns the number of rollup rows written. Whole metrics are rebuilt (per source when
    `source_ids` is given), so a bucket is never computed from part of its samples."""
    params = {
        "p": profile_id,
        "metrics": metrics,
        "sources": source_ids,
        "sleep": list(SLEEP_METRICS),
        "daily": DAILY_METRICS,
    }
    sel = (
        "s.profile_id = :p and (cast(:metrics as text[]) is null or s.metric = any(cast(:metrics as text[])))"
        " and (cast(:sources as smallint[]) is null or s.source_id = any(cast(:sources as smallint[])))"
    )
    conn.execute(
        text(
            "delete from wearable_rollup where profile_id = :p"
            " and (cast(:metrics as text[]) is null or metric = any(cast(:metrics as text[])))"
            " and (cast(:sources as smallint[]) is null or source_id = any(cast(:sources as smallint[])))"
        ),
        params,
    )
    hour = conn.execute(
        text(
            "insert into wearable_rollup (profile_id, metric, source_id, bucket, t, n, v_min, v_max, v_avg, v_sum,"
            f" v_p10, v_p50, v_p90, coverage) select s.profile_id, s.metric, s.source_id, 'hour', {_HOUR}, {_STATS},"
            " case when s.metric = any(cast(:daily as text[])) then 1.0 else least(1.0, count(distinct"
            " floor(extract(minute from s.t at time zone 'UTC') / 5)) / 12.0) end"
            f" from wearable_sample s where {sel} group by 1, 2, 3, 5"
        ),
        params,
    ).rowcount
    day = conn.execute(
        text(
            "insert into wearable_rollup (profile_id, metric, source_id, bucket, t, n, v_min, v_max, v_avg, v_sum,"
            f" v_p10, v_p50, v_p90, coverage) select s.profile_id, s.metric, s.source_id, 'day', {_LOCAL_DAY},"
            f" {_STATS}, case when s.metric = any(cast(:daily as text[])) then 1.0 else least(1.0, count(distinct"
            f" {_HOUR}) / 24.0) end from wearable_sample s where {sel} group by 1, 2, 3, 5"
        ),
        params,
    ).rowcount
    return hour + day


# ------------------------------------------------------------------- queries ----


def _rows(conn: Connection, profile_id: UUID, metric: str, bucket: str, since: datetime, until: datetime) -> list[dict]:
    return [
        dict(r)
        for r in conn.execute(
            text(
                "select r.*, ws.device, ws.provider from wearable_rollup r"
                " join wearable_source ws on ws.id = r.source_id"
                " where r.profile_id = :p and r.metric = :m and r.bucket = :b and r.t >= :s and r.t <= :u order by r.t"
            ),
            {"p": profile_id, "m": metric, "b": bucket, "s": since, "u": until},
        ).mappings()
    ]


def series(
    conn: Connection,
    profile_id: UUID,
    metric: str,
    *,
    since: datetime,
    until: datetime,
    resolution: str = "day",
) -> list[dict]:
    """One point per bucket with data, taken from the best source that has that bucket.

    Each point: t, value (sum or mean by metric), min, max, mean, median, p10, p90, n, coverage,
    source_id, device. Buckets without data are absent; callers fill gaps if they draw them.
    """
    info = metric_info(metric)
    rank = {sid: i for i, sid in enumerate(priority(conn, profile_id, metric))}
    best: dict[datetime, dict] = {}
    for r in _rows(conn, profile_id, metric, resolution, since, until):
        cur = best.get(r["t"])
        if cur is None or rank.get(r["source_id"], 99) < rank.get(cur["source_id"], 99):
            best[r["t"]] = r
    out = []
    for t in sorted(best):
        r = best[t]
        out.append(
            {
                "t": t,
                "value": r["v_sum"] if info.agg == "sum" else r["v_avg"],
                "min": r["v_min"],
                "max": r["v_max"],
                "mean": r["v_avg"],
                "median": r["v_p50"],
                "p10": r["v_p10"],
                "p90": r["v_p90"],
                "n": r["n"],
                "coverage": r["coverage"],
                "source_id": r["source_id"],
                "device": r["device"],
            }
        )
    return out


def metrics_present(conn: Connection, profile_id: UUID) -> list[str]:
    return list(
        conn.execute(
            text("select distinct metric from wearable_rollup where profile_id = :p and bucket = 'day' order by 1"),
            {"p": profile_id},
        ).scalars()
    )


def devices(conn: Connection, profile_id: UUID, *, since: datetime, until: datetime) -> list[dict]:
    """Every source of the profile with its newest sample and its mean daily coverage in the window."""
    days = max(1, (until.date() - since.date()).days + 1)
    rows = conn.execute(
        text(
            "select ws.id, ws.provider, ws.device,"
            " (select max(coalesce(t_end, t)) from wearable_sample where source_id = ws.id) as last_sample,"
            " (select coalesce(sum(c), 0) from (select max(coverage) as c from wearable_rollup where source_id = ws.id"
            "   and bucket = 'day' and t >= :s and t <= :u group by t) d) as cov"
            " from wearable_source ws where ws.profile_id = :p order by ws.id"
        ),
        {"p": profile_id, "s": since, "u": until},
    ).mappings()
    return [
        {
            "source_id": r["id"],
            "provider": r["provider"],
            "device": r["device"],
            "last_sample": r["last_sample"],
            "coverage_pct": round(100 * float(r["cov"]) / days, 1),
        }
        for r in rows
    ]


def day_start(d: date) -> datetime:
    return datetime(d.year, d.month, d.day, tzinfo=UTC)
