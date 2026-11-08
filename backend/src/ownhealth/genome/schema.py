"""Validation of `ownhealth.genome-facts/v1` (genomics.md 2): the JSON Schema plus the rules it
cannot express. A file whose `schema` string we do not know gets one clear error and nothing else."""

from __future__ import annotations

import json
import re
from datetime import datetime
from functools import cache
from importlib import resources

from jsonschema import Draft202012Validator

from ownhealth.genome.model import FACTS_SCHEMA

_CONTROL = re.compile(r"[\x00-\x08\x0b\x0c\x0e-\x1f\x7f]")
CHIP_ASSAYS = ("chip", "chip_imputed")
UNCONFIRMED_ON_CHIP = ("monogenic_pathogenic", "acmg_sf", "carrier")
MAX_ERRORS = 20


@cache
def schema_document() -> dict:
    return json.loads(resources.files("ownhealth.genome").joinpath("data/genome-facts.v1.json").read_text())


@cache
def _validator() -> Draft202012Validator:
    return Draft202012Validator(schema_document())


def _path(parts) -> str:
    out = "$"
    for p in parts:
        out += f"[{p}]" if isinstance(p, int) else f".{p}"
    return out


def version_error(doc) -> str | None:
    if not isinstance(doc, dict):
        return "not a genome-facts document: expected a JSON object"
    found = doc.get("schema")
    if found == FACTS_SCHEMA:
        return None
    if isinstance(found, str) and found.startswith("ownhealth.genome-facts/"):
        return (
            f"unsupported genome-facts version {found!r}: this OwnHealth reads {FACTS_SCHEMA!r} only. "
            "Update OwnHealth, or write the file with an OwnGenome release that emits v1. Nothing was imported."
        )
    return f"not a genome-facts document: the 'schema' field must be {FACTS_SCHEMA!r}, found {found!r}"


def validate_facts(doc: dict) -> list[str]:
    """Every problem with a genome-facts document, as `$.path: message` lines; empty when valid."""
    err = version_error(doc)
    if err:
        return [err]
    errors = [
        f"{_path(e.absolute_path)}: {e.message}"
        for e in sorted(_validator().iter_errors(doc), key=lambda e: list(map(str, e.absolute_path)))
    ]
    if errors:
        return errors[:MAX_ERRORS]
    try:
        datetime.fromisoformat(doc["provenance"]["generated_utc"].replace("Z", "+00:00"))
    except ValueError:
        errors.append("$.provenance.generated_utc: not an ISO 8601 date-time")
    chip = doc["assay"]["type"] in CHIP_ASSAYS
    seen: set[str] = set()
    for i, fact in enumerate(doc["facts"]):
        where = f"$.facts[{i}]"
        if fact["id"] in seen:
            errors.append(f"{where}.id: duplicate fact id {fact['id']}")
        seen.add(fact["id"])
        for j, v in enumerate(fact.get("variants") or []):
            if "call_state" not in (v.get("genotype") or {}):
                errors.append(f"{where}.variants[{j}]: a variant needs genotype.call_state")
        if chip and fact["category"] in UNCONFIRMED_ON_CHIP and fact["confidence"]["level"] != "unconfirmed":
            errors.append(f"{where}.confidence.level: a {fact['category']} fact from a chip must be 'unconfirmed'")
        texts = [("summary", fact.get("summary"))]
        texts += [(f"limitations[{k}]", t) for k, t in enumerate(fact.get("limitations") or [])]
        texts += [
            (f"drug_guidance[{k}].recommendation", g.get("recommendation"))
            for k, g in enumerate(fact.get("drug_guidance") or [])
        ]
        for name, t in texts:
            if isinstance(t, str) and _CONTROL.search(t):
                errors.append(f"{where}.{name}: control characters are not allowed in free text")
    for i, f in enumerate(doc["files"]):
        if f["role"] in ("calls_vcf", "gvcf", "chip_sites") and not f.get("index_sha256"):
            errors.append(f"$.files[{i}]: a {f['role']} file needs its tabix index (index_path, index_sha256)")
    return errors[:MAX_ERRORS]
