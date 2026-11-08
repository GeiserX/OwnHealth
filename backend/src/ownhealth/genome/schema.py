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

# Fields that reach agents bare (and templates such as the drug check) must be vocabulary, not prose:
# an HGNC-shaped gene, a lower-case drug name, a MONDO id, and phenotype / diplotype terms made of
# known phenotype words and allele-shaped tokens (each with a digit, *, /, > or :).
GENE_RE = re.compile(r"^[A-Z0-9][A-Z0-9-]{0,19}$")
DRUG_RE = re.compile(r"^[a-z][a-z0-9 ,/()-]{0,59}$")
MONDO_RE = re.compile(r"^MONDO:\d{7}$")
_CODE_TOKEN = re.compile(r"^[A-Za-z0-9*:./+()>_-]{1,24}$")
_CODE_MARK = re.compile(r"[0-9*/>:]")
TERM_WORDS = frozenset(
    {
        "poor",
        "intermediate",
        "normal",
        "rapid",
        "ultrarapid",
        "extensive",
        "indeterminate",
        "metabolizer",
        "metaboliser",
        "function",
        "decreased",
        "increased",
        "reduced",
        "possible",
        "likely",
        "positive",
        "negative",
        "carrier",
        "non-carrier",
        "noncarrier",
        "activity",
        "uncertain",
        "unknown",
        "high",
        "low",
        "risk",
        "deficient",
        "deficiency",
        "het",
        "hom",
        "homozygous",
        "heterozygous",
        "hemizygous",
        "wild-type",
        "wildtype",
        "variant",
        "allele",
        "alleles",
        "no",
        "not",
        "result",
        "called",
        "favorable",
        "unfavorable",
        "status",
    }
)
MAX_TERM_TOKENS = 6


def term_ok(value, gene: str | None = None, max_len: int = 60) -> bool:
    """A phenotype or diplotype term: short, and every word a phenotype word, the gene, or allele-shaped."""
    if not isinstance(value, str) or not 0 < len(value) <= max_len:
        return False
    tokens = value.split()
    if not tokens or len(tokens) > MAX_TERM_TOKENS:
        return False
    for t in tokens:
        if t.casefold() in TERM_WORDS or (gene and t == gene):
            continue
        if _CODE_TOKEN.match(t) and _CODE_MARK.search(t):
            continue
        return False
    return True


def _vocabulary_errors(where: str, fact: dict) -> list[str]:
    errors = []
    gene = fact.get("gene")
    if gene is not None and not (isinstance(gene, str) and GENE_RE.match(gene)):
        errors.append(f"{where}.gene: not an HGNC gene symbol")
    value = fact.get("value") if isinstance(fact.get("value"), dict) else {}
    for key in ("phenotype", "diplotype"):
        if value.get(key) is not None and not term_ok(value[key], gene):
            errors.append(f"{where}.value.{key}: not a phenotype or allele term")
    for k, g in enumerate(fact.get("drug_guidance") or []):
        if not (isinstance(g.get("drug"), str) and DRUG_RE.match(g["drug"])):
            errors.append(f"{where}.drug_guidance[{k}].drug: not a drug name")
        if g.get("phenotype") is not None and not term_ok(g["phenotype"], gene):
            errors.append(f"{where}.drug_guidance[{k}].phenotype: not a phenotype term")
    cond = fact.get("condition") if isinstance(fact.get("condition"), dict) else {}
    if cond.get("mondo") is not None and not (isinstance(cond["mondo"], str) and MONDO_RE.match(cond["mondo"])):
        errors.append(f"{where}.condition.mondo: not a MONDO id")
    return errors


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
        errors += _vocabulary_errors(where, fact)
        texts = [("summary", fact.get("summary"))]
        cond = fact.get("condition") if isinstance(fact.get("condition"), dict) else {}
        texts += [("condition.name", cond.get("name"))]
        value = fact.get("value") if isinstance(fact.get("value"), dict) else {}
        texts += [("value.condition", value.get("condition"))]
        texts += [(f"limitations[{k}]", t) for k, t in enumerate(fact.get("limitations") or [])]
        texts += [
            (f"drug_guidance[{k}].recommendation", g.get("recommendation"))
            for k, g in enumerate(fact.get("drug_guidance") or [])
        ]
        for name, t in texts:
            if isinstance(t, str) and _CONTROL.search(t):
                errors.append(f"{where}.{name}: control characters are not allowed in free text")
            if isinstance(t, str) and len(t) > 2000:
                errors.append(f"{where}.{name}: longer than 2000 characters")
    for i, f in enumerate(doc["files"]):
        if f["role"] in ("calls_vcf", "gvcf", "chip_sites") and not f.get("index_sha256"):
            errors.append(f"$.files[{i}]: a {f['role']} file needs its tabix index (index_path, index_sha256)")
    return errors[:MAX_ERRORS]
