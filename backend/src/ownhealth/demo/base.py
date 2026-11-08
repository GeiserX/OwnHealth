"""Base demo data (foundation): lab reports with real text-layer PDFs, records, notes, wearables and an
open review queue. Values follow the Alex Rivera fixture (docs/design/architecture.md section 16) and
the mockup dashboard. All synthetic; the only lab is the fictional LabNorte.
"""

from __future__ import annotations

import json
import random
from datetime import UTC, date, datetime, timedelta
from decimal import Decimal

from sqlalchemy import Connection, text

from ownhealth.core.acceptance import Check
from ownhealth.core.documents import create_document
from ownhealth.core.ids import new_src_id
from ownhealth.core.observations import ObservationIn, commit_observation
from ownhealth.core.provenance import SrcIn
from ownhealth.core.records import AllergyIn, ConditionIn, ImmunizationIn, MedicationIn, add_record
from ownhealth.core.review import (
    CandidateIn,
    ReviewItemIn,
    create_extraction_run,
    create_item,
    submit_candidate,
)
from ownhealth.demo import DemoContext
from ownhealth.demo.pdf import PAGE_H, PAGE_W, LabRow, lab_report_pdf

LAB = "LabNorte"
DERIVATION = "demo-seed"

# alias_group: (loinc, printed name (es), unit printed, UCUM, ref text, low, high, page, specimen)
ANALYTES: dict[str, tuple] = {
    "hemoglobin": ("718-7", "Hemoglobina", "g/dL", "g/dL", "13.5 - 17.5", "13.5", "17.5", 1, "blood"),
    "platelets": ("777-3", "Plaquetas", "x10^9/L", "10*9/L", "150 - 400", "150", "400", 1, "blood"),
    "mcv": ("787-2", "VCM", "fL", "fL", "80 - 100", "80", "100", 1, "blood"),
    "alt": ("1742-6", "ALT (GPT)", "U/L", "U/L", "7 - 41", "7", "41", 1, "serum"),
    "ggt": ("2324-2", "GGT", "U/L", "U/L", "8 - 61", "8", "61", 1, "serum"),
    "creatinine": ("2160-0", "Creatinina", "mg/dL", "mg/dL", "0.70 - 1.20", "0.70", "1.20", 1, "serum"),
    "egfr": (
        "98979-8",
        "Filtrado glomerular (CKD-EPI 2021)",
        "mL/min/1.73m2",
        "mL/min/{1.73_m2}",
        "> 90",
        "90",
        None,
        1,
        "serum",
    ),
    "tsh": ("3016-3", "TSH", "mUI/L", "m[IU]/L", "0.40 - 4.00", "0.40", "4.00", 1, "serum"),
    "ft4": ("3024-7", "T4 libre", "ng/dL", "ng/dL", "0.90 - 1.70", "0.90", "1.70", 1, "serum"),
    "hba1c": ("4548-4", "Hemoglobina glicada (HbA1c)", "%", "%", "4.0 - 5.6", "4.0", "5.6", 1, "blood"),
    "urine_acr": ("9318-7", "Cociente albúmina/creatinina", "mg/g", "mg/g", "< 30", None, "30", 1, "urine"),
    "ldl_c": ("13457-7", "Colesterol LDL", "mg/dL", "mg/dL", "< 130", None, "130", 2, "serum"),
    "lpa": ("43583-4", "Lipoproteína (a)", "nmol/L", "nmol/L", "< 75", None, "75", 2, "serum"),
    "crp_hs": ("30522-7", "Proteína C reactiva ultrasensible", "mg/L", "mg/L", "< 3.0", None, "3.0", 2, "serum"),
    "vitamin_d_25oh": ("62292-8", "Vitamina D (25-OH)", "ng/mL", "ng/mL", "30 - 100", "30", "100", 2, "serum"),
    "ferritin": ("2276-4", "Ferritina", "ng/mL", "ng/mL", "30 - 400", "30", "400", 2, "serum"),
    "vitamin_b12": ("2132-9", "Vitamina B12", "pg/mL", "pg/mL", "200 - 900", "200", "900", 2, "serum"),
}

ALEX_SERIES: dict[str, dict[str, str]] = {
    "ldl_c": {
        "2021-11-09": "163",
        "2022-09-15": "96",
        "2023-03-21": "88",
        "2023-10-04": "92",
        "2024-05-22": "84",
        "2025-01-20": "79",
        "2025-09-08": "101",
        "2026-03-02": "118",
        "2026-09-14": "97",
    },
    "lpa": {"2023-03-21": "138", "2025-09-08": "142"},
    "hba1c": {"2022-09-15": "5.3", "2023-03-21": "5.4", "2025-01-20": "5.4", "2026-03-02": "5.5", "2026-09-14": "5.6"},
    "alt": {
        "2023-03-21": "31",
        "2023-10-04": "28",
        "2024-05-22": "26",
        "2025-01-20": "22",
        "2026-03-02": "27",
        "2026-09-14": "24",
    },
    "ggt": {"2023-10-04": "38", "2024-05-22": "35", "2025-01-20": "29", "2026-03-02": "33", "2026-09-14": "31"},
    "egfr": {"2023-10-04": "99", "2024-05-22": "97", "2025-01-20": "98", "2026-03-02": "95", "2026-09-14": "94"},
    "creatinine": {
        "2023-10-04": "0.92",
        "2024-05-22": "0.94",
        "2025-01-20": "0.93",
        "2026-03-02": "0.97",
        "2026-09-14": "0.98",
    },
    "urine_acr": {"2023-03-21": "6", "2023-10-04": "9", "2024-05-22": "8"},
    "tsh": {"2022-09-15": "1.8", "2023-10-04": "2.4", "2024-05-22": "1.9", "2025-01-20": "2.1"},
    "ft4": {"2022-09-15": "1.1", "2023-10-04": "1.3", "2024-05-22": "1.2", "2025-01-20": "1.2"},
    "hemoglobin": {
        "2024-05-22": "14.6",
        "2025-01-20": "14.9",
        "2025-09-08": "15.3",
        "2026-03-02": "15.0",
        "2026-09-14": "15.1",
    },
    "platelets": {
        "2024-05-22": "244",
        "2025-01-20": "238",
        "2025-09-08": "226",
        "2026-03-02": "229",
        "2026-09-14": "231",
    },
    "mcv": {"2024-05-22": "89", "2025-01-20": "90", "2025-09-08": "90", "2026-03-02": "91", "2026-09-14": "91"},
    "crp_hs": {"2024-05-22": "0.9", "2025-01-20": "1.3", "2025-09-08": "0.8", "2026-03-02": "1.1", "2026-09-14": "2.4"},
    "vitamin_d_25oh": {
        "2024-05-22": "33",
        "2025-01-20": "29",
        "2025-09-08": "35",
        "2026-03-02": "31",
        "2026-09-14": "24",
    },
    "ferritin": {"2024-05-22": "88", "2025-01-20": "104", "2025-09-08": "91", "2026-03-02": "99", "2026-09-14": "96"},
    "vitamin_b12": {"2025-01-20": "480", "2025-09-08": "455", "2026-03-02": "530", "2026-09-14": "512"},
}

# The citation ids the docs use (architecture.md section 16). Everything else gets a generated id.
ALEX_SRC_IDS = {
    ("vitamin_d_25oh", "2026-09-14"): "src_a12",
    ("lpa", "2025-09-08"): "src_a07",
    ("tsh", "2025-01-20"): "src_a05",
    ("ldl_c", "2021-11-09"): "src_1a0",
    ("ldl_c", "2022-09-15"): "src_2b4",
    ("ldl_c", "2023-03-21"): "src_2c1",
    ("ldl_c", "2023-10-04"): "src_3c8",
    ("ldl_c", "2024-05-22"): "src_3d2",
    ("ldl_c", "2026-03-02"): "src_9f3",
}
DIRECT_LDL_DATES = {"2023-10-04", "2024-05-22"}  # direct LDL, LOINC 18262-6

OTHER_SERIES: dict[str, dict[str, dict[str, str]]] = {
    "sam": {
        "ldl_c": {"2026-06-10": "122"},
        "hba1c": {"2026-06-10": "5.2"},
        "vitamin_d_25oh": {"2026-06-10": "41"},
        "tsh": {"2026-06-10": "1.6"},
        "ferritin": {"2025-06-12": "28", "2026-06-10": "18"},
        "hemoglobin": {"2026-06-10": "12.6"},
    },
    "carmen": {
        "ldl_c": {"2026-07-22": "110"},
        "hba1c": {"2025-07-15": "6.1", "2026-07-22": "6.4"},
        "egfr": {"2025-07-15": "64", "2026-07-22": "58"},
        "creatinine": {"2026-07-22": "0.98"},
        "vitamin_d_25oh": {"2026-07-22": "34"},
    },
    "lucia": {
        "hemoglobin": {"2026-02-18": "12.9"},
        "ferritin": {"2026-02-18": "35"},
        "vitamin_d_25oh": {"2026-02-18": "32"},
    },
}
# Reference ranges that differ for the other profiles (sex, age).
RANGE_OVERRIDES = {
    ("sam", "ferritin"): ("15 - 150", "15", "150"),
    ("sam", "hemoglobin"): ("12.0 - 15.5", "12.0", "15.5"),
    ("carmen", "egfr"): ("> 60", "60", None),
    ("lucia", "hemoglobin"): ("11.5 - 15.5", "11.5", "15.5"),
    ("lucia", "ferritin"): ("15 - 150", "15", "150"),
}
PATIENT = {
    "alex": ("RIVERA, ALEX", "14/05/1979"),
    "sam": ("RIVERA, SAM", "02/02/1981"),
    "lucia": ("RIVERA, LUCIA", "21/06/2017"),
    "carmen": ("ORTEGA, CARMEN", "30/01/1955"),
}


def _free(conn: Connection, sid: str | None) -> str | None:
    """Use a documented src id only once per database (a forced second seed gets generated ids)."""
    if sid and conn.execute(text("select 1 from src where id = :id"), {"id": sid}).first():
        return None
    return sid


def _dt(day: str, hour: int = 8) -> datetime:
    y, m, d = map(int, day.split("-"))
    return datetime(y, m, d, hour, 0, tzinfo=UTC)


def _interp(value: Decimal, low: Decimal | None, high: Decimal | None) -> str:
    if low is not None and value < low:
        return "L"
    if high is not None and (value > high or (low is None and value >= high)):
        return "H"
    return "N"


def _display(v: str) -> str:
    return v.replace(".", ",")  # Spanish decimal comma on the printed report


def _seed_reports(
    conn: Connection, ctx: DemoContext, slug: str, series: dict[str, dict[str, str]], src_ids: dict | None = None
) -> None:
    fam = ctx.family
    profile_id = fam.profiles[slug]
    by_date: dict[str, list[str]] = {}
    for group, points in series.items():
        for day in points:
            by_date.setdefault(day, []).append(group)
    for n, day in enumerate(sorted(by_date), start=1):
        groups = sorted(by_date[day], key=lambda g: (ANALYTES[g][7], list(ANALYTES).index(g)))
        pages: list[list[tuple[str, LabRow]]] = [[], []]
        for g in groups:
            loinc, name, unit, ucum, ref, low, high, page, specimen = ANALYTES[g]
            ref, low, high = RANGE_OVERRIDES.get((slug, g), (ref, low, high))
            value = series[g][day]
            flag = _interp(Decimal(value), Decimal(low) if low else None, Decimal(high) if high else None)
            pages[page - 1].append((g, LabRow(name, _display(value), unit, ref, "" if flag == "N" else flag)))
        pages = [p for p in pages if p]
        patient, birth = PATIENT[slug]
        report_id = f"LN-{day.replace('-', '')}-{slug[:2].upper()}{n}"
        pdf, boxes = lab_report_pdf(
            lab=LAB,
            title="Informe de resultados de laboratorio",
            patient=patient,
            birth=birth,
            collected=f"{day[8:10]}/{day[5:7]}/{day[0:4]}",
            report_id=report_id,
            pages=[[row for _, row in p] for p in pages],
        )
        sha = ctx.blobs.put_bytes(pdf)
        doc = create_document(
            conn,
            household_id=fam.household_id,
            profile_id=profile_id,
            blob_sha256=sha,
            mime="application/pdf",
            size_bytes=len(pdf),
            filename=f"{LAB}_{day}.pdf",
            channel="demo",
            pages=len(pages),
            doc_type="lab_report",
        )
        conn.execute(
            text(
                "update document set language = 'es', provider_name = :lab, provider_fingerprint = 'labnorte-es',"
                " report_status = 'final', collected_at = :c, reported_at = :r, identity_match = 'matched'"
                " where id = :d"
            ),
            {"lab": LAB, "c": _dt(day), "r": _dt(day) + timedelta(days=1), "d": doc.id},
        )
        ctx.documents[f"{slug}:{day}"] = doc.id
        for page_no, (page_rows, page_boxes) in enumerate(zip(pages, boxes, strict=True), start=1):
            page_text = "\n".join(f"{r.name} {r.value} {r.unit} {r.ref} {r.flag}".strip() for _, r in page_rows)
            conn.execute(
                text(
                    "insert into document_page (document_id, page_no, width_pt, height_pt, text_layer_ok,"
                    " text_content, fts) values (:d, :p, :w, :h, true, :t, to_tsvector('simple', :t))"
                ),
                {"d": doc.id, "p": page_no, "w": PAGE_W, "h": PAGE_H, "t": page_text},
            )
            for (g, row), bbox in zip(page_rows, page_boxes, strict=True):
                loinc, name, unit, ucum, ref, low, high, _, specimen = ANALYTES[g]
                ref, low, high = RANGE_OVERRIDES.get((slug, g), (ref, low, high))
                value = Decimal(series[g][day])
                method = None
                if g == "ldl_c":
                    if day in DIRECT_LDL_DATES:
                        loinc, method = "18262-6", "direct"
                    else:
                        method = "calculated"
                src = SrcIn(
                    kind="document_region",
                    profile_id=profile_id,
                    document_id=doc.id,
                    page_no=page_no,
                    bbox=bbox,
                    label=f"{LAB} · {day} · p{page_no}",
                    id=_free(conn, (src_ids or {}).get((g, day))),
                )
                commit_observation(
                    conn,
                    ObservationIn(
                        profile_id=profile_id,
                        printed_name=name,
                        printed_value=row.value,
                        printed_unit=unit,
                        printed_ref_range=ref,
                        printed_flag=row.flag or None,
                        value_num=value,
                        ref_low=Decimal(low) if low else None,
                        ref_high=Decimal(high) if high else None,
                        specimen=specimen,
                        method=method,
                        loinc=loinc,
                        alias_group=g,
                        unit_ucum=ucum,
                        value_canonical=value,
                        unit_canonical=ucum,
                        interpretation=_interp(value, Decimal(low) if low else None, Decimal(high) if high else None),
                        derivation_version=DERIVATION,
                        effective_at=_dt(day),
                        document_id=doc.id,
                        report_key=report_id,
                    ),
                    src=src,
                    accepted_by="auto",
                )


def _seed_records(conn: Connection, ctx: DemoContext) -> None:
    p = ctx.family.profiles
    alex_user = ctx.family.users["alex"]

    def manual(sid: str | None = None, label: str = "manual entry") -> SrcIn:
        return SrcIn(kind="manual", actor_kind="user", actor_id=alex_user, label=label, id=_free(conn, sid))

    add_record(
        conn,
        MedicationIn(
            profile_id=p["alex"],
            name="Atorvastatin",
            ingredient="atorvastatin",
            strength="20 mg",
            dose_text="1 tablet daily",
            started=date(2022, 6, 20),
            stopped=date(2025, 12, 1),
            med_status="stopped",
            reason_text="paused",
        ),
        src=manual("src_m01", "Rx · 2022-06-20"),
        accepted_by="person",
    )
    add_record(
        conn,
        MedicationIn(
            profile_id=p["alex"],
            name="Atorvastatin",
            ingredient="atorvastatin",
            strength="20 mg",
            dose_text="1 tablet daily",
            started=date(2026, 3, 10),
        ),
        src=manual("src_m02", "Rx · 2026-03-10"),
        accepted_by="person",
    )
    add_record(
        conn,
        MedicationIn(
            profile_id=p["alex"],
            name="Omeprazole",
            ingredient="omeprazole",
            strength="20 mg",
            dose_text="1 capsule before breakfast",
            started=date(2026, 4, 1),
        ),
        src=manual("src_m05", "Rx · 2026-04-01"),
        accepted_by="person",
    )
    add_record(
        conn,
        MedicationIn(
            profile_id=p["carmen"],
            name="Metformin",
            ingredient="metformin",
            strength="850 mg",
            dose_text="1 tablet twice daily",
            started=date(2024, 2, 1),
        ),
        src=manual(label="Rx · 2024-02-01"),
        accepted_by="person",
    )
    add_record(
        conn,
        ConditionIn(
            profile_id=p["alex"],
            text="Hypercholesterolaemia",
            code_system="icd10",
            code="E78.0",
            verification_status="confirmed",
            onset=date(2021, 11, 9),
        ),
        src=manual("src_c02"),
        accepted_by="person",
    )
    add_record(
        conn,
        ConditionIn(
            profile_id=p["carmen"],
            text="Type 2 diabetes mellitus",
            code_system="icd10",
            code="E11.9",
            verification_status="confirmed",
            onset=date(2024, 1, 15),
        ),
        src=manual(),
        accepted_by="person",
    )
    add_record(
        conn,
        AllergyIn(profile_id=p["sam"], substance="Penicillin", criticality="high", reaction_text="hives"),
        src=manual(),
        accepted_by="person",
    )
    add_record(
        conn,
        ImmunizationIn(profile_id=p["alex"], vaccine="Td (tetanus, diphtheria)", occurred=date(2016, 5, 10)),
        src=manual(),
        accepted_by="person",
    )
    add_record(
        conn,
        ImmunizationIn(profile_id=p["alex"], vaccine="Influenza 2025-26", occurred=date(2025, 10, 21)),
        src=manual(),
        accepted_by="person",
    )
    sid = _free(conn, "src_n41") or new_src_id()
    conn.execute(
        text(
            "insert into src (id, profile_id, kind, actor_kind, actor_id, label) values"
            " (:id, :p, 'manual', 'user', :u, 'note · 2026-10-30')"
        ),
        {"id": sid, "p": p["alex"], "u": alex_user},
    )
    conn.execute(
        text(
            "insert into note (profile_id, kind, author_kind, author_id, lang, attach_to, body, effective_at,"
            " recorded_at, src_id) values (:p, 'priorities', 'person', :u, 'en', 'status',"
            " 'Recheck vitamin D in winter.', :t, :t, :s)"
        ),
        {"p": p["alex"], "u": alex_user, "t": _dt("2026-10-30", 19), "s": sid},
    )


WEARABLE_METRICS = {
    # metric: (start value, end value, noise, per-day sample unit)
    "hr_resting": (58.0, 54.0, 1.5),  # bpm
    "hrv_rmssd": (44.0, 48.0, 4.0),  # ms
    "steps": (8100.0, 9240.0, 1800.0),  # count per day
    "sleep_asleep": (420.0, 432.0, 35.0),  # minutes per night
    "vo2max": (41.2, 44.1, 0.3),  # mL/kg/min
}


def _seed_wearables(conn: Connection, ctx: DemoContext) -> None:
    profile_id = ctx.family.profiles["alex"]
    source_id = conn.execute(
        text(
            "insert into wearable_source (profile_id, provider, device) values (:p, 'apple_health', 'Apple Watch')"
            " returning id"
        ),
        {"p": profile_id},
    ).scalar_one()
    rng = random.Random(20261107)
    days = 90
    start = ctx.as_of - timedelta(days=days - 1)
    rows = []
    for metric, (a, b, noise) in WEARABLE_METRICS.items():
        for i in range(days):
            day = start + timedelta(days=i)
            if metric == "vo2max" and i % 7:
                continue
            v = a + (b - a) * i / (days - 1) + rng.uniform(-noise, noise)
            t = datetime(day.year, day.month, day.day, 6, 0, tzinfo=UTC)
            rows.append({"p": profile_id, "m": metric, "t": t, "v": round(v, 1), "s": source_id})
    conn.execute(
        text(
            "insert into wearable_sample (profile_id, metric, t, v, tz_offset_min, source_id)"
            " values (:p, :m, :t, :v, 60, :s)"
        ),
        rows,
    )
    conn.execute(
        text(
            "insert into wearable_rollup (profile_id, metric, source_id, bucket, t, n, v_min, v_max, v_avg, v_sum,"
            " v_p10, v_p50, v_p90, coverage) select profile_id, metric, source_id, 'day', date_trunc('day', t),"
            " count(*), min(v), max(v), avg(v), sum(v), percentile_cont(0.1) within group (order by v),"
            " percentile_cont(0.5) within group (order by v), percentile_cont(0.9) within group (order by v), 1.0"
            " from wearable_sample where profile_id = :p group by 1, 2, 3, 5"
        ),
        {"p": profile_id},
    )


def _seed_review_queue(conn: Connection, ctx: DemoContext) -> None:
    """A new LabNorte report whose rows wait for review, and one agent proposal: 7 open items."""
    fam = ctx.family
    profile_id = fam.profiles["alex"]
    day = "2026-11-03"
    rows = [
        ("Glucosa", "92", "mg/dL", "70 - 110", "glucose", "2345-7", 0.91, []),
        (
            "Colesterol HDL",
            "52",
            "mg/dL",
            "> 40",
            "hdl_c",
            "2085-9",
            0.97,
            [Check(name="grounding", ok=False, reason="not_grounded", detail="value token not inside the row box")],
        ),
        (
            "Triglicéridos",
            "110",
            "mg/dL",
            "< 150 mg/dl",
            "triglycerides",
            "2571-8",
            0.96,
            [Check(name="range", ok=False, reason="range_parse", detail="unit inside the range cell")],
        ),
        (
            "Hierro",
            "162",
            "mg/dL",
            "65 - 175",
            "iron",
            "2498-4",
            0.95,
            [
                Check(
                    name="plausibility", ok=False, reason="implausible", detail="iron 162 mg/dL is above the hard limit"
                ),
                Check(name="factor_1000", ok=False, reason="suspect_separator", detail="162 µg/dL would be in range"),
            ],
        ),
        (
            "Leucocitos",
            "6800",
            "x10^3/µL",
            "4.0 - 11.0",
            "wbc",
            "6690-2",
            0.96,
            [Check(name="factor_1000", ok=False, reason="suspect_separator", detail="6.8 x10^3/µL would be in range")],
        ),
        (
            "Índice aterogénico",
            "3.1",
            "",
            "< 4.5",
            "atherogenic_index",
            None,
            0.93,
            [Check(name="loinc", ok=False, reason="loinc_unknown", detail="no LOINC code for this name")],
        ),
    ]
    pdf, boxes = lab_report_pdf(
        lab=LAB,
        title="Informe de resultados de laboratorio",
        patient=PATIENT["alex"][0],
        birth=PATIENT["alex"][1],
        collected="03/11/2026",
        report_id="LN-20261103-AL",
        pages=[[LabRow(n, _display(v), u, r) for n, v, u, r, *_ in rows]],
    )
    sha = ctx.blobs.put_bytes(pdf)
    doc = create_document(
        conn,
        household_id=fam.household_id,
        profile_id=profile_id,
        blob_sha256=sha,
        mime="application/pdf",
        size_bytes=len(pdf),
        filename=f"{LAB}_{day}.pdf",
        channel="demo",
        pages=1,
        doc_type="lab_report",
    )
    conn.execute(
        text(
            "update document set language = 'es', provider_name = :lab, report_status = 'final', collected_at = :c,"
            " identity_match = 'matched' where id = :d"
        ),
        {"lab": LAB, "c": _dt(day), "d": doc.id},
    )
    page_text = "\n".join(f"{n} {_display(v)} {u} {r}" for n, v, u, r, *_ in rows)
    conn.execute(
        text(
            "insert into document_page (document_id, page_no, width_pt, height_pt, text_layer_ok, text_content, fts)"
            " values (:d, 1, :w, :h, true, :t, to_tsvector('simple', :t))"
        ),
        {"d": doc.id, "w": PAGE_W, "h": PAGE_H, "t": page_text},
    )
    ctx.documents[f"alex:{day}"] = doc.id
    run = create_extraction_run(
        conn,
        doc.id,
        extractor="rules:generic-table",
        extractor_version="demo",
        config_hash=b"demo",
        engine="demo-pdf",
        engine_version="1",
    )
    for (name, value, unit, ref, group, loinc, conf, checks), bbox in zip(rows, boxes[0], strict=True):
        cells = {
            "name": {"text": name, "conf": 0.99},
            "value": {"text": _display(value), "conf": conf},
            "unit": {"text": unit, "conf": 0.99},
            "ref_range": {"text": ref, "conf": 0.98},
        }
        submit_candidate(
            conn,
            run,
            CandidateIn(
                page_no=1,
                bbox=bbox,
                cells=cells,
                row_conf=conf,
                checks=checks,
                observation=ObservationIn(
                    profile_id=profile_id,
                    printed_name=name,
                    printed_value=_display(value),
                    printed_unit=unit or None,
                    printed_ref_range=ref,
                    value_num=Decimal(value),
                    loinc=loinc,
                    alias_group=group,
                    unit_ucum=unit or None,
                    derivation_version=DERIVATION,
                    effective_at=_dt(day),
                ),
            ),
            allowance="rules_text",
            identity="matched",
        )
    create_item(
        conn,
        ReviewItemIn(
            kind="agent_proposal",
            household_id=fam.household_id,
            profile_id=profile_id,
            payload={
                "kind": "note",
                "payload": {
                    "text": "Ask about a winter vitamin D recheck at the next visit.",
                    "attach_to": "src_a12",
                    "lang": "en",
                },
                "token_label": "Claude Code · laptop",
                "based_on": ["src_a12"],
            },
            context_src_ids=["src_a12"],
        ),
    )


def seed_base(conn: Connection, ctx: DemoContext) -> None:
    _seed_reports(conn, ctx, "alex", ALEX_SERIES, ALEX_SRC_IDS)
    for slug, series in OTHER_SERIES.items():
        _seed_reports(conn, ctx, slug, series)
    _seed_records(conn, ctx)
    _seed_wearables(conn, ctx)
    _seed_review_queue(conn, ctx)
    conn.execute(
        text(
            "insert into household_setting (household_id, key, value) values (:h, 'age_of_majority', :v)"
            " on conflict do nothing"
        ),
        {"h": ctx.family.household_id, "v": json.dumps(18)},
    )
    ctx.src_ids.update(
        {
            "alex_vitamin_d": "src_a12",
            "alex_lpa": "src_a07",
            "alex_tsh": "src_a05",
            "alex_atorvastatin": "src_m02",
            "alex_omeprazole": "src_m05",
            "alex_note": "src_n41",
        }
    )
