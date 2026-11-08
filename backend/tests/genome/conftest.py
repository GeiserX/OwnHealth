"""Genome fixtures: Alex's synthetic WGS facts with a calls VCF and gVCF built with pysam, imported
into the rolled-back `conn` of each test."""

from __future__ import annotations

import pytest

from ownhealth.genome import synthetic


@pytest.fixture
def alex_raw(tmp_path):
    return synthetic.alex_raw_files(tmp_path / "raw")


@pytest.fixture
def alex_genome(conn, family, blobs, alex_raw):
    """Alex's facts and raw files imported. Returns (dataset_id, facts, summary)."""
    facts, attachments = synthetic.with_files(synthetic.alex_facts(), blobs, alex_raw)
    doc_id, summary = synthetic.seed_genome(
        conn,
        blobs,
        household_id=family.household_id,
        profile_id=family.profiles["alex"],
        facts=facts,
        attachments=attachments,
    )
    return doc_id, facts, summary
