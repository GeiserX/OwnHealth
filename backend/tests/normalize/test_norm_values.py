"""Printed values, ranges and flags (ingestion.md 8.1, 8.4, 8.5)."""

from decimal import Decimal

import pytest
from hypothesis import given, settings
from hypothesis import strategies as st

from ownhealth.normalize import interpret, parse_range, parse_value, pick_range

D = Decimal


@pytest.mark.parametrize(
    ("printed", "locale", "unit", "value", "cmp", "code", "reason"),
    [
        ("13,5", "es", None, D("13.5"), None, None, None),
        ("13.5", "es", None, D("13.5"), None, None, None),
        ("13.5", "en", None, D("13.5"), None, None, None),
        ("1.234,5", "de", None, D("1234.5"), None, None, None),
        ("1,234.5", "en", None, D("1234.5"), None, None, None),
        ("1,200", "en", "mg/dL", D("1200"), None, None, None),
        ("6.800", "es", "/mm3", D("6800"), None, None, None),
        ("6.800", "es", "x10^3/µL", D("6800"), None, None, None),
        ("1.200", "es", "mg/dL", D("1.200"), None, None, "ambiguous_separator"),
        ("250.000", "fr", "/µL", D("250000"), None, None, None),
        ("1.234.567", "es", None, D("1234567"), None, None, None),
        ("1,2,3", "es", None, None, None, None, "suspect_separator"),
        ("< 0,5", "es", None, D("0.5"), "<", None, None),
        ("≤5", "en", None, D("5"), "<=", None, None),
        ("inferior a 5", "es", None, D("5"), "<", None, None),
        ("kleiner als 0,1", "de", None, D("0.1"), "<", None, None),
        ("> 90", "es", None, D("90"), ">", None, None),
        ("163 H", "es", None, D("163"), None, None, None),
        ("24 *", "es", None, D("24"), None, None, None),
        ("Negativo", "es", None, None, None, "negative", None),
        ("nicht nachweisbar", "de", None, None, None, "not_detected", None),
        ("Négatif", "fr", None, None, None, "negative", None),
        ("Trazas", "es", None, None, None, "trace", None),
        ("+++", "es", None, None, None, "positive", None),
        ("ver nota", "es", None, None, None, None, None),
    ],
)
def test_parse_value(printed, locale, unit, value, cmp, code, reason):
    pv = parse_value(printed, locale=locale, unit=unit)
    assert (pv.value, pv.comparator, pv.text_code, pv.reason) == (value, cmp, code, reason)
    assert pv.printed == printed  # the printed string is kept as found


def _fmt(v: Decimal, locale: str) -> str:
    s = f"{v:,.2f}"  # 1,234.56
    if locale == "en":
        return s
    return s.replace(",", "_").replace(".", ",").replace("_", ".")


@settings(max_examples=300, deadline=None)
@given(
    v=st.decimals(min_value=D("0"), max_value=D("9999999"), places=2), locale=st.sampled_from(["es", "en", "de", "fr"])
)
def test_locale_round_trip(v, locale):
    """Any number a lab prints with two decimals reads back exactly in its own locale."""
    pv = parse_value(_fmt(v, locale), locale=locale)
    assert pv.value == v and pv.reason is None


def test_ranges():
    [r] = parse_range("13.5 - 17.5", locale="es")
    assert (r.low, r.high, r.text) == (D("13.5"), D("17.5"), "13.5 - 17.5")
    [r] = parse_range("0,70 - 1,20", locale="es")
    assert (r.low, r.high) == (D("0.70"), D("1.20"))
    [r] = parse_range("< 130", locale="es")
    assert (r.low, r.high, r.high_inclusive) == (None, D("130"), False)
    [r] = parse_range("Hasta 40", locale="es")
    assert (r.low, r.high, r.high_inclusive) == (None, D("40"), True)
    [r] = parse_range("> 90", locale="es")
    assert (r.low, r.high, r.low_inclusive) == (D("90"), None, False)
    [r] = parse_range("Negativo", locale="es")
    assert r.text_code == "negative"
    [r] = parse_range("150.000 - 450.000", locale="es", unit="/mm3")
    assert (r.low, r.high) == (D("150000"), D("450000"))
    assert parse_range("ver nota", locale="es") == []


def test_conditioned_ranges_and_pick():
    ranges = parse_range("Hombres: 13.5-17.5 Mujeres: 12,0-16,0", locale="es")
    assert [(r.applies_to, r.low, r.high) for r in ranges] == [
        ({"sex": "male"}, D("13.5"), D("17.5")),
        ({"sex": "female"}, D("12.0"), D("16.0")),
    ]
    assert pick_range(ranges, sex="female", age_years=40).low == D("12.0")
    assert pick_range(ranges, sex="male", age_years=40).low == D("13.5")
    assert pick_range(ranges, sex=None, age_years=40) is None  # never guess the sex

    de = parse_range("Männer: 0,7-1,2; Frauen: 0,5-0,9", locale="de")
    assert pick_range(de, sex="female", age_years=30).high == D("0.9")

    ages = parse_range("18-49 años: 1.0-2.0; 50-99 años: 2.0-3.0", locale="es")
    assert pick_range(ages, sex="male", age_years=47).high == D("2.0")
    assert pick_range(ages, sex="male", age_years=71).high == D("3.0")
    assert pick_range(ages, sex="male", age_years=None) is None

    preg = parse_range("Mujeres: 12-16; 3er trimestre: 11-14", locale="es")
    assert pick_range(preg, sex="female", age_years=30).high == D("16")  # a trimester range is never picked


def test_two_equal_ranges_that_disagree_pick_none():
    ranges = parse_range("13-17; 12-16", locale="es")
    assert pick_range(ranges, sex="male", age_years=40) is None


def test_interpret_from_the_printed_range_only():
    [r] = parse_range("13.5 - 17.5", locale="es")
    assert interpret(D("12"), None, r) == "L"
    assert interpret(D("13.5"), None, r) == "N"
    assert interpret(D("18"), None, r) == "H"
    assert interpret(D("18"), None, None) is None  # no printed range, no flag
    [lt] = parse_range("< 130", locale="es")
    assert interpret(D("129"), None, lt) == "N"
    assert interpret(D("130"), None, lt) == "H"  # "< 130" excludes 130
    [gt] = parse_range("> 90", locale="es")
    assert interpret(D("90"), None, gt) == "L"
    assert interpret(D("< 0.5"[2:]), None, lt, comparator="<") == "N"
    [rr] = parse_range("1 - 3", locale="es")
    assert interpret(D("0.5"), None, rr, comparator="<") == "L"
    assert interpret(D("2"), None, rr, comparator="<") is None  # "<2" could be below 1
    [neg] = parse_range("Negativo", locale="es")
    assert interpret(None, "negative", neg) == "N"
    assert interpret(None, "not_detected", neg) == "N"
    assert interpret(None, "positive", neg) == "A"


def test_line_breaks_separate_range_entries():
    by_age = parse_range("0-17 años: 5-10\n18-99 años: 3-8", locale="es")
    assert [(r.low, r.high) for r in by_age] == [(D("5"), D("10")), (D("3"), D("8"))]
    assert pick_range(by_age, sex="male", age_years=40).high == D("8")  # an adult gets the adult range
    two = parse_range("10-20\n30-40", locale="es")
    assert len(two) == 2 and pick_range(two, sex="male", age_years=40) is None  # disagreement seen
    wrapped = parse_range("Hombres:\n13-17\nMujeres:\n12-16", locale="es")  # a label continues below
    assert [(r.applies_to, r.high) for r in wrapped] == [({"sex": "male"}, D("17")), ({"sex": "female"}, D("16"))]
    [age] = parse_range("0-17 años:\n5-10", locale="es")
    assert age.applies_to == {"age_min": 0, "age_max": 17} and age.high == D("10")


def test_a_censored_value_at_an_exclusive_bound_is_inside():
    [lt] = parse_range("< 5", locale="es")
    assert interpret(D("5"), None, lt, comparator="<") == "N"  # "<5" against "<5": below 5
    assert interpret(D("5"), None, lt, comparator="<=") is None  # "<=5" may be 5, which "<5" excludes
    [gt] = parse_range("> 60", locale="es")
    assert interpret(D("60"), None, gt, comparator=">") == "N"
    assert interpret(D("60"), None, gt, comparator=">=") is None
    [le] = parse_range("<= 5", locale="es")
    assert interpret(D("5"), None, le, comparator="<=") == "N"
