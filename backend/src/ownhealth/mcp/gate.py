"""What one caller may see on one profile, and a last pass over every response that drops any cited
fact the caller cannot read, whoever built the response (lane E's status, lane D's findings, ours).

    effective scopes = token.scopes ∩ grant.categories − profile.locks   (core.access)

A fact is hidden when its table needs a scope the caller lacks (an observation's category, a record,
a genome finding), when it is sensitive and the caller lacks read:sensitive, when it is a sensitive
genome finding and the profile's agents switch is off, when the minor rule withholds it, or when it
belongs to another profile. Hidden facts are counted in `withheld`, never described.
"""

from __future__ import annotations

from dataclasses import dataclass, field
from datetime import date

from sqlalchemy import Connection, text

from ownhealth.core.access import Permission
from ownhealth.genome import Gate

OBS_CATEGORY_SCOPE = {
    "laboratory": "read:labs",
    "vital-signs": "read:vitals",
    "survey": "read:vitals",
    "exam": "read:vitals",
    "activity": "read:wearables",
}
RECORD_TABLES = (
    "condition",
    "medication_statement",
    "allergy",
    "immunization",
    "procedure",
    "encounter",
    "imaging_study",
)
READ_SCOPES = (
    "read:summary",
    "read:labs",
    "read:vitals",
    "read:wearables",
    "read:records",
    "read:genome",
)


def scope_label(scope: str) -> str:
    """'read:labs' -> 'labs', the word used in withheld.by_scope."""
    return scope.split(":", 1)[1]


def genome_gate(perm: Permission, as_of: date, age_of_majority: int = 18) -> Gate:
    """Sensitive genome facts need the agents switch, read:genome and read:sensitive (mcp.md 9.7)."""
    open_ = perm.profile.genome_sensitive_agents and perm.has("read:genome") and perm.has("read:sensitive")
    return Gate(include_sensitive=bool(open_), minor=perm.profile.is_minor(as_of, age_of_majority))


@dataclass
class Withheld:
    sensitive: int = 0
    minor_rule: int = 0
    by_scope: set[str] = field(default_factory=set)

    def merge(self, other: dict | None) -> None:
        if not other:
            return
        self.sensitive += int(other.get("sensitive") or 0)
        self.minor_rule += int(other.get("minor_rule") or 0)
        self.by_scope.update(other.get("by_scope") or [])

    def as_dict(self) -> dict:
        return {"sensitive": self.sensitive, "by_scope": sorted(self.by_scope), "minor_rule": self.minor_rule}


def hidden_reasons(conn: Connection, perm: Permission, gate: Gate, src_ids: list[str]) -> dict[str, tuple]:
    """src id -> why the caller may not see it: ('scope', label) | ('sensitive',) | ('minor',) | ('foreign',).
    Visible and unknown ids are absent from the result."""
    if not src_ids:
        return {}
    pid = perm.profile.id
    out: dict[str, tuple] = {}

    def hide(sid: str, why: tuple) -> None:
        # the first reason wins, except that a foreign profile always wins
        if sid not in out or why == ("foreign",):
            out[sid] = why

    def need(sid: str, scope: str) -> None:
        if scope not in perm.scopes:
            hide(sid, ("scope", scope_label(scope)))

    ids = list(dict.fromkeys(src_ids))
    for r in conn.execute(text("select id, kind, profile_id from src where id = any(:ids)"), {"ids": ids}):
        if r.profile_id is not None and r.profile_id != pid:
            hide(r.id, ("foreign",))
        if r.kind == "genome_facts":
            need(r.id, "read:genome")

    for r in conn.execute(
        text("select src_id, profile_id, category, sensitivity from observation where src_id = any(:ids)"),
        {"ids": ids},
    ):
        if r.profile_id != pid:
            hide(r.src_id, ("foreign",))
        need(r.src_id, OBS_CATEGORY_SCOPE.get(r.category, "read:labs"))
        if r.sensitivity and "read:sensitive" not in perm.scopes:
            hide(r.src_id, ("sensitive",))

    for table in RECORD_TABLES:
        sens = "sensitivity" if table != "imaging_study" else "'{}'::text[] as sensitivity"
        for r in conn.execute(
            text(f"select src_id, profile_id, {sens} from {table} where src_id = any(:ids)"), {"ids": ids}
        ):
            if r.profile_id != pid:
                hide(r.src_id, ("foreign",))
            need(r.src_id, "read:records")
            if r.sensitivity and "read:sensitive" not in perm.scopes:
                hide(r.src_id, ("sensitive",))

    for r in conn.execute(
        text("select src_id, profile_id, sensitivity from note where src_id = any(:ids)"), {"ids": ids}
    ):
        if r.profile_id != pid:
            hide(r.src_id, ("foreign",))
        need(r.src_id, "read:summary")
        if r.sensitivity and "read:sensitive" not in perm.scopes:
            hide(r.src_id, ("sensitive",))

    for r in conn.execute(
        text("select src_id, profile_id, sensitive, adult_onset, carrier from genome_finding where src_id = any(:ids)"),
        {"ids": ids},
    ):
        if r.profile_id != pid:
            hide(r.src_id, ("foreign",))
        need(r.src_id, "read:genome")
        if r.sensitive and not gate.include_sensitive:
            hide(r.src_id, ("sensitive",))
        if gate.minor and (r.adult_onset or r.sensitive or r.carrier):
            hide(r.src_id, ("minor",))

    for r in conn.execute(text("select src_id, profile_id from genome_dataset where src_id = any(:ids)"), {"ids": ids}):
        if r.profile_id != pid:
            hide(r.src_id, ("foreign",))
        need(r.src_id, "read:genome")
    return out


def _cites(node, hidden: dict[str, tuple]) -> tuple | None:
    if not isinstance(node, dict) or "src" not in node:
        return None
    src = node["src"]
    ids = [src] if isinstance(src, str) else [s for s in (src or []) if isinstance(s, str)]
    for s in ids:
        if s in hidden:
            return hidden[s]
    return None


def prune(obj, hidden: dict[str, tuple], withheld: Withheld):
    """A copy of obj without the facts that cite a hidden src. List items are removed; a dict field
    that is itself such a fact becomes None."""
    if not hidden:
        return obj

    def count(why: tuple) -> None:
        if why[0] == "scope":
            withheld.by_scope.add(why[1])
        elif why[0] == "sensitive":
            withheld.sensitive += 1
        elif why[0] == "minor":
            withheld.minor_rule += 1
        # a foreign fact is dropped and not counted: its existence is not this profile's business

    def go(node):
        if isinstance(node, list):
            out = []
            for item in node:
                why = _cites(item, hidden)
                if why:
                    count(why)
                    continue
                out.append(go(item))
            return out
        if isinstance(node, dict):
            out = {}
            for k, v in node.items():
                why = _cites(v, hidden)
                if why and not isinstance(v, list):
                    count(why)
                    out[k] = None
                    continue
                out[k] = go(v)
            return out
        return node

    return go(obj)


# Every category a document can carry: an unextracted page can hold any of them (mcp.md 5.7).
PAGE_ALL_SCOPES = ("read:labs", "read:vitals", "read:records", "read:genome", "read:sensitive")


def page_pending(conn: Connection, document_id, page_no: int) -> bool:
    """The page still waits for extraction: an open pending_document item lists it (or lists no pages)."""
    return bool(
        conn.execute(
            text(
                "select exists (select 1 from review_item where document_id = :d and kind = 'pending_document'"
                " and status = 'open' and (jsonb_typeof(payload->'pages') is distinct from 'array'"
                " or payload->'pages' @> to_jsonb(:n)))"
            ),
            {"d": document_id, "n": page_no},
        ).scalar()
    )


def page_refusal(conn: Connection, perm: Permission, gate: Gate, document_id, page_no: int) -> tuple | None:
    """Why the caller may not read one page's text, words or image (mcp.md 5.7, 9.7), or None.

    ('scope', [labels]) | ('sensitive',) | ('minor',) | ('foreign',). A finished page is permitted when the caller can
    read every fact extracted from it (committed, or still in the review queue); a pending page can hold
    anything, so it needs every category, the agents genome switch, and a profile that is not a minor.
    The document-wide sensitivity label is checked by the caller (core.documents.document_is_sensitive).
    """
    if page_pending(conn, document_id, page_no):
        missing = [scope_label(s) for s in PAGE_ALL_SCOPES if s not in perm.scopes]
        if missing:
            return ("scope", missing)
        if gate.minor:
            return ("minor",)
        if not gate.include_sensitive:
            return ("sensitive",)
        return None
    src_ids = list(
        conn.execute(
            text("select id from src where document_id = :d and page_no = :n"), {"d": document_id, "n": page_no}
        ).scalars()
    )
    reasons = list(hidden_reasons(conn, perm, gate, src_ids).values())
    # rows read from the page that wait in the review queue are on the page too
    for obs in conn.execute(
        text(
            "select payload->'observation' from review_item where document_id = :d and status = 'open'"
            " and kind = 'candidate_row' and coalesce(payload->>'page_no', payload->'src'->>'page_no') = :n"
        ),
        {"d": document_id, "n": str(page_no)},
    ).scalars():
        obs = obs or {}
        scope = OBS_CATEGORY_SCOPE.get(obs.get("category"), "read:labs")
        if scope not in perm.scopes:
            reasons.append(("scope", scope_label(scope)))
        if obs.get("sensitivity") and "read:sensitive" not in perm.scopes:
            reasons.append(("sensitive",))
    if ("foreign",) in reasons:
        return ("foreign",)  # a page citing another profile's fact is never shown, and not counted
    scopes = sorted({r[1] for r in reasons if r[0] == "scope"})
    if scopes:
        return ("scope", scopes)
    for kind in ("sensitive", "minor"):
        if (kind,) in reasons:
            return (kind,)
    return None
