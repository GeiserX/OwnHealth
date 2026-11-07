"""Lane A: normalization (docs/design/ingestion.md section 8, architecture.md section 10).

Printed value, unit and range stay the truth layer; everything here is derived and recomputable.
Importers (lanes B and C) and `propose` (lane F) call these functions; nobody else parses units.
Every function below is a contract stub until lane A lands it.
"""

from __future__ import annotations

from dataclasses import dataclass, field
from datetime import date
from decimal import Decimal
from typing import Literal

from sqlalchemy import Connection

from ownhealth.core.acceptance import Check
from ownhealth.core.observations import ObservationIn

DERIVATION_VERSION = "norm-0.0.0"  # lane A bumps this with every alias, unit or molar-mass table change

Locale = Literal["es", "en", "de", "fr"]


@dataclass(frozen=True)
class ParsedValue:
    printed: str
    value: Decimal | None  # None for text results
    comparator: str | None = None  # <, >, <=, >=
    text_code: str | None = None  # negative | positive | not_detected | trace
    reason: str | None = None  # a REASON_CODES entry when the parse is not trusted (suspect_separator...)


@dataclass(frozen=True)
class Resolution:
    alias_group: str  # always set: an uncoded row stays usable under its group
    loinc: str | None
    property: str | None  # MCnc, SCnc, ... of the resolved code
    method: str | None = None  # calculated | direct | ...
    refused: bool = False  # a panel or category name, never coded as an analyte
    reasons: list[str] = field(default_factory=list)


@dataclass(frozen=True)
class RefRange:
    low: Decimal | None
    high: Decimal | None
    text: str
    applies_to: dict = field(default_factory=dict)  # {sex, age_min, age_max, pregnancy}


@dataclass(frozen=True)
class Normalized:
    observation: ObservationIn
    checks: list[Check]


def parse_value(printed: str, *, locale: Locale, unit: str | None = None) -> ParsedValue:
    raise NotImplementedError("lane A: normalize.parse_value")


def resolve_analyte(
    conn: Connection,
    printed_name: str,
    *,
    lang: Locale,
    unit: str | None,
    specimen: str | None = None,
    method: str | None = None,
    provider: str | None = None,
) -> Resolution:
    raise NotImplementedError("lane A: normalize.resolve_analyte")


def to_ucum(printed_unit: str) -> str | None:
    raise NotImplementedError("lane A: normalize.to_ucum")


def canonical_unit(alias_group: str) -> str | None:
    raise NotImplementedError("lane A: normalize.canonical_unit")


def convert(value: Decimal, from_ucum: str, to_ucum: str, *, alias_group: str) -> Decimal | None:
    """None when the conversion is not possible (the row then gets `unit_unconvertible`)."""
    raise NotImplementedError("lane A: normalize.convert")


def parse_range(printed: str, *, locale: Locale) -> list[RefRange]:
    raise NotImplementedError("lane A: normalize.parse_range")


def pick_range(ranges: list[RefRange], *, sex: str | None, age_years: int | None) -> RefRange | None:
    raise NotImplementedError("lane A: normalize.pick_range")


def interpret(value: Decimal | None, text_code: str | None, ref: RefRange | None) -> str | None:
    """HL7 ObservationInterpretation from the printed range only: L, N, H, A. None without a range."""
    raise NotImplementedError("lane A: normalize.interpret")


def normalize_row(
    conn: Connection,
    *,
    profile_id,
    printed_name: str,
    printed_value: str,
    printed_unit: str | None,
    printed_range: str | None,
    printed_flag: str | None,
    effective_at,
    lang: Locale,
    specimen: str | None = None,
    provider: str | None = None,
) -> Normalized:
    """One printed lab row to an ObservationIn plus every validator result (dimension, plausibility,
    flag mismatch, range parse). The caller adds grounding and identity checks and routes the row
    through `core.review.route_fact`."""
    raise NotImplementedError("lane A: normalize.normalize_row")


def rederive(conn: Connection, *, profile_id=None, since: date | None = None) -> int:
    """Recompute the derived layer (loinc, canonical, interpretation) after a table change."""
    raise NotImplementedError("lane A: normalize.rederive")
