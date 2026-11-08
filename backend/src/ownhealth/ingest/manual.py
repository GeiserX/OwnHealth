"""Lane C: values a person types in. Provenance kind `manual`; accepted_by `person`, never the
auto-accept rule. A correction is a new version that supersedes the old one; nothing is overwritten."""

from __future__ import annotations

import re
from decimal import Decimal, InvalidOperation
from uuid import UUID

from sqlalchemy import Connection, text

from ownhealth import normalize
from ownhealth.core.observations import Committed, ObservationIn, commit_observation, supersede_observation
from ownhealth.core.provenance import SrcIn
from ownhealth.ingest.fhir.mapping import loinc_valid, name_tokens

_NUMBER = re.compile(r"^\s*(<=|>=|<|>)?\s*(-?\d+(?:[.,]\d+)?)\s*$")


def _fallback(profile_id, name, value, unit, effective_at, loinc) -> ObservationIn:
    """Used until lane A's normalizer lands: the value parsed plainly, the name as the series key."""
    m = _NUMBER.match(value)
    num = cmp = None
    if m:
        try:
            num, cmp = Decimal(m.group(2).replace(",", ".")), m.group(1)
        except InvalidOperation:
            num = None
    group = f"loinc:{loinc}" if loinc_valid(loinc) else "name:" + "_".join(sorted(name_tokens(name)))
    return ObservationIn(
        profile_id=profile_id,
        printed_name=name,
        printed_value=value,
        printed_unit=unit,
        value_num=num,
        value_cmp=cmp,
        value_text=None if num is not None else value,
        loinc=loinc,
        alias_group=group,
        derivation_version=normalize.DERIVATION_VERSION,
        effective_at=effective_at,
    )


def add_manual_observation(
    conn: Connection,
    *,
    profile_id: UUID,
    user_id: UUID,
    name: str,
    value: str,
    unit: str | None,
    effective_at,
    note: str | None = None,
    category: str | None = None,
    loinc: str | None = None,
    corrects: UUID | None = None,
    reason: str | None = None,
) -> Committed:
    """Commit one typed value. `corrects` names the observation this one replaces (with `reason`)."""
    if effective_at.tzinfo is None:
        raise ValueError("effective_at must be timezone-aware")
    lang = conn.execute(text("select locale from app_user where id = :u"), {"u": user_id}).scalar() or "es"
    try:
        obs = normalize.normalize_row(
            conn,
            profile_id=profile_id,
            printed_name=name,
            printed_value=value,
            printed_unit=unit,
            printed_range=None,
            printed_flag=None,
            effective_at=effective_at,
            lang=lang,
        ).observation
    except NotImplementedError:
        obs = _fallback(profile_id, name, value, unit, effective_at, loinc)
    update: dict = {"source_kind": "self_reported", "document_id": None, "candidate_row_id": None}
    if category:
        update["category"] = category
    if loinc:
        update["loinc"] = loinc
    obs = obs.model_copy(update=update)
    src = SrcIn(kind="manual", profile_id=profile_id, actor_kind="user", actor_id=user_id, label="Entered by hand")
    if corrects is not None:
        committed = supersede_observation(
            conn, corrects, obs, src=src, accepted_by="person", reason=reason or "corrected", reviewed_by=user_id
        )
    else:
        committed = commit_observation(conn, obs, src=src, accepted_by="person", reviewed_by=user_id)
    if note and note.strip():
        conn.execute(
            text(
                "insert into note (profile_id, kind, author_kind, author_id, lang, attach_to, body, effective_at,"
                " src_id) values (:p, 'journal', 'person', :u, :l, :s, :b, :t, :s)"
            ),
            {
                "p": profile_id,
                "u": user_id,
                "l": lang,
                "s": committed.src_id,
                "b": note.strip()[:4000],
                "t": effective_at,
            },
        )
    return committed
