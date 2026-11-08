import pytest

from ownhealth.core.acceptance import Check, auto_accept

OK = [Check(name="loinc", ok=True), Check(name="units", ok=True)]


@pytest.mark.parametrize(
    ("conf", "checks", "source", "identity", "auto"),
    [
        (0.95, OK, "template_text", "matched", True),
        (0.95, OK, "rules_text", "matched", True),
        (0.99, OK, "structured_valid", "matched", True),
        (0.949, OK, "rules_text", "matched", False),  # below threshold
        (0.99, [Check(name="units", ok=False, reason="unit_dimension_mismatch")], "template_text", "matched", False),
        (0.99, OK, "template_text", "hinted", False),  # identity must be matched
        (0.97, OK, "ocr_template", "matched", False),  # OCR needs 0.98
        (0.98, OK, "ocr_template", "matched", True),
        (0.99, OK, "ocr_rules", "matched", False),
        (1.0, OK, "agent", "matched", False),  # agent proposals never auto-accept
        (1.0, OK, "vlm", "matched", False),
        (1.0, OK, "structured_partial", "matched", False),
    ],
)
def test_auto_accept_matrix(conf, checks, source, identity, auto):
    assert auto_accept(conf, checks, source, identity).auto is auto


def test_threshold_never_below_090_and_reasons_collected():
    assert auto_accept(0.85, OK, "rules_text", "matched", threshold=0.5).auto is False
    assert auto_accept(0.91, OK, "rules_text", "matched", threshold=0.9).auto is True
    d = auto_accept(0.99, [Check(name="g", ok=False, reason="not_grounded")], "rules_text", "mismatch")
    assert d.reasons == ["identity_mismatch", "not_grounded"]


@pytest.mark.parametrize("conf", [float("nan"), 1.5, float("inf")])
def test_a_confidence_outside_0_1_fails_closed(conf):
    d = auto_accept(conf, OK, "rules_text", "matched")
    assert d.auto is False and d.why_not
