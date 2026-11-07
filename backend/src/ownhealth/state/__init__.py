"""Lane E: the deterministic current state, status words and check_drug (architecture.md 11,
mcp.md 4 and 5.6). The rule pack lives in `rules/ownhealth-rules/` (YAML, semver).

    body       = build_state(conn, profile_id, as_of=...)   # language-neutral, includes state_hash
    response   = render(body, lang=..., units=..., identity_mode=...)

`status_word` is the single place that maps a value to OK / Watch / Out / Stale / No data; the UI
and MCP never compute it themselves. Contract stubs until lane E lands them.
"""

from __future__ import annotations

from datetime import date
from typing import Literal
from uuid import UUID

from sqlalchemy import Connection

StatusWord = Literal["ok", "watch", "out", "stale", "nodata"]
STATUS_WORDS: tuple[str, ...] = ("ok", "watch", "out", "stale", "nodata")
STATE_SCHEMA = "ownhealth.state/v1"
MAX_STATE_BYTES = 2048


def canonical_json(obj) -> bytes:
    """RFC 8785 (JCS). Decimals are strings with the printed precision; no floats in the hash."""
    raise NotImplementedError("lane E: state.canonical_json")


def rules_version() -> str:
    """'ownhealth-rules 0.1.0' or 'ownhealth-rules 0.1.0+local.<sha8>' with overrides."""
    raise NotImplementedError("lane E: state.rules_version")


def build_state(conn: Connection, profile_id: UUID, *, as_of: date) -> dict:
    """The language-neutral body with `state_hash`; stored in state_snapshot when the hash is new."""
    raise NotImplementedError("lane E: state.build_state")


def render(body: dict, *, lang: str, units: str = "as_reported", identity_mode: str = "pseudonymous") -> dict:
    """Localized text, unit preference and redaction, applied after hashing (never changes the hash)."""
    raise NotImplementedError("lane E: state.render")


def status_word(conn: Connection, profile_id: UUID, alias_group: str, *, as_of: date) -> StatusWord:
    raise NotImplementedError("lane E: state.status_word")


def check_drug(
    conn: Connection,
    profile_id: UUID,
    drug: str,
    *,
    scopes: frozenset[str],
    lang: str = "en",
    as_of: date | None = None,
) -> dict:
    """mcp.md 5.6 `data` block: drug, pgx[], phenoconversion[], current_medications_relevant[],
    relevant_labs[], guidance[] (quoted, cited), not_assessed[], plus withheld.by_scope for parts the
    scopes do not cover. Never imperative advice of its own."""
    raise NotImplementedError("lane E: state.check_drug")


def resolve_drug(conn: Connection, name: str) -> dict | None:
    """es/en generic or brand name, or a CPIC drug id, to {cpic_drug_id, name}. None when unknown."""
    raise NotImplementedError("lane E: state.resolve_drug")
