"""Lane D: genome facts (docs/design/genomics.md). OwnHealth ingests OwnGenome's
`ownhealth.genome-facts/v1`; it never calls variants itself. Job kind `import.genome_facts`.

The constants are the contract and are final; the functions are stubs until lane D lands them.
"""

from __future__ import annotations

from dataclasses import dataclass, field
from datetime import date
from typing import Literal
from uuid import UUID

from sqlalchemy import Connection

from ownhealth.blobs import BlobStore
from ownhealth.ingest import ImportSummary

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


@dataclass(frozen=True)
class Gate:
    """Who may see what: computed by the caller from the permission and the profile."""

    include_sensitive: bool  # agents switch (or UI switch) on AND read:sensitive
    minor: bool  # the minor rule withholds adult-onset, sensitive, carrier and adult PRS


@dataclass
class Findings:
    findings: list[dict] = field(default_factory=list)
    not_assessed: list[dict] = field(default_factory=list)
    withheld_sensitive: int = 0
    withheld_minor: int = 0
    db_versions: dict[str, str] = field(default_factory=dict)
    assay: dict | None = None


def validate_facts(doc: dict) -> list[str]:
    """JSON Schema errors for a genome-facts document; empty when valid."""
    raise NotImplementedError("lane D: genome.validate_facts")


def import_genome_facts(conn: Connection, blobs: BlobStore, *, document_id: UUID) -> ImportSummary:
    raise NotImplementedError("lane D: genome.import_genome_facts")


def list_findings(
    conn: Connection,
    profile_id: UUID,
    *,
    gate: Gate,
    category: str | None = None,
    gene: str | None = None,
    min_evidence: str = "E2",
    as_of: date | None = None,
) -> Findings:
    raise NotImplementedError("lane D: genome.list_findings")


def lookup_variant(
    conn: Connection,
    blobs: BlobStore,
    profile_id: UUID,
    *,
    gate: Gate,
    rsid: str | None = None,
    hgvs: str | None = None,
    region: str | None = None,
    cursor: str | None = None,
) -> dict:
    """Calls per source with `call_state` always set; sensitive genes return `withheld_sensitive`."""
    raise NotImplementedError("lane D: genome.lookup_variant")


def pgx_for_drug(conn: Connection, profile_id: UUID, *, cpic_drug_id: str, as_of: date | None = None) -> list[dict]:
    """PGx findings relevant to one drug, each with gene, diplotype, phenotype, activity_score,
    call_state and src. Used by `state.check_drug` (lane E)."""
    raise NotImplementedError("lane D: genome.pgx_for_drug")


def is_sensitive_gene(gene: str) -> bool:
    raise NotImplementedError("lane D: genome.is_sensitive_gene")
