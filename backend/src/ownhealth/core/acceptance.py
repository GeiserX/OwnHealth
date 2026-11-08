"""The auto-accept rule (docs/design/ingestion.md section 9.1), as one function.

A row is committed without a click only when all of these hold:
  * every check passed;
  * row_conf >= threshold (instance setting, default 0.95, never below 0.90);
  * the source allows it: a vendor template or generic rules on a native text layer, OCR with a
    matching vendor template at row_conf >= 0.98, or a structured import with a valid code and unit;
  * the identity on the page matched the profile (not just hinted).
Agent proposals, VLM rows and manual rows never auto-accept.
"""

from __future__ import annotations

from dataclasses import dataclass, field
from typing import Literal

from pydantic import BaseModel

# Reason codes, one list for every importer (architecture.md 6.2, ingestion.md 9).
REASON_CODES: frozenset[str] = frozenset(
    {
        "loinc_unknown",
        "loinc_deprecated",
        "unmapped",
        "unit_dimension_mismatch",
        "unit_unconvertible",
        "implausible",
        "suspect_separator",
        "range_parse",
        "value_is_range",
        "not_grounded",
        "name_not_on_line",
        "identity_mismatch",
        "identity_missing",
        "date_invalid",
        "duplicate",
        "conflict",
        "amended_value",
        "possible_injection",
        "flag_mismatch",
        "ambiguous_separator",
    }
)

SourceAllowance = Literal[
    "template_text",  # vendor template on a native text layer
    "rules_text",  # generic table rules on a native text layer
    "ocr_template",  # OCR words read through a matching vendor template
    "ocr_rules",  # OCR words, generic rules: never auto
    "structured_valid",  # FHIR / Apple Health with a valid code and unit
    "structured_partial",  # structured import missing a code or unit: never auto
    "vlm",  # never auto
    "agent",  # never auto
    "manual",  # a person typed it: accepted_by person, not this rule
]
Identity = Literal["matched", "hinted", "mismatch", "missing"]

MIN_THRESHOLD = 0.90
DEFAULT_THRESHOLD = 0.95
OCR_TEMPLATE_THRESHOLD = 0.98


class Check(BaseModel):
    """One validator result. `reason` is a code from REASON_CODES when `ok` is False."""

    name: str
    ok: bool
    reason: str | None = None
    detail: str | None = None


@dataclass(frozen=True)
class Decision:
    auto: bool
    reasons: list[str] = field(default_factory=list)  # reason codes from failed checks
    why_not: list[str] = field(default_factory=list)  # human-readable, for the review payload


def auto_accept(
    row_conf: float,
    checks: list[Check],
    source: SourceAllowance,
    identity: Identity,
    threshold: float = DEFAULT_THRESHOLD,
) -> Decision:
    threshold = max(MIN_THRESHOLD, threshold)
    reasons = sorted({c.reason for c in checks if not c.ok and c.reason})
    why_not: list[str] = []
    if any(not c.ok for c in checks):
        why_not.append("a check failed")
    if source in ("template_text", "rules_text", "structured_valid"):
        needed = threshold
    elif source == "ocr_template":
        needed = max(threshold, OCR_TEMPLATE_THRESHOLD)
    else:
        needed = None
        why_not.append(f"source {source} never auto-accepts")
    if needed is not None and row_conf < needed:
        why_not.append(f"row confidence {row_conf:.2f} below {needed:.2f}")
    if identity != "matched":
        why_not.append(f"identity {identity}")
        code = {"mismatch": "identity_mismatch", "missing": "identity_missing"}.get(identity)
        if code and code not in reasons:
            reasons = sorted({*reasons, code})
    return Decision(auto=not why_not, reasons=reasons, why_not=why_not)
