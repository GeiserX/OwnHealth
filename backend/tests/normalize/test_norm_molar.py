"""One check per molar-mass row: the factor labs publish (ingestion.md 8.3 table) and the citation."""

from decimal import Decimal

import pytest

from ownhealth.normalize import _tables, convert

# alias_group: (from, to, factor printed in the conventional-to-SI tables we cite)
PUBLISHED = {
    "glucose": ("mg/dL", "mmol/L", "0.05551"),
    "total_cholesterol": ("mg/dL", "mmol/L", "0.02586"),
    "ldl_c": ("mg/dL", "mmol/L", "0.02586"),
    "hdl_c": ("mg/dL", "mmol/L", "0.02586"),
    "triglycerides": ("mg/dL", "mmol/L", "0.01129"),
    "creatinine": ("mg/dL", "umol/L", "88.40"),
    "urea": ("mg/dL", "mmol/L", "0.1665"),
    "bun": ("mg/dL", "mmol/L", "0.357"),
    "uric_acid": ("mg/dL", "umol/L", "59.48"),
    "bilirubin_total": ("mg/dL", "umol/L", "17.10"),
    "calcium": ("mg/dL", "mmol/L", "0.2495"),
    "phosphate": ("mg/dL", "mmol/L", "0.3229"),
    "magnesium": ("mg/dL", "mmol/L", "0.4114"),
    "iron": ("ug/dL", "umol/L", "0.1791"),
    "vitamin_d_25oh": ("ng/mL", "nmol/L", "2.496"),
    "vitamin_b12": ("pg/mL", "pmol/L", "0.7378"),
    "folate": ("ng/mL", "nmol/L", "2.266"),
    "testosterone": ("ng/dL", "nmol/L", "0.03467"),
    "cortisol": ("ug/dL", "nmol/L", "27.59"),
    "ft4": ("ng/dL", "pmol/L", "12.87"),
    "ft3": ("pg/mL", "pmol/L", "1.536"),
    "estradiol": ("pg/mL", "pmol/L", "3.671"),
    "progesterone": ("ng/mL", "nmol/L", "3.180"),
    "dhea_s": ("ug/dL", "umol/L", "0.02714"),
}


@pytest.mark.parametrize("group", sorted(PUBLISHED))
def test_published_factor(group):
    src, dst, factor = PUBLISHED[group]
    got = convert(Decimal(1), src, dst, alias_group=group)
    assert got is not None
    assert abs(got - Decimal(factor)) / Decimal(factor) < Decimal("0.001"), (group, got, factor)


@pytest.mark.parametrize("group", sorted(_tables.molar()))
def test_every_row_is_cited_and_converts(group):
    m = _tables.molar()[group]
    assert m.citation.startswith("https://"), group
    assert m.g_per_mol is not None or m.valence is not None
    if m.g_per_mol is not None:
        got = convert(Decimal(1), "g/L", "mol/L", alias_group=group)
        assert got is not None and abs(got * Decimal(m.g_per_mol.numerator) / m.g_per_mol.denominator - 1) < Decimal(
            "1e-9"
        )


def test_bun_is_not_urea():
    """Urea and urea nitrogen differ by 60.06 / 28.01 = 2.14: the trap the table names."""
    urea = convert(Decimal(1), "mg/dL", "mmol/L", alias_group="urea")
    bun = convert(Decimal(1), "mg/dL", "mmol/L", alias_group="bun")
    assert (bun / urea).quantize(Decimal("0.01")) == Decimal("2.14")


def test_phosphate_is_phosphorus_not_po4():
    assert convert(Decimal(1), "mg/dL", "mmol/L", alias_group="phosphate").quantize(Decimal("0.0001")) == Decimal(
        "0.3229"
    )
