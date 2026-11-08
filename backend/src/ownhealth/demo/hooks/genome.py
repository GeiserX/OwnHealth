"""Demo genome data (lane D): Alex's WGS facts with a calls VCF and gVCF, Sam's facts for the codeine
example, and Lucía's facts to show the minor rule. All synthetic (ownhealth.genome.synthetic)."""

from __future__ import annotations

import tempfile
from datetime import UTC, datetime
from pathlib import Path

from sqlalchemy import Connection, text

from ownhealth.demo import DemoContext
from ownhealth.genome import synthetic

ORDER = 400
IMPORTED_AT = datetime(2026, 10, 2, 8, 0, tzinfo=UTC)  # "ingested 2026-10-02" in the mockup


def seed(conn: Connection, ctx: DemoContext) -> None:
    fam = ctx.family
    with tempfile.TemporaryDirectory(prefix="ownhealth-demo-genome-") as tmp:
        facts, attachments = synthetic.with_files(
            synthetic.alex_facts(), ctx.blobs, synthetic.alex_raw_files(Path(tmp))
        )
    people = [
        ("alex", facts, attachments),
        ("sam", synthetic.sam_facts(), None),
        ("lucia", synthetic.lucia_facts(), None),
    ]
    for slug, doc, att in people:
        synthetic.seed_genome(
            conn,
            ctx.blobs,
            household_id=fam.household_id,
            profile_id=fam.profiles[slug],
            facts=doc,
            attachments=att,
            filename=f"{doc['subject']['sample_id']}_genome_facts.json",
            imported_at=IMPORTED_AT,
        )
    rows = conn.execute(
        text(
            "select p.slug, f.gene, f.src_id from genome_finding f join profile p on p.id = f.profile_id"
            " where f.profile_id = any(:ids) and f.gene is not null"
        ),
        {"ids": [fam.profiles[s] for s, _, _ in people]},
    )
    for r in rows:
        ctx.src_ids[f"{r.slug}_genome_{r.gene.lower().replace('-', '_')}"] = r.src_id
    ctx.src_ids["alex_genome"] = conn.execute(
        text("select src_id from genome_dataset where profile_id = :p"), {"p": fam.profiles["alex"]}
    ).scalar_one()
