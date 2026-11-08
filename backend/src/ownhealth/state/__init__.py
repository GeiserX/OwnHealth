"""Lane E: the deterministic current state, status words and check_drug (architecture.md 11,
mcp.md 4 and 5.6). The rule pack lives in `ownhealth/rules/pack/` (YAML, semver), the CPIC snapshot in
`ownhealth/rules/cpic/snapshot.json`.

    body       = build_state(conn, profile_id, as_of=...)   # language-neutral, includes state_hash
    response   = render(body, lang=..., units=..., identity_mode=...)

`status_word` is the single place that maps a value to OK / Watch / Out / Stale / No data; the UI
and MCP never compute it themselves.

The body holds codes, parameters and printed values only. Language, unit preference, identity mode and
the caller are not inputs, so every token sees the same `state_hash` for the same data.
"""

from __future__ import annotations

import hashlib
import json
from datetime import UTC, date, datetime, time
from typing import Literal
from uuid import UUID

from sqlalchemy import Connection, text

from ownhealth.state.canonical import canonical_json
from ownhealth.state.pack import load_pack

StatusWord = Literal["ok", "watch", "out", "stale", "nodata"]
STATUS_WORDS: tuple[str, ...] = ("ok", "watch", "out", "stale", "nodata")
STATE_SCHEMA = "ownhealth.state/v1"
MAX_STATE_BYTES = 2048

__all__ = [
    "MAX_STATE_BYTES",
    "STATE_SCHEMA",
    "STATUS_WORDS",
    "StatusWord",
    "UnknownDrug",
    "build_state",
    "canonical_json",
    "check_drug",
    "dashboard",
    "render",
    "resolve_drug",
    "rules_version",
    "state_hash",
    "status_word",
]


def rules_version() -> str:
    """'ownhealth-rules 0.1.0' or 'ownhealth-rules 0.1.0+local.<sha8>' with overrides."""
    return load_pack().version


def state_hash(body: dict) -> str:
    """sha256 of the canonical JSON of the body without its own `state_hash`."""
    clean = {k: v for k, v in body.items() if k != "state_hash"}
    return "sha256:" + hashlib.sha256(canonical_json(clean)).hexdigest()


def _as_date(as_of) -> date:
    from ownhealth.state.engine import to_date

    return to_date(as_of)


def _compact(obj):
    """Drop None values from dicts: the body is a byte budget (2 KB)."""
    if isinstance(obj, dict):
        return {k: _compact(v) for k, v in obj.items() if v is not None}
    if isinstance(obj, list):
        return [_compact(v) for v in obj]
    return obj


def _default_loinc(pack, group: str | None) -> str | None:
    codes = pack.analytes.get(group or "", {}).get("loinc", [])
    return str(codes[0]) if codes else None


def body_from(ev, pack) -> dict:
    """Project an Evaluation to the language-neutral body (no hash)."""
    # The 2 KB body repeats nothing the pack already fixes: the kind follows from the reason; severity,
    # the RCV, the trend length and a stale window from the rule; the LOINC code from the group unless
    # the value carried another one; the limitations from the pack version. List order is the rank.
    drop = {"kind", "severity", "rcv_pct", "n", "window_days"}
    flags = []
    for f in ev.flags:
        out = {k: v for k, v in f.items() if k not in drop}
        if f["kind"] == "gene_drug":
            out.pop("date", None)
        if out.get("loinc") and out["loinc"] == _default_loinc(pack, f.get("group")):
            out.pop("loinc")
        flags.append(out)
    stale = [{k: v for k, v in s.items() if k not in drop} for s in ev.stale]
    withheld = {
        "sensitive": ev.withheld_sensitive + ev.genome_withheld.get("sensitive", 0),
        "minor_rule": ev.genome_withheld.get("minor_rule", 0),
    }
    body = {
        "schema": STATE_SCHEMA,
        "profile": ev.slug,
        "as_of": ev.as_of.isoformat(),
        "rules": pack.version,
        "derivation": ev.derivation or None,
        "flags": flags,
        "stale": stale,
        "never_measured": ev.never_measured,
        "systems": {s.key: s.status for s in ev.systems},
        "medications": [_med(m) for m in ev.medications],
        "supplements": [_med(m) for m in ev.supplements],
        "conditions": [
            {
                "name": c["text"],
                "verification": c["verification_status"],
                "since": c["onset"].isoformat() if c["onset"] else None,
                "src": c["src_id"],
            }
            for c in ev.conditions
        ],
        "notes": ev.notes,
        "pending": ev.pending,
        "corrections": ev.corrections,
        "freshness": ev.freshness,
        "withheld": withheld if any(withheld.values()) else None,
    }
    return _compact(body)


def _med(m: dict) -> dict:
    return {
        "name": m["name"],
        "strength": m.get("strength"),
        "since": m["started"].isoformat() if m["started"] else None,
        "src": m["src_id"],
    }


def evaluate(conn: Connection, profile_id: UUID, *, as_of, known_at: datetime | None = None):
    from ownhealth.state.engine import evaluate as _evaluate

    return _evaluate(conn, profile_id, as_of=_as_date(as_of), pack=load_pack(), known_at=known_at)


def build_state(conn: Connection, profile_id: UUID, *, as_of, known_at: datetime | None = None) -> dict:
    """The language-neutral body with `state_hash`; stored in state_snapshot when the hash is new.

    `known_at` (optional) evaluates what the store knew at that moment (rows recorded and not yet
    corrected then); None is the current view. It selects rows only and is not part of the hash.
    """
    return build_with_evaluation(conn, profile_id, as_of=as_of, known_at=known_at)[0]


def build_with_evaluation(conn: Connection, profile_id: UUID, *, as_of, known_at: datetime | None = None):
    """(body, evaluation): the dashboard renders from the same evaluation the hash covers."""
    pack = load_pack()
    ev = evaluate(conn, profile_id, as_of=as_of, known_at=known_at)
    body = body_from(ev, pack)
    body["state_hash"] = state_hash(body)
    if known_at is None:
        store_snapshot(conn, profile_id, body, pack)
    return body, ev


def dashboard(conn: Connection, profile_id: UUID, *, as_of, lang: str, scopes: frozenset[str]) -> dict:
    """The "Health at a glance" page data (ui `Dashboard` type)."""
    from ownhealth.state.glance import build_dashboard

    body, ev = build_with_evaluation(conn, profile_id, as_of=as_of)
    return build_dashboard(conn, ev, body, load_pack(), lang=lang, scopes=scopes)


def store_snapshot(conn: Connection, profile_id: UUID, body: dict, pack) -> bool:
    """Upsert the snapshot for (profile, as_of, pack). Writes nothing when the hash is unchanged."""
    row = conn.execute(
        text(
            "insert into state_snapshot (profile_id, as_of, rules_version, rules_sha256, derivation_version,"
            " state_hash, body) values (:p, :a, :rv, :rs, :dv, :h, cast(:b as jsonb))"
            " on conflict (profile_id, as_of, rules_sha256) do update set state_hash = excluded.state_hash,"
            " body = excluded.body, derivation_version = excluded.derivation_version, created_at = now()"
            " where state_snapshot.state_hash <> excluded.state_hash returning 1"
        ),
        {
            "p": profile_id,
            "a": datetime.combine(date.fromisoformat(body["as_of"]), time(0), tzinfo=UTC),
            "rv": pack.version,
            "rs": bytes.fromhex(pack.sha256),
            "dv": body.get("derivation") or "",
            "h": bytes.fromhex(body["state_hash"].split(":", 1)[1]),
            "b": json.dumps(body, ensure_ascii=False),
        },
    ).first()
    return row is not None


def render(
    body: dict,
    *,
    lang: str,
    units: str = "as_reported",
    identity_mode: str = "pseudonymous",
    scopes: frozenset[str] | None = None,
) -> dict:
    """Localized text, unit preference and redaction, applied after hashing (never changes the hash).
    With `scopes`, sections the scopes do not cover are left out and named in `withheld.by_scope`."""
    from ownhealth.state.rendering import render_state

    return render_state(body, lang=lang, units=units, identity_mode=identity_mode, scopes=scopes)


def status_word(conn: Connection, profile_id: UUID, alias_group: str, *, as_of) -> StatusWord:
    from ownhealth.state.engine import evaluate as _evaluate

    ev = _evaluate(conn, profile_id, as_of=_as_date(as_of), pack=load_pack(), only_group=alias_group)
    m = ev.markers.get(alias_group)
    return m.status if m else "nodata"  # type: ignore[return-value]


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
    scopes do not cover. Never imperative advice of its own. Raises UnknownDrug for a name we cannot
    resolve (never "no interaction")."""
    from ownhealth.state.drugs import check_drug as _check

    return _check(conn, profile_id, drug, scopes=scopes, lang=lang, as_of=as_of)


def resolve_drug(conn: Connection, name: str) -> dict | None:
    """es/en generic or brand name, or a CPIC drug id, to {cpic_drug_id, name}. None when unknown."""
    from ownhealth.state.drugs import resolve_drug as _resolve

    return _resolve(name)


from ownhealth.state.drugs import UnknownDrug  # noqa: E402
