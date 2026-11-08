"""Constants and shapes of the genome lane (contract: CONTRACTS.md 4.2). The constants are final."""

from __future__ import annotations

from dataclasses import dataclass, field
from typing import Literal

FACTS_SCHEMA = "ownhealth.genome-facts/v1"
# Exactly five values, everywhere (mcp.md 5.5). A contract test rejects any other.
CALL_STATES: tuple[str, ...] = ("ref", "alt", "no_call", "not_on_array", "not_covered")
CallState = Literal["ref", "alt", "no_call", "not_on_array", "not_covered"]
CATEGORIES: tuple[str, ...] = (
    "pgx",
    "monogenic_pathogenic",
    "acmg_sf",
    "carrier",
    "hla",
    "trait",
    "risk_marker",
    "prs",
    "ancestry",
)
EVIDENCE: tuple[str, ...] = ("E1", "E2", "E3", "E4")
CONFIDENCE: tuple[str, ...] = ("high", "medium", "low", "unconfirmed")
NOTICE = "Information for personal records, not a diagnosis. Discuss findings with a clinician."


@dataclass(frozen=True)
class Gate:
    """Who may see what: computed by the caller from the permission and the profile (`gate_for`)."""

    include_sensitive: bool  # agents switch (or UI switch) on AND read:sensitive
    minor: bool  # the minor rule withholds adult-onset, sensitive, carrier and PRS findings


@dataclass
class Findings:
    findings: list[dict] = field(default_factory=list)
    not_assessed: list[dict] = field(default_factory=list)
    withheld_sensitive: int = 0
    withheld_minor: int = 0
    db_versions: dict[str, str] = field(default_factory=dict)
    assay: dict | None = None
    coverage: list[dict] = field(default_factory=list)  # gene queries: the coverage entry comes first
    limitations: list[str] = field(default_factory=list)


class GenomeImportError(ValueError):
    """The facts file or its raw files cannot be imported. Nothing is written."""


class GenomeQueryError(ValueError):
    """A lookup that cannot be answered: `code` is stable (rsid_unknown, hgvs_unresolved, region_too_large,
    bad_input, no_genome)."""

    def __init__(self, code: str, message: str, fix: str | None = None):
        super().__init__(message)
        self.code, self.message, self.fix = code, message, fix

    def as_dict(self) -> dict:
        return {"code": self.code, "message": self.message, "fix": self.fix}


def untrusted_fact_text(text: str | None) -> dict | None:
    """Free text from a facts file, which is an imported file like any PDF (genomics.md 2.1).

    Same shape and origin as `mcp.envelope.untrusted(text, "genome_facts")`; built here so the genome
    package does not import the MCP server package (which imports genome).
    """
    if text is None:
        return None
    return {"trust": "untrusted_text", "origin": "genome_facts", "text": text}
