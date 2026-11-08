"""get_genome_findings and lookup_variant (mcp.md 5.4, 5.5), backed by lane D.

Both need read:genome: a token without it sees neither tool nor any genome fact elsewhere. Sensitive
findings need the profile's agents switch plus read:genome and read:sensitive; the minor rule
withholds adult-onset, sensitive and carrier findings for a minor whatever the scopes. Withheld
findings are counted, never described.
"""

from __future__ import annotations

import re

from ownhealth import genome
from ownhealth.genome.model import GenomeQueryError, untrusted_fact_text
from ownhealth.mcp import limits
from ownhealth.mcp.envelope import ToolError
from ownhealth.mcp.tools import Call, Result, tool

GENOME_LIMITATION = (
    "Research-grade analysis, not a clinical laboratory report. Confirm actionable findings with a clinical test."
)
SUMMARY_FIELDS = (
    "id",
    "category",
    "gene",
    "diplotype",
    "phenotype",
    "activity_score",
    "rsid",
    "hgvs",
    "zygosity",
    "clinvar",
    "evidence",
    "confidence",
    "call_state",
    "verification",
    "drugs",
    "text",
    "src",
)
VALIDATED_VALUE_KEYS = ("phenotype", "diplotype")  # checked as vocabulary at import (genome/schema.py)


def _wrap_strings(v):
    if isinstance(v, str):
        return untrusted_fact_text(v)
    if isinstance(v, list):
        return [_wrap_strings(x) for x in v]
    return v


def _full(f: dict) -> dict:
    """The full finding for an agent: the facts file's prose (condition name, value strings) wrapped."""
    out = dict(f)
    if isinstance(f.get("condition"), dict):
        out["condition"] = {**f["condition"], "name": untrusted_fact_text(f["condition"].get("name"))}
    if isinstance(f.get("value"), dict):
        out["value"] = {k: v if k in VALIDATED_VALUE_KEYS else _wrap_strings(v) for k, v in f["value"].items()}
    return out


REGION_RE = re.compile(r"^chr([0-9XYM]+):(\d+)-(\d+)$")

FINDINGS_SCHEMA = {
    "type": "object",
    "properties": {
        "profile": {"type": "string"},
        "category": {"enum": list(genome.CATEGORIES)},
        "gene": {"type": "string", "pattern": "^[A-Za-z0-9-]{1,20}$"},
        "min_evidence": {"enum": list(genome.EVIDENCE), "default": "E2"},
        "detail": {"enum": ["summary", "full"], "default": "summary"},
        "cursor": {"type": "string"},
        "as_of": {"type": "string"},
    },
    "additionalProperties": False,
}

LOOKUP_SCHEMA = {
    "type": "object",
    "properties": {
        "profile": {"type": "string"},
        "rsid": {"type": "string", "pattern": "^rs\\d+$"},
        "hgvs": {"type": "string", "maxLength": 120, "pattern": "^[A-Za-z0-9_.:>+*()\\-]+$"},
        "region": {"type": "string", "pattern": "^chr[0-9XYM]+:\\d+-\\d+$"},
        "cursor": {"type": "string"},
    },
    "oneOf": [{"required": ["rsid"]}, {"required": ["hgvs"]}, {"required": ["region"]}],
    "additionalProperties": False,
}


@tool(
    name="get_genome_findings",
    title="Genome findings",
    description=(
        "Curated findings from the profile's OwnGenome genome-facts file: pharmacogenes, pathogenic variants, "
        "carrier status, HLA, risk markers, each with evidence E1-E4, confidence and call_state (ref, alt, "
        "no_call, not_on_array, not_covered). Filter by category or gene. E4 and traits are left out unless "
        "asked for. not_assessed lists what the source could not test: not assessed is unknown, not negative. "
        "withheld.sensitive counts findings behind the profile's opt-in; their content is never shown. "
        "Research-grade unless a clinical lab confirmed it."
    ),
    input_schema=FINDINGS_SCHEMA,
    any_scope=("read:genome",),
)
def get_genome_findings(call: Call) -> Result:
    a = call.args
    found = genome.list_findings(
        call.conn,
        call.profile_id,
        gate=call.gate(),
        category=a.get("category"),
        gene=a.get("gene"),
        min_evidence=a.get("min_evidence", "E2"),
        as_of=call.as_of.day,
    )
    findings = list(found.findings)
    if a.get("category") != "trait":
        findings = [f for f in findings if f.get("category") != "trait"]
    if a.get("detail") != "full":
        findings = [{k: f[k] for k in SUMMARY_FIELDS if k in f} for f in findings]
    else:
        findings = [_full(f) for f in findings]
    data = {"assay": found.assay, "findings": findings}
    if not findings:
        data["no_findings"] = True
    return Result(
        data=data,
        pageable="findings",
        budget=15000 if a.get("detail") == "full" else 4000,
        not_assessed=found.not_assessed,
        withheld={"sensitive": found.withheld_sensitive, "minor_rule": found.withheld_minor, "by_scope": []},
        db_versions=found.db_versions,
        limitations=[GENOME_LIMITATION],
    )


@tool(
    name="lookup_variant",
    title="Look up one genome position",
    description=(
        "What the profile's genome says at one position (rsID, HGVS or a region up to 1 Mb), read from the "
        "tabix-indexed VCF. call_state is always one of ref, alt, no_call, not_on_array, not_covered; "
        "not_on_array and not_covered mean unknown, not negative. Positions inside a gene behind the profile's "
        "sensitive opt-in come back withheld. Capped per token per day; quota shows what is left."
    ),
    input_schema=LOOKUP_SCHEMA,
    any_scope=("read:genome_raw",),
    all_scopes=("read:genome",),
)
def lookup_variant(call: Call) -> Result:
    a = call.args
    cap = int(call.limits["genome_raw_lookups_per_day"])
    if call.usage.lookups >= cap:
        raise ToolError(
            "genome_lookup_cap",
            f"This token used its {cap} genome lookups for today.",
            "It resets at 00:00 in the install's timezone.",
            daily_cap=cap,
        )
    if a.get("region"):
        m = REGION_RE.match(a["region"])
        start, end = int(m.group(2)), int(m.group(3))
        if end < start or end - start + 1 > limits.REGION_MAX_BP:
            raise ToolError("bad_input", "A region must run forward and span at most 1 Mb.", "Split the region.")
    try:
        data = genome.lookup_variant(
            call.conn,
            call.blobs,
            call.profile_id,
            gate=call.gate(),
            rsid=a.get("rsid"),
            hgvs=a.get("hgvs"),
            region=a.get("region"),
            cursor=call.inner_cursor,
        )
    except GenomeQueryError as exc:
        raise ToolError(exc.code, exc.message, exc.fix) from exc
    data = dict(data)
    inner_next = data.pop("next_cursor", None)
    envelope_bits = {k: data.pop(k) for k in ("limitations", "db_versions", "withheld", "not_assessed") if k in data}
    # lane D answers a withheld site with withheld="withheld_sensitive" | "withheld_minor": keep the word in
    # data and count it in the envelope's withheld block
    why = envelope_bits.get("withheld")
    if isinstance(why, str):
        data["withheld"] = why
        envelope_bits["withheld"] = {"minor_rule": 1} if why == "withheld_minor" else {"sensitive": 1}
    data["quota"] = {"used_today": call.usage.lookups + 1, "daily_cap": cap}
    limitations = list(envelope_bits.get("limitations") or [])
    for c in data.get("calls") or []:
        if c.get("call_state") in ("not_on_array", "not_covered"):
            limitations.append("Not tested by this source at this position. Unknown, not negative.")
            break
    return Result(
        data=data,
        budget=2000 if a.get("region") else 800,
        limitations=limitations,
        db_versions=envelope_bits.get("db_versions") or {},
        withheld=envelope_bits.get("withheld"),
        not_assessed=envelope_bits.get("not_assessed") or [],
        counters={"lookups": 1},
        inner_next_cursor=inner_next,
    )
