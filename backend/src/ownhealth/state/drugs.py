"""check_drug: PGx facts + current medications + relevant labs + CYP2D6 phenoconversion (genomics.md 8).

Guidance is quoted verbatim from the bundled CPIC snapshot (`rules/cpic/snapshot.json`, CC0) with its
classification, the pair's CPIC level and the guideline URL. Our own words never advise: they say what
the record holds and what is not assessed.
"""

from __future__ import annotations

from datetime import date
from decimal import Decimal
from uuid import UUID

from sqlalchemy import Connection

from ownhealth import genome
from ownhealth.core.access import today_utc
from ownhealth.core.records import active_medications
from ownhealth.state.engine import _finding, drug_key, evaluate, med_drug, num
from ownhealth.state.pack import Pack, cpic_snapshot, dec, load_pack

AS_GENES = ("CYP2D6", "CYP2C9", "DPYD")  # CPIC looks these up by activity score
NO_RESULT = "No Result"
STRENGTH_ORDER = {"strong": 0, "moderate": 1}


class UnknownDrug(LookupError):
    code = "drug_unknown"

    def __init__(self, name: str):
        super().__init__(f"Unknown drug {name!r}.")
        self.name = name


def _drug_ids() -> dict[str, str]:
    snap = cpic_snapshot()
    return {d["name"]: d["drugid"] for d in snap["drugs"] + snap.get("drugs_ids_only", [])}


def resolve_drug(name: str) -> dict | None:
    """es/en generic or brand name, or a CPIC drug id, to {cpic_drug_id, name}."""
    if not name or not name.strip():
        return None
    ids = _drug_ids()
    raw = name.strip()
    by_id = {v.casefold(): k for k, v in ids.items()}
    key = by_id.get(raw.casefold()) or drug_key(load_pack(), raw)
    if key is None:
        return None
    return {"cpic_drug_id": ids.get(key), "name": key}


def _score_key(score: Decimal) -> str:
    s = format(score.normalize(), "f")
    return s if "." in s else s + ".0"


def _norm_value(gene: str, v: str | None) -> str:
    v = (v or "").strip()
    if v.upper().startswith(gene.upper()):
        v = v[len(gene) :]
    return v.casefold().strip()


def _matches(rec: dict, person: dict[str, dict]) -> bool:
    """Does a CPIC recommendation's lookup key fit the person's per-gene values?"""
    for gene, key in rec["lookupkey"].items():
        p = person.get(gene)
        if p is None:
            if key != NO_RESULT:
                return False
            continue
        score = p.get("activity_score")
        if gene in AS_GENES and score is not None:
            if key.startswith("≥"):
                if score < Decimal(key[1:]):
                    return False
            elif key in ("n/a", NO_RESULT) or Decimal(key) != score:
                return False
        else:
            phen = _norm_value(gene, rec.get("phenotypes", {}).get(gene) or rec.get("allelestatus", {}).get(gene))
            if _norm_value(gene, key) != _norm_value(gene, p.get("phenotype")) and phen != _norm_value(
                gene, p.get("phenotype")
            ):
                return False
    return True


def _guidance(snap: dict, drug_id: str, person: dict[str, dict]) -> list[dict]:
    recs = [r for r in snap["recommendations"] if r["drugid"] == drug_id and _matches(r, person)]
    exact = [r for r in recs if not any(str(v).startswith("≥") for v in r["lookupkey"].values())]
    if not exact and recs:  # only "≥" bands matched: the highest one stands in for the score
        top = max(Decimal(v[1:]) for r in recs for v in r["lookupkey"].values() if str(v).startswith("≥"))
        exact = [r for r in recs if any(str(v) == f"≥{_score_key(top)}" for v in r["lookupkey"].values())]
    recs = exact
    useful = [r for r in recs if r["classification"] != "No Recommendation"]
    return useful or recs


def _pairs(snap: dict, key: str) -> list[dict]:
    return [p for p in snap["pairs"] if p["drugname"] == key]


def _band(pack: Pack, score: Decimal) -> str | None:
    for b in pack.phenoconversion["rule"]["bands"]:  # min/max are inclusive, above/below exclusive
        lo_ok = score > Decimal(b["above"]) if "above" in b else score >= Decimal(b["min"])
        hi = dec(b["below"]) if "below" in b else dec(b.get("max"))
        hi_ok = hi is None or (score < hi if "below" in b else score <= hi)
        if lo_ok and hi_ok:
            return b["phenotype"]
    return None


def _inhibitors(pack: Pack, gene: str, meds: list[dict]) -> list[tuple[str, dict]]:
    table = pack.phenoconversion.get("inhibitors", {}).get(gene, {})
    out = []
    for med in meds:
        k = med_drug(pack, med)
        for strength in ("strong", "moderate"):
            if k and k in table.get(strength, []):
                out.append((strength, med))
                break
    return sorted(out, key=lambda x: (STRENGTH_ORDER[x[0]], x[1]["name"].lower()))


def _med_label(m: dict) -> str:
    return " ".join(x for x in (m["name"], m.get("strength")) if x)


def check_drug(
    conn: Connection,
    profile_id: UUID,
    drug: str,
    *,
    scopes: frozenset[str],
    lang: str = "en",
    as_of: date | None = None,
) -> dict:
    pack, snap = load_pack(), cpic_snapshot()
    lang = lang if lang in ("en", "es") else "en"
    resolved = resolve_drug(drug)
    if resolved is None:
        raise UnknownDrug(drug)
    key, drug_id = resolved["name"], resolved["cpic_drug_id"]
    as_of = as_of or today_utc()
    t = pack.text
    has_genome, has_records, has_labs = ("read:genome" in scopes, "read:records" in scopes, "read:labs" in scopes)
    by_scope = [d for d, ok in (("genome", has_genome), ("records", has_records), ("labs", has_labs)) if not ok]

    pairs = _pairs(snap, key)
    genes = sorted({p["genesymbol"] for p in pairs})
    level = {p["genesymbol"]: p["cpiclevel"] for p in pairs}
    gid = next((d.get("guidelineid") for d in snap["drugs"] if d["name"] == key), None)
    guideline = next((g for g in snap["guidelines"] if g["id"] == gid), None)
    url = guideline["url"] if guideline else (pairs[0]["guidelineurl"] if pairs else None)

    meds = []
    if has_records:
        meds = [
            m
            for m in active_medications(conn, profile_id, on=as_of, include_sensitive="read:sensitive" in scopes)
            if m["kind"] == "medication"
        ]
    currently_taking = any(med_drug(pack, m) == key for m in meds) if has_records else None

    not_assessed: list[str] = []
    pgx: list[dict] = []
    findings: dict[str, dict] = {}
    if has_genome and pairs and drug_id:
        for raw in genome.pgx_for_drug(conn, profile_id, cpic_drug_id=drug_id, as_of=as_of):
            f = _finding(raw)
            f["confidence"] = raw.get("confidence")
            if f["gene"] in genes and f["gene"] not in findings:
                findings[f["gene"]] = f
        for gene in genes:
            f = findings.get(gene)
            if f is None:
                not_assessed.append(t("drug.na.gene", lang, gene=gene))
                continue
            pgx.append(
                {
                    "gene": gene,
                    "diplotype": f["diplotype"],
                    "phenotype": f["phenotype"],
                    "activity_score": num(f["activity_score"]),
                    "evidence": f["evidence"],
                    "confidence": f["confidence"],
                    "call_state": f["call_state"],
                    "cpic_level": level.get(gene),
                    "src": f["src"],
                }
            )
    elif has_genome and not pairs:
        not_assessed.append(t("drug.na.no_pair", lang, drug=key))

    # phenoconversion (needs genome and records)
    phenoconversion: list[dict] = []
    functional: dict[str, dict] = {}
    rule = pack.phenoconversion.get("rule", {})
    if has_genome and has_records:
        for gene in genes:
            inhibitors = _inhibitors(pack, gene, meds)
            if not inhibitors:
                continue
            strength, med = inhibitors[0]
            f = findings.get(gene)
            entry = {
                "gene": gene,
                "by": _med_label(med),
                "inhibitor_strength": strength,
                "computed": False,
                "functional_phenotype": None,
                "activity_score": None,
                "activity_score_functional": None,
                "rule": rule.get("id") if gene == rule.get("gene") else None,
                "basis": pack.phenoconversion.get("inhibitors_cite"),
                "src": med["src_id"],
            }
            strength_txt = t(f"strength.{strength}", lang)
            if gene == rule.get("gene") and f and f["activity_score"] is not None:
                score = f["activity_score"]
                fscore = score * Decimal(rule["multipliers"][strength])
                fphen = _band(pack, fscore)
                entry |= {
                    "computed": True,
                    "functional_phenotype": fphen,
                    "activity_score": num(score),
                    "activity_score_functional": _score_key(fscore),
                    "text": t(
                        "drug.pheno.computed",
                        lang,
                        by=_med_label(med),
                        strength=strength_txt,
                        score=num(score),
                        score_functional=_score_key(fscore),
                        functional_phenotype=fphen,
                        phenotype=f["phenotype"],
                    ),  # fmt: skip
                    "based_on": [f["src"], med["src_id"]],
                }
                if fphen and _norm_value(gene, fphen) != _norm_value(gene, f["phenotype"]):
                    functional[gene] = {"phenotype": fphen, "activity_score": fscore}
            elif gene == rule.get("gene"):
                entry["text"] = t("drug.pheno.no_score", lang, by=_med_label(med), strength=strength_txt)
            else:
                entry["text"] = t("drug.pheno.not_computed", lang, by=_med_label(med), strength=strength_txt, gene=gene)
            phenoconversion.append(entry)

    # guidance, verbatim from the CPIC snapshot
    guidance: list[dict] = []
    if findings and drug_id:
        person = {g: {"phenotype": f["phenotype"], "activity_score": f["activity_score"]} for g, f in findings.items()}
        variants = [("genotype", person)]
        if functional:
            variants.append(("functional", {**person, **functional}))
        seen = set()

        def based_on(kind: str, r: dict) -> list[str]:
            srcs = sorted({findings[g]["src"] for g in r["lookupkey"] if g in findings})
            via = next((p["src"] for p in phenoconversion if p["computed"]), None)
            return srcs + [via] if kind == "functional" and via else srcs

        for kind, values in variants:
            recs = _guidance(snap, drug_id, values)
            for r in recs:
                phen = "; ".join(
                    f"{g} {values[g]['phenotype']}" if g in values else f"{g} {NO_RESULT}"
                    for g in sorted(r["lookupkey"])
                )
                applies = t(f"drug.applies.{kind}", lang, phenotype=phen)
                k = (kind, r["population"], r["drugrecommendation"])
                if k in seen:
                    continue
                seen.add(k)
                guidance.append(
                    {
                        "source": "CPIC",
                        "level": "/".join(sorted({level.get(g, "") for g in r["lookupkey"] if level.get(g)})) or None,
                        "strength": r["classification"],
                        "applies_to": applies,
                        "population": (r.get("population") or "").strip() or None,
                        "quote": r["drugrecommendation"],
                        "url": url,
                        "recommendation_id": r["id"],
                        "origin": "cpic_snapshot",
                        "license": "CC0-1.0",
                        "based_on": based_on(kind, r),
                    }
                )
        if not guidance:
            if key == "warfarin":
                not_assessed.append(t("drug.na.warfarin", lang))
            else:
                for gene, f in findings.items():
                    not_assessed.append(t("drug.na.no_rec", lang, gene=gene, phenotype=f["phenotype"]))

    # current medications, each with how it relates to this drug
    current = []
    for m in meds:
        k = med_drug(pack, m)
        relation, relation_text = "not_assessed", t("drug.relation.not_assessed", lang)
        if k == key:
            relation, relation_text = "same_drug", t("drug.relation.same", lang)
        else:
            for gene in genes:
                hit = next((s for s, med in _inhibitors(pack, gene, [m])), None)
                if hit:
                    relation = f"inhibitor:{gene}:{hit}"
                    relation_text = t("drug.relation.inhibitor", lang, gene=gene, strength=t(f"strength.{hit}", lang))
                    break
        current.append(
            {
                "name": _med_label(m),
                "since": m["started"].isoformat() if m["started"] else None,
                "relation": relation,
                "relation_text": relation_text,
                "src": m["src_id"],
            }
        )

    labs = []
    if has_labs:
        for item in pack.drug_labs.get(key, []):
            group = item["lab"]
            ev = evaluate(conn, profile_id, as_of=as_of, pack=pack, only_group=group)
            marker = ev.markers.get(group)
            p = marker.latest if marker else None
            labs.append(
                {
                    "name": pack.name(group, lang),
                    "alias_group": group,
                    "value": p.shown if p else None,
                    "unit": p.unit if p else None,
                    "date": p.date.isoformat() if p else None,
                    "flag": p.flag if p else None,
                    "status": marker.status if marker else "nodata",
                    "stale": marker.stale if marker else False,
                    "reason": item["reason"][lang] if isinstance(item["reason"], dict) else item["reason"],
                    "src": p.src if p else None,
                }
            )

    not_assessed.append(t("drug.na.ddi", lang))
    limitations = [t("drug.lim.snapshot", lang, date=snap["retrieved"])]
    if phenoconversion:
        limitations.append(t("drug.lim.phenoconversion", lang))
    if len({g["population"] for g in guidance}) > 1:
        limitations.append(t("drug.lim.indication", lang))
    return {
        "drug": {
            "name": key.capitalize(),
            "query": drug,
            "cpic_drug_id": drug_id,
            "codes_as_received": sorted(
                {f"{m['code_system']}:{m['code']}" for m in meds if med_drug(pack, m) == key and m.get("code")}
            ),
            "currently_taking": currently_taking,
            "genes": genes,
        },  # fmt: skip
        "pgx": pgx,
        "phenoconversion": phenoconversion,
        "current_medications_relevant": current,
        "relevant_labs": labs,
        "guidance": guidance,
        "not_assessed": not_assessed,
        "limitations": limitations,
        "withheld": {"by_scope": by_scope},
        "db_versions": {"cpic": snap["retrieved"]},
        "notice": t("drug.notice", lang),
    }
