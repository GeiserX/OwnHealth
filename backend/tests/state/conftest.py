"""Lane E fixtures: a database of its own seeded with the base demo family (so the documented src ids
like src_a12 are ours), and a stand-in for lane D's genome functions (the contract in CONTRACTS.md 4.2)
with the synthetic findings of architecture.md section 16."""

from __future__ import annotations

from dataclasses import dataclass
from datetime import date

import pytest
from sqlalchemy import Engine

from ownhealth import genome
from ownhealth.blobs import BlobStore
from ownhealth.config import Settings
from ownhealth.db import make_engine
from ownhealth.db.migrate import migrate
from ownhealth.demo import DemoContext
from ownhealth.demo.base import seed_base
from ownhealth.demo.family import Family, create_rivera_family

AS_OF = date(2026, 11, 7)

# Synthetic genome findings per profile, in the shape lane D returns (genomics.md 2.3, mcp.md 5.4).
FINDINGS = {
    "alex": [
        {"id": "f_1a2b3c4d5e6f7081", "category": "pgx", "gene": "CYP2C19", "diplotype": "*1/*2",
         "phenotype": "Intermediate Metabolizer", "evidence": "E1", "confidence": "high", "call_state": "alt",
         "src": "src_g07"},
        {"id": "f_2b3c4d5e6f708192", "category": "pgx", "gene": "SLCO1B1", "diplotype": "*1/*5",
         "phenotype": "Decreased Function", "evidence": "E1", "confidence": "high", "call_state": "alt",
         "src": "src_g31"},
        {"id": "f_5e6f708192031425", "category": "risk_marker", "gene": "LPA", "rsid": "rs10455872",
         "evidence": "E2", "confidence": "medium", "call_state": "alt", "src": "src_g44"},
    ],
    "sam": [
        {"id": "f_9d1e000000000001", "category": "pgx", "gene": "CYP2D6", "diplotype": "*1/*1", "activity_score": "2.0",
         "phenotype": "Normal Metabolizer", "evidence": "E1", "confidence": "medium", "call_state": "ref",
         "src": "src_g27"},
    ],
}  # fmt: skip


@dataclass
class Seeded:
    engine: Engine
    family: Family
    findings: dict  # profile_id -> list of findings (tests may edit it)

    def pid(self, slug: str):
        return self.family.profiles[slug]


@pytest.fixture(scope="session")
def seeded(make_database, tmp_path_factory) -> Seeded:
    eng = make_engine(make_database())
    migrate(eng)
    tmp = tmp_path_factory.mktemp("lane_e")
    settings = Settings(database_url=str(eng.url), data_dir=tmp)
    with eng.begin() as conn:
        family = create_rivera_family(conn, "Rivera (lane E)")
        seed_base(conn, DemoContext(settings=settings, blobs=BlobStore(tmp / "blobs"), family=family))
    yield Seeded(eng, family, {family.profiles[s]: list(v) for s, v in FINDINGS.items()})
    eng.dispose()


@pytest.fixture
def fake_genome(seeded, monkeypatch):
    """Lane D's list_findings and pgx_for_drug, answering from `seeded.findings`."""

    def list_findings(conn, profile_id, *, gate, category=None, gene=None, min_evidence="E2", as_of=None):
        rows = [f for f in seeded.findings.get(profile_id, []) if not category or f["category"] == category]
        return genome.Findings(findings=[dict(f) for f in rows if not gene or f["gene"] == gene])

    def pgx_for_drug(conn, profile_id, *, cpic_drug_id, as_of=None):
        return [dict(f) for f in seeded.findings.get(profile_id, []) if f["category"] == "pgx"]

    monkeypatch.setattr(genome, "list_findings", list_findings)
    monkeypatch.setattr(genome, "pgx_for_drug", pgx_for_drug)
    return seeded


@pytest.fixture
def sconn(seeded, fake_genome):
    """A connection on the seeded database inside a transaction rolled back after the test."""
    with seeded.engine.connect() as c:
        tx = c.begin()
        try:
            yield c
        finally:
            tx.rollback()
