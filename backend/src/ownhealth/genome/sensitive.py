"""OwnHealth's own sensitive gene list (genomics.md 7), applied on top of the producer's tag.

APOE, the ACMG SF cancer and cardiovascular genes, HTT and other predictive repeat loci. Lookups use
the GRCh38 spans so a raw lookup (rs429358 for APOE) cannot bypass the opt-in.
"""

from __future__ import annotations

from dataclasses import dataclass
from functools import cache
from importlib import resources

SPAN_PAD_BP = 2_000


@dataclass(frozen=True)
class GeneSpan:
    gene: str
    chrom: str
    start: int  # 1-based, closed, padded
    end: int
    group: str


@cache
def _spans() -> tuple[GeneSpan, ...]:
    rows = []
    text = resources.files("ownhealth.genome").joinpath("data/sensitive_genes.tsv").read_text()
    for line in text.splitlines():
        if not line or line.startswith(("#", "gene\t")):
            continue
        gene, chrom, start, end, group = line.split("\t")
        rows.append(GeneSpan(gene, chrom, max(1, int(start) - SPAN_PAD_BP), int(end) + SPAN_PAD_BP, group))
    return tuple(rows)


@cache
def sensitive_genes() -> frozenset[str]:
    return frozenset(s.gene.upper() for s in _spans())


def is_sensitive_gene(gene: str | None) -> bool:
    return bool(gene) and gene.strip().upper() in sensitive_genes()


def norm_chrom(chrom: str) -> str:
    c = chrom.strip()
    if c.lower().startswith("chr"):
        c = c[3:]
    c = {"23": "X", "24": "Y", "25": "X", "26": "M", "MT": "M"}.get(c.upper(), c.upper())
    return "chr" + c


def spans_overlapping(chrom: str, start: int, end: int) -> list[GeneSpan]:
    c = norm_chrom(chrom)
    return [s for s in _spans() if s.chrom == c and s.start <= end and s.end >= start]


def sensitive_gene_at(chrom: str, pos: int) -> str | None:
    hits = spans_overlapping(chrom, pos, pos)
    return hits[0].gene if hits else None
