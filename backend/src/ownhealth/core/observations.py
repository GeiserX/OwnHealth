"""Observation commit API: insert, supersede, retract, confirm, query.

Every observation needs a source (`SrcIn` or an existing src id). The printed fields are the truth
layer and never change; the derived fields (loinc, unit_ucum, value_canonical, interpretation) come
from the normalizer (lane A) and carry `derivation_version`.
"""

from __future__ import annotations

import json
from dataclasses import dataclass
from datetime import datetime
from decimal import Decimal
from typing import Literal
from uuid import UUID

from pydantic import BaseModel, Field
from sqlalchemy import Connection, text

from ownhealth.core import fhir, lifecycle
from ownhealth.core.provenance import SrcIn, ensure_src

Category = Literal["laboratory", "vital-signs", "survey", "exam", "activity"]
SourceKind = Literal["clinical", "consumer", "device", "self_reported", "agent_written"]
AcceptedBy = Literal["auto", "person", "import"]
Sensitivity = Literal["mental_health", "reproductive", "sexual_health"]


class ObservationIn(BaseModel):
    profile_id: UUID
    category: Category = "laboratory"
    source_kind: SourceKind = "clinical"
    # truth layer
    printed_name: str
    printed_value: str
    printed_unit: str | None = None
    printed_ref_range: str | None = None
    printed_flag: str | None = None
    # parsed
    value_num: Decimal | None = None
    value_cmp: Literal["<", "<=", ">", ">="] | None = None
    value_text: str | None = None
    value_code: str | None = None
    method: str | None = None
    ref_low: Decimal | None = None
    ref_high: Decimal | None = None
    ref_applies: dict | None = None
    specimen: str | None = None
    # derived
    loinc: str | None = None
    alias_group: str
    unit_ucum: str | None = None
    value_canonical: Decimal | None = None
    unit_canonical: str | None = None
    interpretation: Literal["L", "N", "H", "LL", "HH", "A"] | None = None
    derivation_version: str
    # time and links
    effective_at: datetime
    document_id: UUID | None = None
    candidate_row_id: UUID | None = None
    report_key: str | None = None
    sensitivity: list[Sensitivity] = Field(default_factory=list)
    fhir: dict | None = None  # received resource for imports; built from columns otherwise


@dataclass(frozen=True)
class Committed:
    id: UUID
    src_id: str


_COLUMNS = [f for f in ObservationIn.model_fields if f != "fhir"]


def commit_observation(
    conn: Connection,
    obs: ObservationIn,
    *,
    src: SrcIn | str,
    accepted_by: AcceptedBy,
    status: Literal["current", "provisional"] = "current",
    reviewed_by: UUID | None = None,
    supersedes: UUID | None = None,
) -> Committed:
    """Insert one observation with its provenance. Use `supersede_observation` for corrections."""
    if obs.effective_at.tzinfo is None:
        raise ValueError("effective_at must be timezone-aware")
    if isinstance(src, SrcIn) and src.profile_id is None:
        src = src.model_copy(update={"profile_id": obs.profile_id})
    src_id = ensure_src(conn, src)
    data = obs.model_dump(exclude={"fhir"})
    resource = obs.fhir or fhir.observation({**data, "status": status})
    params = {
        **data,
        "ref_applies": json.dumps(data["ref_applies"]) if data["ref_applies"] is not None else None,
        "fhir": json.dumps(resource, default=str),
        "src_id": src_id,
        "accepted_by": accepted_by,
        "status": status,
        "reviewed_by": reviewed_by,
        "supersedes": supersedes,
    }
    cols = [*_COLUMNS, "src_id", "accepted_by", "status", "reviewed_by", "supersedes", "fhir"]
    values = []
    for c in cols:
        if c in ("fhir", "ref_applies"):
            values.append(f"cast(:{c} as jsonb)")
        else:
            values.append(f":{c}")
    reviewed_at = ", reviewed_at" if reviewed_by else ""
    reviewed_val = ", now()" if reviewed_by else ""
    oid = conn.execute(
        text(
            f"insert into observation ({', '.join(cols)}{reviewed_at}) values ({', '.join(values)}{reviewed_val})"
            " returning id"
        ),
        params,
    ).scalar_one()
    return Committed(id=oid, src_id=src_id)


def supersede_observation(
    conn: Connection,
    old_id: UUID,
    new: ObservationIn,
    *,
    src: SrcIn | str,
    accepted_by: AcceptedBy,
    reason: str,
    reviewed_by: UUID | None = None,
) -> Committed:
    """A correction or an amended report: the new row replaces the old, which becomes superseded."""
    old = lifecycle.lock_current(conn, "observation", old_id)
    if old["profile_id"] != new.profile_id:
        raise lifecycle.LifecycleError("a correction cannot move a fact to another profile")
    lifecycle.mark_superseded(conn, "observation", old_id, reason)
    return commit_observation(conn, new, src=src, accepted_by=accepted_by, reviewed_by=reviewed_by, supersedes=old_id)


def retract_observation(conn: Connection, obs_id: UUID, reason: str) -> None:
    lifecycle.retract(conn, "observation", obs_id, reason)


def confirm_observation(conn: Connection, obs_id: UUID) -> None:
    lifecycle.confirm_provisional(conn, "observation", obs_id)


def get_observation(conn: Connection, obs_id: UUID) -> dict | None:
    row = conn.execute(text("select * from observation where id = :id"), {"id": obs_id}).mappings().first()
    return dict(row) if row else None


def observation_history(conn: Connection, obs_id: UUID) -> list[dict]:
    return lifecycle.history(conn, "observation", obs_id)


def list_observations(
    conn: Connection,
    profile_id: UUID,
    *,
    alias_group: str | None = None,
    loinc: str | list[str] | None = None,
    category: str | None = None,
    since: datetime | None = None,
    until: datetime | None = None,
    as_of: datetime | None = None,
    include_sensitive: bool = True,
    limit: int | None = None,
) -> list[dict]:
    """Visible observations for one profile, newest first. `as_of` gives what the store knew then.

    Callers that serve a token without read:sensitive pass include_sensitive=False and count the
    withheld rows themselves (`count_sensitive`).
    """
    where = ["profile_id = :p", lifecycle.visible_sql()]
    params: dict = {"p": profile_id, "as_of": as_of}
    if alias_group:
        where.append("alias_group = :ag")
        params["ag"] = alias_group
    if loinc:
        where.append("loinc = any(:loinc)")
        params["loinc"] = [loinc] if isinstance(loinc, str) else list(loinc)
    if category:
        where.append("category = :cat")
        params["cat"] = category
    if since:
        where.append("effective_at >= :since")
        params["since"] = since
    if until:
        where.append("effective_at <= :until")
        params["until"] = until
    if as_of:
        where.append("effective_at <= :as_of")
    if not include_sensitive:
        where.append("sensitivity = '{}'")
    sql = f"select * from observation where {' and '.join(where)} order by effective_at desc, id"
    if limit:
        sql += " limit :limit"
        params["limit"] = limit
    return [dict(r) for r in conn.execute(text(sql), params).mappings()]


def latest_per_group(
    conn: Connection, profile_id: UUID, *, as_of: datetime | None = None, category: str | None = None
) -> list[dict]:
    """The newest visible observation per alias_group."""
    where = ["profile_id = :p", lifecycle.visible_sql()]
    params: dict = {"p": profile_id, "as_of": as_of}
    if category:
        where.append("category = :cat")
        params["cat"] = category
    if as_of:
        where.append("effective_at <= :as_of")
    sql = (
        f"select distinct on (alias_group) * from observation where {' and '.join(where)}"
        " order by alias_group, effective_at desc, recorded_at desc"
    )
    return [dict(r) for r in conn.execute(text(sql), params).mappings()]
