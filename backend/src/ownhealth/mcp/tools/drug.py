"""check_drug (mcp.md 5.6): PGx, phenoconversion by current medications, and the labs a prescriber
looks at, joined for one drug. Lane E builds the answer (`state.check_drug`); here we keep each part
behind its scope, whatever the backing service returns, and name the missing parts in
`withheld.by_scope`."""

from __future__ import annotations

from ownhealth import state
from ownhealth.mcp.envelope import wrap_plain
from ownhealth.mcp.tools import Call, Result, tool

NOTICE = "Information from your records, not medical advice. Discuss with your prescriber."
PART_SCOPES = {  # data key -> scopes it needs, all of them
    "pgx": ("read:genome",),
    "guidance": ("read:genome",),  # each entry applies to the profile's phenotype, so it is genome data
    "phenoconversion": ("read:genome", "read:records"),
    "current_medications_relevant": ("read:records",),
    "relevant_labs": ("read:labs",),
}
ENVELOPE_FIELDS = ("limitations", "not_assessed", "db_versions", "withheld", "evidence", "notice")

SCHEMA = {
    "type": "object",
    "properties": {
        "profile": {"type": "string"},
        "drug": {
            "type": "string",
            "minLength": 1,
            "maxLength": 100,
            "description": "Generic or brand name in es/en, or a CPIC drug id",
        },
        "as_of": {"type": "string"},
    },
    "required": ["drug"],
    "additionalProperties": False,
}


@tool(
    name="check_drug",
    title="Check a drug against the record",
    description=(
        "Everything the record says about one drug for one profile, in one call: pharmacogenomic guidance "
        "(CPIC, DPWG) for the profile's genotypes, current medications that change a gene's function "
        "(phenoconversion), current medications that matter for the drug, and the labs a prescriber would look "
        "at. Guidance is quoted with its source; it is followed by 'Discuss with your prescriber.' Parts this "
        "token cannot read are named in withheld.by_scope. Drug-drug interactions beyond phenoconversion are "
        "not assessed."
    ),
    input_schema=SCHEMA,
    any_scope=("read:genome", "read:records", "read:labs"),
)
def check_drug(call: Call) -> Result:
    scopes = call.perm.scopes
    raw = dict(
        state.check_drug(
            call.conn, call.profile_id, call.args["drug"], scopes=scopes, lang=call.lang, as_of=call.as_of.day
        )
    )
    extra = {k: raw.pop(k) for k in ENVELOPE_FIELDS if k in raw}
    wrap_plain(raw.get("current_medications_relevant"), "name", "fhir_note")
    wrap_plain(raw.get("phenoconversion"), "by", "fhir_note")
    # lane E's sentence quotes the medication name, which is free text from imports or people
    wrap_plain(raw.get("phenoconversion"), "text", "fhir_note")
    by_scope: set[str] = set((extra.get("withheld") or {}).get("by_scope") or [])
    for key, need in PART_SCOPES.items():
        missing = [s for s in need if s not in scopes]
        if missing:
            raw.pop(key, None)
            by_scope.update(s.split(":")[1] for s in missing)
    withheld = dict(extra.get("withheld") or {})
    withheld["by_scope"] = sorted(by_scope)
    not_assessed = list(extra.get("not_assessed") or [])
    if "drug-drug interactions" not in str(not_assessed):
        not_assessed.append("drug-drug interactions beyond phenoconversion (no open-licence source configured)")
    return Result(
        data=raw,
        pageable="relevant_labs" if isinstance(raw.get("relevant_labs"), list) else None,
        budget=4000,
        limitations=extra.get("limitations") or [],
        not_assessed=not_assessed,
        db_versions=extra.get("db_versions") or {},
        withheld=withheld,
        evidence=extra.get("evidence") or [],
        notice=NOTICE,
    )
