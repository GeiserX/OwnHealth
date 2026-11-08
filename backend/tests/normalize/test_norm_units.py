"""Printed units to UCUM, dimensions, and conversion (ingestion.md 8.3), with property-based checks."""

from decimal import Decimal

import pytest
from hypothesis import given, settings
from hypothesis import strategies as st

from ownhealth.normalize import convert, to_ucum
from ownhealth.normalize._units import UnitError, dims_of, parse_ucum

PRINTED = [
    ("mg/dl", "mg/dL"),
    ("mg%", "mg/dL"),
    ("MG/DL", "mg/dL"),
    ("x10^3/µL", "10*3/uL"),
    ("x10E3/uL", "10*3/uL"),
    ("10^9/L", "10*9/L"),
    ("x 10⁹/l", "10*9/L"),
    ("miles/µL", "10*3/uL"),
    ("/mm3", "/mm3"),
    ("UI/L", "[IU]/L"),
    ("IE/l", "[IU]/L"),
    ("U/l", "U/L"),
    ("mUI/mL", "m[IU]/mL"),
    ("µUI/mL", "u[IU]/mL"),
    ("μIU/mL", "u[IU]/mL"),
    ("mU/L", "mU/L"),
    ("mL/min/1.73m2", "mL/min/{1.73_m2}"),
    ("mL/min/1,73 m²", "mL/min/{1.73_m2}"),
    ("mmol/l", "mmol/L"),
    ("µmol/L", "umol/L"),
    ("mcg/dL", "ug/dL"),
    ("fl", "fL"),
    ("pg", "pg"),
    ("%", "%"),
    ("mmol/mol", "mmol/mol"),
    ("mEq/L", "meq/L"),
    ("mOsm/kg", "mosm/kg"),
    ("seg", "s"),
    ("mm/1ªh", "mm/h"),
    ("mmHg", "mm[Hg]"),
    ("mg/g creat", "mg/g"),
    ("mg/mmol", "mg/mmol"),
    ("g/24h", "g/(24.h)"),
    ("cel/campo", "/[HPF]"),
    ("ng/ml", "ng/mL"),
    ("10*3/uL", "10*3/uL"),
    ("m[IU]/L", "m[IU]/L"),
]


@pytest.mark.parametrize(("printed", "ucum"), PRINTED)
def test_printed_units_map_to_ucum(printed, ucum):
    assert to_ucum(printed) == ucum
    parse_ucum(ucum)  # and the result is valid in our UCUM subset


@pytest.mark.parametrize("printed", ["", "-", "xyz", "mg/xyz", "ver nota"])
def test_unreadable_units_are_none(printed):
    assert to_ucum(printed) is None


def test_parse_rejects_unknown_atoms():
    with pytest.raises(UnitError):
        parse_ucum("mg/foo")


def test_iu_and_enzyme_units_never_convert():
    assert convert(Decimal(1), "[IU]/L", "U/L", alias_group="alt") is None
    assert convert(Decimal(1), "m[IU]/L", "mU/L", alias_group="tsh") is None
    assert dims_of("[IU]/L") != dims_of("U/L")


def test_known_factors():
    assert convert(Decimal(1), "U/L", "ukat/L", alias_group="alt") == Decimal("0.0166666666667")
    assert convert(Decimal("6.8"), "10*3/uL", "10*9/L", alias_group="wbc") == Decimal("6.8")
    assert convert(Decimal("6800"), "/uL", "10*3/uL", alias_group="wbc") == Decimal("6.8")
    assert convert(Decimal(1), "mL/s/{1.73_m2}", "mL/min/{1.73_m2}", alias_group="egfr") == Decimal(60)
    assert convert(Decimal(30), "ng/mL", "ug/L", alias_group="ferritin") == Decimal(30)
    assert convert(Decimal("2.5"), "m[IU]/L", "u[IU]/mL", alias_group="tsh") == Decimal("2.5")


def test_mass_and_molar_need_the_molar_mass():
    assert convert(Decimal(100), "mg/dL", "mmol/L", alias_group="glucose") == Decimal("5.55074490997")  # 1000/180.156
    # Lp(a): mass and molar measure different things, never converted
    assert convert(Decimal(100), "mg/dL", "nmol/L", alias_group="lpa") is None
    # a group with no molar entry does not borrow one
    assert convert(Decimal(100), "mg/dL", "mmol/L", alias_group="ferritin") is None


def test_eq_converts_only_with_valence():
    assert convert(Decimal(140), "meq/L", "mmol/L", alias_group="sodium") == Decimal(140)
    assert convert(Decimal(5), "meq/L", "mmol/L", alias_group="calcium") == Decimal("2.5")
    assert convert(Decimal(5), "meq/L", "mmol/L", alias_group="glucose") is None


def test_hba1c_master_equation_not_a_factor_of_ten():
    assert convert(Decimal("6.5"), "%", "mmol/mol", alias_group="hba1c") == Decimal("47.54115")  # 10.929 x 4.35
    assert convert(Decimal("48"), "mmol/mol", "%", alias_group="hba1c").quantize(Decimal("0.01")) == Decimal("6.54")
    # outside hba1c the same pair is a plain scale (and nothing in the tables uses it)
    assert convert(Decimal("6.5"), "%", "mmol/mol", alias_group="hematocrit") == Decimal(65)


def test_urine_acr_converts_through_the_creatinine_in_the_denominator():
    # 3 mg/mmol is about 26.5 mg/g (KDIGO: A2 starts at 3 mg/mmol = 30 mg/g, rounded)
    assert convert(Decimal(3), "mg/mmol", "mg/g", alias_group="urine_acr").quantize(Decimal("0.1")) == Decimal("26.5")


SAME_DIMENSION = [
    ("mg/dL", ["g/L", "mg/L", "ug/mL", "ng/mL", "ug/dL", "g/dL", "kg/m3"]),
    ("mmol/L", ["umol/L", "nmol/L", "pmol/L", "mol/m3"]),
    ("10*3/uL", ["10*9/L", "/uL", "/mm3", "10*6/mL"]),
    ("U/L", ["ukat/L", "nkat/L", "kU/L", "mU/mL"]),
    ("m[IU]/mL", ["[IU]/L", "u[IU]/mL", "m[IU]/L"]),
    ("s", ["min", "h", "ms"]),
]
_values = st.decimals(min_value=Decimal("0.001"), max_value=Decimal("100000"), places=3, allow_nan=False)


def _close(a: Decimal, b: Decimal) -> bool:
    return abs(a - b) <= abs(b) * Decimal("1e-9") + Decimal("1e-12")


@settings(max_examples=200, deadline=None)
@given(v=_values, pair=st.sampled_from([(a, b) for a, bs in SAME_DIMENSION for b in bs]))
def test_round_trip_within_a_dimension(v, pair):
    a, b = pair
    there = convert(v, a, b, alias_group="any")
    assert there is not None
    assert _close(convert(there, b, a, alias_group="any"), v)


@settings(max_examples=200, deadline=None)
@given(
    v=_values,
    k=st.integers(min_value=2, max_value=50),
    pair=st.sampled_from([(a, b) for a, bs in SAME_DIMENSION for b in bs]),
)
def test_conversion_is_linear(v, k, pair):
    a, b = pair
    assert _close(convert(v * k, a, b, alias_group="any"), convert(v, a, b, alias_group="any") * k)


@settings(max_examples=100, deadline=None)
@given(v=st.decimals(min_value=Decimal("2.2"), max_value=Decimal("20"), places=2))
def test_hba1c_round_trip(v):
    ifcc = convert(v, "%", "mmol/mol", alias_group="hba1c")
    assert _close(convert(ifcc, "mmol/mol", "%", alias_group="hba1c"), v)


@settings(max_examples=100, deadline=None)
@given(
    a=st.sampled_from(["mg/dL", "g/L", "ug/dL"]),
    b=st.sampled_from(["mmol/L", "umol/L"]),
    v=_values,
)
def test_molar_round_trip_cholesterol(a, b, v):
    there = convert(v, a, b, alias_group="ldl_c")
    assert there is not None
    assert _close(convert(there, b, a, alias_group="ldl_c"), v)
