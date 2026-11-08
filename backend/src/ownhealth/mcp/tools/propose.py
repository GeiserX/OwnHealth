"""propose (mcp.md 5.8, 9.3): the only write tool. The agent proposes, OwnHealth checks, a person
confirms in the review queue. Nothing here writes a fact: every call creates (or, for the same
payload, returns) one `agent_proposal` review item. Rows that fail a check are queued and marked,
never dropped, and an agent proposal never auto-accepts.

The accept handler at the bottom runs only when a person accepts the item in the review queue.
"""

from __future__ import annotations

import hashlib
import logging
import re
from datetime import UTC, date, datetime, timedelta
from typing import Literal
from uuid import UUID

from pydantic import BaseModel, Field, ValidationError, field_validator
from sqlalchemy import Connection, text

from ownhealth import normalize, state
from ownhealth.core.observations import ObservationIn, commit_observation
from ownhealth.core.provenance import SrcIn, ensure_src, resolve_src
from ownhealth.core.records import MedicationIn, add_record, get_record, supersede_record
from ownhealth.core.review import ReviewError, ReviewItemIn, create_item, register_accept_handler
from ownhealth.mcp import gate as gate_mod
from ownhealth.mcp import limits
from ownhealth.mcp.envelope import ToolError, dumps, jsonable, untrusted
from ownhealth.mcp.tools import Call, Result, tool

log = logging.getLogger("ownhealth.mcp")
LIMITATION = "Nothing is saved until a person confirms it in the review queue."
SRC_RE = re.compile(r"^src_[a-z0-9_]+$")
DOC_RE = re.compile(r"^doc_(?:\d{4}-\d{2}-\d{2}|undated)_([0-9a-f]{8})$")

SCHEMA = {
    "type": "object",
    "properties": {
        "profile": {"type": "string"},
        "kind": {"enum": ["observations", "note", "finding_status", "medication"]},
        "payload": {"type": "object"},
        "based_on": {
            "type": "array",
            "items": {"type": "string", "maxLength": 80},
            "maxItems": 200,
            "description": "src or doc ids the agent read",
        },
    },
    "required": ["kind", "payload"],
    "additionalProperties": False,
}


class ObsRow(BaseModel):
    name: str = Field(min_length=1, max_length=200)
    value: str | int | float
    unit: str | None = Field(default=None, max_length=40)
    range: str | None = Field(default=None, max_length=80)
    date: date
    page: int = Field(ge=1, le=10_000)
    bbox: tuple[float, float, float, float] | None = None


class ObservationsPayload(BaseModel):
    doc_id: str = Field(max_length=80)
    rows: list[ObsRow] = Field(min_length=1)


class NotePayload(BaseModel):
    text: str = Field(min_length=1, max_length=4000)
    attach_to: str | None = None
    lang: Literal["es", "en"] | None = None

    @field_validator("attach_to")
    @classmethod
    def _target(cls, v):
        if v is not None and v != "status" and not SRC_RE.match(v):
            raise ValueError("attach_to is a src id or 'status'")
        return v


class FindingStatusPayload(BaseModel):
    finding_id: str = Field(pattern=r"^f_[0-9a-f]{4,16}$")
    verification: Literal["confirmed", "provisional"] | None = None
    lifecycle: Literal["retracted"] | None = None
    reason: str = Field(min_length=1, max_length=500)
    evidence_src: str | None = Field(default=None, pattern=r"^src_[a-z0-9_]+$")


class MedicationPayload(BaseModel):
    action: Literal["start", "stop", "change"]
    name: str = Field(min_length=1, max_length=200)
    dose: str | None = Field(default=None, max_length=100)
    schedule: str | None = Field(default=None, max_length=100)
    date: date
    reason: str | None = Field(default=None, max_length=500)


MODELS = {
    "observations": ObservationsPayload,
    "note": NotePayload,
    "finding_status": FindingStatusPayload,
    "medication": MedicationPayload,
}


def check(name: str, ok: bool, reason: str | None = None, detail: str | None = None) -> dict:
    return {"name": name, "ok": ok, "reason": None if ok else reason, "detail": detail}


def find_document(call: Call, doc_id: str) -> dict | None:
    m = DOC_RE.match(doc_id)
    try:
        uid = UUID(doc_id) if not m else None
    except ValueError:
        return None
    if m is None and uid is None:
        return None
    where = "id = :id" if uid else "replace(id::text, '-', '') like :hex"
    row = (
        call.conn.execute(
            text(
                "select id, pages, collected_at, reported_at from document where profile_id = :p and "
                f"{where} order by created_at limit 1"
            ),
            {"p": call.profile_id, "id": uid, "hex": (m.group(1) + "%") if m else None},
        )
        .mappings()
        .first()
    )
    return dict(row) if row else None


# ------------------------------------------------------------------- kinds ----


def _observations(call: Call, p: ObservationsPayload) -> tuple[dict, list[str], list[str]]:
    if len(p.rows) > limits.MAX_PROPOSAL_ROWS:
        raise ToolError("propose_cap", f"At most {limits.MAX_PROPOSAL_ROWS} rows per call.", "Split the proposal.")
    doc = find_document(call, p.doc_id)
    rows_out, stored, reasons = [], [], []
    for row in p.rows:
        checks: list[dict] = []
        effective = datetime.combine(row.date, datetime.min.time(), UTC)
        observation = None
        try:
            norm = normalize.normalize_row(
                call.conn,
                profile_id=call.profile_id,
                printed_name=row.name,
                printed_value=str(row.value),
                printed_unit=row.unit,
                printed_range=row.range,
                printed_flag=None,
                effective_at=effective,
                lang=call.lang,
            )
            observation = norm.observation.model_copy(update={"document_id": doc["id"] if doc else None})
            checks += [check(c.name, c.ok, c.reason, c.detail) for c in norm.checks]
        except NotImplementedError:
            checks.append(check("normalize", False, "unmapped", "the normalizer is not available"))
        except Exception:  # noqa: BLE001  a row the normalizer cannot read is a failed check, not a crash
            log.warning("propose: normalizer failed on one row", exc_info=True)
            checks.append(check("normalize", False, "unmapped", "the value could not be read"))
        bbox = None
        if doc is None:
            checks.append(check("grounding", False, "not_grounded", "document not found on this profile"))
        elif doc.get("pages") and row.page > doc["pages"]:
            checks.append(check("grounding", False, "not_grounded", "page outside the document"))
        else:
            try:
                from ownhealth.ingest.pdf import ground_value

                bbox = ground_value(
                    call.conn, call.blobs, document_id=doc["id"], page_no=row.page, name=row.name, value=str(row.value)
                )
                checks.append(
                    check(
                        "grounding",
                        bbox is not None,
                        "not_grounded",
                        None if bbox else "value not found on the page word layer",
                    )
                )
            except NotImplementedError:
                checks.append(check("grounding", False, "not_grounded", "the page word layer is not available"))
            except Exception:  # noqa: BLE001
                log.warning("propose: grounding failed on one row", exc_info=True)
                checks.append(check("grounding", False, "not_grounded", "the page could not be read"))
        if doc is not None:
            days = [d.date() for d in (doc.get("collected_at"), doc.get("reported_at")) if d]
            if days:
                ok = min(days) - timedelta(days=1) <= row.date <= max(days) + timedelta(days=1)
                checks.append(check("date_in_document", ok, "date_invalid", None if ok else "date outside the report"))
        failed = [c for c in checks if not c["ok"]]
        reasons += [c["reason"] for c in failed if c["reason"]]
        grounded = bbox is not None
        rows_out.append(
            {
                "name": untrusted(row.name, "agent_note"),
                "value": row.value if not isinstance(row.value, str) else untrusted(row.value, "agent_note"),
                "unit": untrusted(row.unit, "agent_note"),
                "loinc": observation.loinc if observation else None,
                "grounded": grounded,
                "bbox": list(bbox) if bbox else None,
                "check": failed[0]["reason"] if failed else "ok",
                "failed": [c["name"] for c in failed] or None,
            }
        )
        stored.append(
            {
                "input": row.model_dump(mode="json"),
                "observation": observation.model_dump(mode="json") if observation else None,
                "checks": checks,
                "grounded": grounded,
                "bbox": list(bbox) if bbox else None,
            }
        )
    return {"rows": rows_out}, {"rows": stored, "document_id": str(doc["id"]) if doc else None}, reasons


def _note(call: Call, p: NotePayload) -> tuple[dict, dict, list[str]]:
    checks = []
    if p.attach_to and p.attach_to != "status":
        res = resolve_src(call.conn, p.attach_to)
        visible = (
            res is not None
            and res["profile_id"] == call.profile_id
            and not gate_mod.hidden_reasons(call.conn, call.perm, call.gate(), [p.attach_to])
        )
        checks.append(check("attach_to", visible, "unmapped", None if visible else "src not found on this profile"))
    reasons = [c["reason"] for c in checks if not c["ok"]]
    return {"checks": checks}, {"checks": checks}, reasons


def _finding_status(call: Call, p: FindingStatusPayload) -> tuple[dict, dict, list[str]]:
    if not call.perm.has("read:genome"):
        raise ToolError(
            "scope_missing", "Proposals about genome findings need read:genome.", "Use a token with read:genome."
        )
    if p.verification is None and p.lifecycle is None:
        raise ToolError("bad_input", "Name a verification or a lifecycle change.", "Set verification or lifecycle.")
    row = (
        call.conn.execute(
            text(
                "select id, src_id, status, verification_status from genome_finding where profile_id = :p"
                " and fact_id like :f and status = 'current' order by recorded_at desc limit 1"
            ),
            {"p": call.profile_id, "f": p.finding_id + "%"},
        )
        .mappings()
        .first()
    )
    if row is not None and gate_mod.hidden_reasons(call.conn, call.perm, call.gate(), [row["src_id"]]):
        row = None  # a finding the caller cannot see does not exist for it
    checks = [check("finding_exists", row is not None, "unmapped", None if row else "no such current finding")]
    if row is not None and p.verification is not None:
        ok = p.verification != row["verification_status"]
        checks.append(check("transition", ok, "conflict", None if ok else f"already {p.verification}"))
    reasons = [c["reason"] for c in checks if not c["ok"]]
    stored = {"checks": checks, "finding_row": str(row["id"]) if row else None}
    return {"checks": checks}, stored, reasons


def _medication(call: Call, p: MedicationPayload) -> tuple[dict, dict, list[str], list[str]]:
    checks, limitations = [], []
    try:
        drug = state.resolve_drug(call.conn, p.name)
    except NotImplementedError:
        drug = None
    if drug is None:
        limitations.append("The drug name did not resolve; it is kept as free text.")
    match_id = None
    if p.action in ("stop", "change"):
        if call.perm.has("read:records"):
            from ownhealth.core.records import active_medications

            needle = p.name.casefold()
            for m in active_medications(call.conn, call.profile_id, on=p.date):
                if needle in m["name"].casefold() or m["name"].casefold() in needle:
                    match_id = m["id"]
                    break
            checks.append(
                check(
                    "medication_found",
                    match_id is not None,
                    "unmapped",
                    None if match_id else "no current medication with that name",
                )
            )
        else:
            checks.append(check("medication_found", False, "unmapped", "not checked: needs read:records"))
    reasons = [c["reason"] for c in checks if not c["ok"]]
    stored = {"checks": checks, "drug": drug, "match_id": str(match_id) if match_id else None}
    return {"checks": checks, "drug": drug}, stored, reasons, limitations


# ------------------------------------------------------------------- tool ----


def context_src_ids(call: Call, based_on: list[str]) -> list[str]:
    """based_on plus every src id this token was shown in the last hour (security-privacy.md 4.2 #4)."""
    recent = call.conn.execute(
        text(
            "select distinct unnest(src_ids) from audit_event where token_id = :t and src_ids is not null"
            " and ts > now() - interval '1 hour'"
        ),
        {"t": call.principal.token_id},
    ).scalars()
    ids = [b for b in based_on if SRC_RE.match(b)] + sorted(set(recent))
    return list(dict.fromkeys(ids))[:500]


@tool(
    name="propose",
    title="Propose a change for review",
    description=(
        "The only way to write. Propose observations read from a document (with doc_id, page and value), a "
        "note, a genome finding status change, or a medication start, stop or change. OwnHealth checks each "
        "row (code, unit, plausibility, the value grounded on the page) and puts the proposal in the review "
        "queue; nothing is saved until a person confirms it. Rows that fail a check are still queued and marked. "
        "List in based_on the src or doc ids you read. Never propose a value that is not on the page."
    ),
    input_schema=SCHEMA,
    any_scope=("write:propose",),
    read_only=False,
)
def propose(call: Call) -> Result:
    if not any(call.perm.has(s) for s in gate_mod.READ_SCOPES):
        raise ToolError("scope_missing", "Proposing needs read access to the profile.", "Add a read scope.")
    kind = call.args["kind"]
    based_on = list(call.args.get("based_on") or [])
    try:
        payload = MODELS[kind].model_validate(call.args["payload"])
    except ValidationError as exc:
        first = exc.errors()[0]
        where = ".".join(str(x) for x in first["loc"]) or "payload"
        raise ToolError(
            "bad_input", f"Invalid payload.{where}: {first['msg']}", "Check the payload for this kind."
        ) from exc
    if kind == "note" and payload.lang is None:
        payload = payload.model_copy(update={"lang": call.lang})

    phash = hashlib.sha256(
        dumps({"p": str(call.profile_id), "k": kind, "x": payload.model_dump(mode="json"), "b": based_on}).encode()
    ).hexdigest()
    existing = (
        call.conn.execute(
            text(
                "select id, payload from review_item where kind = 'agent_proposal' and proposed_by_token = :t"
                " and payload->>'payload_hash' = :h and status = 'open' limit 1"
            ),
            {"t": call.principal.token_id, "h": phash},
        )
        .mappings()
        .first()
    )
    if existing is None and call.usage.proposals >= int(call.limits["proposals_per_day"]):
        raise ToolError("propose_cap", "This token reached its proposals for today.", "It resets at 00:00.")

    limitations = [LIMITATION]
    if kind == "observations":
        shown, stored, reasons = _observations(call, payload)
    elif kind == "note":
        shown, stored, reasons = _note(call, payload)
    elif kind == "finding_status":
        shown, stored, reasons = _finding_status(call, payload)
    else:
        shown, stored, reasons, more = _medication(call, payload)
        limitations += more

    counters = {}
    if existing is not None:
        item_id = existing["id"]
    else:
        docs = []
        for b in based_on:
            m = DOC_RE.match(b)
            if m:
                d = find_document(call, b)
                if d:
                    docs.append(d["id"])
        if kind == "observations" and stored.get("document_id"):
            docs.append(UUID(stored["document_id"]))
        rows = stored.get("rows") or []
        item_id = create_item(
            call.conn,
            ReviewItemIn(
                kind="agent_proposal",
                household_id=call.perm.profile.household_id,
                profile_id=call.profile_id,
                payload=jsonable(
                    {
                        "kind": kind,
                        "payload": payload.model_dump(mode="json"),
                        **stored,
                        "token_label": call.principal.token_label,
                        "based_on": based_on,
                        "payload_hash": phash,
                    }
                ),
                reasons=sorted(set(reasons)),
                document_id=docs[-1] if kind == "observations" and docs else None,
                row_conf=0.0 if any(not r["grounded"] for r in rows) else None,
                proposed_by_token=call.principal.token_id,
                context_documents=list(dict.fromkeys(docs)) or None,
                context_src_ids=context_src_ids(call, based_on) or None,
            ),
        )
        counters = {"proposals": 1}
    handle = "prop_" + str(item_id).replace("-", "")[:8]
    data = {
        "proposal_id": handle,
        "status": "queued_for_review",
        "kind": kind,
        **shown,
        "review_url": f"{call.settings.base_url.rstrip('/')}/import-review?item={item_id}",
    }
    if existing is not None:
        data["duplicate_of_open_proposal"] = True
    return Result(data=data, budget=1000, limitations=limitations, counters=counters, prune=False)


# ---------------------------------------------------------- accept handler ----


def _src(item: dict, user_id: UUID, **kw) -> SrcIn:
    return SrcIn(
        kind="agent_proposal",
        profile_id=item["profile_id"],
        actor_kind="user",
        actor_id=user_id,
        label="agent proposal",
        **kw,
    )


@register_accept_handler("agent_proposal")
def accept_proposal(conn: Connection, item: dict, user_id: UUID, edits: dict | None) -> list[UUID]:
    """A person accepted the proposal: write what it proposed, as a person-accepted fact."""
    p = item["payload"]
    kind = p.get("kind")
    body = {**(p.get("payload") or {}), **((edits or {}).get("payload") or {})}
    if kind == "note":
        sid = ensure_src(conn, _src(item, user_id))
        nid = conn.execute(
            text(
                "insert into note (profile_id, kind, author_kind, lang, attach_to, body, effective_at, src_id)"
                " values (:p, :k, 'agent', :l, :a, :b, now(), :s) returning id"
            ),
            {
                "p": item["profile_id"],
                "k": "priorities" if body.get("attach_to") in (None, "status") else "journal",
                "l": body.get("lang"),
                "a": body.get("attach_to"),
                "b": body["text"],
                "s": sid,
            },
        ).scalar_one()
        return [nid]
    if kind == "observations":
        keep = set((edits or {}).get("rows") or range(len(p.get("rows") or [])))
        created = []
        for i, row in enumerate(p.get("rows") or []):
            if i not in keep or not row.get("observation"):
                continue
            obs = ObservationIn.model_validate(row["observation"])
            if row.get("bbox") and obs.document_id:
                src = SrcIn(
                    kind="document_region",
                    profile_id=item["profile_id"],
                    document_id=obs.document_id,
                    page_no=row["input"]["page"],
                    bbox=tuple(row["bbox"]),
                    actor_kind="user",
                    actor_id=user_id,
                )
            else:
                src = _src(item, user_id)
            created.append(commit_observation(conn, obs, src=src, accepted_by="person", reviewed_by=user_id).id)
        if not created:
            raise ReviewError("no row of this proposal could be normalized; reject it or enter the values by hand")
        return created
    if kind == "medication":
        when = date.fromisoformat(body["date"])
        if body["action"] == "start":
            rec = MedicationIn(
                profile_id=item["profile_id"],
                name=body["name"],
                dose_text=body.get("dose"),
                started=when,
                med_status="active",
                reason_text=body.get("reason"),
            )
            return [add_record(conn, rec, src=_src(item, user_id), accepted_by="person").id]
        if not p.get("match_id"):
            raise ReviewError("no current medication matched this proposal; edit the medication on the records page")
        old = get_record(conn, "medication", UUID(p["match_id"]))
        fields = {
            k: old[k]
            for k in (
                "profile_id",
                "kind",
                "name",
                "ingredient",
                "strength",
                "dose_text",
                "code_system",
                "code",
                "started",
                "stopped",
                "med_status",
                "reason_text",
            )
        }
        if body["action"] == "stop":
            fields.update(stopped=when, med_status="stopped")
        else:
            fields.update(dose_text=body.get("dose") or fields["dose_text"])
        new = MedicationIn(**fields)
        return [
            supersede_record(
                conn,
                old["id"],
                new,
                src=_src(item, user_id),
                accepted_by="person",
                reason=f"agent proposal {body['action']}",
            ).id
        ]
    raise ReviewError("genome finding changes are applied on the genome page")
