"""Import of a genome-facts.json and its raw files (genomics.md 2, 5).

The facts file (kilobytes) is parsed into `genome_dataset`, `genome_coverage` and `genome_finding`.
The tabix-indexed VCF, gVCF and chip site list stay in the blob store, and the dataset points at them
only after their sha256 and their index's sha256 match the facts file. A file that fails any check
raises `GenomeImportError` and the caller's transaction rolls back: nothing is half-imported.
"""

from __future__ import annotations

import hashlib
import json
import re
from contextlib import contextmanager
from datetime import datetime
from uuid import UUID

import pysam
from sqlalchemy import Connection, text

from ownhealth.blobs import BlobStore, sha256_hex_to_bytes
from ownhealth.core.ids import short
from ownhealth.core.provenance import SrcIn, ensure_src
from ownhealth.genome.model import GenomeImportError
from ownhealth.genome.schema import DEFER_CHIP_SITES, chip_site_errors, validate_facts
from ownhealth.genome.sensitive import is_sensitive_gene, norm_chrom
from ownhealth.ingest import ImportSummary

RAW_ROLES = {
    "calls_vcf": ("vcf_blob", "tbi_blob"),
    "gvcf": ("gvcf_blob", "gvcf_tbi_blob"),
    "chip_sites": ("sites_blob", "sites_tbi_blob"),
}
FINGERPRINT_MIN_SITES = 20
FINGERPRINT_MIN_CONCORDANCE = 0.95
_UNKNOWN_ORDER = ("no_call", "not_covered", "not_on_array")
_REF_ALLELES = {"*1", "*1A", "*1.001"}


def finding_call_state(fact: dict) -> str | None:
    """One call_state for a finding: alt if any cited site is alt, else the first unknown state, else ref.
    A diplotype without listed sites (CYP2D6 from reads) is ref only when both alleles are *1."""
    states = [(v.get("genotype") or {}).get("call_state") for v in fact.get("variants") or []]
    states = [s for s in states if s]
    if states:
        if "alt" in states:
            return "alt"
        for s in _UNKNOWN_ORDER:
            if s in states:
                return s
        return "ref"
    diplotype = (fact.get("value") or {}).get("diplotype")
    if isinstance(diplotype, str) and diplotype.strip():
        alleles = [a.strip() for a in re.split(r"[/|]", diplotype) if a.strip()]
        return "ref" if alleles and all(a in _REF_ALLELES for a in alleles) else "alt"
    return None


def sensitivity_flags(fact: dict) -> tuple[bool, bool, bool]:
    """(sensitive, adult_onset, carrier). Sensitive covers the producer's `sensitive` and `adult_onset`
    tags and our own gene list, because the consent switch covers both (genomics.md 7). PRS counts as
    adult-onset so the minor rule withholds it."""
    level = (fact.get("sensitivity") or {}).get("level", "normal")
    adult_onset = level == "adult_onset" or fact["category"] == "prs"
    sensitive = level in ("sensitive", "adult_onset") or is_sensitive_gene(fact.get("gene"))
    return sensitive, adult_onset, fact["category"] == "carrier"


def _rehash(blobs: BlobStore, sha: str) -> str:
    h = hashlib.sha256()
    for chunk in blobs.iter_chunks(sha):
        h.update(chunk)
    return h.hexdigest()


def _fingerprint_concordance(a: str, b: str) -> tuple[float, int] | None:
    pairs = [(x, y) for x, y in zip(a, b, strict=False) if x != "." and y != "."]
    if len(pairs) < FINGERPRINT_MIN_SITES:
        return None
    return sum(x == y for x, y in pairs) / len(pairs), len(pairs)


def _owned(conn: Connection, profile_id: UUID, sha: str, attachments: dict[str, str]) -> bool:
    """A raw file may be linked when it came with this upload or is already this profile's: the facts
    file names raw files by sha256, and a hash on its own grants nothing."""
    if sha in attachments.values():
        return True
    return (
        conn.execute(
            text(
                "select 1 from genome_dataset where profile_id = :p and :s in"
                " (vcf_blob, tbi_blob, gvcf_blob, gvcf_tbi_blob, sites_blob, sites_tbi_blob) limit 1"
            ),
            {"p": profile_id, "s": sha256_hex_to_bytes(sha)},
        ).first()
        is not None
    )


def _raw_files(
    conn: Connection, blobs: BlobStore, doc: dict, attachments: dict[str, str], profile_id: UUID
) -> tuple[dict[str, bytes], list[str]]:
    """Columns -> blob sha (bytes) for every raw file the facts list, we hold and this profile may use
    (uploaded with it, or already its own), after a sha256 check."""
    listed: dict[str, str] = {}
    for f in doc["files"]:
        listed[f["sha256"]] = f["path"]
        if f.get("index_sha256"):
            listed[f["index_sha256"]] = f.get("index_path") or f["path"] + ".tbi"
    for name, sha in attachments.items():
        if sha not in listed:
            raise GenomeImportError(
                f"{name}: its sha256 ({sha[:12]}…) matches no file listed in the facts file, so it is not a"
                " file this genome-facts.json describes. Nothing was imported."
            )
    columns: dict[str, bytes] = {}
    missing: list[str] = []
    for f in doc["files"]:
        if f["role"] not in RAW_ROLES:
            continue
        data_col, index_col = RAW_ROLES[f["role"]]
        if data_col in columns:
            raise GenomeImportError(f"the facts file lists two {f['role']} files; v1 takes one per role")
        sha, idx = f["sha256"], f["index_sha256"]
        if not all(blobs.exists(h) and _owned(conn, profile_id, h, attachments) for h in (sha, idx)):
            missing.append(f"{f['role']} {f['path']}")
            continue
        for want, label in ((sha, f["path"]), (idx, f.get("index_path") or "index")):
            if _rehash(blobs, want) != want:
                raise GenomeImportError(f"{label}: stored bytes do not match sha256 {want[:12]}…")
        try:
            with pysam.VariantFile(str(blobs.path(sha)), index_filename=str(blobs.path(idx))) as vf:
                vf.header  # noqa: B018 (opens and parses the header)
        except (OSError, ValueError) as exc:
            raise GenomeImportError(f"{f['path']}: not a bgzipped VCF with a tabix index ({exc})") from exc
        columns[data_col] = sha256_hex_to_bytes(sha)
        columns[index_col] = sha256_hex_to_bytes(idx)
    return columns, missing


@contextmanager
def _chip_sites(blobs: BlobStore, doc: dict, raw: dict[str, bytes]):
    """A (chrom, pos) -> probed predicate over the checked chip_sites file, or None when it is not on hand."""
    f = next((f for f in doc["files"] if f["role"] == "chip_sites"), None)
    if f is None or "sites_blob" not in raw:
        yield None
        return
    with pysam.VariantFile(str(blobs.path(f["sha256"])), index_filename=str(blobs.path(f["index_sha256"]))) as vf:

        def probed(chrom: str, pos: int) -> bool:
            c = norm_chrom(chrom)
            for name in (c, c[3:]):
                try:
                    return any(r.pos == pos for r in vf.fetch(name, pos - 1, pos))
                except ValueError:
                    continue
            return False

        yield probed


def _finding_src_id(conn: Connection, fact_id: str) -> str:
    sid = "src_g_" + fact_id[2:12]
    if conn.execute(text("select 1 from src where id = :s"), {"s": sid}).first():
        sid = f"{sid}_{short(4)}"
    return sid


def import_genome_facts(
    conn: Connection,
    blobs: BlobStore,
    *,
    document_id: UUID,
    attachments: dict[str, str] | None = None,
    actor_user: UUID | None = None,
    imported_at: datetime | None = None,
) -> ImportSummary:
    """Import the genome-facts.json stored as `document_id` (its profile must be set).

    `attachments` maps uploaded file names to their blob sha256 (the .vcf.gz and .tbi uploaded with it).
    Raw files already linked to this profile under the sha256 the facts file names are used too.
    Re-importing the same bytes is a no-op; a new file for the same sample supersedes the old one.
    `imported_at` backdates the import (demo seed); default now.
    """
    row = (
        conn.execute(
            text(
                "select id, household_id, profile_id, encode(blob_sha256, 'hex') as sha, filename from document"
                " where id = :d"
            ),
            {"d": document_id},
        )
        .mappings()
        .first()
    )
    if row is None:
        raise GenomeImportError(f"document {document_id} not found")
    if row["profile_id"] is None:
        raise GenomeImportError("assign the genome-facts file to a profile first")
    profile_id, sha, filename = row["profile_id"], row["sha"], row["filename"] or "genome-facts.json"
    try:
        doc = json.loads(blobs.get_bytes(sha))
    except (ValueError, UnicodeDecodeError) as exc:
        raise GenomeImportError(f"{filename}: not valid JSON ({exc})") from exc
    errors = validate_facts(doc, chip_sites=DEFER_CHIP_SITES)
    if errors:
        raise GenomeImportError(f"{filename} is not a valid genome-facts file:\n- " + "\n- ".join(errors))

    summary = ImportSummary(document_id=str(document_id))
    same = conn.execute(
        text("select profile_id from genome_dataset where facts_sha256 = :s"), {"s": sha256_hex_to_bytes(sha)}
    ).first()
    if same is not None:
        if same.profile_id != profile_id:
            raise GenomeImportError(f"{filename}: this exact file is already imported for another profile")
        summary.skipped = len(doc["facts"])
        summary.reasons["duplicate_facts_file"] = 1
        return summary

    sample_id = doc["subject"]["sample_id"]
    elsewhere = conn.execute(
        text(
            "select p.slug from genome_dataset d join profile p on p.id = d.profile_id where d.sample_id = :s"
            " and d.profile_id <> :p and d.status = 'current' and p.household_id = :h"
        ),
        {"s": sample_id, "p": profile_id, "h": row["household_id"]},
    ).scalar()
    if elsewhere:
        raise GenomeImportError(
            f"sample {sample_id} is already imported for profile '{elsewhere}'; nothing was imported"
        )

    fp = (doc["subject"].get("fingerprint") or {}).get("genotypes")
    panel = (doc["subject"].get("fingerprint") or {}).get("panel")
    if fp:
        others = conn.execute(
            text(
                "select sample_id, subject->'fingerprint'->>'genotypes' as g from genome_dataset where profile_id = :p"
                " and status = 'current' and subject->'fingerprint'->>'panel' = :panel"
            ),
            {"p": profile_id, "panel": panel},
        )
        for other in others:
            got = _fingerprint_concordance(fp, other.g or "")
            if got is not None and got[0] < FINGERPRINT_MIN_CONCORDANCE:
                raise GenomeImportError(
                    f"fingerprint mismatch: this file agrees with source {other.sample_id} at {got[0]:.0%} of"
                    f" {got[1]} sites (at least {FINGERPRINT_MIN_CONCORDANCE:.0%} expected for one person)."
                    " It looks like a different person or a sample swap; nothing was imported."
                )

    raw, missing = _raw_files(conn, blobs, doc, dict(attachments or {}), profile_id)
    if missing:
        summary.reasons["raw_file_missing"] = len(missing)
    with _chip_sites(blobs, doc, raw) as probed:
        errors = chip_site_errors(doc, probed)
    if errors:
        raise GenomeImportError(f"{filename} is not a valid genome-facts file:\n- " + "\n- ".join(errors))

    prov = doc["provenance"]
    old = (
        conn.execute(
            text("select id from genome_dataset where profile_id = :p and sample_id = :s and status = 'current'"),
            {"p": profile_id, "s": sample_id},
        )
        .scalars()
        .all()
    )
    for old_id in old:
        conn.execute(
            text(
                "update genome_dataset set status = 'superseded', status_reason = 'a newer facts file for this"
                " sample', superseded_at = now() where id = :id"
            ),
            {"id": old_id},
        )
    previous: dict[str, UUID] = {}
    if old:
        for r in conn.execute(
            text("select id, fact_id from genome_finding where dataset_id = any(:ids) and status = 'current'"),
            {"ids": list(old)},
        ):
            previous[r.fact_id] = r.id
        conn.execute(
            text(
                "update genome_finding set status = 'superseded', status_reason = 're-run', superseded_at = now()"
                " where dataset_id = any(:ids) and status = 'current'"
            ),
            {"ids": list(old)},
        )

    generated = prov["generated_utc"][:10]
    dataset_src = ensure_src(
        conn,
        SrcIn(
            kind="genome_facts",
            profile_id=profile_id,
            document_id=document_id,
            import_ref=filename,
            label=f"{prov['pipeline']} {prov['version']} · {doc['assay']['type']} · {generated}",
            actor_kind="user" if actor_user else "system",
            actor_id=actor_user,
            id=f"src_gd_{short(8)}",
        ),
    )
    dataset_id = conn.execute(
        text(
            "insert into genome_dataset (profile_id, sample_id, assay, facts_schema, facts_sha256, facts_blob,"
            " vcf_blob, tbi_blob, gvcf_blob, gvcf_tbi_blob, sites_blob, sites_tbi_blob, build, owngenome_version,"
            " db_versions, src_id, document_id, assay_detail, producer, subject, qc, imported_at)"
            " values (:p, :s, :assay, :schema, :sha, :sha, :vcf_blob, :tbi_blob, :gvcf_blob, :gvcf_tbi_blob,"
            " :sites_blob, :sites_tbi_blob,"
            " :build, :ver, cast(:dbv as jsonb), :src, :doc, cast(:ad as jsonb), cast(:pr as jsonb),"
            " cast(:sub as jsonb), cast(:qc as jsonb), coalesce(cast(:at as timestamptz), now())) returning id"
        ),
        {
            "p": profile_id,
            "s": sample_id,
            "assay": doc["assay"]["type"],
            "schema": doc["schema"],
            "sha": sha256_hex_to_bytes(sha),
            **{c: raw.get(c) for cols in RAW_ROLES.values() for c in cols},
            "build": doc["assay"]["build"],
            "ver": f"{prov['pipeline']} {prov['version']}",
            "dbv": json.dumps(prov["db_versions"]),
            "src": dataset_src,
            "doc": document_id,
            "ad": json.dumps(doc["assay"]),
            "pr": json.dumps({k: v for k, v in prov.items() if k != "db_versions"}),
            "sub": json.dumps(doc["subject"]),
            "qc": json.dumps(doc.get("qc") or {}),
            "at": imported_at,
        },
    ).scalar_one()

    for c in doc["coverage"]:
        conn.execute(
            text(
                "insert into genome_coverage (dataset_id, scope, target, panel, state, reason, fraction_callable,"
                " note) values (:d, :scope, :t, :panel, :state, :reason, :fc, :note)"
                " on conflict (dataset_id, scope, target) do nothing"
            ),
            {
                "d": dataset_id,
                "scope": c["scope"],
                "t": c["id"],
                "panel": c.get("panel"),
                "state": c["state"],
                "reason": c.get("reason"),
                "fc": c.get("fraction_callable"),
                "note": c.get("note"),
            },
        )

    raised = 0
    for fact in doc["facts"]:
        sensitive, adult_onset, carrier = sensitivity_flags(fact)
        if sensitive and (fact.get("sensitivity") or {}).get("level", "normal") == "normal":
            raised += 1
        first = (fact.get("variants") or [{}])[0]
        value = fact.get("value") or {}
        score = value.get("activity_score")
        src = ensure_src(
            conn,
            SrcIn(
                kind="genome_facts",
                profile_id=profile_id,
                document_id=document_id,
                import_ref=f"{filename}#{fact['id']}",
                label=f"{prov['pipeline']} {prov['version']} · {fact.get('gene') or fact['category']}",
                actor_kind="user" if actor_user else "system",
                actor_id=actor_user,
                id=_finding_src_id(conn, fact["id"]),
            ),
        )
        conn.execute(
            text(
                "insert into genome_finding (dataset_id, profile_id, fact_id, category, gene, rsid, hgvs, vrs_id,"
                " clinvar_variation_id, diplotype, phenotype, activity_score, zygosity, evidence, confidence,"
                " call_state, sensitive, adult_onset, carrier, src_id, supersedes, fact, recorded_at)"
                " values (:d, :p, :fid, :cat, :gene, :rsid, :hgvs, :vrs, :cv, :dip, :phe, :score, :zyg, :ev, :conf,"
                " :cs, :sens, :ao,"
                " :car, :src, :sup, cast(:fact as jsonb), coalesce(cast(:at as timestamptz), now()))"
            ),
            {
                "d": dataset_id,
                "p": profile_id,
                "fid": fact["id"],
                "cat": fact["category"],
                "gene": fact.get("gene"),
                "rsid": first.get("rsid"),
                "hgvs": first.get("hgvs_c") or first.get("hgvs_p"),
                "vrs": first.get("vrs_id"),
                "cv": first.get("clinvar_variation_id"),
                "dip": value.get("diplotype") or value.get("haplotype"),
                "phe": value.get("phenotype"),
                "score": score if isinstance(score, int | float) else None,
                "zyg": (first.get("genotype") or {}).get("zygosity"),
                "ev": fact["evidence"]["level"],
                "conf": fact["confidence"]["level"],
                "cs": finding_call_state(fact),
                "sens": sensitive,
                "ao": adult_onset,
                "car": carrier,
                "src": src,
                "sup": previous.get(fact["id"]),
                "fact": json.dumps(fact),
                "at": imported_at,
            },
        )
    summary.accepted = len(doc["facts"])
    if raised:
        summary.reasons["sensitivity_raised_by_gene_list"] = raised
    return summary
