"""What the person's genome says at one position (genomics.md 3, 5; mcp.md 5.5).

Every answer carries a `call_state` per source, one of exactly CALL_STATES:

| state        | WGS / WES                                              | chip                               |
|--------------|--------------------------------------------------------|------------------------------------|
| alt / ref    | genotype in the calls VCF, or a gVCF block passing GQ  | genotype in the chip VCF           |
| no_call      | `./.`; or a variant-only VCF with no record (reason)   | probed (site list) but not called  |
| not_on_array | never                                                  | not probed by this array           |
| not_covered  | no gVCF block, a block below GQ/DP, or a not_assessed  | never                              |
|              | gene in the coverage map                               |                                    |

The VCFs are read from the blob store with pysam, indexes passed explicitly (blob names are hashes).
Sensitive genes are gated here too, so a raw lookup cannot bypass the opt-in (mcp.md 9.7).
"""

from __future__ import annotations

import re
from contextlib import ExitStack
from uuid import UUID

import pysam
from sqlalchemy import Connection, text

from ownhealth.blobs import BlobStore
from ownhealth.genome.findings import coverage_rows, current_datasets, visible_rows, withheld_reason
from ownhealth.genome.model import CALL_STATES, Gate, GenomeQueryError
from ownhealth.genome.sensitive import GeneSpan, carrier_spans_overlapping, norm_chrom, spans_overlapping

RSID = re.compile(r"^rs\d+$")
REGION = re.compile(r"^(chr[0-9XYM]+):(\d+)-(\d+)$", re.I)
HGVS_G = re.compile(r"^NC_0*(\d+)\.\d+:g\.(\d+)([ACGT])>([ACGT])$")
MAX_REGION_BP = 1_000_000
PAGE = 50
MIN_GQ = 20
MIN_DP = 8
CHIP = ("chip", "chip_imputed")
_NC_CHROM = {23: "chrX", 24: "chrY", 12920: "chrM"}
_BLOCK_ALTS = {"<NON_REF>", "<*>"}
UNKNOWN_NOTE = "not_on_array and not_covered mean unknown, not negative."


def _contig_names(chrom: str) -> list[str]:
    c = norm_chrom(chrom)
    bare = c[3:]
    return [c, bare] + (["MT", "chrMT"] if bare == "M" else [])


def _open(stack: ExitStack, blobs: BlobStore, data: str, index: str):
    return stack.enter_context(pysam.VariantFile(str(blobs.path(data)), index_filename=str(blobs.path(index))))


def _fetch(vf, chrom: str, start: int, end: int) -> list:
    """Records overlapping [start, end] (1-based, closed)."""
    for name in _contig_names(chrom):
        try:
            return list(vf.fetch(name, start - 1, end))
        except ValueError:
            continue
    return []


def _is_block(rec) -> bool:
    return not rec.alts or all(a in _BLOCK_ALTS for a in rec.alts)


def _sample_value(sample, key):
    try:
        v = sample[key]
    except KeyError:
        return None
    return v[0] if isinstance(v, tuple) else v


def _from_record(rec, *, basis: str) -> dict:
    """call_state, genotype and depth from the first sample of one VCF record."""
    if not rec.samples:
        return {"call_state": "no_call", "basis": basis, "reason": "no_sample_in_vcf"}
    s = rec.samples[0]
    gt = s.get("GT") or (None,)
    phased = getattr(s, "phased", False)
    if any(a is None for a in gt):
        state = "no_call"
    elif all(a == 0 for a in gt):
        state = "ref"
    else:
        state = "alt"
    alleles = rec.alleles or ()
    sep = "|" if phased else "/"
    genotype = sep.join(alleles[a] if a is not None and a < len(alleles) else "." for a in gt)
    if state != "alt":
        zyg = "ref" if state == "ref" else "unknown"
    elif len(gt) == 1:
        zyg = "hemi"
    else:
        zyg = "hom" if len(set(gt)) == 1 else "het"
    return {
        "call_state": state,
        "genotype": genotype,
        "gt": sep.join("." if a is None else str(a) for a in gt),
        "zygosity": zyg,
        "dp": _sample_value(s, "DP"),
        "gq": _sample_value(s, "GQ"),
        "basis": basis,
    }


def _pick(records, pos: int, alt: str | None):
    at = [r for r in records if r.pos == pos and not _is_block(r)]
    if alt:
        exact = [r for r in at if alt in (r.alts or ())]
        if exact:
            return exact[0]
    return at[0] if at else None


def _call_at(stack: ExitStack, blobs: BlobStore, ds: dict, site: dict, cited: dict | None, cov: dict | None) -> dict:
    """One source's answer at one site. Always returns a call_state in CALL_STATES."""
    chrom, pos, alt = site["chrom"], site["pos"], site.get("alt")
    if not ds["vcf"]:
        if cited:
            g = cited.get("genotype") or {}
            out = {k: g.get(k) for k in ("gt", "zygosity", "dp", "gq")}
            return out | {"call_state": g["call_state"], "basis": "facts_file"}
        return {"call_state": "not_covered", "basis": "absent", "reason": "raw_genome_not_on_file"}
    rec = _pick(_fetch(_open(stack, blobs, ds["vcf"], ds["tbi"]), chrom, pos, pos), pos, alt)
    if rec is not None:
        return _from_record(rec, basis="vcf")
    if ds["assay"] in CHIP:
        if not ds["sites"]:
            return {"call_state": "not_on_array", "basis": "absent", "reason": "not_in_array_calls_no_site_list"}
        probed = _fetch(_open(stack, blobs, ds["sites"], ds["sites_tbi"]), chrom, pos, pos)
        if any(r.pos == pos for r in probed):
            return {"call_state": "no_call", "basis": "chip_sites", "reason": "probed_not_called"}
        return {"call_state": "not_on_array", "basis": "chip_sites", "reason": "not_probed"}
    if cov and cov["state"] == "not_assessed":
        return {"call_state": "not_covered", "basis": "coverage_map", "reason": cov["reason"] or "not_assessed"}
    if not ds["gvcf"]:
        return {"call_state": "no_call", "basis": "absent", "reason": "variant_only_vcf_absence_unknown"}
    records = _fetch(_open(stack, blobs, ds["gvcf"], ds["gvcf_tbi"]), chrom, pos, pos)
    rec = _pick(records, pos, alt)
    if rec is not None:
        return _from_record(rec, basis="gvcf")
    blocks = [r for r in records if _is_block(r) and r.start < pos <= r.stop]
    if not blocks:
        return {"call_state": "not_covered", "basis": "gvcf_block", "reason": "no_reference_block"}
    out = _from_record(blocks[0], basis="gvcf_block")
    s = blocks[0].samples[0] if blocks[0].samples else None
    dp = (_sample_value(s, "MIN_DP") or _sample_value(s, "DP")) if s is not None else None
    if out["call_state"] == "ref" and ((out["gq"] or 0) < MIN_GQ or (dp is not None and dp < MIN_DP)):
        return out | {"call_state": "not_covered", "reason": "block_below_quality", "dp": dp}
    if out["call_state"] == "ref":
        ref = site.get("ref") or "N"
        out["genotype"] = f"{ref}/{ref}"
    return out | {"dp": dp}


def _source_label(ds: dict) -> str:
    generated = ((ds.get("producer") or {}).get("generated_utc") or ds["imported_at"].isoformat())[:10]
    return f"{ds['assay']}-{generated}"


def _resolve(conn: Connection, rows: list[dict], *, rsid: str | None, hgvs: str | None) -> dict:
    """A site {chrom, pos, ref, alt, rsid, hgvs_c, hgvs_p, gene}: the person's own facts first, then the
    curated rsID index. Never a remote lookup."""
    for r in rows:
        for v in r["fact"].get("variants") or []:
            if (rsid and v.get("rsid") == rsid) or (hgvs and hgvs in (v.get("hgvs_c"), v.get("hgvs_p"))):
                keys = ("chrom", "pos", "ref", "alt", "rsid", "hgvs_c", "hgvs_p")
                return {k: v.get(k) for k in keys} | {"chrom": norm_chrom(v["chrom"]), "gene": r["gene"]}
    if rsid:
        row = (
            conn.execute(
                text(
                    "select chrom, pos, ref, alts, gene from genome_ref_site where rsid = :r"
                    " order by chrom, pos limit 1"
                ),
                {"r": rsid},
            )
            .mappings()
            .first()
        )
        if row is None:
            raise GenomeQueryError(
                "rsid_unknown",
                f"{rsid} is not in the curated rsID index, and remote lookups are off (they leak interest).",
                "Ask by position instead: region chrN:pos-pos (GRCh38).",
            )
        alts = list(row["alts"] or [])
        return {
            "chrom": norm_chrom(row["chrom"]),
            "pos": row["pos"],
            "ref": row["ref"],
            "alt": alts[0] if len(alts) == 1 else None,
            "rsid": rsid,
            "gene": row["gene"],
        }
    m = HGVS_G.match(hgvs or "")
    if m:
        n = int(m.group(1))
        chrom = _NC_CHROM.get(n, f"chr{n}" if 1 <= n <= 22 else None)
        if chrom:
            return {"chrom": chrom, "pos": int(m.group(2)), "ref": m.group(3), "alt": m.group(4), "hgvs_g": hgvs}
    raise GenomeQueryError(
        "hgvs_unresolved",
        "Only GRCh38 genomic HGVS (NC_...:g.<pos><ref>>(alt)) or HGVS your own findings cite resolves in v0.1.",
        "Ask by rsid or by region chrN:pos-pos.",
    )


def _span_reason(gate: Gate) -> str | None:
    if gate.minor:
        return "minor"
    return None if gate.include_sensitive else "sensitive"


def _gated_spans(gate: Gate, chrom: str, start: int, end: int) -> list[GeneSpan]:
    """The fixed spans this gate withholds around [start, end]: the sensitive genes, and for a minor the
    carrier-panel genes too. Never the person's own findings (genomics.md 6, 7): an answer that depended
    on them would say what this person carries. Importer rule 9 keeps every gated finding inside them."""
    why = _span_reason(gate)
    if why is None:
        return []
    hits = spans_overlapping(chrom, start, end)
    if why == "minor":
        hits += carrier_spans_overlapping(chrom, start, end)
    return hits


def lookup_variant(
    conn: Connection,
    blobs: BlobStore,
    profile_id: UUID,
    *,
    gate: Gate,
    rsid: str | None = None,
    hgvs: str | None = None,
    region: str | None = None,
    cursor: str | None = None,
) -> dict:
    """Calls per source with `call_state` always set; sensitive genes return `withheld_sensitive`
    (or `withheld_minor`) and no genotype. Raises GenomeQueryError for input it cannot answer.

    `cursor` (regions only) is the plain record offset returned as `next_cursor`; the MCP layer may
    sign it. Reads only; the caller writes the audit row and counts the daily cap.
    """
    if sum(x is not None for x in (rsid, hgvs, region)) != 1:
        raise GenomeQueryError("bad_input", "Give exactly one of rsid, hgvs or region.")
    if rsid is not None and not RSID.match(rsid):
        raise GenomeQueryError("bad_input", f"{rsid!r} is not an rsID (rs followed by digits).")
    datasets = current_datasets(conn, profile_id)
    if not datasets:
        raise GenomeQueryError(
            "no_genome", "No genome data on file for this profile.", "Import OwnGenome output first."
        )
    rows = visible_rows(conn, profile_id)
    current_ids = {d["id"] for d in datasets}
    # findings the gate withholds never shape an answer: not the site resolution, not cited_by_findings
    rows = [r for r in rows if r["dataset_id"] in current_ids and withheld_reason(r, gate) is None]
    if region is not None:
        return _region(blobs, datasets, gate, region, cursor)

    site = _resolve(conn, rows, rsid=rsid, hgvs=hgvs)
    query = rsid or hgvs
    span_hits = spans_overlapping(site["chrom"], site["pos"], site["pos"])
    gene = site.get("gene") or (span_hits[0].gene if span_hits else None)
    variant = {k: site.get(k) for k in ("chrom", "pos", "ref", "alt", "rsid", "hgvs_c", "hgvs_p")} | {"gene": gene}
    why = _span_reason(gate) if _gated_spans(gate, site["chrom"], site["pos"], site["pos"]) else None
    if why:
        return {
            "query": query,
            "variant": variant,
            "calls": [],
            "withheld": f"withheld_{why}",
            "annotations": {},
            "cited_by_findings": [],
            "coverage": None,
            "limitations": [
                "Withheld: the minor rule keeps this site back until the person is an adult."
                if why == "minor"
                else "Withheld: this site is in a sensitive gene and the profile's opt-in for this surface is off."
            ],
        }

    cited_rows = [
        (r, v)
        for r in rows
        for v in r["fact"].get("variants") or []
        if norm_chrom(v["chrom"]) == site["chrom"] and int(v["pos"]) == site["pos"]
    ]
    cov_by_ds: dict = {}
    if gene:
        for c in coverage_rows(conn, list(current_ids)):
            if c["target"].upper() == gene.upper():
                cov_by_ds[c["dataset_id"]] = c
    calls = []
    with ExitStack() as stack:
        for ds in datasets:
            cited = next((v for r, v in cited_rows if r["dataset_id"] == ds["id"]), None)
            call = _call_at(stack, blobs, ds, site, cited, cov_by_ds.get(ds["id"]))
            if call["call_state"] not in CALL_STATES:  # the contract: exactly five values, always
                raise RuntimeError(f"call_state outside the contract: {call['call_state']!r}")
            calls.append({"source": _source_label(ds), "assay": ds["assay"]} | call | {"src": ds["src_id"]})
    clinvar = next(
        (
            {
                "variation_id": v["clinvar_variation_id"],
                "stars": (r["fact"].get("evidence") or {}).get("clinvar_stars"),
                "native": (r["fact"].get("evidence") or {}).get("native"),
            }
            for r, v in cited_rows
            if v.get("clinvar_variation_id")
        ),
        None,
    )
    newest_cov = cov_by_ds.get(datasets[0]["id"])
    limitations = []
    if any(c["call_state"] in ("not_on_array", "not_covered") for c in calls):
        limitations.append(UNKNOWN_NOTE)
    limitations.append("This lookup covers one position; it does not test other variants of the gene.")
    return {
        "query": query,
        "variant": variant,
        "calls": calls,
        "withheld": None,
        "annotations": {"clinvar": clinvar} if clinvar else {},
        "cited_by_findings": sorted({r["fact_id"] for r, _ in cited_rows}),
        "coverage": {k: newest_cov[k] for k in ("state", "reason", "fraction_callable")} if newest_cov else None,
        "limitations": limitations,
    }


def _subtract(start: int, end: int, cuts: list[tuple[int, int]]) -> list[tuple[int, int]]:
    parts = [(start, end)]
    for cs, ce in sorted(cuts):
        nxt = []
        for s, e in parts:
            if ce < s or cs > e:
                nxt.append((s, e))
                continue
            if s < cs:
                nxt.append((s, cs - 1))
            if ce < e:
                nxt.append((ce + 1, e))
        parts = nxt
    return parts


def _region(blobs: BlobStore, datasets: list[dict], gate: Gate, region: str, cursor: str | None):
    m = REGION.match(region)
    if not m:
        raise GenomeQueryError(
            "bad_input", "A region is chrN:start-end on GRCh38, for example chr19:44905000-44910000."
        )
    chrom, start, end = norm_chrom(m.group(1)), int(m.group(2)), int(m.group(3))
    if start < 1 or end < start:
        raise GenomeQueryError("bad_input", "The region end must not be before its start.")
    if end - start + 1 > MAX_REGION_BP:
        raise GenomeQueryError("region_too_large", "A region spans at most 1 Mb.", "Split it into smaller regions.")
    try:
        offset = int(cursor) if cursor else 0
    except ValueError:
        raise GenomeQueryError("bad_input", "Unknown cursor.") from None
    clipped = []
    if _span_reason(gate):
        for s in _gated_spans(gate, chrom, start, end):
            clipped.append(
                {
                    "gene": s.gene,
                    "start": max(start, s.start),
                    "end": min(end, s.end),
                    "reason": f"withheld_{_span_reason(gate)}",
                }
            )
    parts = _subtract(start, end, [(c["start"], c["end"]) for c in clipped])
    records = []
    with ExitStack() as stack:
        for ds in datasets:
            if not ds["vcf"]:
                continue
            vf = _open(stack, blobs, ds["vcf"], ds["tbi"])
            for s, e in parts:
                for rec in _fetch(vf, chrom, s, e):
                    if _is_block(rec) or not (s <= rec.pos <= e):
                        continue
                    records.append(
                        {"chrom": chrom, "pos": rec.pos, "id": rec.id, "ref": rec.ref, "alts": list(rec.alts or ())}
                        | {"source": _source_label(ds), "assay": ds["assay"]}
                        | _from_record(rec, basis="vcf")
                        | {"src": ds["src_id"]}
                    )
    records.sort(key=lambda r: (r["pos"], r["source"]))
    page = records[offset : offset + PAGE]
    limitations = ["Region answers list the records in the VCF; a position with no record is not listed."]
    if any(not ds["vcf"] for ds in datasets):
        limitations.append("A source without a VCF on file is not part of region answers.")
    return {
        "query": region,
        "region": {"chrom": chrom, "start": start, "end": end},
        "records": page,
        "clipped": clipped,
        "next_cursor": str(offset + PAGE) if offset + PAGE < len(records) else None,
        "limitations": limitations,
    }


def load_ref_sites(conn: Connection, rows, *, source: str) -> int:
    """Load curated rsID rows (rsid, chrom, pos, ref, alts, gene) into the index. Returns the count."""
    n = 0
    for r in rows:
        conn.execute(
            text(
                "insert into genome_ref_site (rsid, chrom, pos, ref, alts, gene, source) values (:rsid, :chrom,"
                " :pos, :ref, :alts, :gene, :source) on conflict (rsid, chrom, pos) do update set ref = excluded.ref,"
                " alts = excluded.alts, gene = excluded.gene, source = excluded.source"
            ),
            {
                "rsid": r["rsid"],
                "chrom": norm_chrom(r["chrom"]),
                "pos": int(r["pos"]),
                "ref": r["ref"],
                "alts": list(r.get("alts") or []),
                "gene": r.get("gene"),
                "source": source,
            },
        )
        n += 1
    return n
