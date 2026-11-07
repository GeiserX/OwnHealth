"""One lifecycle for every fact: provisional -> current -> superseded | retracted.

Rows are immutable (a trigger enforces it). A correction inserts a new row with
`supersedes = old.id`; the old row only changes `status`, `status_reason` and `superseded_at`, once.
`superseded_at` is set for both superseded and retracted rows: it is when the row left the current view.
"""

from __future__ import annotations

from datetime import datetime
from uuid import UUID

from sqlalchemy import Connection, text

LIFECYCLE_TABLES = frozenset(
    {
        "observation",
        "condition",
        "medication_statement",
        "allergy",
        "immunization",
        "procedure",
        "encounter",
        "note",
        "genome_finding",
    }
)


class LifecycleError(ValueError):
    pass


def _check(table: str) -> None:
    if table not in LIFECYCLE_TABLES:
        raise LifecycleError(f"{table} has no lifecycle")


def lock_current(conn: Connection, table: str, row_id: UUID) -> dict:
    """Lock a row that may still change (provisional or current) and return it."""
    _check(table)
    row = conn.execute(text(f"select * from {table} where id = :id for update"), {"id": row_id}).mappings().first()
    if row is None:
        raise LifecycleError(f"{table} {row_id} not found")
    if row["status"] not in ("provisional", "current"):
        raise LifecycleError(f"{table} {row_id} is {row['status']}; only provisional or current rows change")
    return dict(row)


def mark_superseded(conn: Connection, table: str, row_id: UUID, reason: str | None = None) -> None:
    lock_current(conn, table, row_id)
    conn.execute(
        text(
            f"update {table} set status = 'superseded', status_reason = :r, superseded_at = clock_timestamp()"
            " where id = :id"
        ),
        {"id": row_id, "r": reason},
    )


def retract(conn: Connection, table: str, row_id: UUID, reason: str) -> None:
    """Withdraw a fact (entered in error, deleted by a person). Reason required; nothing is deleted."""
    if not reason or not reason.strip():
        raise LifecycleError("a retraction needs a reason")
    lock_current(conn, table, row_id)
    conn.execute(
        text(
            f"update {table} set status = 'retracted', status_reason = :r, superseded_at = clock_timestamp()"
            " where id = :id"
        ),
        {"id": row_id, "r": reason.strip()},
    )


def confirm_provisional(conn: Connection, table: str, row_id: UUID) -> None:
    """A final report confirmed a preliminary value: provisional -> current."""
    row = lock_current(conn, table, row_id)
    if row["status"] != "provisional":
        raise LifecycleError(f"{table} {row_id} is not provisional")
    conn.execute(text(f"update {table} set status = 'current' where id = :id"), {"id": row_id})


def history(conn: Connection, table: str, row_id: UUID) -> list[dict]:
    """Every version of a fact, oldest first, following `supersedes` both ways."""
    _check(table)
    rows = conn.execute(
        text(
            f"with recursive back as (select t.*, 0 as chain_pos from {table} t where id = :id"
            f" union all select t.*, b.chain_pos - 1 from {table} t join back b on t.id = b.supersedes),"
            f" fwd as (select t.*, 0 as chain_pos from {table} t where id = :id"
            f" union all select t.*, f.chain_pos + 1 from {table} t join fwd f on t.supersedes = f.id)"
            f" select * from back union select * from fwd order by chain_pos"
        ),
        {"id": row_id},
    ).mappings()
    # Chain order, not recorded_at: a correction made in the same transaction has the same now().
    return [{k: v for k, v in r.items() if k != "chain_pos"} for r in rows]


def visible_sql(alias: str = "") -> str:
    """SQL predicate: rows visible `as of` :as_of (bind it). Current view when :as_of is null."""
    a = f"{alias}." if alias else ""
    return (
        f"(case when cast(:as_of as timestamptz) is null then {a}status = 'current'"
        f" else {a}recorded_at <= cast(:as_of as timestamptz) and {a}status <> 'provisional'"
        f" and ({a}superseded_at is null or {a}superseded_at > cast(:as_of as timestamptz)) end)"
    )


def as_of_param(as_of: datetime | None) -> dict:
    return {"as_of": as_of}
