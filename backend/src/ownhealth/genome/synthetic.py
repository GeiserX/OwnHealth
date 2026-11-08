"""Synthetic genome fixtures for the demo seed and the tests. No real genomes, no real people.

`alex_facts()`, `sam_facts()` and `lucia_facts()` return `ownhealth.genome-facts/v1` documents for
the Rivera family; `write_vcf()` writes a bgzipped, tabix-indexed VCF or gVCF with pysam;
`alex_raw_files()` writes Alex's calls VCF and gVCF that agree with his facts; `seed_genome()` stores
a facts document plus raw files and imports them. Positions are GRCh38 positions of the named rsIDs as
the design docs give them; genotypes, guidance text and scores are invented. The recommendation text
is a stand-in, never a quote: a real file quotes CPIC verbatim.
"""

from __future__ import annotations

import base64
import copy
import hashlib
import json
from dataclasses import dataclass
from datetime import datetime
from pathlib import Path
from uuid import UUID

import pysam
from sqlalchemy import Connection

from ownhealth.blobs import BlobStore
from ownhealth.core.documents import create_document
from ownhealth.genome.ingest import import_genome_facts
from ownhealth.genome.model import FACTS_SCHEMA
from ownhealth.ingest import ImportSummary

CONTIGS = [f"chr{i}" for i in range(1, 23)] + ["chrX", "chrY", "chrM"]
CPIC_URL = "https://cpicpgx.org/guidelines/"


def fact_id(category: str, gene: str, key: str) -> str:
    """The deterministic id of genomics.md 2.1: sha256("v1|category|gene|key"), first 16 hex."""
    return "f_" + hashlib.sha256(f"v1|{category}|{gene}|{key}".encode()).hexdigest()[:16]


def vrs_id(chrom: str, pos: int, ref: str, alt: str) -> str:
    """A synthetic GA4GH VRS-shaped id (not a real VRS computation)."""
    digest = hashlib.sha256(f"{chrom}:{pos}:{ref}:{alt}".encode()).digest()[:24]
    return "ga4gh:VA." + base64.urlsafe_b64encode(digest).decode().rstrip("=")


def stand_in(drug: str, phenotype: str) -> str:
    return f"Synthetic stand-in for the CPIC recommendation text for {drug} with {phenotype}."


def _variant(chrom, pos, ref, alt, rsid, state, gt, **extra) -> dict:
    zyg = {"0/1": "het", "1/1": "hom", "0/0": "ref"}.get(gt, "unknown")
    v = {"chrom": chrom, "pos": pos, "ref": ref, "alt": alt, "rsid": rsid, "vrs_id": vrs_id(chrom, pos, ref, alt)}
    v |= extra
    v["genotype"] = {"call_state": state, "gt": gt, "zygosity": zyg, "dp": 31, "gq": 60}
    return v


def _guidance(drug: str, rxnorm: str, phenotype: str, level: str = "A") -> dict:
    return {
        "drug": drug,
        "rxnorm": rxnorm,
        "source": "CPIC",
        "level": level,
        "phenotype": phenotype,
        "recommendation": stand_in(drug, phenotype),
        "url": CPIC_URL,
    }


def _pgx(gene, diplotype, phenotype, variants, guidance, *, score=None, basis="two callers agree", conf="high", **kw):
    value = {"diplotype": diplotype, "phenotype": phenotype}
    if score is not None:
        value["activity_score"] = score
    fact = {
        "id": fact_id("pgx", gene, gene),
        "category": "pgx",
        "gene": gene,
        "variants": variants,
        "value": value,
        "summary": f"{gene} {diplotype}: {phenotype}.",
        "drug_guidance": guidance,
        "evidence": {"level": "E1", "native": "CPIC A", "tools": ["PharmCAT", "pypgx"]},
        "confidence": {"level": conf, "basis": basis},
        "sensitivity": {"level": "normal"},
        "origin": {"step": "pharmcat", "file": "pharmcat/report.json"},
    }
    return fact | kw


def _base(sample: str, sex: str, assay: dict, generated: str, fingerprint: str) -> dict:
    return {
        "schema": FACTS_SCHEMA,
        "subject": {
            "sample_id": sample,
            "declared_sex": sex,
            "fingerprint": {"panel": "ownhealth-fp-v1", "genotypes": fingerprint},
        },
        "assay": {"build": "GRCh38"} | assay,
        "provenance": {
            "pipeline": "OwnGenome",
            "version": "1.4.2",
            "commit": "abc1234",
            "generated_utc": generated,
            "db_versions": {
                "clinvar": "2026-09-28",
                "pharmcat": "3.1.0",
                "cpic": "2026-09",
                "ipd_imgt_hla": "3.62",
                "pgs_catalog": "2026-08",
                "acmg_sf": "v3.3",
            },
        },
        "files": [],
        "coverage": [],
        "facts": [],
        "qc": {"sex_check": "pass", "contamination": 0.002},
    }


CPIC_GENES = ("CYP2C19", "CYP2D6", "CYP2C9", "CYP3A5", "SLCO1B1", "DPYD", "TPMT", "NUDT15", "VKORC1", "HLA-B", "UGT1A1")


def _apoe(state_429358: str, gt_429358: str) -> dict:
    return {
        "id": fact_id("risk_marker", "APOE", "apoe_haplotype"),
        "category": "risk_marker",
        "gene": "APOE",
        "variants": [
            _variant("chr19", 44908684, "T", "C", "rs429358", state_429358, gt_429358),
            _variant("chr19", 44908822, "C", "T", "rs7412", "ref", "0/0"),
        ],
        "value": {"haplotype": "e3/e4"},
        "summary": "APOE e3/e4. Linked to a higher average Alzheimer's disease risk than e3/e3; it is not a diagnosis.",
        "evidence": {"level": "E2", "native": "replicated association with effect size"},
        "confidence": {"level": "high", "basis": "both sites called, GQ > 50"},
        "sensitivity": {"level": "adult_onset", "reason": "APOE e4"},
    }


def _cftr_carrier() -> dict:
    v = _variant(
        "chr7", 117559590, "ATCT", "A", "rs113993960", "alt", "0/1", hgvs_p="p.Phe508del", clinvar_variation_id=7105
    )
    return {
        "id": fact_id("carrier", "CFTR", v["vrs_id"]),
        "category": "carrier",
        "gene": "CFTR",
        "variants": [v],
        "condition": {"name": "Cystic fibrosis", "inheritance": "AR"},
        "summary": (
            "Heterozygous carrier of a pathogenic CFTR variant. Carriers are not affected;"
            " it matters for a partner's testing."
        ),
        "evidence": {"level": "E1", "native": "ClinVar Pathogenic, 3 stars", "clinvar_stars": 3},
        "confidence": {"level": "high", "basis": "GQ 99, DP 35"},
        "sensitivity": {"level": "normal"},
    }


def _prs(percentile: int) -> dict:
    return {
        "id": fact_id("prs", "", "PGS000018"),
        "category": "prs",
        "value": {
            "condition": "coronary artery disease",
            "pgs_id": "PGS000018",
            "percentile": percentile,
            "matched_pct": 97.2,
            "reference": "HGDP+1kGP",
            "most_similar_pop": "EUR",
            "evaluated_in": ["EUR"],
        },
        "summary": f"Higher than {percentile}% of the EUR reference group for this score.",
        "evidence": {"level": "E3", "native": "PGS Catalog score with published evaluation"},
        "confidence": {"level": "medium", "basis": "matched 97% of variants; ancestry inside evaluation set"},
        "limitations": ["Percentile relative to the EUR reference group; not an absolute risk."],
        "sensitivity": {"level": "normal"},
    }


def alex_facts() -> dict:
    """Alex Rivera, sample AR-01, 30x WGS (synthetic)."""
    doc = _base(
        "AR-01",
        "male",
        {"type": "wgs", "platform": "Illumina 30x", "mean_depth": 33.8, "callable_fraction": 0.94},
        "2026-10-01T09:15:00Z",
        "0121012001201210120120012012102101201",
    )
    doc["subject"]["inferred_karyotype"] = "XY"
    doc["coverage"] = [
        {"scope": "gene", "id": g, "panel": "cpic", "state": "assessed", "fraction_callable": 0.999} for g in CPIC_GENES
    ]
    doc["coverage"][1]["note"] = "pypgx and Cyrius agree"
    doc["coverage"] += [
        {"scope": "gene", "id": "BRCA1", "panel": "acmg_sf_v3.3", "state": "assessed", "fraction_callable": 0.997},
        {"scope": "gene", "id": "BRCA2", "panel": "acmg_sf_v3.3", "state": "assessed", "fraction_callable": 0.998},
        {
            "scope": "gene",
            "id": "PMS2",
            "panel": "acmg_sf_v3.3",
            "state": "partial",
            "reason": "paralog",
            "fraction_callable": 0.81,
        },
        {"scope": "gene", "id": "CFTR", "panel": "carrier", "state": "assessed", "fraction_callable": 0.996},
        {"scope": "gene", "id": "FMR1", "panel": "carrier", "state": "not_assessed", "reason": "not_run"},
        {"scope": "gene", "id": "SMN1", "panel": "carrier", "state": "not_assessed", "reason": "paralog"},
        {"scope": "panel", "id": "repeat_expansions", "panel": "other", "state": "not_assessed", "reason": "not_run"},
    ]
    im = "Intermediate Metabolizer"
    doc["facts"] = [
        _pgx(
            "CYP2C19",
            "*1/*2",
            im,
            [_variant("chr10", 94781859, "G", "A", "rs4244285", "alt", "0/1")],
            [_guidance("clopidogrel", "32968", im), _guidance("omeprazole", "7646", im)],
        ),
        _pgx(
            "CYP2D6",
            "*1/*41",
            "Normal Metabolizer",
            [_variant("chr22", 42127803, "C", "T", "rs28371725", "alt", "0/1")],
            [_guidance("codeine", "2670", "Normal Metabolizer")],
            score=1.5,
            basis="pypgx and Cyrius from reads agree",
        ),
        _pgx(
            "SLCO1B1",
            "*1/*5",
            "Decreased Function",
            [_variant("chr12", 21178615, "T", "C", "rs4149056", "alt", "0/1")],
            [
                _guidance("atorvastatin", "83367", "Decreased Function"),
                _guidance("simvastatin", "36567", "Decreased Function"),
            ],
            basis="single SNV, GQ 58",
            related_loinc=["2157-6", "1742-6"],
        ),
        _pgx(
            "DPYD",
            "*1/*1",
            "Normal Metabolizer",
            [
                _variant("chr1", 97450058, "C", "T", "rs3918290", "ref", "0/0"),
                _variant("chr1", 97082391, "T", "A", "rs67376798", "ref", "0/0"),
            ],
            [
                _guidance("fluorouracil", "4492", "Normal Metabolizer"),
                _guidance("capecitabine", "194000", "Normal Metabolizer"),
            ],
            score=2.0,
        ),
        _pgx(
            "TPMT",
            "*1/*1",
            "Normal Metabolizer",
            [
                _variant("chr6", 18130687, "T", "C", "rs1142345", "ref", "0/0"),
                _variant("chr6", 18138997, "C", "T", "rs1800460", "ref", "0/0"),
            ],
            [_guidance("azathioprine", "1256", "Normal Metabolizer")],
        ),
        _pgx(
            "VKORC1",
            "-1639G>A het",
            None,
            [_variant("chr16", 31096368, "C", "T", "rs9923231", "alt", "0/1")],
            [_guidance("warfarin", "11289", "VKORC1 -1639 G/A")],
        ),
        _pgx(
            "CYP2C9",
            "*1/*1",
            "Normal Metabolizer",
            [
                _variant("chr10", 94942290, "C", "T", "rs1799853", "ref", "0/0"),
                _variant("chr10", 94981296, "A", "C", "rs1057910", "ref", "0/0"),
            ],
            [
                _guidance("warfarin", "11289", "Normal Metabolizer"),
                _guidance("celecoxib", "140587", "Normal Metabolizer"),
            ],
            score=2.0,
        ),
        {
            "id": fact_id("hla", "HLA-B", "HLA-B"),
            "category": "hla",
            "gene": "HLA-B",
            "value": {"alleles": ["HLA-B*07:02", "HLA-B*44:02"], "marker": "HLA-B*58:01", "present": False},
            "summary": "HLA-B*58:01 not detected. CPIC ties this allele to severe skin reactions with allopurinol.",
            "drug_guidance": [_guidance("allopurinol", "519", "HLA-B*58:01 negative")],
            "evidence": {"level": "E1", "native": "CPIC A"},
            "confidence": {"level": "medium", "basis": "T1K 2-field typing from reads"},
            "sensitivity": {"level": "normal"},
        },
        _apoe("alt", "0/1"),
        {
            "id": fact_id("risk_marker", "LPA", "rs10455872"),
            "category": "risk_marker",
            "gene": "LPA",
            "variants": [_variant("chr6", 160589086, "A", "G", "rs10455872", "alt", "0/1")],
            "value": {"kiv2_copies": 18, "kiv2_ci95": [15, 22]},
            "summary": (
                "LPA risk allele and a low KIV-2 copy estimate, both linked to higher Lp(a)."
                " A blood Lp(a) value is the measurement that counts."
            ),
            "evidence": {"level": "E2", "native": "replicated association"},
            "confidence": {"level": "medium", "basis": "depth estimate, experimental step"},
            "limitations": ["The SNPs were found in European cohorts."],
            "sensitivity": {"level": "normal"},
            "related_loinc": ["43583-4"],
        },
        _cftr_carrier(),
        _prs(86),
        {
            "id": fact_id("trait", "LCT", "rs4988235"),
            "category": "trait",
            "gene": "LCT",
            "variants": [_variant("chr2", 135851076, "G", "A", "rs4988235", "alt", "0/1")],
            "value": {"trait": "lactase persistence", "call": "likely"},
            "summary": "LCT -13910 C/T: lactase persistence likely.",
            "evidence": {"level": "E2", "native": "replicated association"},
            "confidence": {"level": "high", "basis": "GQ 60"},
            "sensitivity": {"level": "normal"},
        },
        {
            "id": fact_id("trait", "MTHFR", "rs1801133"),
            "category": "trait",
            "gene": "MTHFR",
            "variants": [_variant("chr1", 11796321, "G", "A", "rs1801133", "alt", "0/1")],
            "summary": "MTHFR C677T heterozygous: enzyme note only.",
            "evidence": {"level": "E4", "native": "not supported as health advice (PMID 23288205)"},
            "confidence": {"level": "high", "basis": "GQ 60"},
            "sensitivity": {"level": "normal"},
            "hidden_by_default": True,
        },
    ]
    return doc


def sam_facts() -> dict:
    """Sam Rivera, sample SR-01, a variant-only VCF run, facts only (CYP2D6 *1/*1 for the codeine example)."""
    doc = _base(
        "SR-01",
        "female",
        {"type": "vcf_only", "platform": "clinical exome VCF"},
        "2026-08-19T10:00:00Z",
        "1020120120102012021012012001202102012",
    )
    nm = "Normal Metabolizer"
    doc["coverage"] = [{"scope": "gene", "id": g, "panel": "cpic", "state": "assessed"} for g in ("CYP2C19", "CYP2D6")]
    doc["coverage"].append(
        {"scope": "gene", "id": "CYP2D6-SV", "panel": "cpic", "state": "not_assessed", "reason": "no_bam"}
    )
    doc["facts"] = [
        _pgx(
            "CYP2D6",
            "*1/*1",
            nm,
            [],
            [_guidance("codeine", "2670", nm)],
            score=2.0,
            conf="medium",
            basis="single caller; SVs not assessed",
        ),
        _pgx("CYP2C19", "*1/*1", nm, [], [_guidance("clopidogrel", "32968", nm)], conf="medium", basis="single caller"),
    ]
    return doc


def lucia_facts() -> dict:
    """Lucía Rivera (a minor), sample LR-01: PGx shown to guardians, the rest withheld by the minor rule."""
    doc = _base(
        "LR-01",
        "female",
        {"type": "wgs", "platform": "Illumina 30x", "mean_depth": 31.0},
        "2026-09-30T08:00:00Z",
        "0121010120120120120120102012012012010",
    )
    doc["coverage"] = [{"scope": "gene", "id": g, "panel": "cpic", "state": "assessed"} for g in ("TPMT", "NUDT15")]
    im = "Intermediate Metabolizer"
    doc["facts"] = [
        _pgx(
            "TPMT",
            "*1/*3A",
            im,
            [
                _variant("chr6", 18130687, "T", "C", "rs1142345", "alt", "0/1"),
                _variant("chr6", 18138997, "C", "T", "rs1800460", "alt", "0/1"),
            ],
            [_guidance("mercaptopurine", "103", im), _guidance("azathioprine", "1256", im)],
        ),
        _cftr_carrier(),
        _apoe("alt", "0/1"),
        _prs(64),
    ]
    return doc


# ------------------------------------------------------------------ VCFs ----


@dataclass(frozen=True)
class VcfRow:
    chrom: str
    pos: int
    ref: str
    alt: str  # "<NON_REF>" for a gVCF reference block
    gt: str = "0/1"
    id: str = "."
    dp: int = 30
    gq: int = 60
    end: int | None = None  # gVCF block end (INFO/END)


def write_vcf(path: Path, rows: list[VcfRow], *, sample: str = "SAMPLE") -> tuple[Path, Path]:
    """Write rows as a bgzipped VCF with a tabix index. Returns (vcf.gz, vcf.gz.tbi)."""
    path = Path(path)
    order = {c: i for i, c in enumerate(CONTIGS)}
    lines = [
        "##fileformat=VCFv4.2",
        '##INFO=<ID=END,Number=1,Type=Integer,Description="End of the reference block">',
        '##FORMAT=<ID=GT,Number=1,Type=String,Description="Genotype">',
        '##FORMAT=<ID=DP,Number=1,Type=Integer,Description="Depth">',
        '##FORMAT=<ID=GQ,Number=1,Type=Integer,Description="Genotype quality">',
        '##FORMAT=<ID=MIN_DP,Number=1,Type=Integer,Description="Minimum depth in the block">',
        *[f"##contig=<ID={c}>" for c in CONTIGS],
        "\t".join(["#CHROM", "POS", "ID", "REF", "ALT", "QUAL", "FILTER", "INFO", "FORMAT", sample]),
    ]
    for r in sorted(rows, key=lambda r: (order[r.chrom], r.pos)):
        if r.end is not None:
            info, fmt, val = f"END={r.end}", "GT:DP:GQ:MIN_DP", f"{r.gt}:{r.dp}:{r.gq}:{r.dp}"
        else:
            info, fmt, val = ".", "GT:DP:GQ", f"{r.gt}:{r.dp}:{r.gq}"
        lines.append("\t".join([r.chrom, str(r.pos), r.id, r.ref, r.alt, "50", "PASS", info, fmt, val]))
    plain = path.with_suffix("") if path.suffix == ".gz" else path
    plain.write_text("\n".join(lines) + "\n")
    gz = Path(pysam.tabix_index(str(plain), preset="vcf", force=True))
    return gz, Path(str(gz) + ".tbi")


def alex_raw_files(workdir: Path) -> dict[str, tuple[Path, Path]]:
    """Alex's calls VCF (every alt site of his facts plus a no-call) and gVCF (reference blocks around
    every ref site, one block below the GQ threshold). Agrees with `alex_facts()`."""
    workdir = Path(workdir)
    workdir.mkdir(parents=True, exist_ok=True)
    calls, blocks = [], []
    for fact in alex_facts()["facts"]:
        for v in fact.get("variants") or []:
            g = v["genotype"]
            if g["call_state"] == "alt":
                calls.append(VcfRow(v["chrom"], v["pos"], v["ref"], v["alt"], g["gt"], v.get("rsid", ".")))
            else:
                blocks.append(VcfRow(v["chrom"], v["pos"] - 40, "N", "<NON_REF>", "0/0", end=v["pos"] + 40))
    calls.append(VcfRow("chr3", 1_000_500, "A", "G", "./.", gq=0, dp=2))  # a site that failed calling
    blocks.append(VcfRow("chr3", 2_000_000, "N", "<NON_REF>", "0/0", gq=8, dp=4, end=2_000_200))  # low quality
    gvcf_rows = blocks + calls
    return {
        "calls_vcf": write_vcf(workdir / "AR-01.vcf.gz", calls, sample="AR-01"),
        "gvcf": write_vcf(workdir / "AR-01.g.vcf.gz", gvcf_rows, sample="AR-01"),
    }


def with_files(doc: dict, blobs: BlobStore, raw: dict[str, tuple[Path, Path]]) -> tuple[dict, dict[str, str]]:
    """Put raw files into the blob store and list them in a copy of the facts. Returns (facts, attachments)."""
    doc = copy.deepcopy(doc)
    attachments: dict[str, str] = {}
    for role, (data, index) in raw.items():
        sha, size = blobs.put_file(data)
        isha, _ = blobs.put_file(index)
        doc["files"].append(
            {
                "role": role,
                "path": f"vcf/{data.name}",
                "sha256": sha,
                "index_path": f"vcf/{index.name}",
                "index_sha256": isha,
                "bytes": size,
            }
        )
        attachments[data.name] = sha
        attachments[index.name] = isha
    return doc, attachments


def seed_genome(
    conn: Connection,
    blobs: BlobStore,
    *,
    household_id: UUID,
    profile_id: UUID,
    facts: dict,
    attachments: dict[str, str] | None = None,
    filename: str = "genome-facts.json",
    imported_at: datetime | None = None,
) -> tuple[UUID, ImportSummary]:
    """Store a facts document as an uploaded document and import it. Returns (document_id, summary)."""
    data = json.dumps(facts, indent=1, ensure_ascii=False).encode()
    sha = blobs.put_bytes(data)
    ref = create_document(
        conn,
        household_id=household_id,
        profile_id=profile_id,
        blob_sha256=sha,
        mime="application/json",
        size_bytes=len(data),
        filename=filename,
        channel="genome",
        doc_type="genomics",
    )
    return ref.id, import_genome_facts(
        conn, blobs, document_id=ref.id, attachments=attachments, imported_at=imported_at
    )
