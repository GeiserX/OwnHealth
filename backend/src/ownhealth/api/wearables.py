"""Wearables (lane C): daily rollups with the source-priority rule applied, one source per day.

Needs `read:wearables`; every read writes one audit row.
"""

from __future__ import annotations

import re
from datetime import UTC, date, datetime, timedelta
from typing import Literal

from fastapi import APIRouter, Depends, HTTPException, Query
from sqlalchemy import Connection

from ownhealth import wearables as w
from ownhealth.api._deps import get_conn, get_principal, profile_access
from ownhealth.core.access import Permission, Principal
from ownhealth.core.audit import AuditEvent, write_audit

router = APIRouter(prefix="/profiles/{p}/wearables", tags=["wearables"])

ORDER = ["sleep_asleep", "hrv_rmssd", "hrv_sdnn", "hr_resting", "steps", "vo2max"]
_WINDOW = re.compile(r"^P(\d{1,4})([DMY])$")


def window_days(window: str) -> int:
    if window == "all":
        return 365 * 30
    m = _WINDOW.match(window)
    if not m:
        raise HTTPException(422, detail={"code": "bad_input", "message": "window is P<n>D, P<n>M, P<n>Y or all."})
    return int(m.group(1)) * {"D": 1, "M": 30, "Y": 365}[m.group(2)]


def _audit(conn: Connection, principal: Principal, perm: Permission, tool: str, metrics: list[str]) -> None:
    write_audit(
        conn,
        AuditEvent(
            actor_kind=principal.actor_kind,
            actor_id=principal.user_id,
            token_id=principal.token_id,
            via="api" if principal.kind == "token" else "ui",
            action="read",
            tool=tool,
            profile_id=perm.profile.id,
            resource_type="wearable_rollup",
            resource_ids=metrics,
        ),
    )


def _r(v: float | None, metric: str) -> float | None:
    if v is None:
        return None
    return round(float(v), 2 if metric in ("vo2max", "weight", "distance") else 1)


@router.get("/summary")
def summary(
    p: str,
    lang: Literal["es", "en"] = "es",
    days: int = Query(90, ge=7, le=366),
    as_of: date | None = None,
    conn: Connection = Depends(get_conn),
    principal: Principal = Depends(get_principal),
) -> dict:
    """Every metric of the profile: a daily series over `days`, the newest value and the 30-day mean."""
    perm = profile_access(conn, principal, p, scope="read:wearables")
    end = as_of or datetime.now(UTC).date()
    start = end - timedelta(days=days - 1)
    since, until = w.day_start(start), w.day_start(end)
    present = w.metrics_present(conn, perm.profile.id)
    present.sort(key=lambda m: (ORDER.index(m) if m in ORDER else len(ORDER), m))
    out = []
    for metric in present:
        info = w.metric_info(metric)
        points = {pt["t"].date(): pt for pt in w.series(conn, perm.profile.id, metric, since=since, until=until)}
        if not points:
            continue
        series = []
        for i in range(days):
            d = start + timedelta(days=i)
            pt = points.get(d)
            series.append(
                {
                    "date": d,
                    "value": _r(pt["value"], metric) if pt else None,
                    "min": _r(pt["min"], metric) if pt else None,
                    "max": _r(pt["max"], metric) if pt else None,
                }
            )
        last30 = [pt["value"] for d, pt in points.items() if d > end - timedelta(days=30)]
        newest = points[max(points)]
        out.append(
            {
                "metric": metric,
                "label": info.label_es if lang == "es" else info.label_en,
                "unit": info.unit,
                "agg": info.agg,
                "latest": _r(newest["value"], metric),
                "latest_date": max(points),
                "avg_30d": _r(sum(last30) / len(last30), metric) if last30 else None,
                "series": series,
                "coverage_pct": round(100 * sum(pt["coverage"] for pt in points.values()) / days, 1),
                "source": newest["device"],
            }
        )
    _audit(conn, principal, perm, "wearables.summary", [m["metric"] for m in out])
    return {"metrics": out, "devices": w.devices(conn, perm.profile.id, since=since, until=until)}


@router.get("/{metric}")
def metric_series(
    p: str,
    metric: str,
    window: str = "P30D",
    resolution: Literal["day", "hour"] = "day",
    as_of: date | None = None,
    lang: Literal["es", "en"] = "es",
    conn: Connection = Depends(get_conn),
    principal: Principal = Depends(get_principal),
) -> dict:
    """One metric: every bucket with data from the best source for it (hour buckets up to 14 days)."""
    perm = profile_access(conn, principal, p, scope="read:wearables")
    days = window_days(window)
    if resolution == "hour" and days > 14:
        raise HTTPException(422, detail={"code": "bad_input", "message": "Hourly series go up to P14D."})
    end = as_of or datetime.now(UTC).date()
    since, until = w.day_start(end - timedelta(days=days - 1)), w.day_start(end) + timedelta(days=1, seconds=-1)
    info = w.metric_info(metric)
    points = w.series(conn, perm.profile.id, metric, since=since, until=until, resolution=resolution)
    _audit(conn, principal, perm, "wearables.series", [metric])
    return {
        "metric": metric,
        "label": info.label_es if lang == "es" else info.label_en,
        "unit": info.unit,
        "agg": info.agg,
        "resolution": resolution,
        "window": window,
        "priority": w.priority(conn, perm.profile.id, metric),
        "points": [
            {
                "t": pt["t"],
                "value": _r(pt["value"], metric),
                **{k: _r(pt[k], metric) for k in ("min", "max", "mean", "median", "p10", "p90")},
                "n": pt["n"],
                "coverage": round(float(pt["coverage"]), 3),
                "source_id": pt["source_id"],
                "device": pt["device"],
            }
            for pt in points
        ],
    }
