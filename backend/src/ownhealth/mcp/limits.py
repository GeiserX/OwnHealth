"""Per-token limits (mcp.md 9.4): a call bucket per minute, daily egress budgets in facts and bytes,
a daily cap on `lookup_variant` and on proposals. Counters live in Postgres (migration 0500).

Defaults come from the token row (`api_token.rate_per_min`, `egress_*`, ...), so a person can edit
them per token. The burst is fixed at 20.
"""

from __future__ import annotations

from dataclasses import dataclass
from datetime import UTC, date, datetime
from uuid import UUID
from zoneinfo import ZoneInfo

from sqlalchemy import Connection, text

BURST = 20
MAX_PROPOSAL_ROWS = 200
REGION_MAX_BP = 1_000_000

DEFAULTS = {
    "rate_per_min": 60,
    "egress_facts_per_day": 2000,
    "egress_bytes_per_day": 2_000_000,
    "genome_raw_lookups_per_day": 200,
    "proposals_per_day": 50,
}


@dataclass(frozen=True)
class Usage:
    calls: int = 0
    facts: int = 0
    bytes: int = 0
    lookups: int = 0
    proposals: int = 0


def limits_of(principal) -> dict:
    got = (principal.extra or {}).get("limits") or {}
    return {k: (got.get(k) if got.get(k) is not None else v) for k, v in DEFAULTS.items()}


def today(tz: str | None = None) -> date:
    """The day in the install's timezone (OWNHEALTH_TIMEZONE, default UTC); budgets reset at 00:00."""
    try:
        zone = ZoneInfo(tz) if tz else UTC
    except Exception:  # noqa: BLE001  an unknown zone falls back to UTC
        zone = UTC
    return datetime.now(zone).date()


def take_call(conn: Connection, token_id: UUID, rate_per_min: int, now: datetime | None = None) -> float | None:
    """Take one call from the token's bucket. None when allowed; seconds to wait when not."""
    now = now or datetime.now(UTC)
    rate = max(rate_per_min, 1) / 60.0  # tokens per second
    burst = float(min(BURST, max(rate_per_min, 1)))
    row = conn.execute(
        text("select tokens, updated_at from mcp_rate where token_id = :t for update"), {"t": token_id}
    ).first()
    if row is None:
        conn.execute(
            text(
                "insert into mcp_rate (token_id, tokens, updated_at) values (:t, :n, :now)"
                " on conflict (token_id) do nothing"
            ),
            {"t": token_id, "n": burst - 1, "now": now},
        )
        return None
    elapsed = max((now - row.updated_at).total_seconds(), 0.0)
    tokens = min(burst, row.tokens + elapsed * rate)
    if tokens < 1.0:
        conn.execute(
            text("update mcp_rate set tokens = :n, updated_at = :now where token_id = :t"),
            {"t": token_id, "n": tokens, "now": now},
        )
        return round((1.0 - tokens) / rate, 1)
    conn.execute(
        text("update mcp_rate set tokens = :n, updated_at = :now where token_id = :t"),
        {"t": token_id, "n": tokens - 1.0, "now": now},
    )
    return None


def usage(conn: Connection, token_id: UUID, day: date) -> Usage:
    row = conn.execute(
        text("select calls, facts, bytes, lookups, proposals from mcp_usage where token_id = :t and day = :d"),
        {"t": token_id, "d": day},
    ).first()
    return Usage(*row) if row else Usage()


def add_usage(conn: Connection, token_id: UUID, day: date, **counts: int) -> None:
    cols = ("calls", "facts", "bytes", "lookups", "proposals")
    vals = {c: int(counts.get(c, 0)) for c in cols}
    conn.execute(
        text(
            "insert into mcp_usage (token_id, day, calls, facts, bytes, lookups, proposals)"
            " values (:t, :d, :calls, :facts, :bytes, :lookups, :proposals) on conflict (token_id, day) do update set"
            " calls = mcp_usage.calls + excluded.calls, facts = mcp_usage.facts + excluded.facts,"
            " bytes = mcp_usage.bytes + excluded.bytes, lookups = mcp_usage.lookups + excluded.lookups,"
            " proposals = mcp_usage.proposals + excluded.proposals"
        ),
        {"t": token_id, "d": day, **vals},
    )


def history(conn: Connection, token_id: UUID, days: int = 30) -> list[dict]:
    rows = conn.execute(
        text(
            "select day, calls, facts, bytes, lookups, proposals from mcp_usage where token_id = :t"
            " order by day desc limit :n"
        ),
        {"t": token_id, "n": days},
    ).mappings()
    return [dict(r) for r in rows]
