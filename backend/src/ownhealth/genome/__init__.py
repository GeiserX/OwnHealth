"""Lane D: genome facts (docs/design/genomics.md). OwnHealth ingests OwnGenome's
`ownhealth.genome-facts/v1`; it never calls variants itself. Job kind `import.genome_facts`.

This module is the interface other lanes call (CONTRACTS.md 4.2); the constants are final. The work
happens in `schema` (validation), `ingest` (import), `findings` (gating, PGx per drug) and `lookup`
(tabix lookups with call_state).
"""

from __future__ import annotations

from datetime import date, datetime
from uuid import UUID

from sqlalchemy import Connection

from ownhealth.blobs import BlobStore
from ownhealth.genome import findings as _findings
from ownhealth.genome import ingest as _ingest
from ownhealth.genome import lookup as _lookup
from ownhealth.genome import schema as _schema
from ownhealth.genome import sensitive as _sensitive
from ownhealth.genome.findings import gate_for
from ownhealth.genome.lookup import load_ref_sites
from ownhealth.genome.model import (
    CALL_STATES,
    CATEGORIES,
    EVIDENCE,
    FACTS_SCHEMA,
    CallState,
    Findings,
    Gate,
    GenomeImportError,
    GenomeQueryError,
)
from ownhealth.ingest import ImportSummary

__all__ = [
    "CALL_STATES",
    "CATEGORIES",
    "EVIDENCE",
    "FACTS_SCHEMA",
    "CallState",
    "Findings",
    "Gate",
    "GenomeImportError",
    "GenomeQueryError",
    "gate_for",
    "import_genome_facts",
    "is_sensitive_gene",
    "list_findings",
    "load_ref_sites",
    "lookup_variant",
    "pgx_for_drug",
    "validate_facts",
]


def validate_facts(doc: dict) -> list[str]:
    """JSON Schema errors (plus the importer's own rules) for a genome-facts document; empty when valid.
    An unknown `schema` version gives one clear error and nothing else."""
    return _schema.validate_facts(doc)


def import_genome_facts(
    conn: Connection,
    blobs: BlobStore,
    *,
    document_id: UUID,
    attachments: dict[str, str] | None = None,
    actor_user: UUID | None = None,
    imported_at: datetime | None = None,
) -> ImportSummary:
    """Import a stored genome-facts.json; `attachments` maps uploaded raw file names to blob sha256.
    Raises GenomeImportError (nothing written) for an invalid file or a raw file whose sha256 differs."""
    return _ingest.import_genome_facts(
        conn, blobs, document_id=document_id, attachments=attachments, actor_user=actor_user, imported_at=imported_at
    )


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
    """Gated findings with evidence, confidence, call_state and src; coverage first for a gene query."""
    return _findings.list_findings(
        conn, profile_id, gate=gate, category=category, gene=gene, min_evidence=min_evidence, as_of=as_of
    )


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
    return _lookup.lookup_variant(
        conn, blobs, profile_id, gate=gate, rsid=rsid, hgvs=hgvs, region=region, cursor=cursor
    )


def pgx_for_drug(
    conn: Connection,
    profile_id: UUID,
    *,
    cpic_drug_id: str,
    as_of: date | None = None,
    genes: list[str] | None = None,
    drug_name: str | None = None,
) -> list[dict]:
    """PGx findings relevant to one drug, each with gene, diplotype, phenotype, activity_score,
    call_state and src; genes CPIC ties to the drug with no finding come back as not_assessed or
    no_finding. Used by `state.check_drug` (lane E)."""
    return _findings.pgx_for_drug(
        conn, profile_id, cpic_drug_id=cpic_drug_id, as_of=as_of, genes=genes, drug_name=drug_name
    )


def is_sensitive_gene(gene: str) -> bool:
    """True for APOE, the ACMG SF cancer and cardiovascular genes, HTT and other predictive repeat loci."""
    return _sensitive.is_sensitive_gene(gene)
