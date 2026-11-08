"""normalize_row: one printed row to an ObservationIn plus every validator (ingestion.md 8, 9)."""

from datetime import UTC, datetime
from decimal import Decimal

import pytest

from ownhealth.core.observations import commit_observation
from ownhealth.core.provenance import SrcIn
from ownhealth.core.review import route_fact
from ownhealth.normalize import DERIVATION_VERSION, normalize_row

WHEN = datetime(2026, 9, 14, 8, tzinfo=UTC)

# A synthetic LabNorte report, one row per line: name, value, unit, range (es locale).
REPORT = [
    ("Hemoglobina", "14,8", "g/dL", "13,5 - 17,5"),
    ("Plaquetas", "245", "x10^9/L", "150 - 400"),
    ("VCM", "91", "fL", "80 - 100"),
    ("Leucocitos", "6,8", "x10^3/µL", "4,0 - 11,0"),
    ("Glucosa", "92", "mg/dL", "70 - 110"),
    ("Hemoglobina glicada (HbA1c)", "5,6", "%", "4,0 - 5,6"),
    ("Creatinina", "0,98", "mg/dL", "0,70 - 1,20"),
    ("Filtrado glomerular (CKD-EPI 2021)", "92", "mL/min/1.73m2", "> 90"),
    ("TSH", "2,10", "µUI/mL", "0,40 - 4,00"),
    ("T4 libre", "1,2", "ng/dL", "0,90 - 1,70"),
    ("Colesterol LDL", "97", "mg/dL", "< 130"),
    ("Triglicéridos", "88", "mg/dL", "< 150"),
    ("Ferritina", "120", "ng/mL", "30 - 400"),
    ("Vitamina D (25-OH)", "24", "ng/mL", "30 - 100"),
    ("ALT (GPT)", "31", "U/L", "7 - 41"),
    ("Sodio", "141", "mmol/L", "136 - 145"),
]


def _row(conn, family, name, value, unit, rng, flag=None, **kw):
    return normalize_row(
        conn,
        profile_id=family.profiles["alex"],
        printed_name=name,
        printed_value=value,
        printed_unit=unit,
        printed_range=rng,
        printed_flag=flag,
        effective_at=kw.pop("effective_at", WHEN),
        lang=kw.pop("lang", "es"),
        **kw,
    )


def _failed(n):
    return sorted(c.reason for c in n.checks if not c.ok)


@pytest.mark.parametrize("row", REPORT, ids=[r[0] for r in REPORT])
def test_clean_rows_pass_every_check(conn, family, row):
    n = _row(conn, family, *row)
    assert _failed(n) == [], [c for c in n.checks if not c.ok]
    o = n.observation
    assert o.loinc and o.value_canonical is not None and o.unit_canonical
    assert o.derivation_version == DERIVATION_VERSION
    assert o.printed_value == row[1] and o.printed_unit == row[2]  # truth layer kept as printed


def test_row_values_are_derived(conn, family):
    o = _row(conn, family, "Colesterol LDL", "3,4", "mmol/L", "< 3,36").observation
    assert (o.loinc, o.alias_group, o.unit_ucum, o.unit_canonical) == ("39469-2", "ldl_c", "mmol/L", "mg/dL")
    assert o.value_canonical.quantize(Decimal("0.1")) == Decimal("131.5")
    assert (o.value_num, o.ref_high, o.interpretation) == (Decimal("3.4"), Decimal("3.36"), "H")
    vit = _row(conn, family, "Vitamina D (25-OH)", "24", "ng/mL", "30 - 100").observation
    assert vit.interpretation == "L"


def _shift(rows):
    """Each row gets the unit printed on the next row: what a misaligned table extraction produces."""
    return [(n, v, rows[(i + 1) % len(rows)][2], r) for i, (n, v, _u, r) in enumerate(rows)]


def test_column_shift_is_caught_on_every_row(conn, family):
    """Unit from the next column: every shifted row fails at least one check, none would auto-accept.
    Rows whose neighbour prints the same unit are not shifted in any visible way and are skipped."""
    missed, shifted = [], 0
    for (name, value, unit, rng), (_n, _v, own, _r) in zip(_shift(REPORT), REPORT, strict=True):
        if unit == own:
            continue
        shifted += 1
        n = _row(conn, family, name, value, unit, rng)
        if not _failed(n):
            missed.append((name, unit))
    assert shifted >= 12 and missed == []


@pytest.mark.parametrize(
    ("name", "value", "unit"),
    [
        ("Hemoglobina glicada (HbA1c)", "5,6", "mg/dL"),  # glucose's unit
        ("TSH", "2,10", "ng/dL"),  # free T4's unit
        ("Creatinina", "0,98", "mL/min/1.73m2"),  # eGFR's unit
        ("Plaquetas", "245", "fL"),  # MCV's unit
        ("Ferritina", "120", "U/L"),  # ALT's unit
        ("Sodio", "141", "%"),
    ],
)
def test_shift_to_another_dimension_is_unit_dimension_mismatch(conn, family, name, value, unit):
    n = _row(conn, family, name, value, unit, None)
    assert "unit_dimension_mismatch" in _failed(n)
    assert n.observation.value_canonical is None and n.observation.unit_canonical is None


def test_shift_inside_the_same_dimension_is_caught_by_plausibility(conn, family):
    n = _row(conn, family, "Colesterol LDL", "97", "ng/mL", "< 130")  # vitamin D's unit: also mass/volume
    assert "implausible" in _failed(n)


def test_unconvertible_unit(conn, family):
    n = _row(conn, family, "Lipoproteína (a)", "45", "mg/dL", "< 30")  # canonical is nmol/L: never converted
    assert "unit_unconvertible" in _failed(n)
    assert n.observation.value_canonical is None and n.observation.printed_value == "45"


def test_flag_mismatch(conn, family):
    n = _row(conn, family, "Glucosa", "92", "mg/dL", "70 - 110", flag="H")
    assert "flag_mismatch" in _failed(n)
    assert _failed(_row(conn, family, "Glucosa", "120", "mg/dL", "70 - 110", flag="H")) == []
    hh = _row(conn, family, "Potasio", "7,1", "mmol/L", "3,5 - 5,1", flag="HH")
    assert hh.observation.interpretation == "HH"  # critical only because the lab printed it


def test_separator_and_factor_1000(conn, family):
    n = _row(conn, family, "Hierro", "162", "mg/dL", "60 - 160")  # µg/dL misread as mg/dL
    assert {"implausible", "suspect_separator"} <= set(_failed(n))
    amb = _row(conn, family, "Glucosa", "1.200", "mg/dL", None)
    assert "ambiguous_separator" in _failed(amb)


def test_value_is_range_and_range_parse(conn, family):
    assert "value_is_range" in _failed(_row(conn, family, "Glucosa", "70 - 110", "mg/dL", "70 - 110"))
    # a value on a bound is common and real (HbA1c 5.6 with 4.0 - 5.6): not a range
    assert _failed(_row(conn, family, "Glucosa", "110", "mg/dL", "70 - 110")) == []
    assert "range_parse" in _failed(_row(conn, family, "Glucosa", "92", "mg/dL", "ver nota"))
    assert "range_parse" in _failed(_row(conn, family, "Glucosa", "92", "mg/dL", "110 - 70"))


def test_sex_specific_range_uses_the_profile(conn, family):
    rng = "Hombres: 13,5-17,5 Mujeres: 12,0-16,0"
    alex = _row(conn, family, "Hemoglobina", "13,0", "g/dL", rng).observation
    assert (alex.ref_low, alex.interpretation, alex.ref_applies) == (Decimal("13.5"), "L", {"sex": "male"})
    sam = normalize_row(
        conn,
        profile_id=family.profiles["sam"],
        printed_name="Hemoglobina",
        printed_value="13,0",
        printed_unit="g/dL",
        printed_range=rng,
        printed_flag=None,
        effective_at=WHEN,
        lang="es",
    )
    assert (sam.observation.ref_low, sam.observation.interpretation) == (Decimal("12.0"), "N")


def test_unmapped_and_panel_rows(conn, family):
    n = _row(conn, family, "Índice aterogénico", "3,1", "", "< 4,5", provider="LabNorte")
    assert "unmapped" in _failed(n) and n.observation.alias_group == "indice_aterogenico@labnorte"
    assert n.observation.loinc is None
    p = _row(conn, family, "Perfil lipídico", "", None, None)
    assert "unmapped" in _failed(p)


def test_text_result(conn, family):
    o = _row(conn, family, "Nitritos", "Negativo", None, "Negativo").observation
    assert (o.value_code, o.value_text, o.interpretation, o.loinc) == ("negative", "Negativo", "N", "5802-4")
    pos = _row(conn, family, "Nitritos", "Positivo", None, "Negativo")
    assert pos.observation.interpretation == "A" and _failed(pos) == []


def test_structured_code_is_kept_and_checked(conn, family):
    ok = _row(conn, family, "LDL Chol Calc (NIH)", "97", "mg/dL", None, loinc="13457-7", lang="en")
    assert ok.observation.loinc == "13457-7" and _failed(ok) == []
    bad = _row(conn, family, "LDL", "97", "mg/dL", None, loinc="99999-9", lang="en")
    assert "loinc_unknown" in _failed(bad)
    shifted = _row(conn, family, "LDL", "2.5", "mmol/mol", None, loinc="13457-7", lang="en")
    assert "unit_dimension_mismatch" in _failed(shifted)


def test_duplicate_and_conflict(conn, family):
    first = _row(conn, family, "Colesterol LDL", "97", "mg/dL", "< 130")
    commit_observation(conn, first.observation, src=SrcIn(kind="manual", actor_kind="user"), accepted_by="person")
    same = _row(conn, family, "Colesterol LDL", "2,51", "mmol/L", "< 3,36")  # 2.51 mmol/L is 97.06 mg/dL: not equal
    assert "conflict" in _failed(same)
    dup = _row(conn, family, "LDL-C", "97", "mg/dL", "<130", lang="en")
    assert "duplicate" in _failed(dup)


def test_clean_row_auto_accepts_through_route_fact(conn, family):
    n = _row(conn, family, "Colesterol LDL", "97", "mg/dL", "< 130")
    res = route_fact(
        conn,
        observation=n.observation,
        src=SrcIn(kind="manual", actor_kind="user"),
        checks=n.checks,
        row_conf=0.99,
        allowance="structured_valid",
        identity="matched",
    )
    assert res.state == "auto_accepted"
    shifted = _row(conn, family, "Hemoglobina glicada (HbA1c)", "5,6", "mg/dL", "4,0 - 5,6")
    res = route_fact(
        conn,
        observation=shifted.observation,
        src=SrcIn(kind="manual", actor_kind="user"),
        checks=shifted.checks,
        row_conf=0.99,
        allowance="structured_valid",
        identity="matched",
    )
    assert res.state == "in_review" and "unit_dimension_mismatch" in res.reasons


def test_a_partial_name_and_a_text_on_a_quantity_go_to_review(conn, family):
    part = _row(conn, family, "Cortisol (tarde)", "12", "ug/dL", "5 - 25")
    assert "unmapped" in _failed(part)
    neg = _row(conn, family, "Glucosa", "Negativo", None, None)  # serum glucose has no ordinal code
    assert "implausible" in _failed(neg)
    strip = _row(conn, family, "Proteínas en orina", "Negativo", None, "Negativo")
    assert _failed(strip) == [] and strip.observation.loinc == "20454-5"
