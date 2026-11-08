"""Streaming reader for Apple Health `export.xml` (connectors.md 2.2).

`iterparse` with every finished top-level element cleared at once, so memory stays flat whatever
the file size: the export of a decade of Apple Watch data is several GB and never fits a DOM.
"""

from __future__ import annotations

import re
import xml.etree.ElementTree as ET
from collections import Counter
from collections.abc import Iterator
from dataclasses import dataclass, field
from datetime import UTC, date, datetime, timedelta
from typing import IO, NamedTuple

# Apple type -> (metric, {unit as exported: factor to our unit})
QUANTITY: dict[str, tuple[str, dict[str, float]]] = {
    "HKQuantityTypeIdentifierStepCount": ("steps", {"count": 1.0}),
    "HKQuantityTypeIdentifierHeartRate": ("hr", {"count/min": 1.0}),
    "HKQuantityTypeIdentifierRestingHeartRate": ("hr_resting", {"count/min": 1.0}),
    "HKQuantityTypeIdentifierHeartRateVariabilitySDNN": ("hrv_sdnn", {"ms": 1.0}),
    "HKQuantityTypeIdentifierVO2Max": ("vo2max", {"mL/min·kg": 1.0, "mL/(kg·min)": 1.0, "ml/(kg*min)": 1.0}),
    "HKQuantityTypeIdentifierOxygenSaturation": ("spo2", {"%": 100.0}),  # exported as a fraction
    "HKQuantityTypeIdentifierRespiratoryRate": ("resp_rate", {"count/min": 1.0}),
    "HKQuantityTypeIdentifierBodyMass": ("weight", {"kg": 1.0, "lb": 0.45359237, "g": 0.001}),
    "HKQuantityTypeIdentifierBloodPressureSystolic": ("bp_systolic", {"mmHg": 1.0}),
    "HKQuantityTypeIdentifierBloodPressureDiastolic": ("bp_diastolic", {"mmHg": 1.0}),
    "HKQuantityTypeIdentifierActiveEnergyBurned": ("active_energy", {"kcal": 1.0, "Cal": 1.0, "kJ": 1 / 4.184}),
    "HKQuantityTypeIdentifierDistanceWalkingRunning": ("distance", {"km": 1.0, "mi": 1.609344, "m": 0.001}),
}
SLEEP_TYPE = "HKCategoryTypeIdentifierSleepAnalysis"
SLEEP_VALUES = {
    "HKCategoryValueSleepAnalysisInBed": "sleep_in_bed",
    "HKCategoryValueSleepAnalysisAsleep": "sleep_asleep",
    "HKCategoryValueSleepAnalysisAsleepUnspecified": "sleep_asleep",
    "HKCategoryValueSleepAnalysisAsleepCore": "sleep_asleep",
    "HKCategoryValueSleepAnalysisAsleepDeep": "sleep_asleep",
    "HKCategoryValueSleepAnalysisAsleepREM": "sleep_asleep",
}

_DEVICE_NAME = re.compile(r"name:([^,>]+)")


class Sample(NamedTuple):
    metric: str
    t: datetime  # UTC
    t_end: datetime | None  # UTC
    v: float
    tz_offset_min: int
    device: str


def parse_time(s: str) -> tuple[datetime, int]:
    """'2026-10-01 08:00:00 +0200' -> (UTC datetime, offset minutes). Hand-sliced: strptime is the
    bottleneck on a few million records."""
    sign = -1 if s[20] == "-" else 1
    off = sign * (int(s[21:23]) * 60 + int(s[23:25]))
    local = datetime(int(s[0:4]), int(s[5:7]), int(s[8:10]), int(s[11:13]), int(s[14:16]), int(s[17:19]), tzinfo=UTC)
    return local - timedelta(minutes=off), off


def device_name(attrib: dict) -> str:
    m = _DEVICE_NAME.search(attrib.get("device") or "")
    if m:
        return m.group(1).strip()
    return (attrib.get("sourceName") or "unknown").strip()


@dataclass
class ExportReader:
    """Iterate `samples()` once; `me`, `clinical_paths` and `counts` fill in as the file is read."""

    stream: IO[bytes]
    me: dict = field(default_factory=dict)
    export_date: str | None = None
    clinical_paths: list[str] = field(default_factory=list)
    counts: Counter = field(default_factory=Counter)  # what was skipped, by kind

    @property
    def birth_date(self) -> date | None:
        dob = self.me.get("HKCharacteristicTypeIdentifierDateOfBirth")
        try:
            return date.fromisoformat(dob) if dob else None
        except ValueError:
            return None

    def samples(self) -> Iterator[Sample]:
        depth = 0
        root = None
        for event, el in ET.iterparse(self.stream, events=("start", "end")):
            if event == "start":
                if root is None:
                    root = el
                depth += 1
                continue
            depth -= 1
            if depth != 1:
                continue  # nested metadata: handled with its parent
            sample = self._element(el)
            if sample is not None:
                yield sample
            root.clear()  # drop the finished element: this is what keeps memory flat

    def _element(self, el: ET.Element) -> Sample | None:
        tag, a = el.tag, el.attrib
        if tag == "Record":
            return self._record(a)
        if tag == "Me":
            self.me = dict(a)
        elif tag == "ExportDate":
            self.export_date = a.get("value")
        elif tag == "ClinicalRecord":
            path = a.get("resourceFilePath")
            if path:
                self.clinical_paths.append(path.lstrip("/"))
        else:
            self.counts[f"skipped:{tag}"] += 1
        return None

    def _record(self, a: dict) -> Sample | None:
        typ = a.get("type", "")
        try:
            t, off = parse_time(a["startDate"])
            t_end = parse_time(a["endDate"])[0] if a.get("endDate") else None
            if typ == SLEEP_TYPE:
                metric = SLEEP_VALUES.get(a.get("value", ""))
                if metric is None or t_end is None:
                    self.counts["skipped:sleep_awake"] += 1
                    return None
                return Sample(metric, t, t_end, (t_end - t).total_seconds() / 60, off, device_name(a))
            spec = QUANTITY.get(typ)
            if spec is None:
                self.counts[f"skipped:{typ}"] += 1
                return None
            metric, units = spec
            factor = units.get(a.get("unit", ""))
            if factor is None:
                self.counts[f"unit_unconvertible:{metric}"] += 1
                return None
            return Sample(metric, t, t_end, float(a["value"]) * factor, off, device_name(a))
        except (KeyError, ValueError, IndexError):
            self.counts["date_invalid"] += 1
            return None
