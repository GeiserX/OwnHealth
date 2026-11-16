//! Family patterns (data/family/patterns.yaml) into ownhealth.family/v1: each rule with a case that
//! fires and a near miss, the loader's refusals, and a document that is the same bytes every run and
//! whatever order the relatives arrive in.

use chrono::NaiveDate;
use ownhealth_core::family::{FamilyCondition, catalog};
use ownhealth_rules::family::{Relative, document, document_with, parse, render, status_section};
use serde_json::Value;
use uuid::Uuid;

type R = anyhow::Result<()>;

const SHIPPED: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../../data/family/patterns.yaml"
));

fn day() -> NaiveDate {
    NaiveDate::from_ymd_opt(2026, 6, 1).expect("date")
}

fn cond(kind: &str, onset: Option<i32>) -> FamilyCondition {
    FamilyCondition {
        kind: kind.into(),
        text: None,
        code_system: None,
        code: None,
        onset_age: onset,
        onset_approx: false,
    }
}

/// A relative with one condition; the sex is what the relationship implies.
fn rel(n: u128, code: &str, kind: &str, onset: Option<i32>) -> Relative {
    let sex = catalog()
        .expect("catalog")
        .relationship(code)
        .and_then(|r| r.sex.clone());
    Relative {
        id: Uuid::from_u128(n),
        relationship: code.into(),
        sex,
        conditions: vec![cond(kind, onset)],
        src: format!("src_{n}"),
    }
}

fn doc(relatives: &[Relative]) -> Value {
    document("alex", day(), relatives).expect("document")
}

/// The rule ids present, in document order.
fn present(relatives: &[Relative]) -> Vec<String> {
    doc(relatives)["items"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|i| i["status"] == "present")
        .map(|i| i["rule"].as_str().unwrap_or_default().to_string())
        .collect()
}

fn fires(relatives: &[Relative], rule: &str) -> bool {
    present(relatives).iter().any(|r| r == rule)
}

#[test]
fn premature_heart_disease_counts_men_before_55_and_women_before_65() {
    let r = "fam.ascvd";
    assert!(
        !fires(&[rel(1, "FTH", "mi", Some(55))], r),
        "a father at 55 is not premature"
    );
    assert!(fires(&[rel(1, "FTH", "mi", Some(54))], r));
    assert!(fires(&[rel(1, "MTH", "cad", Some(64))], r));
    assert!(!fires(&[rel(1, "MTH", "cad", Some(65))], r));
    assert!(fires(&[rel(1, "BRO", "stroke", Some(50))], r));
    // a sibling of unknown sex needs the lower limit
    assert!(!fires(&[rel(1, "SIB", "mi", Some(60))], r));
    assert!(fires(&[rel(1, "SIB", "mi", Some(54))], r));
    // the guideline names parents and siblings: a grandfather or a son does not count
    assert!(!fires(&[rel(1, "PGRFTH", "mi", Some(40))], r));
    assert!(!fires(&[rel(1, "SON", "mi", Some(40))], r));
    // a partner is not a blood relative
    assert!(!fires(&[rel(1, "HUSB", "mi", Some(40))], r));
}

#[test]
fn a_matching_condition_without_onset_age_is_not_assessed() {
    let d = doc(&[rel(1, "FTH", "mi", None)]);
    let item = &d["items"][0];
    assert_eq!(item["rule"], "fam.ascvd");
    assert_eq!(item["status"], "not_assessed");
    assert_eq!(item["reason_code"], "no_onset_age");
    // the status says it could not check, rather than staying silent
    let s = status_section(&d).expect("section");
    assert_eq!(s["not_assessed"], serde_json::json!(["fam.ascvd"]), "{s}");
    assert!(s.get("items").is_none(), "{s}");
    assert!(status_section(&doc(&[rel(1, "FTH", "asthma", Some(10))])).is_none());
    // a dated match elsewhere decides it
    let d = doc(&[rel(1, "FTH", "mi", None), rel(2, "MTH", "mi", Some(60))]);
    assert_eq!(d["items"][0]["status"], "present");
    assert_eq!(d["items"][0]["rel"], serde_json::json!(["MTH"]));
}

#[test]
fn colon_cancer_counts_one_first_degree_before_60_or_two_at_any_age() {
    let r = "fam.crc";
    assert!(fires(&[rel(1, "MTH", "cancer_colon", Some(59))], r));
    assert!(!fires(&[rel(1, "MTH", "cancer_colon", Some(60))], r));
    assert!(fires(
        &[
            rel(1, "MTH", "cancer_colon", Some(70)),
            rel(2, "BRO", "cancer_colon", Some(72))
        ],
        r
    ));
    assert!(!fires(
        &[
            rel(1, "MTH", "cancer_colon", Some(70)),
            rel(2, "MUNCLE", "cancer_colon", Some(50))
        ],
        r
    ));
}

#[test]
fn breast_and_ovarian_cancers_follow_the_brca_family_history_factors() {
    let r = "fam.hboc";
    assert!(fires(&[rel(1, "MAUNT", "cancer_breast", Some(45))], r));
    assert!(!fires(&[rel(1, "MAUNT", "cancer_breast", Some(50))], r));
    assert!(
        fires(&[rel(1, "PUNCLE", "cancer_breast", Some(70))], r),
        "breast cancer in a man"
    );
    assert!(fires(&[rel(1, "MGRMTH", "cancer_ovary", Some(70))], r));
    // two on one side
    assert!(fires(
        &[
            rel(1, "MUNCLE", "cancer_prostate", Some(70)),
            rel(2, "MAUNT", "cancer_breast", Some(60))
        ],
        r
    ));
    assert!(fires(
        &[
            rel(1, "FTH", "cancer_pancreas", Some(70)),
            rel(2, "SIS", "cancer_breast", Some(60))
        ],
        r
    ));
    // one on each side is not a cluster
    assert!(!fires(
        &[
            rel(1, "MUNCLE", "cancer_prostate", Some(70)),
            rel(2, "PAUNT", "cancer_breast", Some(60))
        ],
        r
    ));
    // a half-sister's side is not known, so she joins no one-side cluster
    assert!(!fires(
        &[
            rel(1, "HSIS", "cancer_breast", Some(60)),
            rel(2, "MUNCLE", "cancer_prostate", Some(70))
        ],
        r
    ));
    // a cousin is third-degree, past the close relatives the factors name
    assert!(!fires(&[rel(1, "MCOUSN", "cancer_ovary", Some(40))], r));
}

#[test]
fn a_known_gene_variant_in_a_blood_relative_counts() {
    let r = "fam.variant";
    assert!(fires(&[rel(1, "PCOUSN", "hereditary_variant", None)], r));
    assert!(!fires(&[rel(1, "WIFE", "hereditary_variant", None)], r));
}

#[test]
fn the_same_kind_on_one_side_counts_and_free_text_never_does() {
    let r = "fam.same_side";
    assert!(fires(
        &[
            rel(1, "FTH", "hypertension", Some(60)),
            rel(2, "PAUNT", "hypertension", Some(62))
        ],
        r
    ));
    // a full sibling and a child share both sides
    assert!(fires(
        &[rel(1, "MTH", "t2d", Some(60)), rel(2, "SIS", "t2d", Some(40))],
        r
    ));
    assert!(fires(
        &[rel(1, "PGRMTH", "t2d", Some(60)), rel(2, "SON", "t2d", Some(20))],
        r
    ));
    // a half-sibling or a parent of unknown side may share only one side, and which is not known
    assert!(!fires(
        &[rel(1, "HBRO", "t2d", Some(40)), rel(2, "MGRMTH", "t2d", Some(70))],
        r
    ));
    assert!(!fires(
        &[rel(1, "PRN", "t2d", Some(40)), rel(2, "PGRMTH", "t2d", Some(70))],
        r
    ));
    assert!(!fires(
        &[
            rel(1, "FTH", "hypertension", Some(60)),
            rel(2, "MTH", "hypertension", Some(62))
        ],
        r
    ));
    assert!(!fires(
        &[
            rel(1, "FTH", "hypertension", Some(60)),
            rel(2, "PAUNT", "t2d", Some(62))
        ],
        r
    ));
    let mut a = rel(1, "FTH", "other", Some(60));
    let mut b = rel(2, "PAUNT", "other", Some(62));
    a.conditions[0].text = Some("gout".into());
    b.conditions[0].text = Some("gout".into());
    assert!(present(&[a, b]).is_empty(), "free text never fires a pattern");
    // the kinds named are the ones shared, not every condition of the members
    let mut f = rel(1, "FTH", "hypertension", Some(60));
    f.conditions.push(cond("asthma", Some(10)));
    let d = doc(&[f, rel(2, "PAUNT", "hypertension", Some(62))]);
    assert_eq!(d["items"][0]["kinds"], serde_json::json!(["hypertension"]));
}

#[test]
fn every_item_cites_its_rule_and_names_its_sources() {
    let d = doc(&[rel(1, "FTH", "mi", Some(52)), rel(2, "PUNCLE", "mi", Some(60))]);
    assert_eq!(d["schema"], "ownhealth.family/v1");
    assert!(d["rules"].as_str().unwrap_or_default().starts_with("patterns.yaml "));
    for it in d["items"].as_array().into_iter().flatten() {
        assert!(it["cite"].as_str().unwrap_or_default().starts_with("https://"), "{it}");
        assert!(!it["src"].as_array().is_none_or(Vec::is_empty), "{it}");
        assert!(!it["members"].as_array().is_none_or(Vec::is_empty), "{it}");
    }
    assert_eq!(
        present(&[rel(1, "FTH", "mi", Some(52)), rel(2, "PUNCLE", "mi", Some(60))]),
        ["fam.ascvd", "fam.same_side"]
    );
    assert_eq!(d["items"][1]["basis"], "pack_default");
    assert_eq!(d["items"][1]["src"], serde_json::json!(["src_1", "src_2"]));
}

#[test]
fn the_document_is_the_same_bytes_every_run_and_any_order() {
    let mut rels = vec![
        rel(3, "MAUNT", "cancer_breast", Some(45)),
        rel(1, "FTH", "mi", Some(52)),
        rel(2, "PUNCLE", "mi", Some(60)),
    ];
    let a = serde_json::to_string(&doc(&rels)).expect("json");
    rels.reverse();
    let b = serde_json::to_string(&doc(&rels)).expect("json");
    assert_eq!(a, b);
    let d = doc(&rels);
    assert!(d["family_hash"].as_str().unwrap_or_default().starts_with("sha256:"));
    // the words come after the hash: every language gets the same one
    let r = render(&d).expect("render");
    assert_eq!(r["family_hash"], d["family_hash"]);
    let title = &r["items"][0]["title"];
    assert_ne!(title["en"], title["es"]);
    assert_eq!(title["es"].as_str().map(str::is_empty), Some(false));
}

#[test]
fn the_status_section_lists_three_patterns_by_rule_and_relationship_only() {
    let rels = [
        rel(1, "FTH", "mi", Some(52)),
        rel(2, "PUNCLE", "mi", Some(60)),
        rel(3, "MAUNT", "cancer_ovary", Some(45)),
        rel(4, "MTH", "cancer_colon", Some(50)),
        rel(5, "SIS", "hereditary_variant", None),
    ];
    let d = doc(&rels);
    let s = status_section(&d).expect("section");
    assert_eq!(s["pattern_total"], 5);
    assert_eq!(s["items"].as_array().map(Vec::len), Some(3));
    // a prefix of the document's hash, under a key that says it is one
    assert!(s.get("family_hash").is_none(), "{s}");
    let full = d["family_hash"].as_str().unwrap_or_default();
    let short = s["family_hash12"].as_str().unwrap_or_default();
    assert_eq!(short.len(), 12);
    assert!(full.starts_with(&format!("sha256:{short}")), "{short} {full}");
    let text = s.to_string();
    assert!(!text.contains("http"), "{text}");
    assert!(!text.contains("src_"), "{text}");
    for it in s["items"].as_array().into_iter().flatten() {
        let keys: Vec<&String> = it.as_object().map(|m| m.keys().collect()).unwrap_or_default();
        assert_eq!(keys, ["rule", "rel"], "{it}");
    }
    assert_eq!(s["items"][0], serde_json::json!({"rule": "fam.ascvd", "rel": "FTH"}));
}

#[test]
fn a_large_family_names_a_few_distinct_relationships_and_counts_the_rest() {
    let codes = [
        "PGRFTH", "PGRMTH", "PUNCLE", "PAUNT", "FTH", "MTH", "MGRFTH", "MGRMTH", "MUNCLE", "MAUNT", "BRO", "BRO", "SIS",
    ];
    let rels: Vec<Relative> = codes
        .iter()
        .enumerate()
        .map(|(n, c)| rel(n as u128 + 1, c, "hypertension", Some(60)))
        .collect();
    let s = status_section(&doc(&rels)).expect("section");
    let item = &s["items"][0];
    assert_eq!(item["rule"], "fam.same_side", "{s}");
    // drawing order, each code once: the two brothers are one BRO
    assert_eq!(item["rel"], "PGRFTH PGRMTH MGRFTH MGRMTH", "{s}");
    assert_eq!(item["rel_total"], 12, "{s}");
    assert!(s.to_string().len() < 200, "{s}");
}

#[test]
fn the_loader_refuses_a_rule_without_cite_template_or_matchable_kind() -> R {
    let cat = catalog()?;
    let pat = parse(SHIPPED.as_bytes(), cat)?;
    assert_eq!(pat.rules.len(), 5);
    let no_cite = SHIPPED.replacen("    cite: \"https://doi.org/10.1053/j.gastro.2017.05.013\"\n", "", 1);
    assert_ne!(no_cite, SHIPPED);
    let e = parse(no_cite.as_bytes(), cat).expect_err("a row without cite").0;
    assert!(e.contains("https `cite`"), "{e}");
    let http = SHIPPED.replacen("https://doi.org/10.1053", "http://doi.org/10.1053", 1);
    assert!(parse(http.as_bytes(), cat).is_err());
    let no_title = SHIPPED.replacen(
        "    fam.variant:\n      en: \"A gene",
        "    fam.other:\n      en: \"A gene",
        1,
    );
    let e = parse(no_title.as_bytes(), cat).expect_err("no title").0;
    assert!(e.contains("fam.variant: no title template"), "{e}");
    let free_text = SHIPPED.replacen("kinds: [hereditary_variant]", "kinds: [other]", 1);
    let e = parse(free_text.as_bytes(), cat).expect_err("other").0;
    assert!(e.contains("kind other cannot be matched"), "{e}");
    let no_es = SHIPPED.replacen(
        "      es: \"Una variante genética conocida en un familiar de sangre\"\n",
        "",
        1,
    );
    assert!(parse(no_es.as_bytes(), cat).is_err());
    let advice = SHIPPED.replacen(
        "en: \"A gene variant known in a blood relative\"",
        "en: \"Ask your doctor about a gene test\"",
        1,
    );
    assert!(parse(advice.as_bytes(), cat).is_err());
    let unknown = SHIPPED.replacen("[FTH, MTH,", "[XYZ, MTH,", 1);
    assert!(parse(unknown.as_bytes(), cat).is_err());
    // a value of the wrong type is refused by name, never read as the default that widens the rule
    for (from, to, named) in [
        (
            "relationships: [FTH, MTH, PRN, BRO, SIS, SIB]",
            "relationships: FTH",
            "relationships is a list of strings",
        ),
        (
            "kinds: [mi, cad, stroke]",
            "kinds: [mi, 5]",
            "kinds is a list of strings",
        ),
        (
            "count: 2, same_side: true}",
            "count: 2, same_side: \"yes\"}",
            "same_side is true or false",
        ),
        (
            "{same_kind: true,",
            "{same_kind: \"true\",",
            "same_kind is true or false",
        ),
        ("sex: male}", "sex: 1}", "sex is female or male"),
    ] {
        let bad = SHIPPED.replacen(from, to, 1);
        assert_ne!(bad, SHIPPED, "{from}");
        let e = parse(bad.as_bytes(), cat).expect_err(to).0;
        assert!(e.contains(named), "{to}: {e}");
    }
    // a rule the mutated file still loads evaluates the same way
    let doc = document_with(&pat, cat, "alex", day(), &[rel(1, "FTH", "mi", Some(52))])?;
    assert_eq!(doc["items"][0]["rule"], "fam.ascvd");
    Ok(())
}
