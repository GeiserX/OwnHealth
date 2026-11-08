"""Rule-based classifier: type, language, patient block, dates, status, identity (ingestion.md 5, 9)."""

from __future__ import annotations

from datetime import date, datetime

import pytest

from ownhealth.ingest.classify import classify, identity_match, parse_date, parse_datetime


@pytest.mark.parametrize(
    ("s", "want"),
    [
        ("14/05/1979", date(1979, 5, 14)),
        ("14.05.1979", date(1979, 5, 14)),
        ("1979-05-14", date(1979, 5, 14)),
        ("14 May 1979", date(1979, 5, 14)),
        ("May 14, 1979", date(1979, 5, 14)),
        ("3 de marzo", None),
        ("31/02/2026", None),
    ],
)
def test_parse_date(s, want):
    assert parse_date(s) == want


def test_parse_datetime_keeps_the_time():
    assert parse_datetime("14/09/2026 08:15") == datetime(2026, 9, 14, 8, 15)


def test_patient_block_dates_and_language_es():
    c = classify(
        [
            "LabNorte",
            "Informe de resultados de laboratorio",
            "Paciente: RIVERA, ALEX      F. nacimiento: 14/05/1979",
            "Fecha de toma: 14/09/2026 08:15      Nº muestra: LN-1",
            "Fecha de informe: 14/09/2026 13:40      Página 1/3",
            "Prueba Resultado Unidades Valores de referencia",
            "HEMOGRAMA",
        ]
    )
    assert (c.doc_type, c.language, c.report_status) == ("lab_report", "es", "final")
    assert c.patient_name == "RIVERA, ALEX"
    assert c.birth_date == date(1979, 5, 14)
    assert c.collected_at == datetime(2026, 9, 14, 8, 15)
    assert c.reported_at == datetime(2026, 9, 14, 13, 40)
    assert c.sample_id == "LN-1"


@pytest.mark.parametrize(
    ("line", "status"),
    [
        ("Laborbefund - Vorläufiger Befund", "preliminary"),
        ("Laborbefund - KORRIGIERTER BEFUND", "amended"),
        ("Informe de resultados - INFORME RECTIFICADO", "amended"),
        ("Compte rendu provisoire", "preliminary"),
        ("Laboratory report", "final"),
    ],
)
def test_report_status(line, status):
    assert classify([line, "Patient: X"]).report_status == status


def test_non_lab_types():
    assert classify(["Informe de alta hospitalaria", "Fecha de ingreso: 01/02/2026"]).doc_type == "discharge"
    assert classify(["Radiology report", "Findings: no acute change", "Impression: normal MRI"]).doc_type == (
        "imaging_report"
    )
    assert classify(["Hola", "nada que ver"]).doc_type == "other"


def test_identity_match():
    born = date(2017, 6, 21)
    assert identity_match("RIVERA, LUCÍA", born, "Lucía Rivera", born) == "matched"
    assert identity_match("Lucia Rivera", born, "Lucía Rivera", born) == "matched"  # accent-insensitive
    assert identity_match("RIVERA, ALEX", date(1979, 5, 14), "Lucía Rivera", born) == "mismatch"
    assert identity_match("RIVERA, LUCIA", date(2017, 6, 22), "Lucía Rivera", born) == "mismatch"
    assert identity_match("RIVERA, LUCIA", None, "Lucía Rivera", born) == "hinted"
    assert identity_match(None, None, "Lucía Rivera", born) == "missing"
