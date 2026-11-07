"""Review queue: candidate rows, agent proposals and anything else a person must confirm.

Importers call `submit_candidate` (document rows) or `route_fact` (structured imports). Both apply
the auto-accept rule; a row that passes is committed with accepted_by='auto', anything else becomes
an open review item with its reason codes and check results. Nothing is dropped.

A person then accepts (optionally with edits), rejects with a reason, reopens, or bulk-accepts the
rows of one document that passed every check. Kinds other than candidate rows register an accept
handler with `register_accept_handler` (lane F for agent proposals, lane C for FHIR, ...).
"""

from __future__ import annotations

import json
from collections.abc import Callable
from dataclasses import dataclass
from typing import Literal
from uuid import UUID

from pydantic import BaseModel, Field
from sqlalchemy import Connection, text

from ownhealth.core.acceptance import DEFAULT_THRESHOLD, Check, Identity, SourceAllowance, auto_accept
from ownhealth.core.observations import ObservationIn, commit_observation
from ownhealth.core.provenance import SrcIn

ReviewKind = Literal[
    "candidate_row",
    "agent_proposal",
    "fhir_unmapped",
    "document_identity",
    "amended_report",
    "rederive_diff",
    "genome_reinterpretation",
    "job_failed",
    "restore_check_failed",
]


class ReviewError(ValueError):
    pass


class ReviewItemIn(BaseModel):
    kind: ReviewKind
    household_id: UUID | None
    profile_id: UUID | None = None
    payload: dict
    reasons: list[str] = Field(default_factory=list)
    document_id: UUID | None = None
    candidate_row_id: UUID | None = None
    observation_id: UUID | None = None
    ref_table: str | None = None
    ref_id: UUID | None = None
    row_conf: float | None = None
    proposed_by_token: UUID | None = None
    context_documents: list[UUID] | None = None
    context_src_ids: list[str] | None = None


class CandidateIn(BaseModel):
    """One row an extractor read from a page, already normalized into an ObservationIn."""

    page_no: int
    bbox: tuple[float, float, float, float]
    cells: dict  # name, value, unit, ref_range, flag: {text, bbox, conf}
    row_conf: float
    observation: ObservationIn
    checks: list[Check] = Field(default_factory=list)


@dataclass(frozen=True)
class RouteResult:
    state: Literal["auto_accepted", "in_review"]
    observation_id: UUID | None
    src_id: str | None
    review_item_id: UUID | None
    reasons: list[str]


AcceptHandler = Callable[[Connection, dict, UUID, dict | None], list[UUID]]
_ACCEPT_HANDLERS: dict[str, AcceptHandler] = {}


def register_accept_handler(kind: str) -> Callable[[AcceptHandler], AcceptHandler]:
    """Decorator: `fn(conn, item: dict, user_id, edits) -> created row ids` for one review kind."""

    def deco(fn: AcceptHandler) -> AcceptHandler:
        _ACCEPT_HANDLERS[kind] = fn
        return fn

    return deco


# ------------------------------------------------------------- extraction runs ----


def create_extraction_run(
    conn: Connection,
    document_id: UUID,
    *,
    extractor: str,
    extractor_version: str,
    config_hash: bytes,
    engine: str | None = None,
    engine_version: str | None = None,
) -> UUID:
    return conn.execute(
        text(
            "insert into extraction_run (document_id, engine, engine_version, extractor, extractor_version,"
            " config_hash) values (:d, :e, :ev, :x, :xv, :c) returning id"
        ),
        {
            "d": document_id,
            "e": engine,
            "ev": engine_version,
            "x": extractor,
            "xv": extractor_version,
            "c": config_hash,
        },
    ).scalar_one()


def finish_extraction_run(
    conn: Connection, run_id: UUID, status: Literal["done", "failed"] = "done", error: str | None = None
) -> None:
    conn.execute(
        text("update extraction_run set status = :s, error = :e, finished_at = now() where id = :id"),
        {"s": status, "e": error, "id": run_id},
    )


# -------------------------------------------------------------------- routing ----


def create_item(conn: Connection, item: ReviewItemIn) -> UUID:
    data = item.model_dump()
    data["payload"] = json.dumps(data["payload"], default=str)
    return conn.execute(
        text(
            "insert into review_item (kind, household_id, profile_id, payload, reasons, document_id,"
            " candidate_row_id, observation_id, ref_table, ref_id, row_conf, proposed_by_token,"
            " context_documents, context_src_ids) values (:kind, :household_id, :profile_id,"
            " cast(:payload as jsonb), :reasons, :document_id, :candidate_row_id, :observation_id, :ref_table,"
            " :ref_id, :row_conf, :proposed_by_token, :context_documents, :context_src_ids) returning id"
        ),
        data,
    ).scalar_one()


def _household_of_profile(conn: Connection, profile_id: UUID) -> UUID:
    return conn.execute(text("select household_id from profile where id = :p"), {"p": profile_id}).scalar_one()


def route_fact(
    conn: Connection,
    *,
    observation: ObservationIn,
    src: SrcIn,
    checks: list[Check],
    row_conf: float,
    allowance: SourceAllowance,
    identity: Identity,
    threshold: float = DEFAULT_THRESHOLD,
    provisional: bool = False,
    candidate_row_id: UUID | None = None,
    extra_payload: dict | None = None,
) -> RouteResult:
    """Apply the auto-accept rule to one fact: commit it, or queue it for a person."""
    decision = auto_accept(row_conf, checks, allowance, identity, threshold)
    if decision.auto:
        committed = commit_observation(
            conn, observation, src=src, accepted_by="auto", status="provisional" if provisional else "current"
        )
        return RouteResult("auto_accepted", committed.id, committed.src_id, None, [])
    payload = {
        "observation": observation.model_dump(mode="json"),
        "src": src.model_dump(mode="json"),
        "checks": [c.model_dump() for c in checks],
        "allowance": allowance,
        "identity": identity,
        "row_conf": row_conf,
        "why_not": decision.why_not,
        "provisional": provisional,
        **(extra_payload or {}),
    }
    item_id = create_item(
        conn,
        ReviewItemIn(
            kind="candidate_row",
            household_id=_household_of_profile(conn, observation.profile_id),
            profile_id=observation.profile_id,
            payload=payload,
            reasons=decision.reasons,
            document_id=observation.document_id,
            candidate_row_id=candidate_row_id,
            row_conf=row_conf,
        ),
    )
    return RouteResult("in_review", None, None, item_id, decision.reasons)


def submit_candidate(
    conn: Connection,
    run_id: UUID,
    cand: CandidateIn,
    *,
    allowance: SourceAllowance,
    identity: Identity,
    threshold: float = DEFAULT_THRESHOLD,
    provisional: bool = False,
) -> RouteResult:
    """Store a candidate row from a document and route it (auto-accept or review)."""
    run = conn.execute(text("select document_id from extraction_run where id = :r"), {"r": run_id}).first()
    if run is None:
        raise ReviewError(f"unknown extraction run {run_id}")
    reasons = sorted({c.reason for c in cand.checks if not c.ok and c.reason})
    row_id = conn.execute(
        text(
            "insert into candidate_row (extraction_run_id, page_no, bbox, cells, row_conf, reasons)"
            " values (:r, :p, :b, cast(:c as jsonb), :rc, :rs) returning id"
        ),
        {
            "r": run_id,
            "p": cand.page_no,
            "b": list(cand.bbox),
            "c": json.dumps(cand.cells),
            "rc": cand.row_conf,
            "rs": reasons,
        },
    ).scalar_one()
    obs = cand.observation.model_copy(update={"document_id": run.document_id, "candidate_row_id": row_id})
    src = SrcIn(
        kind="document_region",
        profile_id=obs.profile_id,
        document_id=run.document_id,
        page_no=cand.page_no,
        bbox=cand.bbox,
        extraction_run_id=run_id,
        candidate_row_id=row_id,
    )
    result = route_fact(
        conn,
        observation=obs,
        src=src,
        checks=cand.checks,
        row_conf=cand.row_conf,
        allowance=allowance,
        identity=identity,
        threshold=threshold,
        provisional=provisional,
        candidate_row_id=row_id,
        extra_payload={"cells": cand.cells, "page_no": cand.page_no, "bbox": list(cand.bbox)},
    )
    conn.execute(
        text("update candidate_row set state = :s, observation_id = :o where id = :id"),
        {"s": result.state, "o": result.observation_id, "id": row_id},
    )
    return result


# ---------------------------------------------------------------- decisions ----


def get_item(conn: Connection, item_id: UUID) -> dict | None:
    row = conn.execute(text("select * from review_item where id = :id"), {"id": item_id}).mappings().first()
    return dict(row) if row else None


def _lock_item(conn: Connection, item_id: UUID, want: str) -> dict:
    row = conn.execute(text("select * from review_item where id = :id for update"), {"id": item_id}).mappings().first()
    if row is None:
        raise ReviewError(f"review item {item_id} not found")
    if row["status"] != want:
        raise ReviewError(f"review item {item_id} is {row['status']}, not {want}")
    return dict(row)


def _accept_candidate(conn: Connection, item: dict, user_id: UUID, edits: dict | None) -> list[UUID]:
    payload = item["payload"]
    obs_data = {**payload["observation"], **(edits or {})}
    obs = ObservationIn.model_validate(obs_data)
    src = SrcIn.model_validate({**payload["src"], "actor_kind": "user", "actor_id": str(user_id)})
    committed = commit_observation(
        conn,
        obs,
        src=src,
        accepted_by="person",
        reviewed_by=user_id,
        status="provisional" if payload.get("provisional") else "current",
    )
    if item["candidate_row_id"]:
        conn.execute(
            text("update candidate_row set state = 'confirmed', observation_id = :o where id = :id"),
            {"o": committed.id, "id": item["candidate_row_id"]},
        )
    return [committed.id]


_ACCEPT_HANDLERS["candidate_row"] = _accept_candidate


def accept(conn: Connection, item_id: UUID, user_id: UUID, edits: dict | None = None) -> list[UUID]:
    """Accept an open item (with optional edits). Returns the ids of the rows it created."""
    item = _lock_item(conn, item_id, "open")
    handler = _ACCEPT_HANDLERS.get(item["kind"])
    if handler is None:
        raise ReviewError(f"items of kind {item['kind']} cannot be accepted; dismiss them instead")
    created = handler(conn, item, user_id, edits)
    conn.execute(
        text(
            "update review_item set status = 'accepted', decided_by = :u, decided_at = now(), result_ids = :r"
            " where id = :id"
        ),
        {"u": user_id, "r": created, "id": item_id},
    )
    return created


def reject(conn: Connection, item_id: UUID, user_id: UUID, reason: str) -> None:
    """A rejected row never becomes a fact; the item keeps what was read, so it can be reopened."""
    if not reason or not reason.strip():
        raise ReviewError("a rejection needs a reason")
    item = _lock_item(conn, item_id, "open")
    conn.execute(
        text(
            "update review_item set status = 'rejected', decided_by = :u, decided_at = now(),"
            " decision_reason = :r where id = :id"
        ),
        {"u": user_id, "r": reason.strip(), "id": item_id},
    )
    if item["candidate_row_id"]:
        conn.execute(
            text("update candidate_row set state = 'rejected' where id = :id"), {"id": item["candidate_row_id"]}
        )


def reopen(conn: Connection, item_id: UUID, user_id: UUID) -> None:
    item = _lock_item(conn, item_id, "rejected")
    conn.execute(
        text(
            "update review_item set status = 'open', decided_by = null, decided_at = null,"
            " decision_reason = null where id = :id"
        ),
        {"id": item_id},
    )
    if item["candidate_row_id"]:
        conn.execute(
            text("update candidate_row set state = 'in_review' where id = :id"), {"id": item["candidate_row_id"]}
        )


def dismiss(conn: Connection, item_id: UUID, user_id: UUID, reason: str | None = None) -> None:
    """For informational items (job_failed, restore_check_failed, rederive_diff)."""
    _lock_item(conn, item_id, "open")
    conn.execute(
        text(
            "update review_item set status = 'dismissed', decided_by = :u, decided_at = now(),"
            " decision_reason = :r where id = :id"
        ),
        {"u": user_id, "r": reason, "id": item_id},
    )


def bulk_accept(conn: Connection, *, document_id: UUID, user_id: UUID, min_row_conf: float) -> list[UUID]:
    """Accept every open candidate row of a document that passed every check and has
    row_conf >= min_row_conf. Rows with any reason code stay open. Returns accepted item ids."""
    ids = (
        conn.execute(
            text(
                "select id from review_item where document_id = :d and kind = 'candidate_row' and status = 'open'"
                " and reasons = '{}' and coalesce(row_conf, 0) >= :m order by created_at, id"
            ),
            {"d": document_id, "m": min_row_conf},
        )
        .scalars()
        .all()
    )
    for item_id in ids:
        accept(conn, item_id, user_id)
    return list(ids)


def list_items(
    conn: Connection,
    *,
    household_id: UUID | None = None,
    profile_ids: list[UUID] | None = None,
    status: str | None = "open",
    kind: str | None = None,
    document_id: UUID | None = None,
    limit: int = 100,
    offset: int = 0,
) -> list[dict]:
    where = ["true"]
    params: dict = {"limit": limit, "offset": offset}
    if household_id is not None:
        where.append("household_id = :h")
        params["h"] = household_id
    if profile_ids is not None:
        where.append("(profile_id = any(:pids) or profile_id is null)")
        params["pids"] = profile_ids
    if status:
        where.append("status = :s")
        params["s"] = status
    if kind:
        where.append("kind = :k")
        params["k"] = kind
    if document_id:
        where.append("document_id = :d")
        params["d"] = document_id
    rows = conn.execute(
        text(
            f"select * from review_item where {' and '.join(where)} order by created_at, id limit :limit offset :offset"
        ),
        params,
    ).mappings()
    return [dict(r) for r in rows]


def count_open(conn: Connection, *, household_id: UUID, profile_ids: list[UUID] | None = None) -> int:
    params: dict = {"h": household_id}
    extra = ""
    if profile_ids is not None:
        extra = " and (profile_id = any(:pids) or profile_id is null)"
        params["pids"] = profile_ids
    return conn.execute(
        text(f"select count(*) from review_item where household_id = :h and status = 'open'{extra}"), params
    ).scalar_one()
