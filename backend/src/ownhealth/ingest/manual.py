"""Lane C: values a person types in. Provenance kind `manual`; accepted_by `person`, never the
auto-accept rule. Contract stub until lane C lands it."""

from __future__ import annotations

from uuid import UUID

from sqlalchemy import Connection

from ownhealth.core.observations import Committed


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
) -> Committed:
    raise NotImplementedError("lane C: ingest.manual.add_manual_observation")
