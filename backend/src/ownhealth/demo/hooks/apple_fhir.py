"""Lane C demo data, imported through the real importers.

- Sam and Lucía: 90 days of Apple Watch and iPhone data (steps on both, resting HR, sleep, VO2max).
- Alex: iPhone steps on top of the base seed's Apple Watch series, so the source-priority rule has
  overlap to resolve (the summary keeps showing the Watch, never the sum).
- Carmen: iPhone steps, and an IPS bundle with a condition, an allergy, a medication, a vaccine and
  a procedure. Sam's export carries two Apple clinical records (a prescription and a vaccine).

The demo test pins 7 open review items, so this hook adds no lab observations that would wait for
review.
"""

from __future__ import annotations

import json
import tempfile
from pathlib import Path

from sqlalchemy import Connection

from ownhealth.core.access import get_profile
from ownhealth.core.documents import create_document
from ownhealth.demo import DemoContext
from ownhealth.ingest.apple_health import import_apple_health
from ownhealth.ingest.apple_health.synthetic import ExportSpec, write_export
from ownhealth.ingest.fhir import import_fhir
from ownhealth.ingest.fhir.synthetic import ips_bundle

ORDER = 300

SAM_CLINICAL = [
    {
        "resourceType": "MedicationRequest",
        "id": "ah-sam-cetirizine",
        "status": "active",
        "intent": "order",
        "medicationCodeableConcept": {
            "coding": [{"system": "http://www.nlm.nih.gov/research/umls/rxnorm", "code": "1014678"}],
            "text": "Cetirizine 10 mg tablet",
        },
        "authoredOn": "2026-04-03",
        "dosageInstruction": [{"text": "1 tablet a day in spring"}],
    },
    {
        "resourceType": "Immunization",
        "id": "ah-sam-tdap",
        "status": "completed",
        "vaccineCode": {"coding": [{"system": "http://hl7.org/fhir/sid/cvx", "code": "115", "display": "Tdap"}]},
        "occurrenceDateTime": "2024-02-11",
    },
]


def _document(conn: Connection, ctx: DemoContext, slug: str, data_or_path, filename: str, mime: str):
    if isinstance(data_or_path, Path):
        sha, size = ctx.blobs.put_file(data_or_path)
    else:
        sha, size = ctx.blobs.put_bytes(data_or_path), len(data_or_path)
    return create_document(
        conn,
        household_id=ctx.family.household_id,
        profile_id=ctx.family.profiles[slug],
        blob_sha256=sha,
        mime=mime,
        size_bytes=size,
        filename=filename,
        channel="demo",
        doc_type="other",
    ).id


def _apple(conn: Connection, ctx: DemoContext, slug: str, spec: ExportSpec) -> None:
    with tempfile.TemporaryDirectory() as tmp:
        path = write_export(Path(tmp) / "export.zip", spec)
        doc = _document(conn, ctx, slug, path, "export.zip", "application/zip")
    import_apple_health(conn, ctx.blobs, document_id=doc)


def seed(conn: Connection, ctx: DemoContext) -> None:
    base = {"end": ctx.as_of, "days": 90}
    _apple(
        conn,
        ctx,
        "sam",
        ExportSpec(first_name="Sam", birth_date=_born(conn, ctx, "sam"), seed=11, clinical=SAM_CLINICAL, **base),
    )
    _apple(conn, ctx, "lucia", ExportSpec(first_name="Lucía", birth_date=_born(conn, ctx, "lucia"), seed=12, **base))
    _apple(
        conn,
        ctx,
        "alex",
        ExportSpec(first_name="Alex", birth_date=_born(conn, ctx, "alex"), seed=13, watch=False, **base),
    )
    _apple(
        conn,
        ctx,
        "carmen",
        ExportSpec(first_name="Carmen", birth_date=_born(conn, ctx, "carmen"), seed=14, watch=False, **base),
    )
    bundle = ips_bundle(
        bundle_id="ips-carmen-2026",
        given="Carmen",
        family="Ortega",
        birth=_born(conn, ctx, "carmen"),
        with_observations=False,
    )
    doc = _document(conn, ctx, "carmen", json.dumps(bundle).encode(), "carmen-ips.json", "application/fhir+json")
    import_fhir(conn, ctx.blobs, document_id=doc)


def _born(conn: Connection, ctx: DemoContext, slug: str):
    return get_profile(conn, ctx.family.profiles[slug]).birth_date
