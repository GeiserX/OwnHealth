"""Synthetic Apple Health export.zip for tests, the demo seed and the memory measurement.

The layout follows a real export (`apple_health_export/export.xml` with its DTD header, `<Me>`,
`Record` elements with `sourceName` and an escaped `device` string, `clinical-records/*.json`,
`electrocardiograms/*.csv`). Values are made up; the people are the synthetic Rivera family.
The XML is written in a stream, so a file of any size costs no memory to make.
"""

from __future__ import annotations

import json
import random
import zipfile
from dataclasses import dataclass, field
from datetime import date, timedelta
from pathlib import Path
from xml.sax.saxutils import quoteattr

HEADER = """<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE HealthData [
<!ELEMENT HealthData (ExportDate,Me,(Record|Correlation|Workout|ActivitySummary|ClinicalRecord)*)>
<!ATTLIST HealthData locale CDATA #REQUIRED>
<!ELEMENT ExportDate EMPTY>
<!ATTLIST ExportDate value CDATA #REQUIRED>
<!ELEMENT Me EMPTY>
<!ELEMENT Record ((MetadataEntry|HeartRateVariabilityMetadataList)*)>
<!ELEMENT MetadataEntry EMPTY>
]>
<HealthData locale="es_ES">
"""

WATCH = "<<HKDevice: 0x6000>, name:Apple Watch, manufacturer:Apple Inc., model:Watch, hardware:Watch7,2, software:11.1>"
IPHONE = "<<HKDevice: 0x6001>, name:iPhone, manufacturer:Apple Inc., model:iPhone, hardware:iPhone16,1, software:26.1>"


@dataclass
class ExportSpec:
    first_name: str = "Alex"
    birth_date: date = date(1979, 5, 14)
    end: date = date(2026, 11, 7)
    days: int = 90
    tz: str = "+0100"
    watch: bool = True  # steps, resting HR, HRV, sleep, VO2max
    iphone: bool = True  # steps too: the overlap the priority rule must not double count
    in_bed: bool = True  # an InBed record around each night's sleep stages
    hrv: bool = True  # nightly HRV (Apple measures SDNN)
    hr_every_min: int = 0  # >0 adds Watch heart rate samples this often (the large-file knob)
    seed: int = 7
    clinical: list[dict] = field(default_factory=list)  # FHIR resources for clinical-records/
    ecg_count: int = 0


def _ts(d: date, h: int, m: int, tz: str) -> str:
    return f"{d.isoformat()} {h:02d}:{m:02d}:00 {tz}"


def _rec(typ: str, src: str, device: str, unit: str | None, start: str, end: str, value) -> str:
    unit_attr = f" unit={quoteattr(unit)}" if unit else ""
    return (
        f'<Record type={quoteattr(typ)} sourceName={quoteattr(src)} sourceVersion="26.1" device={quoteattr(device)}'
        f"{unit_attr} creationDate={quoteattr(end)} startDate={quoteattr(start)} endDate={quoteattr(end)}"
        f" value={quoteattr(str(value))}/>\n"
    )


def iter_xml(spec: ExportSpec):
    """export.xml as a stream of text chunks."""
    rng = random.Random(spec.seed)
    yield HEADER
    yield f' <ExportDate value="{_ts(spec.end, 8, 0, spec.tz)}"/>\n'
    yield (
        f' <Me HKCharacteristicTypeIdentifierDateOfBirth="{spec.birth_date.isoformat()}"'
        ' HKCharacteristicTypeIdentifierBiologicalSex="HKBiologicalSexNotSet"'
        ' HKCharacteristicTypeIdentifierBloodType="HKBloodTypeNotSet"/>\n'
    )
    watch_src, phone_src = f"Apple Watch de {spec.first_name}", f"iPhone de {spec.first_name}"
    start = spec.end - timedelta(days=spec.days - 1)
    for i in range(spec.days):
        d = start + timedelta(days=i)
        prev = d - timedelta(days=1)
        chunk: list[str] = []
        if spec.watch:
            # Sleep that started the evening before, counted on the morning it ends.
            if spec.in_bed:
                chunk.append(
                    _rec(
                        "HKCategoryTypeIdentifierSleepAnalysis",
                        watch_src,
                        WATCH,
                        None,
                        _ts(prev, 23, 10, spec.tz),
                        _ts(d, 7, 0, spec.tz),
                        "HKCategoryValueSleepAnalysisInBed",
                    )
                )
            asleep_end = 6 * 60 + 30 + rng.randint(-25, 25)
            stages = [(23 * 60 + 30, 24 * 60 + 90, "Core"), (24 * 60 + 90, 24 * 60 + 150, "Deep")]
            stages += [(24 * 60 + 150, 24 * 60 + 155, "Awake"), (24 * 60 + 155, 24 * 60 + asleep_end, "REM")]
            for a, b, stage in stages:
                da, db = (prev if a < 24 * 60 else d), (prev if b < 24 * 60 else d)
                chunk.append(
                    _rec(
                        "HKCategoryTypeIdentifierSleepAnalysis",
                        watch_src,
                        WATCH,
                        None,
                        _ts(da, (a // 60) % 24, a % 60, spec.tz),
                        _ts(db, (b // 60) % 24, b % 60, spec.tz),
                        f"HKCategoryValueSleepAnalysis{'Asleep' + stage if stage != 'Awake' else 'Awake'}",
                    )
                )
            chunk.append(
                _rec(
                    "HKQuantityTypeIdentifierRestingHeartRate",
                    watch_src,
                    WATCH,
                    "count/min",
                    _ts(d, 7, 5, spec.tz),
                    _ts(d, 7, 5, spec.tz),
                    56 + rng.randint(-3, 3),
                )
            )
            if spec.hrv:
                chunk.append(
                    _rec(
                        "HKQuantityTypeIdentifierHeartRateVariabilitySDNN",
                        watch_src,
                        WATCH,
                        "ms",
                        _ts(d, 3, 15, spec.tz),
                        _ts(d, 3, 16, spec.tz),
                        round(46 + rng.uniform(-8, 8), 1),
                    )
                )
            if i % 7 == 0:
                chunk.append(
                    _rec(
                        "HKQuantityTypeIdentifierVO2Max",
                        watch_src,
                        WATCH,
                        "mL/min·kg",
                        _ts(d, 18, 30, spec.tz),
                        _ts(d, 18, 30, spec.tz),
                        round(42 + i / 60 + rng.uniform(-0.3, 0.3), 2),
                    )
                )
        for h in range(7, 23):
            steps = rng.randint(150, 900)
            if spec.watch:
                chunk.append(
                    _rec(
                        "HKQuantityTypeIdentifierStepCount",
                        watch_src,
                        WATCH,
                        "count",
                        _ts(d, h, 0, spec.tz),
                        _ts(d, h, 59, spec.tz),
                        steps,
                    )
                )
            if spec.iphone and 8 <= h <= 21:
                chunk.append(
                    _rec(
                        "HKQuantityTypeIdentifierStepCount",
                        phone_src,
                        IPHONE,
                        "count",
                        _ts(d, h, 0, spec.tz),
                        _ts(d, h, 59, spec.tz),
                        int(steps * 0.9),
                    )
                )
        if spec.watch and spec.hr_every_min:
            for m in range(0, 24 * 60, spec.hr_every_min):
                t = _ts(d, m // 60, m % 60, spec.tz)
                chunk.append(
                    _rec(
                        "HKQuantityTypeIdentifierHeartRate",
                        watch_src,
                        WATCH,
                        "count/min",
                        t,
                        t,
                        60 + rng.randint(-8, 30),
                    )
                )
        yield "".join(chunk)
    for r in spec.clinical:
        path = f"/clinical-records/{r['resourceType']}-{r['id']}.json"
        yield (
            f' <ClinicalRecord type="HKClinicalTypeIdentifierLabResultRecord" identifier="{r["id"]}"'
            f' sourceName="Hospital LabNorte" fhirVersion="4.0.1" receivedDate="{_ts(spec.end, 8, 0, spec.tz)}"'
            f' resourceFilePath="{path}"/>\n'
        )
    yield "</HealthData>\n"


def ecg_csv(spec: ExportSpec, n: int) -> str:
    d = spec.end - timedelta(days=10 * (n + 1))
    lines = [
        f"Name,{spec.first_name} Rivera",
        f"Date of Birth,{spec.birth_date:%d %b %Y}",
        f"Recorded Date,{_ts(d, 9, 30, spec.tz)}",
        "Classification,Sinus Rhythm",
        "Symptoms,",
        "Software Version,2.0",
        "Device,Watch7.2",
        "Sample Rate,512 hertz",
        "",
        "Lead,Lead I",
        "Unit,µV",
        "",
    ]
    rng = random.Random(n)
    lines += [f"{rng.uniform(-80, 300):.3f}" for _ in range(512)]
    return "\n".join(lines) + "\n"


def write_export(path: Path, spec: ExportSpec) -> Path:
    """Write the zip to `path`. Returns `path`."""
    with zipfile.ZipFile(path, "w", compression=zipfile.ZIP_DEFLATED, compresslevel=1) as z:
        with z.open("apple_health_export/export.xml", "w", force_zip64=True) as f:
            for chunk in iter_xml(spec):
                f.write(chunk.encode())
        for r in spec.clinical:
            z.writestr(f"apple_health_export/clinical-records/{r['resourceType']}-{r['id']}.json", json.dumps(r))
        for n in range(spec.ecg_count):
            z.writestr(f"apple_health_export/electrocardiograms/ecg_{n + 1}.csv", ecg_csv(spec, n))
    return path


def demo_clinical(first_name: str = "Alex") -> list[dict]:
    """Two clinical records as Apple stores them: one lab result, one prescription."""
    return [
        {
            "resourceType": "Observation",
            "id": f"ah-{first_name.lower()}-ferritin",
            "status": "final",
            "category": [
                {
                    "coding": [
                        {
                            "system": "http://terminology.hl7.org/CodeSystem/observation-category",
                            "code": "laboratory",
                        }
                    ]
                }
            ],
            "code": {
                "coding": [{"system": "http://loinc.org", "code": "2276-4", "display": "Ferritin"}],
                "text": "Ferritin",
            },
            "effectiveDateTime": "2026-06-02T08:15:00+02:00",
            "valueQuantity": {"value": 68, "unit": "ng/mL", "system": "http://unitsofmeasure.org", "code": "ng/mL"},
            "referenceRange": [{"low": {"value": 30}, "high": {"value": 300}}],
        },
        {
            "resourceType": "MedicationRequest",
            "id": f"ah-{first_name.lower()}-cetirizine",
            "status": "active",
            "intent": "order",
            "medicationCodeableConcept": {
                "coding": [{"system": "http://www.nlm.nih.gov/research/umls/rxnorm", "code": "1014678"}],
                "text": "Cetirizine 10 mg tablet",
            },
            "authoredOn": "2026-04-03",
            "dosageInstruction": [{"text": "1 tablet daily in spring"}],
        },
    ]
