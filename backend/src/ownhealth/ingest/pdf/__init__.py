"""Lane B: lab PDF import (ingestion.md sections 5-7, 9, 11). Job kind `import.pdf`.

    import_pdf(conn, blobs, document_id=...)   one stored PDF or image -> pages, guesses, rows

Steps, all in the caller's transaction (so a crashed job leaves nothing half done):
  1. read the text layer (`textlayer`): words with bboxes and fonts, hidden-text and injection flags;
  2. store one `document_page` per page (word layer as a blob, page text, full-text vector);
  3. classify (`classify`): type, language, patient block, dates, report status; check the identity
     on the page against the profile and guess the right profile when it does not match;
  4. read the lab table (`extract` -> vendor template, else generic rules) into candidate rows;
  5. normalize each row (lane A), add grounding, identity, date, duplicate and conflict checks, and
     route it through `core.review.submit_candidate` (auto-accept or review queue);
  6. draw grouping: `report_key` per draw; an amended or final edition of an earlier report links
     identical rows, confirms provisional ones, and queues changed values as `amended_report`;
     "pending" lines become `expected_test` rows.
A page without a usable text layer, an image, or a lab report whose table nobody could read becomes
one `pending_document` review item for agent-assisted extraction (MCP `propose`). No OCR in v0.1.
"""

from __future__ import annotations

import hashlib
import io
import json
import re
from dataclasses import dataclass
from datetime import UTC, datetime
from decimal import Decimal, InvalidOperation
from uuid import UUID

from sqlalchemy import Connection, text

from ownhealth import normalize
from ownhealth.blobs import BlobStore, sha256_hex_to_bytes
from ownhealth.core.acceptance import DEFAULT_THRESHOLD, Check
from ownhealth.core.observations import ObservationIn, confirm_observation, retract_observation, supersede_observation
from ownhealth.core.provenance import SrcIn
from ownhealth.core.review import (
    CandidateIn,
    ReviewItemIn,
    create_extraction_run,
    create_item,
    dismiss,
    finish_extraction_run,
    register_accept_handler,
    submit_candidate,
)
from ownhealth.ingest import ImportSummary
from ownhealth.ingest.classify import fold, identity_match, name_tokens
from ownhealth.ingest.pdf.extract import Extraction, extract
from ownhealth.ingest.pdf.tables import Row, TableSpec
from ownhealth.ingest.pdf.textlayer import ENGINE, ENGINE_VERSION, PageModel, injection_hits, read_pdf

FTS_CONFIG = {"es": "spanish", "en": "english", "de": "german", "fr": "french"}
GROUND_PAD = 4.0


@dataclass(frozen=True)
class Word:
    text: str
    bbox: tuple[float, float, float, float]  # x0, top, x1, bottom in PDF points, top-left origin
    font: str = ""
    size: float = 0.0
    bold: bool = False
    hidden: str | None = None  # white_on_white | tiny | outside_page: never read as a value


class DocumentNotFound(LookupError):
    """Unknown document id. The job handler turns it into a permanent failure."""


# ------------------------------------------------------------------ import ----


def import_pdf(
    conn: Connection,
    blobs: BlobStore,
    *,
    document_id: UUID,
    force: bool = False,
    threshold: float = DEFAULT_THRESHOLD,
) -> ImportSummary:
    doc = conn.execute(text("select * from document where id = :d for update"), {"d": document_id}).mappings().first()
    if doc is None:
        raise DocumentNotFound(f"document {document_id} not found")
    summary = ImportSummary(document_id=str(document_id))
    if doc["profile_id"] is None or doc["status"] != "active":
        summary.reasons["not_assigned_or_retracted"] = 1
        return summary
    if (
        not force
        and conn.execute(
            text("select 1 from extraction_run where document_id = :d and status = 'done' and extractor <> 'agent'"),
            {"d": document_id},
        ).first()
    ):
        summary.reasons["already_imported"] = 1
        return summary
    data = blobs.get_bytes(bytes(doc["blob_sha256"]).hex())
    if doc["mime"] != "application/pdf":
        _pending_document(conn, doc, pages=[1], why="image")
        summary.queued += 1
        summary.reasons["pending_document"] = 1
        return summary

    ext = extract(data)
    profile = dict(conn.execute(text("select * from profile where id = :p"), {"p": doc["profile_id"]}).mappings().one())
    _store_pages(conn, blobs, document_id, ext)
    identity = identity_match(ext.cls.patient_name, ext.cls.birth_date, profile["display_name"], profile["birth_date"])
    guess = _guess_profile(conn, profile["household_id"], ext) if identity != "matched" else None
    sample_hash = hashlib.sha256(fold(ext.cls.sample_id).encode()).hexdigest()[:16] if ext.cls.sample_id else None
    _update_document(conn, doc, ext, identity, guess, sample_hash)
    if guess and guess != profile["id"]:
        _identity_item(conn, doc, guess)
        summary.queued += 1

    results = ext.results() if ext.cls.doc_type == "lab_report" else []
    unread = ext.pending_pages + (ext.no_table_pages if ext.cls.doc_type == "lab_report" else [])
    if unread:
        why = "no_text_layer" if ext.pending_pages else "no_table"
        _pending_document(conn, doc, pages=unread, why=why)
        summary.queued += 1
        summary.reasons["pending_document"] = 1
    if not results and not ext.pending():
        return summary

    run_id = create_extraction_run(
        conn,
        document_id,
        extractor=ext.extractor,
        extractor_version=ext.extractor_version,
        config_hash=ext.config_hash,
        engine=ENGINE,
        engine_version=ENGINE_VERSION,
    )
    effective_at, date_check = _effective_at(ext, profile)
    report_key = _report_key(ext, sample_hash, document_id)
    provider = ext.template.provider_name if ext.template else None
    pages = {p.page_no: p for p in ext.pdf.pages}

    normalized: list[tuple[TableSpec, Row, ObservationIn, list[Check]]] = []
    for spec, row in results:
        n = normalize.normalize_row(
            conn,
            profile_id=profile["id"],
            printed_name=row.text("name"),
            printed_value=row.text("value"),
            printed_unit=row.text("unit") or None,
            printed_range=row.text("ref_range") or None,
            printed_flag=row.text("flag") or None,
            effective_at=effective_at,
            lang=ext.locale,
            specimen=row.specimen,
            provider=provider,
        )
        obs = n.observation.model_copy(update={"report_key": report_key, "document_id": document_id})
        checks = [*n.checks, *_row_checks(row, pages[row.page_no]), date_check]
        normalized.append((spec, row, obs, checks))

    earlier = _earlier_edition(conn, doc, ext, sample_hash, effective_at, {o.alias_group for _, _, o, _ in normalized})
    if earlier:
        conn.execute(
            text("update document set supersedes_document = :e where id = :d"), {"e": earlier, "d": document_id}
        )
    old_rows = _observations_of(conn, earlier) if earlier else {}
    provisional = ext.cls.report_status == "preliminary"

    for spec, row, obs, checks in normalized:
        cand = CandidateIn(
            page_no=row.page_no,
            bbox=row.bbox,
            cells={k: c.as_dict() for k, c in row.cells.items()},
            row_conf=row.conf(spec.prior),
            observation=obs,
            checks=checks,
        )
        old = old_rows.get(obs.alias_group)
        if old is not None:
            if _same_value(old, obs):
                _link(conn, run_id, cand, old["id"], state="confirmed")
                if old["status"] == "provisional" and not provisional:
                    confirm_observation(conn, old["id"])
                summary.skipped += 1
                summary.reasons["identical_in_earlier_edition"] = (
                    summary.reasons.get("identical_in_earlier_edition", 0) + 1
                )
            else:
                _amended_item(conn, run_id, doc, cand, old, provisional)
                summary.queued += 1
                summary.reasons["amended_value"] = summary.reasons.get("amended_value", 0) + 1
            continue
        dup = _duplicate(conn, obs, document_id)
        if dup is not None and _same_value(dup, obs):
            _link(conn, run_id, cand, dup["id"], state="confirmed")
            summary.skipped += 1
            summary.reasons["duplicate"] = summary.reasons.get("duplicate", 0) + 1
            continue
        if dup is not None:
            cand.checks.append(
                Check(
                    name="conflict",
                    ok=False,
                    reason="conflict",
                    detail=f"another document holds {dup['printed_value']} for this date",
                )
            )
        result = submit_candidate(
            conn,
            run_id,
            cand,
            allowance="template_text" if spec.prior >= 1.0 else "rules_text",
            identity=identity,
            threshold=threshold,
            provisional=provisional,
        )
        if result.state == "auto_accepted":
            summary.accepted += 1
        else:
            summary.queued += 1
            for r in result.reasons:
                summary.reasons[r] = summary.reasons.get(r, 0) + 1

    conn.execute(text("delete from expected_test where report_document_id = :d"), {"d": document_id})
    for row in ext.pending():
        conn.execute(
            text(
                "insert into expected_test (report_document_id, printed_name, alias_group, reason)"
                " values (:d, :n, :g, :r)"
            ),
            {"d": document_id, "n": row.text("name"), "g": _alias_group(conn, row, ext.locale), "r": row.text("value")},
        )
    finish_extraction_run(conn, run_id)
    return summary


# ------------------------------------------------------------------- pages ----


def _store_pages(conn: Connection, blobs: BlobStore, document_id: UUID, ext: Extraction) -> None:
    cfg = FTS_CONFIG.get(ext.cls.language or "", "simple")
    for page in ext.pdf.pages:
        layer = {
            "engine": f"{ENGINE} {ENGINE_VERSION}",
            "width": page.width,
            "height": page.height,
            "words": [w.as_dict() for w in page.words],
        }
        words_sha = blobs.put_bytes(json.dumps(layer, ensure_ascii=False, separators=(",", ":")).encode())
        conn.execute(
            text(
                "insert into document_page (document_id, page_no, width_pt, height_pt, text_layer_ok, words_blob,"
                " flags, text_content, fts) values (:d, :n, :w, :h, :ok, :wb, cast(:f as jsonb), :t,"
                " to_tsvector(cast(:cfg as regconfig), :vt))"
                " on conflict (document_id, page_no) do update set width_pt = excluded.width_pt,"
                " height_pt = excluded.height_pt, text_layer_ok = excluded.text_layer_ok,"
                " words_blob = excluded.words_blob, flags = excluded.flags, text_content = excluded.text_content,"
                " fts = excluded.fts"
            ),
            {
                "d": document_id,
                "n": page.page_no,
                "w": page.width,
                "h": page.height,
                "ok": page.text_layer_ok,
                "wb": sha256_hex_to_bytes(words_sha),
                "f": json.dumps(page.flags),
                "t": page.text(),
                "cfg": cfg,
                "vt": page.text(visible_only=True),
            },
        )


def _update_document(conn, doc, ext: Extraction, identity: str, guess: UUID | None, sample_hash: str | None) -> None:
    c = ext.cls
    flags = dict(ext.pdf.flags)
    if ext.pdf.metadata or ext.pdf.annotations:
        flags["metadata_text"] = {
            **ext.pdf.metadata,
            **({"annotations": ext.pdf.annotations} if ext.pdf.annotations else {}),
        }
    flags["guess"] = {
        "doc_type": c.doc_type,
        "profile_id": str(guess) if guess else (str(doc["profile_id"]) if identity == "matched" else None),
        "collected_at": c.collected_at.isoformat() if c.collected_at else None,
        "provider": ext.template.provider_name if ext.template else None,
        "template_score": ext.template_score,
        "confirmed": False,
    }
    if sample_hash:
        flags["sample_hash"] = sample_hash
    conn.execute(
        text(
            "update document set pages = :pages, doc_type = coalesce(doc_type, :dt), language = :lang,"
            " provider_name = coalesce(provider_name, :prov), provider_fingerprint = :fp, report_status = :rs,"
            " collected_at = :ca, reported_at = :ra, identity_name_hash = :nh, identity_birth_date = :bd,"
            " identity_match = :im, flags = flags || cast(:flags as jsonb) where id = :id"
        ),
        {
            "pages": len(ext.pdf.pages),
            "dt": c.doc_type,
            "lang": c.language,
            "prov": ext.template.provider_name if ext.template else None,
            "fp": ext.template.id if ext.template else None,
            "rs": c.report_status if c.doc_type == "lab_report" else None,
            "ca": c.collected_at.replace(tzinfo=UTC) if c.collected_at else None,
            "ra": c.reported_at.replace(tzinfo=UTC) if c.reported_at else None,
            "nh": hashlib.sha256(" ".join(sorted(name_tokens(c.patient_name))).encode()).digest()
            if c.patient_name
            else None,
            "bd": c.birth_date,
            "im": identity,
            "flags": json.dumps(flags, default=str),
            "id": doc["id"],
        },
    )


def _guess_profile(conn: Connection, household_id: UUID, ext: Extraction) -> UUID | None:
    rows = conn.execute(
        text("select id, display_name, birth_date from profile where household_id = :h"), {"h": household_id}
    ).all()
    matched = [
        r.id
        for r in rows
        if identity_match(ext.cls.patient_name, ext.cls.birth_date, r.display_name, r.birth_date) == "matched"
    ]
    return matched[0] if len(matched) == 1 else None


# ------------------------------------------------------------- review items ----


def _pending_document(conn: Connection, doc, *, pages: list[int], why: str) -> UUID:
    """One review item: this document waits for an outside agent (MCP propose) or manual entry."""
    existing = conn.execute(
        text("select id from review_item where document_id = :d and kind = 'pending_document' and status = 'open'"),
        {"d": doc["id"]},
    ).scalar()
    if existing:
        return existing
    item = ReviewItemIn(
        kind="pending_document",
        household_id=doc["household_id"],
        profile_id=doc["profile_id"],
        payload={"document_id": str(doc["id"]), "pages": pages, "why": why},
        document_id=doc["id"],
        ref_table="document",
        ref_id=doc["id"],
    )
    return create_item(conn, item)


def _identity_item(conn: Connection, doc, guess: UUID) -> UUID:
    return create_item(
        conn,
        ReviewItemIn(
            kind="document_identity",
            household_id=doc["household_id"],
            profile_id=doc["profile_id"],
            payload={
                "document_id": str(doc["id"]),
                "uploaded_to": str(doc["profile_id"]),
                "suggested_profile_id": str(guess),
            },
            reasons=["identity_mismatch"],
            document_id=doc["id"],
            ref_table="profile",
            ref_id=guess,
        ),
    )


def _insert_candidate(conn: Connection, run_id: UUID, cand: CandidateIn, state: str, obs_id: UUID | None) -> UUID:
    reasons = sorted({c.reason for c in cand.checks if not c.ok and c.reason})
    return conn.execute(
        text(
            "insert into candidate_row (extraction_run_id, page_no, bbox, cells, row_conf, reasons, state,"
            " observation_id) values (:r, :p, :b, cast(:c as jsonb), :rc, :rs, :s, :o) returning id"
        ),
        {
            "r": run_id,
            "p": cand.page_no,
            "b": list(cand.bbox),
            "c": json.dumps(cand.cells),
            "rc": cand.row_conf,
            "rs": reasons,
            "s": state,
            "o": obs_id,
        },
    ).scalar_one()


def _link(conn: Connection, run_id: UUID, cand: CandidateIn, obs_id: UUID, state: str) -> UUID:
    """The same fact is already in the record: keep the reading, point it at the existing observation."""
    return _insert_candidate(conn, run_id, cand, state, obs_id)


def _amended_item(conn: Connection, run_id: UUID, doc, cand: CandidateIn, old: dict, provisional: bool) -> UUID:
    row_id = _insert_candidate(conn, run_id, cand, "in_review", None)
    obs = cand.observation.model_copy(update={"candidate_row_id": row_id})
    src = SrcIn(
        kind="document_region",
        profile_id=obs.profile_id,
        document_id=doc["id"],
        page_no=cand.page_no,
        bbox=cand.bbox,
        extraction_run_id=run_id,
        candidate_row_id=row_id,
    )
    reasons = sorted({"amended_value", *(c.reason for c in cand.checks if not c.ok and c.reason)})
    return create_item(
        conn,
        ReviewItemIn(
            kind="amended_report",
            household_id=doc["household_id"],
            profile_id=doc["profile_id"],
            payload={
                "observation": obs.model_dump(mode="json"),
                "src": src.model_dump(mode="json"),
                "checks": [c.model_dump() for c in cand.checks],
                "cells": cand.cells,
                "page_no": cand.page_no,
                "bbox": list(cand.bbox),
                "old_observation_id": str(old["id"]),
                "old_printed_value": old["printed_value"],
                "old_document_id": str(old["document_id"]),
                "provisional": provisional,
            },
            reasons=reasons,
            document_id=doc["id"],
            candidate_row_id=row_id,
            observation_id=old["id"],
            row_conf=cand.row_conf,
        ),
    )


@register_accept_handler("amended_report")
def _accept_amended(conn: Connection, item: dict, user_id: UUID, edits: dict | None) -> list[UUID]:
    """Accepting an amended value supersedes the earlier edition's observation."""
    p = item["payload"]
    obs = ObservationIn.model_validate({**p["observation"], **(edits or {})})
    src = SrcIn.model_validate({**p["src"], "actor_kind": "user", "actor_id": str(user_id)})
    committed = supersede_observation(
        conn,
        UUID(p["old_observation_id"]),
        obs,
        src=src,
        accepted_by="person",
        reason="amended report",
        reviewed_by=user_id,
    )
    if item["candidate_row_id"]:
        conn.execute(
            text("update candidate_row set state = 'confirmed', observation_id = :o where id = :id"),
            {"o": committed.id, "id": item["candidate_row_id"]},
        )
    return [committed.id]


@register_accept_handler("document_identity")
def _accept_identity(conn: Connection, item: dict, user_id: UUID, edits: dict | None) -> list[UUID]:
    """Accepting the suggestion moves the document to the profile named on its pages and re-imports it."""
    target = UUID((edits or {}).get("profile_id") or item["payload"]["suggested_profile_id"])
    assign_document(conn, item["document_id"], target, user_id=user_id, skip_item=item["id"])
    return []


def assign_document(
    conn: Connection, document_id: UUID, profile_id: UUID, *, user_id: UUID, skip_item: UUID | None = None
) -> int | None:
    """Confirm or change a document's profile. Open items of the old reading are dismissed, values
    already committed from it are retracted, and the import runs again for the new profile.
    Returns the new import job id (None when the profile did not change)."""
    from ownhealth.jobs import enqueue

    doc = conn.execute(text("select * from document where id = :d for update"), {"d": document_id}).mappings().one()
    target = conn.execute(text("select household_id from profile where id = :p"), {"p": profile_id}).scalar()
    if target is None or target != doc["household_id"]:
        raise ValueError("the profile is not in this document's household")
    conn.execute(
        text(
            "update document set flags = jsonb_set(flags, '{guess,confirmed}', 'true') where id = :d"
            " and flags ? 'guess'"
        ),
        {"d": document_id},
    )
    if doc["profile_id"] == profile_id:
        return None
    clash = conn.execute(
        text("select 1 from document where profile_id = :p and blob_sha256 = :b and id <> :d"),
        {"p": profile_id, "b": doc["blob_sha256"], "d": document_id},
    ).first()
    if clash:
        raise ValueError("that profile already holds this file")
    for item_id in conn.execute(
        text(
            "select id from review_item where document_id = :d and status = 'open'"
            " and id is distinct from cast(:s as uuid)"
        ),
        {"d": document_id, "s": skip_item},
    ).scalars():
        dismiss(conn, item_id, user_id, "document moved to another profile")
    for obs_id in conn.execute(
        text("select id from observation where document_id = :d and status in ('current', 'provisional')"),
        {"d": document_id},
    ).scalars():
        retract_observation(conn, obs_id, "document moved to another profile")
    conn.execute(
        text("update document set profile_id = :p, identity_match = null where id = :d"),
        {"p": profile_id, "d": document_id},
    )
    return enqueue(
        conn,
        "import.pdf",
        {
            "document_id": str(document_id),
            "profile_id": str(profile_id),
            "household_id": str(doc["household_id"]),
            "force": True,
        },
        dedupe_key=f"import.pdf:{document_id}:{profile_id}",
    )


# ------------------------------------------------------------------ checks ----


def _row_checks(row: Row, page: PageModel) -> list[Check]:
    checks: list[Check] = []
    value = row.cells.get("value")
    rb = row.bbox
    grounded = bool(value and value.text and value.bbox) and _inside(value.bbox, rb, GROUND_PAD)
    checks.append(
        Check(
            name="grounding",
            ok=grounded,
            reason=None if grounded else "not_grounded",
            detail=None if grounded else "no value token inside the row",
        )
    )
    name = row.cells.get("name")
    on_line = bool(name and name.text and name.bbox and value and value.bbox) and (
        min(name.bbox[3], value.bbox[3]) - max(name.bbox[1], value.bbox[1]) > 0
    )
    checks.append(Check(name="name_on_line", ok=on_line, reason=None if on_line else "name_not_on_line"))
    row_text = " ".join(c.text for c in row.cells.values())
    hidden_inside = [w for w in page.words if w.hidden and _inside(w.bbox, rb, 0.0)]
    hits = injection_hits(row_text)
    if hits or hidden_inside:
        checks.append(
            Check(
                name="injection",
                ok=False,
                reason="possible_injection",
                detail="hidden text inside the row" if hidden_inside else ", ".join(hits),
            )
        )
    return checks


def _effective_at(ext: Extraction, profile: dict) -> tuple[datetime, Check]:
    c = ext.cls
    when = c.collected_at or c.reported_at
    if when is None:
        return datetime.now(UTC), Check(
            name="dates", ok=False, reason="date_invalid", detail="no collection date found"
        )
    eff = when.replace(tzinfo=UTC)
    problems = []
    if eff > datetime.now(UTC):
        problems.append("collection date in the future")
    if profile.get("birth_date") and eff.date() < profile["birth_date"]:
        problems.append("collection date before birth")
    if c.collected_at and c.reported_at and c.collected_at > c.reported_at:
        problems.append("collection date after report date")
    if not c.collected_at:
        problems.append("only a report date was found")
    if problems:
        return eff, Check(name="dates", ok=False, reason="date_invalid", detail="; ".join(problems))
    return eff, Check(name="dates", ok=True)


def _report_key(ext: Extraction, sample_hash: str | None, document_id: UUID) -> str:
    when = ext.cls.collected_at or ext.cls.reported_at
    provider = ext.template.id if ext.template else "generic"
    return f"{provider}:{when.date().isoformat() if when else 'nodate'}:{sample_hash or str(document_id)[:8]}"


def _observations_of(conn: Connection, document_id: UUID) -> dict[str, dict]:
    rows = conn.execute(
        text("select * from observation where document_id = :d and status in ('current', 'provisional')"),
        {"d": document_id},
    ).mappings()
    return {r["alias_group"]: dict(r) for r in rows}


def _earlier_edition(conn, doc, ext: Extraction, sample_hash, effective_at: datetime, groups: set[str]) -> UUID | None:
    """The report this one amends or finalises: same profile and collection day, and the same sample
    id, or >= 50% of its analyte groups on that day (ingestion.md 11)."""
    if ext.cls.collected_at is None:
        return None
    cands = conn.execute(
        text(
            "select d.id, d.flags->>'sample_hash' as sh, d.provider_fingerprint as fp from document d"
            " where d.profile_id = :p and d.id <> :d and d.status = 'active'"
            " and d.collected_at::date = :day and d.created_at <= (select created_at from document where id = :d)"
            " order by d.created_at desc, d.id"
        ),
        {"p": doc["profile_id"], "d": doc["id"], "day": effective_at.date()},
    ).all()
    for c in cands:
        if sample_hash and c.sh == sample_hash:
            return c.id
        theirs = set(_observations_of(conn, c.id))
        if theirs and groups and len(theirs & groups) / len(theirs | groups) >= 0.5:
            return c.id
    return None


def _duplicate(conn: Connection, obs: ObservationIn, document_id: UUID) -> dict | None:
    row = (
        conn.execute(
            text(
                "select * from observation where profile_id = :p and alias_group = :g and status in ('current',"
                " 'provisional') and effective_at::date = :day and document_id is distinct from :d"
                " order by recorded_at desc limit 1"
            ),
            {"p": obs.profile_id, "g": obs.alias_group, "day": obs.effective_at.date(), "d": document_id},
        )
        .mappings()
        .first()
    )
    return dict(row) if row else None


def _num(v) -> Decimal | None:
    if v is None:
        return None
    try:
        return Decimal(str(v))
    except InvalidOperation:
        return None


def _same_value(old: dict, new: ObservationIn) -> bool:
    a, b = _num(old.get("value_num")), _num(new.value_num)
    if a is not None and b is not None:
        return a == b and (old.get("unit_ucum") or "") == (new.unit_ucum or "")
    return fold(str(old.get("printed_value") or "")).strip() == fold(new.printed_value).strip()


def _alias_group(conn: Connection, row: Row, lang: str) -> str | None:
    try:
        return normalize.resolve_analyte(
            conn, row.text("name"), lang=lang, unit=None, specimen=row.specimen
        ).alias_group
    except NotImplementedError:
        return None


def _inside(inner, outer, pad: float) -> bool:
    return (
        inner[0] >= outer[0] - pad
        and inner[1] >= outer[1] - pad
        and inner[2] <= outer[2] + pad
        and inner[3] <= outer[3] + pad
    )


# ------------------------------------------------------------- page access ----


def _page_row(conn: Connection, document_id: UUID, page_no: int):
    return conn.execute(
        text(
            "select p.words_blob, d.blob_sha256, d.mime from document d left join document_page p"
            " on p.document_id = d.id and p.page_no = :n where d.id = :d"
        ),
        {"d": document_id, "n": page_no},
    ).first()


def page_words(conn: Connection, blobs: BlobStore, *, document_id: UUID, page_no: int) -> list[Word]:
    """The stored word layer of one page (hidden words included and marked). [] for an unknown page."""
    row = _page_row(conn, document_id, page_no)
    if row is None:
        return []
    if row.words_blob is not None:
        layer = json.loads(blobs.get_bytes(bytes(row.words_blob).hex()))
        words = layer["words"]
    elif row.mime == "application/pdf":
        model = read_pdf(blobs.get_bytes(bytes(row.blob_sha256).hex()))
        if not 1 <= page_no <= len(model.pages):
            return []
        words = [w.as_dict() for w in model.pages[page_no - 1].words]
    else:
        return []
    return [
        Word(
            w["text"],
            (w["x0"], w["top"], w["x1"], w["bottom"]),
            w.get("font", ""),
            w.get("size", 0.0),
            w.get("bold", False),
            w.get("hidden"),
        )
        for w in words
    ]


def render_page_png(blobs: BlobStore, *, blob_sha256: str, page_no: int, scale: float = 2.0) -> bytes:
    import pypdfium2 as pdfium

    pdf = pdfium.PdfDocument(blobs.get_bytes(blob_sha256))
    try:
        if not 1 <= page_no <= len(pdf):
            raise IndexError(f"page {page_no} out of range")
        image = pdf[page_no - 1].render(scale=max(0.5, min(scale, 4.0))).to_pil()
        buf = io.BytesIO()
        image.save(buf, format="PNG")
        return buf.getvalue()
    finally:
        pdf.close()


def _num_key(s: str) -> str | None:
    """'2,41' / '2.41' / '250.000' -> a separator-free key; None when it is not a number."""
    s = s.strip().lstrip("<>≤≥=").strip()
    if not re.fullmatch(r"[-+]?\d+(?:[.,' ]\d+)*", s):
        return None
    return re.sub(r"[.,' ]", "", s).lstrip("0") or "0"


def ground_value(
    conn: Connection, blobs: BlobStore, *, document_id: UUID, page_no: int, name: str, value: str
) -> tuple[float, float, float, float] | None:
    """Find a printed value on the page word layer (decimal-separator tolerant). Used by `propose`.

    Returns the bbox of the value word, widened to the name words when they sit on the same line;
    None when the value is not on the page. Hidden words never ground a value."""
    words = [w for w in page_words(conn, blobs, document_id=document_id, page_no=page_no) if not w.hidden]
    want = _num_key(value)
    folded_value = fold(value).strip()
    hits = [
        w
        for w in words
        if (want is not None and _num_key(w.text) == want) or (want is None and fold(w.text).strip() == folded_value)
    ]
    if not hits:
        return None
    name_toks = {t for t in re.findall(r"[a-z0-9]+", fold(name)) if len(t) > 1}

    def same_line(a: Word, b: Word) -> bool:
        return min(a.bbox[3], b.bbox[3]) - max(a.bbox[1], b.bbox[1]) > 0.5 * min(
            a.bbox[3] - a.bbox[1], b.bbox[3] - b.bbox[1]
        )

    best, best_score = hits[0], -1
    best_names: list[Word] = []
    for h in hits:
        on_line = [
            w
            for w in words
            if same_line(w, h) and w.bbox[2] <= h.bbox[0] and set(re.findall(r"[a-z0-9]+", fold(w.text))) & name_toks
        ]
        score = len({t for w in on_line for t in re.findall(r"[a-z0-9]+", fold(w.text))} & name_toks)
        if score > best_score:
            best, best_score, best_names = h, score, on_line
    boxes = [best.bbox, *(w.bbox for w in best_names)]
    return (min(b[0] for b in boxes), min(b[1] for b in boxes), max(b[2] for b in boxes), max(b[3] for b in boxes))
