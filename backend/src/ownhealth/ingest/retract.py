"""Withdraw everything one document brought in, for a retraction or a move to another profile.

A document reaches the current view three ways: observations carry `document_id`; records,
encounters, notes and genome findings link through their `src.document_id`; wearable samples carry
`document_id` (0200). The facts are retracted (kept, out of the current view); samples have no
lifecycle, so they are deleted and the rollups they fed rebuilt. The document's import flags are
cleared, so importing it again runs the import instead of returning the old summary.

Samples another still-active export also holds belong to whichever export brought them first; they
leave with that one.
"""

from __future__ import annotations

from uuid import UUID

from sqlalchemy import Connection, text

from ownhealth import wearables
from ownhealth.core import lifecycle

_LIVE = "('current', 'provisional')"


def retract_document_facts(conn: Connection, document_id: UUID, reason: str) -> dict:
    """Returns {"observations": [ids], "records": {table: [ids]}, "wearable_samples": n}."""
    out: dict = {"observations": [], "records": {}, "wearable_samples": 0}
    for table in sorted(lifecycle.LIFECYCLE_TABLES):
        direct = " or f.document_id = :d" if table == "observation" else ""
        ids = (
            conn.execute(
                text(
                    f"select f.id from {table} f left join src s on s.id = f.src_id"
                    f" where (s.document_id = :d{direct}) and f.status in {_LIVE} order by f.id"
                ),
                {"d": document_id},
            )
            .scalars()
            .all()
        )
        for row_id in ids:
            lifecycle.retract(conn, table, row_id, reason)
        if table == "observation":
            out["observations"] = list(ids)
        elif ids:
            out["records"][table] = list(ids)

    gone = conn.execute(
        text("delete from wearable_sample where document_id = :d returning profile_id, metric, source_id"),
        {"d": document_id},
    ).all()
    out["wearable_samples"] = len(gone)
    for profile_id in {g.profile_id for g in gone}:
        wearables.recompute_rollups(
            conn,
            profile_id=profile_id,
            metrics=sorted({g.metric for g in gone if g.profile_id == profile_id}),
            source_ids=sorted({g.source_id for g in gone if g.profile_id == profile_id}),
        )
    conn.execute(
        text("update document set flags = flags - 'import' - 'apple_health' - 'fhir_issuer' where id = :d"),
        {"d": document_id},
    )
    return out
