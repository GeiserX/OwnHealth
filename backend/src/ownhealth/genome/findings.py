"""Reading genome findings: gating, coverage first, evidence filter, PGx for one drug (genomics.md 3, 6, 7).

Gating happens here, in the server, never in a prompt:
- sensitive findings (APOE e4, SF cancer and cardiac genes, producer-tagged sensitive or adult-onset)
  need `gate.include_sensitive`; otherwise they are counted in `withheld_sensitive` and not described;
- for a minor, sensitive, adult-onset, carrier and PRS findings are counted in `withheld_minor`,
  whatever the switches say.
"""

from __future__ import annotations

import json
import re
from datetime import UTC, date, datetime, time, timedelta
from functools import cache
from importlib import resources
from uuid import UUID

from sqlalchemy import Connection, text

from ownhealth.core.access import Permission
from ownhealth.core.lifecycle import visible_sql
from ownhealth.genome.model import CATEGORIES, CONFIDENCE, EVIDENCE, Findings, Gate, untrusted_fact_text

_CONF_RANK = {c: i for i, c in enumerate(CONFIDENCE)}
_CAT_RANK = {c: i for i, c in enumerate(CATEGORIES)}
_NORMAL_PHENOTYPE = re.compile(r"^(normal|normal metabolizer|normal function|typical|extensive)", re.I)


def gate_for(perm: Permission, *, surface: str, on: date, age_of_majority: int = 18) -> Gate:
    """The gate for one caller on one profile. `surface` is "ui" (the profile's UI switch) or "agents"
    (the agents switch plus read:genome; mcp.md 9.7). Both also need read:sensitive."""
    p = perm.profile
    if surface == "ui":
        open_ = p.genome_sensitive_ui and perm.has("read:sensitive")
    elif surface == "agents":
        open_ = p.genome_sensitive_agents and perm.has("read:genome") and perm.has("read:sensitive")
    else:
        raise ValueError(f"unknown surface {surface!r}")
    return Gate(include_sensitive=bool(open_), minor=p.is_minor(on, age_of_majority))


def as_of_bound(as_of: date | datetime | None) -> datetime | None:
    """End of the as_of day (UTC) for the lifecycle predicate; a datetime passes through."""
    if as_of is None or isinstance(as_of, datetime):
        return as_of
    return datetime.combine(as_of + timedelta(days=1), time.min, UTC) - timedelta(microseconds=1)


def withheld_reason(row, gate: Gate) -> str | None:
    """'minor', 'sensitive' or None for one genome_finding row (mapping with the flag columns)."""
    if gate.minor and (row["sensitive"] or row["adult_onset"] or row["carrier"]):
        return "minor"
    if row["sensitive"] and not gate.include_sensitive:
        return "sensitive"
    return None


def visible_rows(conn: Connection, profile_id: UUID, *, as_of=None) -> list[dict]:
    """Every finding row of the profile visible at as_of, with its dataset's columns, ungated."""
    sql = (
        "select f.*, d.assay, d.owngenome_version, d.db_versions, d.sample_id, d.imported_at,"
        " d.src_id as dataset_src from genome_finding f join genome_dataset d on d.id = f.dataset_id"
        f" where f.profile_id = :p and d.status <> 'retracted' and {visible_sql('f')}"
    )
    rows = conn.execute(text(sql), {"p": profile_id, "as_of": as_of_bound(as_of)}).mappings()
    return [dict(r) for r in rows]


def current_datasets(conn: Connection, profile_id: UUID, *, as_of=None) -> list[dict]:
    """The profile's datasets visible at as_of, newest first."""
    bound = as_of_bound(as_of)
    sql = (
        "select *, encode(vcf_blob, 'hex') as vcf, encode(tbi_blob, 'hex') as tbi, encode(gvcf_blob, 'hex') as gvcf,"
        " encode(gvcf_tbi_blob, 'hex') as gvcf_tbi, encode(sites_blob, 'hex') as sites,"
        " encode(sites_tbi_blob, 'hex') as sites_tbi from genome_dataset where profile_id = :p and"
        " (case when cast(:as_of as timestamptz) is null then status = 'current' else imported_at <="
        " cast(:as_of as timestamptz) and status <> 'retracted' and (superseded_at is null or superseded_at >"
        " cast(:as_of as timestamptz)) end) order by imported_at desc, id"
    )
    return [dict(r) for r in conn.execute(text(sql), {"p": profile_id, "as_of": bound}).mappings()]


def coverage_rows(conn: Connection, dataset_ids: list[UUID]) -> list[dict]:
    if not dataset_ids:
        return []
    rows = conn.execute(
        text(
            "select dataset_id, scope, target, panel, state, reason, fraction_callable, note from genome_coverage"
            " where dataset_id = any(:ids) order by panel nulls last, target"
        ),
        {"ids": dataset_ids},
    ).mappings()
    return [dict(r) for r in rows]


def _coverage_out(c: dict) -> dict:
    out = {"scope": c["scope"], "target": c["target"], "panel": c["panel"], "state": c["state"], "reason": c["reason"]}
    if c["fraction_callable"] is not None:
        out["fraction_callable"] = round(float(c["fraction_callable"]), 4)
    if c["note"]:
        out["note"] = untrusted_fact_text(c["note"])
    return out


def _not_assessed(c: dict) -> dict:
    """{"gene": "FMR1", "panel": "carrier", "reason": "not_run"}; a panel entry is {"panel": id, "reason"}."""
    reason = c["reason"] or "other"
    if c["scope"] == "panel":
        return {"panel": c["target"], "reason": reason}
    return {c["scope"]: c["target"], "panel": c["panel"], "reason": reason}


def _title(row: dict) -> str:
    fact = row["fact"]
    value = fact.get("value") or {}
    head = row["gene"] or value.get("condition") or row["category"]
    parts = [p for p in (row["diplotype"], row["phenotype"]) if p]
    if not parts and value.get("pgs_id"):
        parts = [value["pgs_id"], f"{value['percentile']}th percentile" if "percentile" in value else ""]
    if not parts and value.get("alleles"):
        parts = [", ".join(map(str, value["alleles"]))]
    if not parts and row["rsid"]:
        parts = [row["rsid"]]
    return " · ".join([str(head)] + [p for p in parts if p])


def finding_out(row: dict) -> dict:
    """One finding as every surface returns it. Free text is wrapped as untrusted_text."""
    fact = row["fact"]
    ev = fact.get("evidence") or {}
    conf = fact.get("confidence") or {}
    variants = [
        {
            k: v.get(k)
            for k in ("chrom", "pos", "ref", "alt", "rsid", "hgvs_c", "hgvs_p", "clinvar_variation_id")
            if v.get(k) is not None
        }
        | {
            "call_state": (v.get("genotype") or {}).get("call_state"),
            "gt": (v.get("genotype") or {}).get("gt"),
            "zygosity": (v.get("genotype") or {}).get("zygosity"),
        }
        for v in fact.get("variants") or []
    ]
    guidance = [
        {
            "drug": g["drug"],
            "rxnorm": g.get("rxnorm"),
            "source": g["source"],
            "level": g["level"],
            "phenotype": g.get("phenotype"),
            "recommendation": untrusted_fact_text(g.get("recommendation")),
            "url": g.get("url"),
        }
        for g in fact.get("drug_guidance") or []
    ]
    clinvar = None
    first = next((v for v in fact.get("variants") or [] if v.get("clinvar_variation_id")), None)
    if first:
        clinvar = {
            "variation_id": first["clinvar_variation_id"],
            "stars": ev.get("clinvar_stars"),
            "native": ev.get("native"),
        }
    score = row["activity_score"]
    return {
        "id": row["fact_id"],
        "category": row["category"],
        "gene": row["gene"],
        "title": _title(row),
        "diplotype": row["diplotype"],
        "phenotype": row["phenotype"],
        "activity_score": float(score) if score is not None else None,
        "value": fact.get("value") or {},
        "rsid": row["rsid"],
        "hgvs": row["hgvs"],
        "zygosity": row["zygosity"],
        "variants": variants,
        "clinvar": clinvar,
        "condition": fact.get("condition"),
        "evidence": row["evidence"],
        "evidence_native": ev.get("native"),
        "confidence": row["confidence"],
        "confidence_basis": conf.get("basis"),
        "call_state": row["call_state"],
        "status": row["status"],
        "verification": row["verification_status"],
        "sensitive": row["sensitive"],
        "hidden_by_default": bool(fact.get("hidden_by_default")),
        "confirm_clinically": bool(fact.get("confirm_clinically")),
        "summary": untrusted_fact_text(fact.get("summary")),
        "limitations": [untrusted_fact_text(t) for t in fact.get("limitations") or []],
        "drugs": [g["drug"] for g in guidance],
        "drug_guidance": guidance,
        "related_loinc": fact.get("related_loinc") or [],
        "source": {"assay": row["assay"], "tool": row["owngenome_version"], "sample_id": row["sample_id"]},
        "src": row["src_id"],
    }


def _dedupe(rows: list[dict]) -> list[dict]:
    """One row per fact id across sources: the higher confidence wins, the count of sources is kept."""
    best: dict[str, dict] = {}
    count: dict[str, int] = {}
    for r in rows:
        fid = r["fact_id"]
        count[fid] = count.get(fid, 0) + 1
        cur = best.get(fid)
        if cur is None or (_CONF_RANK[r["confidence"]], -r["imported_at"].timestamp()) < (
            _CONF_RANK[cur["confidence"]],
            -cur["imported_at"].timestamp(),
        ):
            best[fid] = r
    for fid, r in best.items():
        r["n_sources"] = count[fid]
    return list(best.values())


def _sort_key(r: dict):
    return (r["evidence"], _CAT_RANK.get(r["category"], 99), (r["gene"] or "").upper(), r["fact_id"])


def assay_out(ds: dict) -> dict:
    detail = ds.get("assay_detail") or {}
    producer = ds.get("producer") or {}
    return {
        "type": ds["assay"],
        "platform": detail.get("platform"),
        "build": ds["build"],
        "mean_depth": detail.get("mean_depth"),
        "callable_fraction": detail.get("callable_fraction"),
        "source_tool": ds["owngenome_version"],
        "generated_utc": producer.get("generated_utc"),
        "imported_at": ds["imported_at"].isoformat(),
        "raw_files": {"vcf": bool(ds["vcf"]), "gvcf": bool(ds["gvcf"]), "chip_sites": bool(ds["sites"])},
        "src": ds["src_id"],
    }


def dataset_limitations(datasets: list[dict]) -> list[str]:
    out = []
    for ds in datasets:
        kind = ds["assay"]
        if not ds["vcf"]:
            out.append(f"{kind}: no VCF on file, so variant lookups can only answer sites the findings cite.")
        elif kind in ("wgs", "wes", "long_read") and not ds["gvcf"]:
            out.append(f"{kind}: variant-only VCF; a site with no record cannot be told from reference (no_call).")
        if kind in ("chip", "chip_imputed"):
            out.append("Chip: rare pathogenic calls are unconfirmed and a negative is not reassurance.")
    out.append("CRAM/BAM not on file: structural variant, CYP2D6 and HLA re-calls are not possible here.")
    return out


def list_findings(
    conn: Connection,
    profile_id: UUID,
    *,
    gate: Gate,
    category: str | None = None,
    gene: str | None = None,
    min_evidence: str = "E2",
    as_of: date | None = None,
) -> Findings:
    """Gated findings, strongest evidence first. `min_evidence` is the weakest level included (E2 shows
    E1 and E2; E4 also shows hidden-by-default facts). With `gene`, the coverage entry comes first and
    a gene with no finding answers its coverage state, never an empty silence."""
    if min_evidence not in EVIDENCE:
        raise ValueError(f"min_evidence must be one of {EVIDENCE}")
    if category is not None and category not in CATEGORIES:
        raise ValueError(f"category must be one of {CATEGORIES}")
    datasets = current_datasets(conn, profile_id, as_of=as_of)
    result = Findings()
    if not datasets:
        result.limitations = ["No genome data on file for this profile. OwnGenome produces genome-facts.json."]
        return result
    newest = datasets[0]
    result.assay = assay_out(newest)
    result.db_versions = dict(newest["db_versions"] or {})
    result.limitations = dataset_limitations(datasets)
    want_gene = gene.strip().upper() if gene else None

    shown = []
    for r in _dedupe(visible_rows(conn, profile_id, as_of=as_of)):
        if r["dataset_id"] not in {d["id"] for d in datasets}:
            continue
        if category and r["category"] != category:
            continue
        if want_gene and (r["gene"] or "").upper() != want_gene:
            continue
        why = withheld_reason(r, gate)
        if why == "minor":
            result.withheld_minor += 1
            continue
        if why == "sensitive":
            result.withheld_sensitive += 1
            continue
        if r["evidence"] > min_evidence:
            continue
        if r["fact"].get("hidden_by_default") and min_evidence != "E4":
            continue
        shown.append(r)
    result.findings = [finding_out(r) | {"n_sources": r["n_sources"]} for r in sorted(shown, key=_sort_key)]

    cov = coverage_rows(conn, [d["id"] for d in datasets])
    newest_cov = [c for c in cov if c["dataset_id"] == newest["id"]]
    result.not_assessed = [_not_assessed(c) for c in newest_cov if c["state"] == "not_assessed"]
    if want_gene:
        result.coverage = [_coverage_out(c) for c in newest_cov if c["target"].upper() == want_gene]
        result.not_assessed = [n for n in result.not_assessed if str(n.get("gene", "")).upper() == want_gene]
        if not result.coverage and not result.findings and not (result.withheld_minor or result.withheld_sensitive):
            result.not_assessed.append({"gene": gene.strip(), "panel": None, "reason": "not_in_coverage_map"})
    else:
        result.coverage = [_coverage_out(c) for c in newest_cov]
    return result


# ----------------------------------------------------------------- drug ----


@cache
def _cpic_pairs() -> dict[str, dict]:
    """CPIC drug id -> {name, genes} from the CPIC API snapshot lane E ships (CC0)."""
    try:
        doc = json.loads(resources.files("ownhealth.rules.cpic").joinpath("snapshot.json").read_text())
    except (ModuleNotFoundError, FileNotFoundError, ValueError):
        return {}
    out: dict[str, dict] = {}
    for p in doc.get("pairs") or []:
        entry = out.setdefault(p["drugid"], {"name": p.get("drugname"), "genes": []})
        if p["genesymbol"] not in entry["genes"]:
            entry["genes"].append(p["genesymbol"])
    return out


def drug_genes(cpic_drug_id: str) -> tuple[str | None, list[str]]:
    entry = _cpic_pairs().get(cpic_drug_id) or {}
    return entry.get("name"), list(entry.get("genes") or [])


def _guidance_matches(g: dict, cpic_drug_id: str, name: str | None) -> bool:
    rx = cpic_drug_id.split(":", 1)[1] if ":" in cpic_drug_id else None
    return bool(
        (rx and g.get("rxnorm") in (rx, cpic_drug_id))
        or (name and g.get("drug", "").strip().lower() == name.lower())
        or g.get("drug", "").strip().lower() == cpic_drug_id.lower()
    )


def pgx_for_drug(
    conn: Connection,
    profile_id: UUID,
    *,
    cpic_drug_id: str,
    as_of: date | None = None,
    genes: list[str] | None = None,
    drug_name: str | None = None,
) -> list[dict]:
    """PGx (and HLA) findings relevant to one drug: one entry per gene CPIC ties to the drug (from
    `genes`, else the CPIC snapshot pairs) plus any gene whose facts carry guidance for it.

    Each entry: gene, status ("found" | "not_assessed" | "no_finding"), and for found ones fact_id,
    diplotype, phenotype, activity_score, evidence, evidence_native, confidence, confidence_basis,
    call_state, actionable, guidance[] (recommendation as untrusted_text, only guidance for this drug),
    src; coverage {state, reason} when the coverage map has the gene. Sensitive findings are never
    returned here (no PGx gene is on the sensitive list; this keeps it so).
    """
    snap_name, snap_genes = drug_genes(cpic_drug_id)
    name = drug_name or snap_name
    wanted = [g.upper() for g in (genes if genes is not None else snap_genes)]
    datasets = current_datasets(conn, profile_id, as_of=as_of)
    ds_ids = {d["id"] for d in datasets}
    rows = [
        r
        for r in _dedupe(visible_rows(conn, profile_id, as_of=as_of))
        if r["dataset_id"] in ds_ids and r["category"] in ("pgx", "hla") and not r["sensitive"]
    ]
    for r in rows:
        if any(_guidance_matches(g, cpic_drug_id, name) for g in r["fact"].get("drug_guidance") or []):
            gene = (r["gene"] or "").upper()
            if gene and gene not in wanted:
                wanted.append(gene)
    newest = datasets[0]["id"] if datasets else None
    cov = {c["target"].upper(): c for c in coverage_rows(conn, [newest] if newest else [])}
    out = []
    for gene in wanted:
        hits = sorted((r for r in rows if (r["gene"] or "").upper() == gene), key=_sort_key)
        c = cov.get(gene)
        coverage = {"state": c["state"], "reason": c["reason"]} if c else None
        if not hits:
            status = "no_finding" if c and c["state"] != "not_assessed" else "not_assessed"
            out.append({"gene": gene, "status": status, "coverage": coverage, "call_state": None, "guidance": []})
            continue
        for r in hits:
            f = finding_out(r)
            guidance = [g for g in f["drug_guidance"] if _guidance_matches(g, cpic_drug_id, name)]
            out.append(
                {
                    "gene": r["gene"],
                    "status": "found",
                    "fact_id": f["id"],
                    "category": f["category"],
                    "diplotype": f["diplotype"],
                    "phenotype": f["phenotype"],
                    "activity_score": f["activity_score"],
                    "value": f["value"],
                    "evidence": f["evidence"],
                    "evidence_native": f["evidence_native"],
                    "confidence": f["confidence"],
                    "confidence_basis": f["confidence_basis"],
                    "call_state": f["call_state"],
                    "actionable": bool(f["phenotype"]) and not _NORMAL_PHENOTYPE.match(f["phenotype"] or ""),
                    "guidance": guidance,
                    "coverage": coverage,
                    "src": f["src"],
                }
            )
    return out
