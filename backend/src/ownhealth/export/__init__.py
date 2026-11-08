"""Full export of one profile: FHIR R4 NDJSON plus the original files, in one zip.

A UI action behind step-up auth; never an MCP tool (mcp.md 9.2, security-privacy.md 12).

    export-manifest.json                  format, versions, and sha256 + size of every entry
    fhir/Patient.ndjson                   the person
    fhir/<ResourceType>.ndjson            every clinical row, all lifecycle states (meta.tag says which)
    fhir/DocumentReference.ndjson         one per document, pointing at files/
    fhir/Provenance.ndjson                one per src id: which facts it backs, document, page, bbox
    files/<sha256>/<filename>             the original uploads, byte for byte
    genome/<dataset>/...                  genome-facts.json, the VCF and its index
    ownhealth/<table>.ndjson              rows with no FHIR shape (wearables, notes, findings, review, audit)

Rows stream from Postgres into the zip, and files stream from the blob store, so memory stays flat.
"""

from __future__ import annotations

import hashlib
import json
import re
import shutil
import tempfile
import zipfile
from datetime import UTC, date, datetime
from decimal import Decimal
from typing import BinaryIO, Literal
from uuid import UUID

from sqlalchemy import Connection, text

from ownhealth import __version__
from ownhealth.blobs import BlobStore

ExportFormat = Literal["zip"]
FORMAT = "ownhealth.export/v1"
LIFECYCLE_TAG = "urn:ownhealth:lifecycle"
# Not part of a person's export: access control and secrets, and tables that only repeat the profile.
_SKIP_RAW = {"profile", "profile_grant", "api_token"}
_PATIENT_FIELD = {"AllergyIntolerance", "Immunization"}  # these name the person `patient`, not `subject`
_GENOME_FILES = {
    "facts_blob": "genome-facts.json",
    "vcf_blob": "genome.vcf.gz",
    "tbi_blob": "genome.vcf.gz.tbi",
    "coverage_blob": "coverage.bin",
}


def _json_default(v):
    if isinstance(v, datetime | date):
        return v.isoformat()
    if isinstance(v, Decimal):
        return str(v)
    if isinstance(v, UUID):
        return str(v)
    if isinstance(v, bytes | memoryview):
        return bytes(v).hex()
    raise TypeError(type(v).__name__)


def _line(obj) -> bytes:
    return json.dumps(obj, default=_json_default, ensure_ascii=False, separators=(",", ":")).encode() + b"\n"


def _safe_name(name: str | None) -> str:
    base = re.sub(r"[^A-Za-z0-9._-]+", "_", (name or "").rsplit("/", 1)[-1]).strip("._")[:80]
    return base or "file"


class _Writer:
    """Writes zip entries and remembers sha256, size and line count for the manifest.

    A zip takes one entry at a time, so each NDJSON file builds in a spooled temp file (memory up to
    8 MB, then disk) and is copied in when the export finishes; files from the blob store go straight in.
    """

    def __init__(self, out: BinaryIO):
        self.zf = zipfile.ZipFile(out, "w", compression=zipfile.ZIP_DEFLATED, allowZip64=True)
        self.files: list[dict] = []
        self._paths: set[str] = set()
        self._open: dict[str, tuple] = {}

    def lines(self, path: str):
        if path not in self._open:
            self._open[path] = (tempfile.SpooledTemporaryFile(8 << 20), hashlib.sha256(), [0, 0])  # noqa: SIM115
        buf, h, counts = self._open[path]

        def write(obj) -> None:
            data = _line(obj)
            buf.write(data)
            h.update(data)
            counts[0] += 1
            counts[1] += len(data)

        return write

    def close_lines(self) -> None:
        for path, (buf, h, (n, size)) in sorted(self._open.items()):
            buf.seek(0)
            with self.zf.open(path, "w", force_zip64=True) as dst:
                shutil.copyfileobj(buf, dst, 1 << 20)
            buf.close()
            self.files.append({"path": path, "sha256": h.hexdigest(), "bytes": size, "lines": n})
        self._open.clear()

    def blob(self, path: str, blobs: BlobStore, sha: str) -> None:
        if path in self._paths:
            return
        self._paths.add(path)
        h, size = hashlib.sha256(), 0
        info = zipfile.ZipInfo(path, date_time=datetime.now(UTC).timetuple()[:6])
        info.compress_type = zipfile.ZIP_STORED
        with self.zf.open(info, "w", force_zip64=True) as dst:
            for chunk in blobs.iter_chunks(sha):
                dst.write(chunk)
                h.update(chunk)
                size += len(chunk)
        self.files.append({"path": path, "sha256": h.hexdigest(), "bytes": size, "source_blob": sha})

    def finish(self, manifest: dict) -> None:
        self.zf.writestr("export-manifest.json", json.dumps(manifest, default=_json_default, indent=2))
        self.zf.close()


def _tables(conn: Connection) -> tuple[list[str], list[str]]:
    """(tables with a FHIR column, other tables) among those with a profile_id column."""
    rows = conn.execute(
        text(
            "select c.table_name, bool_or(f.column_name is not null) as has_fhir from information_schema.columns c"
            " join information_schema.tables t on t.table_schema = c.table_schema and t.table_name = c.table_name"
            " left join information_schema.columns f on f.table_schema = c.table_schema"
            "  and f.table_name = c.table_name and f.column_name = 'fhir'"
            " where c.table_schema = current_schema() and c.column_name = 'profile_id' and t.table_type = 'BASE TABLE'"
            " group by c.table_name order by c.table_name"
        )
    ).all()
    fhir = [r.table_name for r in rows if r.has_fhir and r.table_name not in _SKIP_RAW]
    raw = [r.table_name for r in rows if not r.has_fhir and r.table_name not in _SKIP_RAW]
    return fhir, raw


def _patient(p) -> dict:
    gender = {"female": "female", "male": "male", "unknown": "unknown"}.get(p.sex_at_birth or "")
    return {
        k: v
        for k, v in {
            "resourceType": "Patient",
            "id": str(p.id),
            "name": [{"text": p.display_name}],
            "birthDate": p.birth_date.isoformat() if p.birth_date else None,
            "gender": gender,
            "deceasedBoolean": True if p.deceased else None,
            "address": [{"country": p.country}] if p.country else None,
        }.items()
        if v is not None
    }


def _stream(conn: Connection, sql: str, params: dict):
    # per statement: Connection.execution_options() would change the caller's connection for good
    return conn.execute(text(sql), params, execution_options={"stream_results": True, "max_row_buffer": 500})


def export_profile(
    conn: Connection, blobs: BlobStore, profile_id: UUID, out: BinaryIO, *, fmt: ExportFormat = "zip"
) -> dict:
    """Write the full export of one profile into `out` (a writable binary file). Returns the manifest.

    `out` is written to, never stored: the export does not land in the blob store or in backups.
    """
    if fmt != "zip":
        raise ValueError(f"unknown export format {fmt}")
    prof = conn.execute(text("select * from profile where id = :p"), {"p": profile_id}).first()
    if prof is None:
        raise LookupError("profile not found")
    w = _Writer(out)
    w.lines("fhir/Patient.ndjson")(_patient(prof))
    patient_ref = {"reference": f"Patient/{prof.id}"}
    fhir_tables, raw_tables = _tables(conn)
    cites: dict[str, list[str]] = {}
    counts: dict[str, int] = {}
    for table in fhir_tables:
        cols = set(conn.execute(text(f'select * from "{table}" limit 0')).keys())
        status = "status" if "status" in cols else "null"
        src = "src_id" if "src_id" in cols else "null"
        rows = _stream(
            conn,
            f'select id, fhir, {status} as status, {src} as src_id from "{table}" where profile_id = :p order by id',
            {"p": profile_id},
        )
        for r in rows:
            res = dict(r.fhir or {})
            rtype = res.get("resourceType") or "Basic"
            res["resourceType"] = rtype
            res.setdefault("id", str(r.id))
            meta = dict(res.get("meta") or {})
            if r.status:
                meta["tag"] = [*(meta.get("tag") or []), {"system": LIFECYCLE_TAG, "code": r.status}]
            if r.src_id:
                meta["source"] = f"urn:ownhealth:src:{r.src_id}"
                cites.setdefault(r.src_id, []).append(f"{rtype}/{res['id']}")
            res["meta"] = meta
            ref_field = "patient" if rtype in _PATIENT_FIELD else "subject"
            if "subject" not in res and "patient" not in res:
                res[ref_field] = patient_ref
            w.lines(f"fhir/{rtype}.ndjson")(res)
            counts[rtype] = counts.get(rtype, 0) + 1
    # documents and their files
    doc_write = w.lines("fhir/DocumentReference.ndjson")
    docs = conn.execute(
        text(
            "select id, blob_sha256, filename, mime, size_bytes, doc_type, status, collected_at, created_at"
            " from document where profile_id = :p order by created_at, id"
        ),
        {"p": profile_id},
    ).all()
    for d in docs:
        sha = bytes(d.blob_sha256).hex()
        path = f"files/{sha}/{_safe_name(d.filename)}"
        doc_write(
            {
                "resourceType": "DocumentReference",
                "id": str(d.id),
                "status": "current" if d.status == "active" else "entered-in-error",
                "type": {"text": d.doc_type or "other"},
                "subject": patient_ref,
                "date": (d.collected_at or d.created_at).isoformat(),
                "content": [
                    {"attachment": {"contentType": d.mime, "url": path, "title": d.filename, "size": d.size_bytes}}
                ],
            }
        )
        if blobs.exists(sha):
            w.blob(path, blobs, sha)
    counts["DocumentReference"] = len(docs)
    # provenance: one per src id of the profile
    prov = w.lines("fhir/Provenance.ndjson")
    for s in _stream(conn, "select * from src where profile_id = :p order by id", {"p": profile_id}):
        entry = {
            "resourceType": "Provenance",
            "id": s.id.replace("_", "-"),
            "target": [{"reference": ref} for ref in cites.get(s.id, [])] or [patient_ref],
            "recorded": s.created_at.isoformat(),
            "agent": [{"type": {"text": s.actor_kind}, "who": {"display": s.kind}}],
            "extension": [{"url": "urn:ownhealth:src", "valueString": s.id}],
        }
        if s.document_id:
            loc = {"document_id": str(s.document_id), "page": s.page_no, "bbox": s.bbox}
            entry["entity"] = [{"role": "source", "what": {"reference": f"DocumentReference/{s.document_id}"}}]
            entry["extension"].append({"url": "urn:ownhealth:location", "valueString": json.dumps(loc)})
        if s.import_ref:
            entry["extension"].append({"url": "urn:ownhealth:import-ref", "valueString": s.import_ref})
        prov(entry)
        counts["Provenance"] = counts.get("Provenance", 0) + 1
    # genome files
    for g in conn.execute(text("select * from genome_dataset where profile_id = :p"), {"p": profile_id}).mappings():
        for col, name in _GENOME_FILES.items():
            if g.get(col) is not None and blobs.exists(bytes(g[col]).hex()):
                w.blob(f"genome/{g['id']}/{name}", blobs, bytes(g[col]).hex())
    # everything else, as stored
    for table in raw_tables:
        write = w.lines(f"ownhealth/{table}.ndjson")
        n = 0
        for (row,) in _stream(conn, f'select to_jsonb(t) from "{table}" t where profile_id = :p', {"p": profile_id}):
            write(row)
            n += 1
        counts[f"ownhealth/{table}"] = n
    w.close_lines()
    versions = conn.execute(text("select version from schema_migrations order by version")).scalars().all()
    manifest = {
        "format": FORMAT,
        "generated_at": datetime.now(UTC).isoformat(),
        "ownhealth_version": __version__,
        "schema_migrations": list(versions),
        "fhir_version": "4.0.1",
        "profile": {"id": str(prof.id), "slug": prof.slug, "display_name": prof.display_name},
        "counts": counts,
        "files": w.files,
    }
    w.finish(manifest)
    return manifest
