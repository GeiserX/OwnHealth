//! The couple carrier view (genomics.md section 10): two adult partners' carrier facts joined by gene.
//!
//! Each side is one profile's current carrier facts, plus its `monogenic_pathogenic` facts in an
//! autosomal recessive (AR) or X-linked (XL) gene. Inheritance comes from the fact, else from
//! `data/genome/carrier_genes.tsv`. A side without a carrier fact in the gene is, in this order:
//! "a finding in this gene is listed under Findings" (any other current finding there, never read as
//! not detected); "not detected" (a carrier or ACMG SF coverage row of a sequencing source); "not
//! detected, coverage not stated" (a sequencing source with no such row); "not assessed beyond the
//! probed sites" (a chip); "not assessed"; or "no genome data" (no current dataset).
//!
//! A 1 in 4 figure is printed only when both sides carry a variant in the same AR gene and both are
//! confirmed: a sequencing assay (WGS, WES, long reads) with a confidence above `unconfirmed`, or a
//! finding marked `confirmed`. A sensitive gene is named only when both gates open sensitive results;
//! otherwise the row reads "a withheld result" with no gene. The route checks access, age and
//! deceased before calling this. Wording and citations live in `data/genome/couple.json`.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::LazyLock;

use ownhealth_core::{Error, Result};
use serde_json::{Value, json};
use sqlx::PgConnection;
use uuid::Uuid;

use crate::Gate;
use crate::findings::{FindingRow, coverage_rows, current_datasets, visible_rows};
use crate::sensitive::is_sensitive_gene;

const COUPLE_JSON: &str = include_str!("../../../../data/genome/couple.json");
const CARRIER_TSV: &str = include_str!("../../../../data/genome/carrier_genes.tsv");
/// Assays that read a whole gene, so a result from them needs no clinical confirmation for the figure.
const SEQUENCING: [&str; 3] = ["wgs", "wes", "long_read"];

struct Data {
    version: String,
    cites: HashMap<String, String>,
    templates: Value,
}

#[allow(clippy::expect_used)]
static DATA: LazyLock<Data> = LazyLock::new(|| {
    let doc: Value = serde_json::from_str(COUPLE_JSON).expect("data/genome/couple.json is valid JSON");
    let cites = doc["rows"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|r| Some((r["id"].as_str()?.to_string(), r["cite"].as_str()?.to_string())))
        .collect();
    Data {
        version: doc["version"].as_str().unwrap_or_default().to_string(),
        cites,
        templates: doc["templates"].clone(),
    }
});

/// gene (upper case) -> "AR" or "XL", from the carrier panel.
static PANEL_INHERITANCE: LazyLock<HashMap<String, String>> = LazyLock::new(|| {
    CARRIER_TSV
        .lines()
        .filter(|l| !l.is_empty() && !l.starts_with('#') && !l.starts_with("gene\t"))
        .filter_map(|l| {
            let cols: Vec<&str> = l.split('\t').collect();
            Some((cols.first()?.to_uppercase(), cols.get(4)?.trim().to_string()))
        })
        .collect()
});

/// The version of the wording file, for the answer's `rules`.
pub fn rules_version() -> &'static str {
    &DATA.version
}

fn cite(id: &str) -> Value {
    DATA.cites.get(id).map_or(Value::Null, |c| json!(c))
}

/// One template in `lang` ("es" or "en"; anything else reads English) with `{name}` filled in.
pub fn text(key: &str, lang: &str, vars: &[(&str, &str)]) -> String {
    let lang = if lang == "es" { "es" } else { "en" };
    let mut s = DATA.templates[key][lang].as_str().unwrap_or(key).to_string();
    for (k, v) in vars {
        s = s.replace(&format!("{{{k}}}"), v);
    }
    s
}

/// Every key of the wording file, for the wording gate.
pub fn template_keys() -> Vec<String> {
    DATA.templates
        .as_object()
        .map(|m| m.keys().cloned().collect())
        .unwrap_or_default()
}

/// One side's carrier facts in one gene, merged across its sources.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Carried {
    pub inheritance: String,
    pub condition: Option<String>,
    pub confirmed: bool,
    pub assays: BTreeSet<String>,
    pub zygosity: Option<String>,
    /// The distinct variants (VRS id, else position) across the facts: more than one may be two altered
    /// copies of the gene.
    pub variants: BTreeSet<String>,
    pub src: Vec<String>,
    pub fact_ids: Vec<String>,
    pub sensitive: bool,
}

/// One side's other current findings in one gene (not carrier facts): enough to say they exist.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Other {
    pub assays: BTreeSet<String>,
    pub src: Vec<String>,
    pub fact_ids: Vec<String>,
    pub sensitive: bool,
}

/// What one side holds for a gene where it has no finding at all.
#[derive(Debug, Clone, PartialEq)]
pub enum Absent {
    /// A carrier or ACMG SF coverage row says the gene was assessed and nothing was found; `partial`
    /// when only part was callable.
    NotDetected {
        partial: bool,
        assay: String,
    },
    /// A sequencing source with no carrier or ACMG SF coverage row for the gene: nothing found, but how
    /// well the gene was read is not stated.
    NotStated {
        assay: String,
    },
    /// Only a chip covers the gene: it read some positions, not the gene.
    ProbedOnly {
        assay: String,
    },
    NotAssessed,
    /// No current dataset at all.
    NoGenome,
}

impl Absent {
    /// Lower is a stronger statement; the strongest source of a side wins.
    fn strength(&self) -> u8 {
        match self {
            Absent::NotDetected { partial: false, .. } => 0,
            Absent::NotDetected { partial: true, .. } => 1,
            Absent::NotStated { .. } => 2,
            Absent::ProbedOnly { .. } => 3,
            Absent::NotAssessed => 4,
            Absent::NoGenome => 5,
        }
    }
}

/// One side of a row.
#[derive(Debug, Clone, PartialEq)]
pub enum Side<'a> {
    Carrier(&'a Carried),
    /// Another finding in the gene: never read as "not detected".
    Other(&'a Other),
    Absent(Absent),
}

/// Whether a coverage row speaks for carrier status. A sequencing source counts only its carrier and
/// ACMG SF rows (a CPIC row covers the star-allele sites, not the gene); a chip reads probed sites
/// whatever the panel.
pub fn coverage_counts(assay: &str, panel: Option<&str>) -> bool {
    assay.starts_with("chip") || panel.is_some_and(|p| p == "carrier" || p.starts_with("acmg_sf"))
}

/// Everything about one profile the join reads.
#[derive(Debug, Clone, Default)]
pub struct Person {
    pub name: String,
    pub carried: BTreeMap<String, Carried>,
    /// Genes with another current finding (any category) that is not a carrier fact.
    pub other: BTreeMap<String, Other>,
    /// Per current dataset: assay and the gene coverage rows that count (`coverage_counts`), gene upper
    /// case -> state. Empty when the person has no current dataset.
    pub coverage: Vec<(String, HashMap<String, String>)>,
}

impl Person {
    /// What this person's coverage says about `gene` when no finding names it.
    pub fn absent(&self, gene: &str) -> Absent {
        let mut best = Absent::NoGenome;
        for (assay, genes) in &self.coverage {
            let chip = assay.starts_with("chip");
            let here = match (genes.get(gene).map(String::as_str), chip) {
                (Some("assessed"), false) => Absent::NotDetected {
                    partial: false,
                    assay: assay.clone(),
                },
                (Some("partial"), false) => Absent::NotDetected {
                    partial: true,
                    assay: assay.clone(),
                },
                (Some("assessed" | "partial"), true) => Absent::ProbedOnly { assay: assay.clone() },
                (None, false) if SEQUENCING.contains(&assay.as_str()) => Absent::NotStated { assay: assay.clone() },
                _ => Absent::NotAssessed,
            };
            if here.strength() < best.strength() {
                best = here;
            }
        }
        best
    }

    fn side(&self, gene: &str) -> Side<'_> {
        match (self.carried.get(gene), self.other.get(gene)) {
            (Some(c), _) => Side::Carrier(c),
            (None, Some(o)) => Side::Other(o),
            (None, None) => Side::Absent(self.absent(gene)),
        }
    }
}

/// A current, not refuted finding that reads a variant (or a result with no call state).
fn is_present(r: &FindingRow) -> bool {
    r.status == "current" && r.verification_status != "refuted" && matches!(r.call_state.as_deref(), None | Some("alt"))
}

/// The inheritance a fact joins under, or None when it is not a carrier fact for this view. A declared
/// AR or XL wins; with none declared the carrier panel decides; a monogenic fact that declares another
/// mode (a dominant form of the gene) stays out.
fn inheritance_of(category: &str, gene: &str, declared: &str) -> Option<String> {
    if !matches!(category, "carrier" | "monogenic_pathogenic") {
        return None;
    }
    let inheritance = match declared {
        "AR" | "XL" => declared.to_string(),
        _ if category == "monogenic_pathogenic" && !declared.is_empty() => return None,
        _ => PANEL_INHERITANCE.get(gene)?.clone(),
    };
    matches!(inheritance.as_str(), "AR" | "XL").then_some(inheritance)
}

fn is_carrier_row(r: &FindingRow) -> Option<String> {
    if !is_present(r) {
        return None;
    }
    let gene = r.gene.as_deref()?.trim().to_uppercase();
    let declared = r.fact.0["condition"]["inheritance"].as_str().unwrap_or_default();
    inheritance_of(&r.category, &gene, declared)
}

/// Read one profile's carrier facts and gene coverage. `name` is how the texts call this person.
pub async fn load_person(conn: &mut PgConnection, profile_id: Uuid, name: &str) -> Result<Person> {
    let mut carried: BTreeMap<String, Carried> = BTreeMap::new();
    let mut other: BTreeMap<String, Other> = BTreeMap::new();
    for r in visible_rows(&mut *conn, profile_id, None).await? {
        let Some(gene) = r
            .gene
            .as_deref()
            .map(|g| g.trim().to_uppercase())
            .filter(|g| !g.is_empty())
        else {
            continue;
        };
        let Some(inheritance) = is_carrier_row(&r) else {
            if is_present(&r) {
                let o = other.entry(gene.clone()).or_default();
                o.assays.insert(r.assay.clone());
                o.sensitive |= r.sensitive || is_sensitive_gene(Some(&gene));
                if !o.src.contains(&r.src_id) {
                    o.src.push(r.src_id.clone());
                }
                if !o.fact_ids.contains(&r.fact_id) {
                    o.fact_ids.push(r.fact_id.clone());
                }
            }
            continue;
        };
        let c = carried.entry(gene.clone()).or_default();
        c.inheritance = inheritance;
        if c.condition.is_none() {
            c.condition = r.fact.0["condition"]["name"].as_str().map(str::to_string);
        }
        c.confirmed |= r.verification_status == "confirmed"
            || (SEQUENCING.contains(&r.assay.as_str()) && r.confidence != "unconfirmed");
        c.assays.insert(r.assay.clone());
        if c.zygosity.is_none() {
            c.zygosity = r.zygosity.clone();
        }
        for v in r.fact.0["variants"].as_array().into_iter().flatten() {
            let key = v["vrs_id"]
                .as_str()
                .map_or_else(|| format!("{}:{}", v["chrom"], v["pos"]), str::to_string);
            c.variants.insert(key);
        }
        c.sensitive |= r.sensitive || is_sensitive_gene(Some(&gene));
        if !c.src.contains(&r.src_id) {
            c.src.push(r.src_id.clone());
        }
        if !c.fact_ids.contains(&r.fact_id) {
            c.fact_ids.push(r.fact_id.clone());
        }
    }
    let datasets = current_datasets(&mut *conn, profile_id, None).await?;
    let ids: Vec<Uuid> = datasets.iter().map(|d| d.id).collect();
    let rows = coverage_rows(&mut *conn, &ids).await?;
    let coverage = datasets
        .iter()
        .map(|d| {
            let genes = rows
                .iter()
                .filter(|c| c.dataset_id == d.id && c.scope == "gene" && coverage_counts(&d.assay, c.panel.as_deref()))
                .map(|c| (c.target.to_uppercase(), c.state.clone()))
                .collect();
            (d.assay.clone(), genes)
        })
        .collect();
    other.retain(|g, _| !carried.contains_key(g));
    Ok(Person {
        name: name.to_string(),
        carried,
        other,
        coverage,
    })
}

fn side_out(side: &Side<'_>, lang: &str) -> Value {
    let plain = |state: &str, assay: Option<&String>, coverage: Value| {
        json!({
            "state": state,
            "label": text(&format!("side.{state}"), lang, &[]),
            "confirmed": Value::Null,
            "assays": assay.into_iter().collect::<Vec<_>>(),
            "zygosity": Value::Null,
            "coverage": coverage,
            "src": [],
        })
    };
    match side {
        Side::Carrier(c) => json!({
            "state": "carrier",
            "label": text(if c.confirmed { "side.carrier" } else { "side.carrier_unconfirmed" }, lang, &[]),
            "confirmed": c.confirmed,
            "assays": c.assays,
            "zygosity": c.zygosity,
            "coverage": Value::Null,
            "src": c.src,
        }),
        Side::Absent(Absent::NotDetected { partial, assay }) => json!({
            "state": "not_detected",
            "label": text(if *partial { "side.not_detected_partial" } else { "side.not_detected" }, lang, &[]),
            "confirmed": Value::Null,
            "assays": [assay],
            "zygosity": Value::Null,
            "coverage": if *partial { "partial" } else { "assessed" },
            "src": [],
        }),
        Side::Other(o) => json!({
            "state": "other_finding",
            "label": text("side.other_finding", lang, &[]),
            "confirmed": Value::Null,
            "assays": o.assays,
            "zygosity": Value::Null,
            "coverage": Value::Null,
            "src": o.src,
        }),
        Side::Absent(Absent::NotStated { assay }) => plain("not_stated", Some(assay), json!("not_stated")),
        Side::Absent(Absent::ProbedOnly { assay }) => plain("probed_only", Some(assay), json!("partial")),
        Side::Absent(Absent::NotAssessed) => plain("not_assessed", None, json!("not_assessed")),
        Side::Absent(Absent::NoGenome) => plain("no_genome", None, Value::Null),
    }
}

/// The kind of row, its wording key and its citation id, for two sides of one gene.
fn kind(inheritance: &str, a: &Side<'_>, b: &Side<'_>) -> (&'static str, &'static str) {
    let carrier = |s: &Side<'_>| matches!(s, Side::Carrier(_));
    let confirmed = |s: &Side<'_>| matches!(s, Side::Carrier(c) if c.confirmed);
    if inheritance == "XL" {
        return (if carrier(a) && carrier(b) { "xl_both" } else { "xl_one" }, "couple.xl");
    }
    // The 1 in 4 figure assumes one altered copy in each partner: one heterozygous variant per side.
    let single_het =
        |s: &Side<'_>| matches!(s, Side::Carrier(c) if c.zygosity.as_deref() == Some("het") && c.variants.len() <= 1);
    if carrier(a) && carrier(b) {
        let k = if !(confirmed(a) && confirmed(b)) {
            "ar_both_unconfirmed"
        } else if single_het(a) && single_het(b) {
            "ar_both"
        } else {
            "ar_both_no_figure"
        };
        return (k, "couple.ar");
    }
    let other = if carrier(a) { b } else { a };
    let k = match other {
        Side::Other(_) => "ar_one_other_finding",
        Side::Absent(Absent::NotDetected { .. }) => "ar_one_not_detected",
        Side::Absent(Absent::NotStated { .. }) => "ar_one_not_stated",
        Side::Absent(Absent::ProbedOnly { .. }) => "ar_one_probed",
        Side::Absent(Absent::NoGenome) => "ar_one_no_genome",
        _ => "ar_one_not_assessed",
    };
    (k, "couple.residual")
}

/// Order of rows: shared genes first, then X-linked, then one-side genes, withheld last.
fn rank(kind: &str) -> u8 {
    match kind {
        "ar_both" => 0,
        "ar_both_unconfirmed" | "ar_both_no_figure" => 1,
        "xl_both" | "xl_one" => 2,
        "withheld" => 9,
        _ => 3,
    }
}

/// The join's answer and what each side gave to it (for the audit).
#[derive(Debug, Clone, Default)]
pub struct Couple {
    pub rows: Vec<Value>,
    pub withheld: usize,
    /// Fact ids and src ids shown, per side.
    pub a_used: (Vec<String>, Vec<String>),
    pub b_used: (Vec<String>, Vec<String>),
}

/// Join two people gene by gene. `gate_a` and `gate_b` are each side's gate for the caller.
pub fn join(a: &Person, b: &Person, gate_a: &Gate, gate_b: &Gate, lang: &str) -> Result<Couple> {
    if gate_a.minor || gate_b.minor {
        return Err(Error::Invalid("the couple view never includes a minor".into()));
    }
    let open = gate_a.sensitive && gate_b.sensitive;
    let genes: BTreeSet<&String> = a.carried.keys().chain(b.carried.keys()).collect();
    let mut out = Couple::default();
    let mut rows: Vec<(u8, String, Value)> = Vec::new();
    for gene in genes {
        let (sa, sb) = (a.side(gene), b.side(gene));
        let carried: Vec<&Carried> = [&sa, &sb]
            .into_iter()
            .filter_map(|s| match s {
                Side::Carrier(c) => Some(*c),
                Side::Other(_) | Side::Absent(_) => None,
            })
            .collect();
        let others: Vec<&Other> = [&sa, &sb]
            .into_iter()
            .filter_map(|s| match s {
                Side::Other(o) => Some(*o),
                Side::Carrier(_) | Side::Absent(_) => None,
            })
            .collect();
        let inheritance = carried.first().map(|c| c.inheritance.clone()).unwrap_or_default();
        let sensitive =
            carried.iter().any(|c| c.sensitive) || others.iter().any(|o| o.sensitive) || is_sensitive_gene(Some(gene));
        if sensitive && !open {
            out.withheld += 1;
            rows.push((
                rank("withheld"),
                String::new(),
                json!({
                    "kind": "withheld", "gene": null, "inheritance": null, "condition": null,
                    "a": null, "b": null, "risk": null,
                    "text": text("row.withheld", lang, &[]), "counsel": text("counsel", lang, &[]), "cite": null,
                }),
            ));
            continue;
        }
        for (side, used) in [(&sa, &mut out.a_used), (&sb, &mut out.b_used)] {
            match side {
                Side::Carrier(c) => {
                    used.0.extend(c.fact_ids.iter().cloned());
                    used.1.extend(c.src.iter().cloned());
                }
                Side::Other(o) => {
                    used.0.extend(o.fact_ids.iter().cloned());
                    used.1.extend(o.src.iter().cloned());
                }
                Side::Absent(_) => {}
            }
        }
        let (k, cite_id) = kind(&inheritance, &sa, &sb);
        let (carrier, other) = if matches!(sa, Side::Carrier(_)) {
            (&a.name, &b.name)
        } else {
            (&b.name, &a.name)
        };
        let vars = [
            ("gene", gene.as_str()),
            ("carrier", carrier.as_str()),
            ("other", other.as_str()),
        ];
        rows.push((
            rank(k),
            gene.clone(),
            json!({
                "kind": k,
                "gene": gene,
                "inheritance": inheritance,
                "condition": carried.iter().find_map(|c| c.condition.clone()),
                "a": side_out(&sa, lang),
                "b": side_out(&sb, lang),
                "risk": if k == "ar_both" { json!("1/4") } else { Value::Null },
                "text": text(&format!("row.{k}"), lang, &vars),
                "counsel": text("counsel", lang, &[]),
                "cite": cite(cite_id),
            }),
        ));
    }
    rows.sort_by(|x, y| (x.0, &x.1).cmp(&(y.0, &y.1)));
    out.rows = rows.into_iter().map(|(_, _, v)| v).collect();
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn carried(inheritance: &str, assay: &str, confirmed: bool) -> Carried {
        Carried {
            inheritance: inheritance.into(),
            confirmed,
            zygosity: Some("het".into()),
            assays: [assay.to_string()].into(),
            src: vec![format!("src_{assay}")],
            fact_ids: vec!["f_0000000000000001".into()],
            ..Carried::default()
        }
    }

    fn person(name: &str, genes: &[(&str, Carried)], coverage: &[(&str, &[(&str, &str)])]) -> Person {
        Person {
            name: name.into(),
            carried: genes.iter().map(|(g, c)| ((*g).to_string(), c.clone())).collect(),
            other: BTreeMap::new(),
            coverage: coverage
                .iter()
                .map(|(assay, genes)| {
                    (
                        (*assay).to_string(),
                        genes
                            .iter()
                            .map(|(g, s)| ((*g).to_string(), (*s).to_string()))
                            .collect(),
                    )
                })
                .collect(),
        }
    }

    fn adult(sensitive: bool) -> Gate {
        Gate::new(sensitive, false)
    }

    #[test]
    fn two_confirmed_carriers_of_one_recessive_gene_give_the_one_in_four_row() -> Result<()> {
        let alex = person("Alex", &[("GJB2", carried("AR", "wgs", true))], &[]);
        let sam = person("Sam", &[("GJB2", carried("AR", "wes", true))], &[]);
        let c = join(&alex, &sam, &adult(false), &adult(false), "en")?;
        assert_eq!(c.rows[0]["kind"], "ar_both");
        assert_eq!(c.rows[0]["risk"], "1/4");
        assert!(c.rows[0]["text"].as_str().unwrap_or_default().contains("1 in 4"));
        assert!(c.rows[0]["cite"].as_str().unwrap_or_default().starts_with("https://"));
        Ok(())
    }

    #[test]
    fn a_homozygous_or_two_variant_side_gives_no_figure() -> Result<()> {
        let alex = person("Alex", &[("GJB2", carried("AR", "wgs", true))], &[]);
        let mut hom = carried("AR", "wes", true);
        hom.zygosity = Some("hom".into());
        let mut two = carried("AR", "wes", true);
        two.variants = ["ga4gh:VA.one".to_string(), "ga4gh:VA.two".to_string()].into();
        for side in [hom, two] {
            let sam = person("Sam", &[("GJB2", side)], &[]);
            let c = join(&alex, &sam, &adult(false), &adult(false), "en")?;
            assert_eq!(c.rows[0]["kind"], "ar_both_no_figure");
            assert_eq!(c.rows[0]["risk"], Value::Null);
            assert!(!c.rows[0]["text"].as_str().unwrap_or_default().contains("chance"));
        }
        Ok(())
    }

    #[test]
    fn inheritance_comes_from_the_fact_else_the_panel() {
        assert_eq!(inheritance_of("monogenic_pathogenic", "CFTR", ""), Some("AR".into()));
        assert_eq!(inheritance_of("monogenic_pathogenic", "CFTR", "AR"), Some("AR".into()));
        assert_eq!(inheritance_of("monogenic_pathogenic", "GJB2", "AD"), None);
        assert_eq!(inheritance_of("carrier", "DMD", ""), Some("XL".into()));
        assert_eq!(inheritance_of("monogenic_pathogenic", "LPA", ""), None);
        assert_eq!(inheritance_of("pgx", "CFTR", "AR"), None);
    }

    #[test]
    fn one_unconfirmed_side_gives_no_figure() -> Result<()> {
        let alex = person("Alex", &[("GJB2", carried("AR", "wgs", true))], &[]);
        let sam = person("Sam", &[("GJB2", carried("AR", "chip", false))], &[]);
        let c = join(&alex, &sam, &adult(false), &adult(false), "es")?;
        assert_eq!(c.rows[0]["kind"], "ar_both_unconfirmed");
        assert_eq!(c.rows[0]["risk"], Value::Null);
        assert_eq!(c.rows[0]["b"]["label"], text("side.carrier_unconfirmed", "es", &[]));
        assert!(!c.rows[0]["text"].as_str().unwrap_or_default().contains("1 entre 4"));
        Ok(())
    }

    #[test]
    fn the_partner_side_reads_its_coverage() {
        let sam = person(
            "Sam",
            &[],
            &[
                ("chip", &[("GJB2", "partial")]),
                (
                    "wgs",
                    &[("CFTR", "assessed"), ("HBB", "partial"), ("SMN1", "not_assessed")],
                ),
            ],
        );
        assert_eq!(
            sam.absent("CFTR"),
            Absent::NotDetected {
                partial: false,
                assay: "wgs".into()
            }
        );
        assert_eq!(
            sam.absent("HBB"),
            Absent::NotDetected {
                partial: true,
                assay: "wgs".into()
            }
        );
        // The chip probed GJB2, but the whole genome read it: the genome's statement is the stronger one.
        assert_eq!(sam.absent("GJB2"), Absent::NotStated { assay: "wgs".into() });
        let chip_only = person("Sam", &[], &[("chip", &[("GJB2", "partial")])]);
        assert_eq!(chip_only.absent("GJB2"), Absent::ProbedOnly { assay: "chip".into() });
        assert_eq!(sam.absent("SMN1"), Absent::NotAssessed);
        // A whole genome with no row for the gene found nothing there, but says nothing of its coverage.
        assert_eq!(sam.absent("PAH"), Absent::NotStated { assay: "wgs".into() });
        // A chip with no row for the gene did not read it.
        let chip = person("Sam", &[], &[("chip", &[])]);
        assert_eq!(chip.absent("PAH"), Absent::NotAssessed);
        // No current dataset at all.
        assert_eq!(person("Carmen", &[], &[]).absent("CFTR"), Absent::NoGenome);
    }

    #[test]
    fn only_carrier_and_acmg_rows_of_a_sequencing_source_count() {
        assert!(coverage_counts("wgs", Some("carrier")));
        assert!(coverage_counts("wgs", Some("acmg_sf_v3.3")));
        assert!(!coverage_counts("wgs", Some("cpic")));
        assert!(!coverage_counts("wgs", None));
        assert!(coverage_counts("chip", Some("cpic")));
    }

    #[test]
    fn another_finding_in_the_gene_is_never_read_as_not_detected() -> Result<()> {
        let sam = person("Sam", &[("BRCA2", carried("AR", "chip", false))], &[]);
        let mut alex = person("Alex", &[], &[("wgs", &[("BRCA2", "assessed")])]);
        alex.other.insert(
            "BRCA2".into(),
            Other {
                assays: ["wgs".to_string()].into(),
                src: vec!["src_wgs".into()],
                fact_ids: vec!["f_0000000000000002".into()],
                sensitive: true,
            },
        );
        let c = join(&alex, &sam, &adult(true), &adult(true), "en")?;
        assert_eq!(c.rows[0]["kind"], "ar_one_other_finding");
        assert_eq!(c.rows[0]["a"]["state"], "other_finding");
        assert_eq!(c.rows[0]["a"]["src"], json!(["src_wgs"]));
        assert_eq!(c.rows[0]["risk"], Value::Null);
        assert!(!c.rows[0].to_string().contains("not_detected"));
        assert_eq!(c.a_used.0, vec!["f_0000000000000002".to_string()]);
        // Its sensitive flag closes the row like a carrier fact's would.
        let c = join(&alex, &sam, &adult(false), &adult(true), "en")?;
        assert_eq!(c.rows[0]["kind"], "withheld");
        assert!(c.a_used.0.is_empty());
        Ok(())
    }

    #[test]
    fn one_side_kinds_and_x_linked_rows() -> Result<()> {
        let alex = person(
            "Alex",
            &[
                ("CFTR", carried("AR", "wgs", true)),
                ("GALT", carried("AR", "wgs", true)),
                ("HBB", carried("AR", "wgs", true)),
                ("PAH", carried("AR", "wgs", true)),
                ("DMD", carried("XL", "wgs", true)),
            ],
            &[],
        );
        let sam = person(
            "Sam",
            &[],
            &[
                ("chip", &[("HBB", "partial")]),
                ("vcf_only", &[("PAH", "assessed"), ("CFTR", "not_assessed")]),
            ],
        );
        let c = join(&alex, &sam, &adult(false), &adult(false), "en")?;
        let kinds: Vec<(&str, &str)> = c
            .rows
            .iter()
            .map(|r| {
                (
                    r["gene"].as_str().unwrap_or_default(),
                    r["kind"].as_str().unwrap_or_default(),
                )
            })
            .collect();
        assert_eq!(
            kinds,
            vec![
                ("DMD", "xl_one"),
                ("CFTR", "ar_one_not_assessed"),
                ("GALT", "ar_one_not_assessed"),
                ("HBB", "ar_one_probed"),
                ("PAH", "ar_one_not_detected"),
            ]
        );
        let cftr = c.rows[1]["text"].as_str().unwrap_or_default();
        assert!(cftr.contains("not assessed is not the same as not a carrier"), "{cftr}");
        assert!(!cftr.contains("not detected"), "{cftr}");
        let rosa = person("Rosa", &[], &[("wgs", &[])]);
        let c = join(&alex, &rosa, &adult(false), &adult(false), "en")?;
        assert_eq!(c.rows[2]["kind"], "ar_one_not_stated");
        assert_eq!(
            c.rows[2]["b"]["label"],
            "Not detected; coverage of this gene not stated"
        );
        let carmen = person("Carmen", &[], &[]);
        let c = join(&alex, &carmen, &adult(false), &adult(false), "en")?;
        assert_eq!(c.rows[1]["kind"], "ar_one_no_genome");
        assert_eq!(c.rows[1]["b"]["label"], "No genome data");
        assert!(c.rows.iter().all(|r| r["risk"].is_null()));
        Ok(())
    }

    #[test]
    fn a_sensitive_gene_is_named_only_when_both_gates_open() -> Result<()> {
        let mut brca2 = carried("AR", "wgs", true);
        brca2.sensitive = true;
        let alex = person("Alex", &[("BRCA2", brca2)], &[]);
        let sam = person("Sam", &[], &[]);
        for (ga, gb) in [(true, false), (false, true), (false, false)] {
            let c = join(&alex, &sam, &adult(ga), &adult(gb), "en")?;
            assert_eq!(c.withheld, 1);
            assert_eq!(c.rows[0]["kind"], "withheld");
            assert!(!c.rows[0].to_string().contains("BRCA2"), "{}", c.rows[0]);
            assert!(c.a_used.0.is_empty());
        }
        let c = join(&alex, &sam, &adult(true), &adult(true), "en")?;
        assert_eq!(c.rows[0]["gene"], "BRCA2");
        Ok(())
    }

    #[test]
    fn a_minor_is_never_joined() {
        let p = Person::default();
        assert!(join(&p, &p, &Gate::new(true, true), &adult(true), "en").is_err());
    }

    #[test]
    fn every_template_has_both_languages() {
        for k in template_keys() {
            for lang in ["en", "es"] {
                assert!(
                    DATA.templates[&k][lang].as_str().is_some_and(|s| !s.is_empty()),
                    "{k} {lang}"
                );
            }
        }
        assert_eq!(DATA.cites.len(), 3);
    }
}
