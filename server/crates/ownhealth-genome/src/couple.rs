//! The couple carrier view (genomics.md section 10): two adult partners' carrier facts joined by gene.
//!
//! Each side is one profile's current carrier facts, plus its `monogenic_pathogenic` facts in an
//! autosomal recessive (AR) or X-linked (XL) gene. Inheritance comes from the fact, else from
//! `data/genome/carrier_genes.tsv`. A side without a fact in the gene is "not detected" (with its
//! coverage), "not assessed beyond the probed sites" (a chip) or "not assessed".
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
    pub src: Vec<String>,
    pub fact_ids: Vec<String>,
    pub sensitive: bool,
}

/// What one side holds for a gene where it has no carrier fact.
#[derive(Debug, Clone, PartialEq)]
pub enum Absent {
    /// A sequencing source assessed the gene and found nothing; `partial` when only part was callable.
    NotDetected {
        partial: bool,
        assay: String,
    },
    /// Only a chip covers the gene: it read some positions, not the gene.
    ProbedOnly {
        assay: String,
    },
    NotAssessed,
}

/// One side of a row.
#[derive(Debug, Clone, PartialEq)]
pub enum Side<'a> {
    Carrier(&'a Carried),
    Absent(Absent),
}

/// Everything about one profile the join reads.
#[derive(Debug, Clone, Default)]
pub struct Person {
    pub name: String,
    pub carried: BTreeMap<String, Carried>,
    /// Per dataset: assay and the gene coverage rows (gene upper case -> state).
    pub coverage: Vec<(String, HashMap<String, String>)>,
}

impl Person {
    /// What this person holds in `gene` when no carrier fact names it.
    pub fn absent(&self, gene: &str) -> Absent {
        let mut chip = None;
        let mut partial = None;
        for (assay, genes) in &self.coverage {
            let Some(state) = genes.get(gene) else { continue };
            if state != "assessed" && state != "partial" {
                continue;
            }
            if assay.starts_with("chip") {
                chip.get_or_insert_with(|| assay.clone());
            } else if state == "assessed" {
                return Absent::NotDetected {
                    partial: false,
                    assay: assay.clone(),
                };
            } else {
                partial.get_or_insert_with(|| assay.clone());
            }
        }
        match (partial, chip) {
            (Some(assay), _) => Absent::NotDetected { partial: true, assay },
            (None, Some(assay)) => Absent::ProbedOnly { assay },
            (None, None) => Absent::NotAssessed,
        }
    }

    fn side(&self, gene: &str) -> Side<'_> {
        match self.carried.get(gene) {
            Some(c) => Side::Carrier(c),
            None => Side::Absent(self.absent(gene)),
        }
    }
}

fn is_carrier_row(r: &FindingRow) -> Option<String> {
    if r.status != "current" || r.verification_status == "refuted" {
        return None;
    }
    if !matches!(r.call_state.as_deref(), None | Some("alt")) {
        return None;
    }
    let gene = r.gene.as_deref()?.trim().to_uppercase();
    let declared = r.fact.0["condition"]["inheritance"].as_str().unwrap_or_default();
    let inheritance = match declared {
        "AR" | "XL" => declared.to_string(),
        _ if r.category == "monogenic_pathogenic" => return None,
        _ => PANEL_INHERITANCE.get(&gene)?.clone(),
    };
    match r.category.as_str() {
        "carrier" | "monogenic_pathogenic" if inheritance == "AR" || inheritance == "XL" => Some(inheritance),
        _ => None,
    }
}

/// Read one profile's carrier facts and gene coverage. `name` is how the texts call this person.
pub async fn load_person(conn: &mut PgConnection, profile_id: Uuid, name: &str) -> Result<Person> {
    let mut carried: BTreeMap<String, Carried> = BTreeMap::new();
    for r in visible_rows(&mut *conn, profile_id, None).await? {
        let Some(inheritance) = is_carrier_row(&r) else {
            continue;
        };
        let gene = r.gene.as_deref().unwrap_or_default().trim().to_uppercase();
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
                .filter(|c| c.dataset_id == d.id && c.scope == "gene")
                .map(|c| (c.target.to_uppercase(), c.state.clone()))
                .collect();
            (d.assay.clone(), genes)
        })
        .collect();
    Ok(Person {
        name: name.to_string(),
        carried,
        coverage,
    })
}

fn side_out(side: &Side<'_>, lang: &str) -> Value {
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
        Side::Absent(Absent::ProbedOnly { assay }) => json!({
            "state": "probed_only",
            "label": text("side.probed_only", lang, &[]),
            "confirmed": Value::Null,
            "assays": [assay],
            "zygosity": Value::Null,
            "coverage": "partial",
            "src": [],
        }),
        Side::Absent(Absent::NotAssessed) => json!({
            "state": "not_assessed",
            "label": text("side.not_assessed", lang, &[]),
            "confirmed": Value::Null,
            "assays": [],
            "zygosity": Value::Null,
            "coverage": "not_assessed",
            "src": [],
        }),
    }
}

/// The kind of row, its wording key and its citation id, for two sides of one gene.
fn kind(inheritance: &str, a: &Side<'_>, b: &Side<'_>) -> (&'static str, &'static str) {
    let carrier = |s: &Side<'_>| matches!(s, Side::Carrier(_));
    let confirmed = |s: &Side<'_>| matches!(s, Side::Carrier(c) if c.confirmed);
    if inheritance == "XL" {
        return (if carrier(a) && carrier(b) { "xl_both" } else { "xl_one" }, "couple.xl");
    }
    if carrier(a) && carrier(b) {
        return if confirmed(a) && confirmed(b) {
            ("ar_both", "couple.ar")
        } else {
            ("ar_both_unconfirmed", "couple.ar")
        };
    }
    let other = if carrier(a) { b } else { a };
    match other {
        Side::Absent(Absent::NotDetected { .. }) => ("ar_one_not_detected", "couple.residual"),
        Side::Absent(Absent::ProbedOnly { .. }) => ("ar_one_probed", "couple.residual"),
        _ => ("ar_one_not_assessed", "couple.residual"),
    }
}

/// Order of rows: shared genes first, then X-linked, then one-side genes, withheld last.
fn rank(kind: &str) -> u8 {
    match kind {
        "ar_both" => 0,
        "ar_both_unconfirmed" => 1,
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
                Side::Absent(_) => None,
            })
            .collect();
        let inheritance = carried.first().map(|c| c.inheritance.clone()).unwrap_or_default();
        let sensitive = carried.iter().any(|c| c.sensitive) || is_sensitive_gene(Some(gene));
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
            if let Side::Carrier(c) = side {
                used.0.extend(c.fact_ids.iter().cloned());
                used.1.extend(c.src.iter().cloned());
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
        assert_eq!(sam.absent("GJB2"), Absent::ProbedOnly { assay: "chip".into() });
        assert_eq!(sam.absent("SMN1"), Absent::NotAssessed);
        assert_eq!(sam.absent("PAH"), Absent::NotAssessed);
    }

    #[test]
    fn one_side_kinds_and_x_linked_rows() -> Result<()> {
        let alex = person(
            "Alex",
            &[
                ("CFTR", carried("AR", "wgs", true)),
                ("HBB", carried("AR", "wgs", true)),
                ("PAH", carried("AR", "wgs", true)),
                ("DMD", carried("XL", "wgs", true)),
            ],
            &[],
        );
        let sam = person(
            "Sam",
            &[],
            &[("chip", &[("HBB", "partial")]), ("wes", &[("PAH", "assessed")])],
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
                ("HBB", "ar_one_probed"),
                ("PAH", "ar_one_not_detected"),
            ]
        );
        let cftr = &c.rows[1];
        assert!(
            cftr["text"]
                .as_str()
                .unwrap_or_default()
                .contains("not detected is not the same as not a carrier")
        );
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
