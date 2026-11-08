"""get_health_status (mcp.md 4, 5.1): the rule pack's deterministic snapshot of one profile.

The body comes from lane E (`state.build_state`), hashed before anything else happens; language,
units and identity mode are applied after (`state.render`, then our redaction), so every token gets
the same `state_hash`. Sections for domains the token cannot read are dropped by the pipeline's
fact gate and named in `withheld.by_scope`.
"""

from __future__ import annotations

from ownhealth import state
from ownhealth.mcp.envelope import wrap_plain
from ownhealth.mcp.tools import Call, Result, tool

ENVELOPE_FIELDS = ("limitations", "not_assessed", "evidence", "db_versions", "withheld", "notice")
SECTION_SCOPE = {  # data sections that belong to one domain, dropped whole without the scope
    "medications": "read:records",
    "supplements": "read:records",
    "active_conditions": "read:records",
}
FRESHNESS_SCOPE = {
    "labs": "read:labs",
    "vitals": "read:vitals",
    "wearables": "read:wearables",
    "records": "read:records",
    "genome": "read:genome",
}
# Keep a 5-year profile under 2 KB whole (mcp.md 5.1) without paging: a flag keeps its template text
# (the wording that is information, not diagnosis) and drops the structured copies of what the text
# says; a stale or never-measured gap keeps its structured fields and drops the text that repeats them.
FLAG_FIELDS = ("rank", "kind", "rule", "code", "text", "src")
GAP_DROP = {"stale": ("text",), "never_measured": ("text",)}
GENOME_FLAG_KINDS = {"gene_drug", "gene_lab", "GENE_DRUG", "GENE_LAB"}

DESCRIPTION = (
    "The deterministic health snapshot of one profile: flags from the rule pack (out of the lab's own "
    "range, significant change, trend, stale, never measured, gene x drug), current medications and "
    "conditions, gaps and corrections, each fact with its src id. Call this first for any 'how am I doing' "
    "or 'what should I ask my doctor' question. Quote flags with their src ids; drill down with get_trend or "
    "search_observations only for the flags that matter. never_measured and stale are gaps: missing is not "
    "normal. Keep state_hash: the same hash later means nothing changed. Do not restate a flag as a "
    "diagnosis; use the template text or the cited range. Information, not diagnosis."
)

SCHEMA = {
    "type": "object",
    "properties": {
        "profile": {"type": "string", "description": "Profile slug or 'me'. Omitted: the token's default."},
        "as_of": {"type": "string", "format": "date", "description": "What the store knew on this date."},
        "cursor": {"type": "string"},
    },
    "additionalProperties": False,
}


def status_data(call: Call) -> tuple[dict, dict]:
    """(data, envelope fields) of the rendered state for the call's profile, scope-trimmed."""
    body = state.build_state(call.conn, call.profile_id, as_of=call.as_of.day)
    rendered = state.render(body, lang=call.lang, units=call.units, identity_mode=call.mode)
    rendered = dict(rendered.get("data", rendered)) if "data" in rendered else dict(rendered)
    extra = {k: rendered.pop(k) for k in ENVELOPE_FIELDS if k in rendered}
    for k in ("profile", "as_of", "lang", "identity_mode", "generated_at", "schema"):
        rendered.pop(k, None)
    for key in ("medications", "supplements", "active_conditions"):
        wrap_plain(rendered.get(key), "name", "fhir_note")
    wrap_plain(rendered.get("notes"), "text", "person_note")
    rendered.setdefault("state_hash", body.get("state_hash"))
    rendered.setdefault("rules_version", body.get("rules_version") or body.get("rules"))

    scopes = call.perm.scopes
    by_scope: set[str] = set()
    for key, scope in SECTION_SCOPE.items():
        if key in rendered and scope not in scopes:
            rendered.pop(key)
            by_scope.add(scope.split(":")[1])
    if "read:genome" not in scopes and isinstance(rendered.get("flags"), list):
        kept = [
            f
            for f in rendered["flags"]
            if not (isinstance(f, dict) and f.get("kind", f.get("reason")) in GENOME_FLAG_KINDS)
        ]
        if len(kept) != len(rendered["flags"]):
            by_scope.add("genome")
        rendered["flags"] = kept
    fresh = rendered.get("data_freshness")
    if isinstance(fresh, dict):
        for k, scope in FRESHNESS_SCOPE.items():
            if k in fresh and scope not in scopes:
                fresh.pop(k)
                by_scope.add(k)
    if isinstance(rendered.get("flags"), list):
        rendered["flags"] = [
            {k: f[k] for k in FLAG_FIELDS if k in f} if isinstance(f, dict) and f.get("text") else f
            for f in rendered["flags"]
        ]
    for key, fields in GAP_DROP.items():
        for item in rendered.get(key) or []:
            if isinstance(item, dict) and item.get("name"):
                for f in fields:
                    item.pop(f, None)
    withheld = dict(extra.pop("withheld", None) or {})
    withheld["by_scope"] = sorted(set(withheld.get("by_scope") or []) | by_scope)
    extra["withheld"] = withheld
    return rendered, extra


@tool(
    name="get_health_status",
    title="Health status",
    description=DESCRIPTION,
    input_schema=SCHEMA,
    any_scope=("read:summary",),
)
def get_health_status(call: Call) -> Result:
    data, extra = status_data(call)
    flags = data.get("flags")
    return Result(
        data=data,
        pageable="flags" if isinstance(flags, list) else None,
        budget=2000,
        max_bytes=state.MAX_STATE_BYTES,
        limitations=extra.get("limitations") or [],
        not_assessed=extra.get("not_assessed") or [],
        evidence=extra.get("evidence") or [],
        db_versions=extra.get("db_versions") or {},
        withheld=extra.get("withheld"),
        notice=extra.get("notice"),
    )
