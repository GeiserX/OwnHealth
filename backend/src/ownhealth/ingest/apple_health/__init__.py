"""Lane C: Apple Health export.zip import (connectors.md 2.2 and 4). Job kind `import.apple_health`.

- `export.xml` is streamed (`parse.ExportReader`, iterparse, constant memory) straight from the zip;
  samples go to `wearable_sample` in batches through COPY, then the hour and day rollups of the
  touched metrics are rebuilt. Every raw sample is kept: the Watch and the iPhone both stay, and
  `wearables.series` picks one per day by source priority.
- `clinical-records/*.json` are FHIR R4 resources and go through `ingest.fhir.import_resources`
  (Apple's own clinical medications, conditions, labs and vaccines among them).
- `electrocardiograms/*.csv` are stored as documents of type `ecg`.
- The birth date in `<Me>` must match the profile: a different one stops the import (wrong profile).
"""

from __future__ import annotations

import json
import zipfile
from uuid import UUID

from sqlalchemy import Connection, text

from ownhealth import wearables
from ownhealth.blobs import BlobStore
from ownhealth.core.access import get_profile
from ownhealth.core.documents import create_document
from ownhealth.core.provenance import SrcIn, ensure_src
from ownhealth.ingest import ImportSummary
from ownhealth.ingest.apple_health.parse import ExportReader, Sample, parse_time
from ownhealth.ingest.fhir import Entry, import_resources, mark_imported
from ownhealth.jobs import PermanentJobError

BATCH = 10_000
PROVIDER = "apple_health"


def _members(z: zipfile.ZipFile) -> tuple[str | None, list[str], list[str]]:
    export, clinical, ecg = None, [], []
    for name in z.namelist():
        low = name.lower()
        base = low.rsplit("/", 1)[-1]
        if base == "export.xml":
            export = name
        elif "/clinical-records/" in "/" + low and low.endswith(".json"):
            clinical.append(name)
        elif "/electrocardiograms/" in "/" + low and low.endswith(".csv"):
            ecg.append(name)
    return export, sorted(clinical), sorted(ecg)


def import_apple_health(conn: Connection, blobs: BlobStore, *, document_id: UUID) -> ImportSummary:
    doc = (
        conn.execute(
            text(
                "select profile_id, household_id, encode(blob_sha256, 'hex') as sha, flags from document where id = :d"
            ),
            {"d": document_id},
        )
        .mappings()
        .one()
    )
    done = (doc["flags"] or {}).get("import")
    if done:
        return ImportSummary(**{k: done[k] for k in ("document_id", "accepted", "queued", "skipped", "reasons")})
    profile = get_profile(conn, doc["profile_id"])
    summary = ImportSummary(document_id=str(document_id))
    with zipfile.ZipFile(blobs.path(doc["sha"])) as z:
        export, clinical, ecg = _members(z)
        if export is None:
            raise PermanentJobError("no export.xml in the zip")
        with z.open(export) as stream:
            reader = ExportReader(stream)
            staged, metrics, sources = _stage_samples(conn, profile.id, reader)
        dob = reader.birth_date
        if dob and profile.birth_date and dob != profile.birth_date:
            raise PermanentJobError(
                f"the export's birth date does not match profile '{profile.slug}': import it into the right profile"
            )
        identity = "matched" if dob and dob == profile.birth_date else "missing"
        inserted = _commit_samples(conn, profile.id)
        if metrics:
            wearables.recompute_rollups(
                conn, profile_id=profile.id, metrics=sorted(metrics), source_ids=sorted(set(sources.values()))
            )
        if inserted:  # the citation for these samples: get_trend and the dashboard name this export
            ensure_src(
                conn,
                SrcIn(
                    kind="apple_health",
                    profile_id=profile.id,
                    document_id=document_id,
                    import_ref=f"export.xml#{document_id}",
                    label="Apple Health export",
                ),
            )
        summary.accepted += inserted
        summary.skipped += staged - inserted
        if staged - inserted:
            summary.reasons["duplicate"] = staged - inserted

        entries = [Entry(name, json.loads(z.read(name))) for name in clinical]
        if entries:
            fhir_summary, _ = import_resources(
                conn,
                entries,
                profile=profile,
                document_id=document_id,
                bundle_key=f"apple-{doc['sha'][:12]}",
                identity=identity,
                label="Apple Health",
            )
            summary.accepted += fhir_summary.accepted
            summary.queued += fhir_summary.queued
            summary.skipped += fhir_summary.skipped
            for k, v in fhir_summary.reasons.items():
                summary.reasons[k] = summary.reasons.get(k, 0) + v

        for name in ecg:
            _store_ecg(conn, blobs, doc["household_id"], profile.id, name, z.read(name))

    details = {
        "samples": inserted,
        "duplicates": staged - inserted,
        "metrics": sorted(metrics),
        "clinical_records": len(clinical),
        "ecg_documents": len(ecg),
        "not_imported": dict(reader.counts.most_common(30)),
        "export_date": reader.export_date,
    }
    conn.execute(
        text(
            "update document set flags = flags || jsonb_build_object('apple_health', cast(:x as jsonb)) where id = :d"
        ),
        {"x": json.dumps(details), "d": document_id},
    )
    mark_imported(conn, document_id, summary, identity)
    return summary


def rebuild_rollups(conn: Connection, *, profile_id: UUID, metric: str | None = None) -> int:
    return wearables.recompute_rollups(conn, profile_id=profile_id, metrics=[metric] if metric else None)


# ------------------------------------------------------------------ samples ----


def _stage_samples(conn: Connection, profile_id: UUID, reader: ExportReader) -> tuple[int, set[str], dict[str, int]]:
    """Stream samples into a temp table in COPY batches. Returns (rows staged, metrics, source ids by device)."""
    conn.execute(
        text(
            "create temp table if not exists ah_stage (metric text, t timestamptz, t_end timestamptz, v real,"
            " tz smallint, source_id smallint) on commit drop"
        )
    )
    conn.execute(text("truncate ah_stage"))
    sources: dict[str, int] = {}
    metrics: set[str] = set()
    batch: list[Sample] = []
    total = 0
    for s in reader.samples():
        batch.append(s)
        if len(batch) >= BATCH:
            total += _copy(conn, profile_id, batch, sources, metrics)
            batch = []
    if batch:
        total += _copy(conn, profile_id, batch, sources, metrics)
    return total, metrics, sources


def _copy(conn: Connection, profile_id: UUID, batch: list[Sample], sources: dict[str, int], metrics: set[str]) -> int:
    for s in batch:
        if s.device not in sources:
            sources[s.device] = wearables.source_id(conn, profile_id, PROVIDER, s.device)
        metrics.add(s.metric)
    raw = conn.connection.driver_connection
    with raw.cursor() as cur, cur.copy("copy ah_stage (metric, t, t_end, v, tz, source_id) from stdin") as cp:
        for s in batch:
            cp.write_row((s.metric, s.t, s.t_end, s.v, s.tz_offset_min, sources[s.device]))
    return len(batch)


def _commit_samples(conn: Connection, profile_id: UUID) -> int:
    """Move staged rows into wearable_sample. A re-import of the same samples inserts nothing."""
    return conn.execute(
        text(
            "insert into wearable_sample (profile_id, metric, t, t_end, v, tz_offset_min, source_id)"
            " select distinct on (metric, source_id, t) :p, metric, t, t_end, v, tz, source_id from ah_stage"
            " order by metric, source_id, t on conflict do nothing"
        ),
        {"p": profile_id},
    ).rowcount


# ---------------------------------------------------------- clinical, ECG ----


def _store_ecg(conn: Connection, blobs: BlobStore, household_id, profile_id, name: str, data: bytes) -> None:
    sha = blobs.put_bytes(data)
    ref = create_document(
        conn,
        household_id=household_id,
        profile_id=profile_id,
        blob_sha256=sha,
        mime="text/csv",
        size_bytes=len(data),
        filename=name.rsplit("/", 1)[-1],
        channel="apple_health",
        doc_type="ecg",
    )
    recorded = None
    for line in data.decode("utf-8", "replace").splitlines()[:20]:
        key, _, value = line.partition(",")
        if key.strip().lower() in ("recorded date", "fecha de registro"):
            try:
                recorded = parse_time(value.strip().strip('"'))[0]
            except (ValueError, IndexError):
                recorded = None
    if recorded is not None:
        conn.execute(text("update document set collected_at = :c where id = :d"), {"c": recorded, "d": ref.id})
