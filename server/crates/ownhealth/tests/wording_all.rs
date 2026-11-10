//! The wording gate (CI step "Wording lint"): information, never advice. Every text we write for a
//! person (status, dashboard, drug check, and every es/en string under `data/`) has 0 imperative
//! phrases outside attributed quotes. A new text source adds one line to `SOURCES`; a new data file
//! or directory is read with no code change. The controls below prove the gate can go red.

#[path = "../../ownhealth-rules/tests/common/mod.rs"]
mod common;

use std::future::Future;
use std::pin::Pin;

use common::{Seeded, as_of, seeded};
use ownhealth_core::access::all_scopes;
use ownhealth_rules::wording::{SKIP_KEYS, imperative_phrases, lint};
use ownhealth_rules::{build_state, check_drug, dashboard, load_pack, render};
use serde_json::{Value, json};

type R = anyhow::Result<()>;
type Out<'a> = Pin<Box<dyn Future<Output = anyhow::Result<Vec<Value>>> + 'a>>;
type Source = for<'a> fn(&'a Seeded) -> Out<'a>;

const SLUGS: [&str; 4] = ["alex", "sam", "lucia", "carmen"];
const LANGS: [&str; 2] = ["en", "es"];

/// Every user-facing output we compute. One line per source.
const SOURCES: &[(&str, Source)] = &[
    ("status", |s| Box::pin(status(s))),
    ("dashboard", |s| Box::pin(dashboards(s))),
    ("check_drug", |s| Box::pin(drug_checks(s))),
];

async fn status(s: &Seeded) -> anyhow::Result<Vec<Value>> {
    let mut tx = s.pool().begin().await?;
    let mut out = Vec::new();
    for slug in SLUGS {
        let body = build_state(&mut tx, s.pid(slug), as_of(), None).await?;
        for lang in LANGS {
            for mode in ["full", "pseudonymous", "minimal"] {
                out.push(render(&body, lang, "as_reported", mode, None)?);
            }
        }
    }
    Ok(out)
}

async fn dashboards(s: &Seeded) -> anyhow::Result<Vec<Value>> {
    let mut tx = s.pool().begin().await?;
    let mut out = Vec::new();
    for slug in SLUGS {
        for lang in LANGS {
            out.push(dashboard(&mut tx, s.pid(slug), as_of(), lang, &all_scopes()).await?);
        }
    }
    Ok(out)
}

async fn drug_checks(s: &Seeded) -> anyhow::Result<Vec<Value>> {
    let mut tx = s.pool().begin().await?;
    let mut drugs: Vec<String> = load_pack()?.drugs.keys().cloned().collect();
    drugs.sort();
    let mut out = Vec::new();
    for slug in ["alex", "sam"] {
        for drug in &drugs {
            for lang in LANGS {
                out.push(check_drug(&mut tx, s.pid(slug), drug, &all_scopes(), lang, Some(as_of())).await?);
            }
        }
    }
    Ok(out)
}

#[tokio::test]
async fn every_output_source_is_information_not_advice() -> R {
    let s = seeded().await?;
    let mut hits = Vec::new();
    for (name, source) in SOURCES {
        let outputs = source(&s).await?;
        assert!(!outputs.is_empty(), "{name} produced nothing to lint");
        for out in &outputs {
            hits.extend(lint(out, &SKIP_KEYS).into_iter().map(|(p, h)| format!("{name} {p}: {h}")));
        }
    }
    assert_eq!(hits, Vec::<String>::new());
    Ok(())
}

/// The 0 above is not a lint that cannot see: the drug check's CPIC quotes, once stripped of their
/// attribution, are imperatives and fail.
#[tokio::test]
async fn the_gate_goes_red_on_an_unattributed_quote() -> R {
    let s = seeded().await?;
    let mut tx = s.pool().begin().await?;
    let mut out = check_drug(&mut tx, s.pid("sam"), "codeine", &all_scopes(), "en", Some(as_of())).await?;
    assert_eq!(lint(&out, &SKIP_KEYS), Vec::new());
    for g in out["guidance"].as_array_mut().into_iter().flatten() {
        if let Some(m) = g.as_object_mut() {
            for k in ["source", "url", "cite"] {
                m.remove(k);
            }
        }
    }
    assert_ne!(lint(&out, &SKIP_KEYS), Vec::new());
    Ok(())
}

#[test]
fn spanish_infinitive_instructions_are_imperatives() {
    for s in [
        "Evitar codeína.",
        "Suspender la estatina.",
        "Tomar con comida.",
        "Dejar de fumar.",
        "No tomar alcohol.",
        "Reducir la sal.",
        "Aumentar la dosis.",
        "Dosis: evitar el alcohol.",
    ] {
        assert!(!imperative_phrases(s).is_empty(), "{s}");
    }
    for ok in ["La dosis se tomó con comida.", "Valor reducido respecto al anterior."] {
        assert_eq!(imperative_phrases(ok), Vec::<String>::new(), "{ok}");
    }
}

#[test]
fn a_quote_is_exempt_only_with_its_attribution() {
    let quote = "Avoid codeine.";
    assert_ne!(lint(&json!({"quote": quote}), &SKIP_KEYS), Vec::new());
    assert_ne!(lint(&json!({"quote": quote, "source": " "}), &SKIP_KEYS), Vec::new());
    assert_ne!(lint(&json!({"quote": quote, "url": null}), &SKIP_KEYS), Vec::new());
    for attribution in ["cite", "url", "source"] {
        let v = json!({"quote": quote, attribution: "https://cpicpgx.org/guidelines/"});
        assert_eq!(lint(&v, &SKIP_KEYS), Vec::new(), "{attribution}");
    }
}
