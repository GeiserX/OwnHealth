"""Validation of `ownhealth.genome-facts/v1` (genomics.md 2): the JSON Schema plus the rules it
cannot express. A file whose `schema` string we do not know gets one clear error and nothing else."""

from __future__ import annotations

import hashlib
import json
import re
from collections.abc import Callable
from datetime import datetime
from functools import cache
from importlib import resources

from jsonschema import Draft202012Validator

from ownhealth.genome.model import FACTS_SCHEMA
from ownhealth.genome.sensitive import carrier_spans_overlapping, is_sensitive_gene, spans_overlapping

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
    for key in ("phenotype", "diplotype", "haplotype"):
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


# ------------------------------------------------- importer rules 5, 6, 7, 9 ----

# A fact that rests on one variant: its id key is that variant's VRS id (genomics.md 2.1).
VARIANT_CATEGORIES = ("monogenic_pathogenic", "acmg_sf", "carrier")
SINGLE_SITE_CATEGORIES = ("trait", "risk_marker")
_CONF_RANK = {"unconfirmed": 0, "low": 1, "medium": 2, "high": 3}
IMPUTED_R2_MEDIUM = 0.9

# A site predicate: (chrom, pos) -> True when the chip's site list probes it.
ChipSites = Callable[[str, int], bool]


def fact_id(category: str, gene: str, key: str) -> str:
    """The deterministic id of genomics.md 2.1: "f_" + sha256("v1|category|gene|key"), first 16 hex."""
    return "f_" + hashlib.sha256(f"v1|{category}|{gene}|{key}".encode()).hexdigest()[:16]


def _id_errors(where: str, fact: dict) -> list[str]:
    """Rule 5: the id is the hash recomputed from category, gene and key; a variant finding's variants
    carry vrs_id. A multi-site rule (APOE) and ancestry name their key nowhere in the file: not checked."""
    cat, gene = fact["category"], fact.get("gene") or ""
    value = fact.get("value") if isinstance(fact.get("value"), dict) else {}
    variants = fact.get("variants") or []
    if cat in VARIANT_CATEGORIES or (cat in SINGLE_SITE_CATEGORIES and len(variants) == 1):
        missing = [j for j, v in enumerate(variants) if not v.get("vrs_id")]
        if missing or not variants:
            what = ", ".join(f"variants[{j}]" for j in missing) or "variants"
            return [f"{where}.{what}: a {cat} finding needs vrs_id on every variant it cites"]
        keys = [v["vrs_id"] for v in variants]
    elif cat == "pgx":
        keys = [gene]
    elif cat == "hla":
        if not isinstance(value.get("marker"), str):
            return [f"{where}.value.marker: an hla finding needs the tested allele, its id key"]
        keys = [value["marker"]]
    elif cat == "prs":
        if not isinstance(value.get("pgs_id"), str):
            return [f"{where}.value.pgs_id: a prs finding needs its PGS id, its id key"]
        keys = [value["pgs_id"]]
    else:
        return []
    if fact["id"] not in {fact_id(cat, gene, k) for k in keys}:
        return [f"{where}.id: {fact['id']} is not the id of genomics.md 2.1 recomputed from category, gene and key"]
    return []


def _confidence_cap(fact: dict, assay: str) -> tuple[str, str] | None:
    """Rule 6: (cap, why) from the table of genomics.md 4 for a chip file, else None."""
    if assay not in CHIP_ASSAYS:
        return None
    gene = (fact.get("gene") or "").upper()
    if fact["category"] == "hla" or gene.startswith("HLA-"):
        return "low", "an HLA call from a chip"
    if fact["category"] != "pgx":
        return None
    if gene == "CYP2D6":
        return "low", "CYP2D6 from a chip"
    if assay == "chip_imputed":
        imputed = [(v.get("genotype") or {}) for v in fact.get("variants") or []]
        imputed = [g for g in imputed if g.get("imputed") is True]
        if imputed:
            if all(isinstance(g.get("r2"), int | float) and g["r2"] >= IMPUTED_R2_MEDIUM for g in imputed):
                return "medium", f"a PGx call on imputed sites, all with R2 >= {IMPUTED_R2_MEDIUM}"
            return "low", f"a PGx call on an imputed site with R2 below {IMPUTED_R2_MEDIUM} or not given"
    return "medium", "a PGx call from a chip"


def _span_errors(where: str, fact: dict) -> list[str]:
    """Rule 9: a sensitive or adult-onset fact cites only positions in the sensitive gene spans, a carrier
    fact only positions in the carrier-panel spans, because lookups are gated by those spans alone."""
    level = (fact.get("sensitivity") or {}).get("level")
    checks = []
    if level in ("sensitive", "adult_onset") or is_sensitive_gene(fact.get("gene")):
        checks.append(("the sensitive gene spans", spans_overlapping))
    if fact["category"] == "carrier":
        checks.append(("the carrier-panel gene spans", carrier_spans_overlapping))
    errors = []
    for j, v in enumerate(fact.get("variants") or []):
        for label, overlapping in checks:
            if not overlapping(v["chrom"], v["pos"], v["pos"]):
                errors.append(
                    f"{where}.variants[{j}]: {fact.get('gene') or fact['category']} {v['chrom']}:{v['pos']} is outside"
                    f" {label}, so a raw lookup would not be gated; the gene list in OwnHealth needs an update"
                )
    return errors


def chip_site_errors(doc: dict, chip_sites: ChipSites | None) -> list[str]:
    """The part of rule 7 that needs the chip's site list: a site marked `imputed: false` must be on it,
    and without it a P/LP, ACMG SF or carrier fact from chip_imputed is rejected (nothing proves the
    array typed it). `chip_sites` is None when the list is not on hand."""
    if doc["assay"]["type"] != "chip_imputed":
        return []
    errors = []
    for i, fact in enumerate(doc["facts"]):
        where = f"$.facts[{i}]"
        if chip_sites is None:
            if fact["category"] in UNCONFIRMED_ON_CHIP:
                errors.append(
                    f"{where}: a {fact['category']} fact from chip_imputed needs the chip_sites list on hand to"
                    " prove the array typed it"
                )
            continue
        for j, v in enumerate(fact.get("variants") or []):
            if (v.get("genotype") or {}).get("imputed") is False and not chip_sites(v["chrom"], v["pos"]):
                errors.append(f"{where}.variants[{j}]: marked imputed: false but not in the chip_sites list")
    return errors


def _imputed_errors(where: str, fact: dict) -> list[str]:
    """Rules 2 and 7 for chip_imputed: every variant says whether it was imputed, and a P/LP, ACMG SF or
    carrier fact never rests on an imputed genotype ("never reported", genomics.md 4)."""
    errors = []
    for j, v in enumerate(fact.get("variants") or []):
        imputed = (v.get("genotype") or {}).get("imputed")
        if not isinstance(imputed, bool):
            errors.append(f"{where}.variants[{j}].genotype.imputed: required (true or false) from chip_imputed")
        elif imputed and fact["category"] in UNCONFIRMED_ON_CHIP:
            errors.append(
                f"{where}.variants[{j}]: a {fact['category']} fact from chip_imputed rests on an imputed site"
            )
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


DEFER_CHIP_SITES = object()


def validate_facts(doc: dict, *, chip_sites: ChipSites | None | object = None) -> list[str]:
    """Every problem with a genome-facts document, as `$.path: message` lines; empty when valid.

    `chip_sites` answers whether the chip's site list probes a position (rule 7); None means the list
    is not on hand. The importer passes `DEFER_CHIP_SITES` and runs `chip_site_errors` once the raw files
    are checked."""
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
    assay = doc["assay"]["type"]
    chip = assay in CHIP_ASSAYS
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
        cap = _confidence_cap(fact, assay)
        if cap and _CONF_RANK[fact["confidence"]["level"]] > _CONF_RANK[cap[0]]:
            errors.append(f"{where}.confidence.level: {cap[1]} is at most '{cap[0]}'")
        if assay == "chip_imputed":
            errors += _imputed_errors(where, fact)
        errors += _id_errors(where, fact)
        errors += _span_errors(where, fact)
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
    if chip_sites is not DEFER_CHIP_SITES:
        errors += chip_site_errors(doc, chip_sites)
    return errors[:MAX_ERRORS]
