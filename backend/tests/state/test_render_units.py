"""get_health_status honours the token's unit preference: si and conventional, both ways."""

from decimal import Decimal

from ownhealth.state.rendering import _convert


def test_si_and_conventional_targets():
    si = _convert("glucose", "92", "mg/dL", "si")
    assert si["unit"] == "mmol/L" and abs(Decimal(si["value"]) - Decimal("5.106")) < Decimal("0.01")
    conv = _convert("glucose", "5.3", "mmol/L", "conventional")
    assert conv["unit"] == "mg/dL" and abs(Decimal(conv["value"]) - Decimal("95.48")) < Decimal("0.01")
    assert _convert("glucose", "5.3", "mmol/L", "si") is None  # already in the preferred unit
    assert _convert("glucose", "5.3", "mmol/L", "as_reported") is None
