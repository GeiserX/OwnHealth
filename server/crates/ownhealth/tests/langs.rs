//! The language registry gates (`ownhealth_core::langs`): every localized string under `data/` has
//! every registry language, every registry language has its own wording-lint patterns, and the UI
//! registry lists the same languages. A missing string fails here instead of falling back to English.
//! The controls run each check with a dummy language and watch it go red.

use std::path::{Path, PathBuf};

use ownhealth_core::langs::LANGS;
use ownhealth_rules::cited;
use ownhealth_rules::load_pack;
use ownhealth_rules::wording::missing_patterns;
use serde_json::json;

type R = anyhow::Result<()>;

const DATA: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../../data");
const CRATES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/..");
const UI_LANGS: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../../ui/src/lib/i18n/langs.ts");

/// Every YAML or JSON file under `data/`, parsed.
fn data_files() -> anyhow::Result<Vec<(PathBuf, serde_json::Value)>> {
    let mut out = Vec::new();
    let mut dirs = vec![PathBuf::from(DATA)];
    while let Some(dir) = dirs.pop() {
        for entry in std::fs::read_dir(&dir)? {
            let path = entry?.path();
            if path.is_dir() {
                dirs.push(path);
            } else if ["yaml", "yml", "json"].contains(&path.extension().and_then(|e| e.to_str()).unwrap_or_default()) {
                let doc = cited::read_doc(&path)?;
                out.push((path, doc));
            }
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(out)
}

fn missing_under_data(langs: &[&str]) -> anyhow::Result<Vec<String>> {
    let root = Path::new(DATA);
    Ok(data_files()?
        .iter()
        .flat_map(|(path, doc)| {
            let name = path.strip_prefix(root).unwrap_or(path).display().to_string();
            cited::missing_langs(doc, langs)
                .into_iter()
                .map(move |m| format!("{name} {m}"))
        })
        .collect())
}

#[test]
fn every_localized_string_under_data_has_every_registry_language() -> R {
    assert_eq!(missing_under_data(&LANGS)?, Vec::<String>::new());
    Ok(())
}

/// The control: one more language, with no strings, is missing from every localized mapping, the
/// language-suffixed ones (`short_en`) included, in every area of `data/`.
#[test]
fn a_dummy_language_is_missing_everywhere_under_data() -> R {
    let mut langs = LANGS.to_vec();
    langs.push("xx");
    let missing = missing_under_data(&langs)?;
    assert!(missing.iter().all(|m| m.contains("xx")), "{missing:?}");
    // 0.4.0 ships about 500 localized mappings; the floor leaves room for later trims.
    assert!(missing.len() >= 300, "{} mappings", missing.len());
    assert!(
        missing.iter().any(|m| m.ends_with("has no `short_xx` text")),
        "suffixed keys unseen"
    );
    for area in [
        "calculators",
        "family",
        "genome",
        "prevention",
        "rules",
        "summary",
        "timeline",
    ] {
        assert!(
            missing.iter().any(|m| m.starts_with(&format!("{area}/"))),
            "{area} unseen"
        );
    }
    Ok(())
}

#[test]
fn the_completeness_check_names_each_missing_or_empty_text() {
    let doc = json!({
        "version": "x 0.1.0",
        "templates": {"a": {"en": "One.", "es": "Uno."}, "b": {"en": "Two.", "es": " "}, "c": {"es": "Tres."}},
        "rows": [{"kind": "k", "short_en": "K", "short_es": "K", "cite": "https://x.example"}],
        "plain": {"name": "not localized"}
    });
    assert_eq!(
        cited::missing_langs(&doc, &["es", "en"]),
        vec!["$.templates.b has no `es` text", "$.templates.c has no `en` text"]
    );
    assert_eq!(
        cited::missing_langs(&doc, &["es", "en", "xx"]),
        vec![
            "$.templates.a has no `xx` text",
            "$.templates.b has no `es` text",
            "$.templates.b has no `xx` text",
            "$.templates.c has no `en` text",
            "$.templates.c has no `xx` text",
            "$.rows[0] has no `short_xx` text",
        ]
    );
}

#[test]
fn every_registry_language_has_its_own_imperative_patterns() {
    assert_eq!(missing_patterns(&LANGS), Vec::<&str>::new());
    let mut langs = LANGS.to_vec();
    langs.push("xx");
    assert_eq!(missing_patterns(&langs), vec!["xx"]);
}

/// The codes of `ui/src/lib/i18n/langs.ts`, in order (`code: 'es'`).
fn ui_codes(src: &str) -> Vec<String> {
    src.split("code: '")
        .skip(1)
        .filter_map(|rest| rest.split_once('\'').map(|(code, _)| code.to_string()))
        .collect()
}

#[test]
fn the_ui_registry_lists_the_server_languages_in_order() -> R {
    let src = std::fs::read_to_string(UI_LANGS)?;
    assert_eq!(ui_codes(&src), LANGS.to_vec());
    assert_eq!(ui_codes("{ code: 'es', x: 1 }, { code: 'xx' }"), vec!["es", "xx"]);
    Ok(())
}

/// A pick of es or en written into the code: a comparison with the code, a lookup of one of them,
/// a two-language literal list, or an es/en field read (`r.es`, `info.label_en`).
const BY_HAND: &str = r#"(==|!=) ?"e[sn]"|\{ ?"e[sn]": |get\("e[sn]"\)|(unwrap_or|map_or)\("e[sn]"|starts_with\("e[sn]"\)|\["e[sn]", "e[sn]"\]|"e[sn]" \| "e[sn]"|\.([a-z]+_)?e[sn]\b"#;

/// The lines allowed to name es or en, each with its reason: MCP speaks es/en by design, the
/// registry itself, and the readers of what arrives (a document's or a resource's language, a printed
/// number), which are not the person's language.
const ALLOWED: &[(&str, &str)] = &[
    ("ownhealth-core/src/langs.rs", ""),
    ("ownhealth-mcp/", ""),
    // the language a lab report is printed in, as the PDF reader recognizes it
    (
        "ownhealth-ingest-pdf/src/extract.rs",
        r#"Some(l @ ("es" | "en" | "de" | "fr")) => l"#,
    ),
    // the language a received FHIR resource declares, for the normalizer's number parsing
    (
        "ownhealth-ingest-structured/src/fhir/mod.rs",
        r#".is_some_and(|l| l.starts_with("es"))"#,
    ),
    // the normalizer's number parsing of a printed value
    ("ownhealth-normalize/src/text.rs", ""),
    // a printed range parsed again with the normalizer's two parse locales
    (
        "ownhealth-rules/src/summary.rs",
        r#"["es", "en"].iter().find_map(|locale| {"#,
    ),
    // the catalogs whose fields are es and en, read only through their per-language accessors,
    // which `text_tables` below checks against the registry
    ("ownhealth-core/src/family.rs", r#"[("es", &self."#),
    (
        "ownhealth-normalize/src/tables.rs",
        r#"[("es", &self.name_es), ("en", &self.name_en)]"#,
    ),
    (
        "ownhealth-ingest-structured/src/wearables.rs",
        r#"[("es", &self.label_es), ("en", &self.label_en)]"#,
    ),
    // the FHIR text we store is English whatever the reader's language (`fhir_text`)
    ("ownhealth-core/src/family.rs", "        &self.en"),
    // the vaccine names as `vaccines.tsv` holds them, checked by `prevention::missing_langs`
    (
        "ownhealth-rules/src/prevention.rs",
        r#"name: json!({"en": label_en.trim(), "es": label_es.trim()}),"#,
    ),
    // English-only phrase lists for the wording lint, never shown in a language
    ("ownhealth-api/src/routes/shl.rs", r#"json!({"en": keys.iter()"#),
    ("ownhealth-ingest-structured/src/shc.rs", r#"json!({ "en": all.iter()"#),
    // an absent `lang` on the timeline and the couple view reads English, their REST default since
    // 0.4.0
    (
        "ownhealth-api/src/routes/timeline.rs",
        r#"let lang = param(&q, "lang").unwrap_or("en");"#,
    ),
    (
        "ownhealth-api/src/routes/genome/couple.rs",
        r#"let lang = q.get("lang").map_or("en", String::as_str);"#,
    ),
];

/// Every line of the server's sources, tests left out, that picks es or en by hand.
fn picks_by_hand() -> anyhow::Result<Vec<String>> {
    let re = regex::Regex::new(BY_HAND)?;
    let root = PathBuf::from(CRATES);
    let mut out = Vec::new();
    let mut dirs = vec![root.clone()];
    while let Some(dir) = dirs.pop() {
        for entry in std::fs::read_dir(&dir)? {
            let path = entry?.path();
            let name = path.strip_prefix(&root)?.display().to_string();
            if path.is_dir() {
                if !name.ends_with("/tests") && !name.ends_with("target") {
                    dirs.push(path);
                }
                continue;
            }
            if !name.contains("/src/") || path.extension().and_then(|e| e.to_str()) != Some("rs") {
                continue;
            }
            let src = std::fs::read_to_string(&path)?;
            for (n, line) in src.lines().enumerate() {
                if line.trim_start().starts_with("#[cfg(test)]") {
                    break;
                }
                let code = line.split("//").next().unwrap_or_default();
                if !re.is_match(code) {
                    continue;
                }
                let allowed = ALLOWED
                    .iter()
                    .any(|(f, l)| name.starts_with(f) && (l.is_empty() || line.contains(l)));
                if !allowed {
                    out.push(format!("{name}:{}: {}", n + 1, line.trim()));
                }
            }
        }
    }
    out.sort();
    Ok(out)
}

/// A renderer picks its text through `ownhealth_core::langs` (`lookup`, `pick`), so a registry
/// language never falls back to es or en without a test seeing it.
#[test]
fn no_server_code_picks_es_or_en_by_hand() -> R {
    assert_eq!(picks_by_hand()?, Vec::<String>::new());
    // the control: the scan sees each form of a pick
    let re = regex::Regex::new(BY_HAND)?;
    for pick in [
        r#"let lang = if lang == "es" { "es" } else { "en" };"#,
        r#"v.get(lang).or_else(|| v.get("en"))"#,
        r#"param(&q, "lang").unwrap_or("en")"#,
        r#"literal(&q, "lang", &["es", "en"], "")"#,
        r#"if lang == "es" { r.es.as_str() } else { r.en.as_str() }"#,
        r#"info.label_es.clone()"#,
        r#"t if t.starts_with("es") => Lang::Es,"#,
        r#""rerun": {"en": rerun_text("en"), "es": rerun_text("es")},"#,
    ] {
        assert!(re.is_match(pick), "{pick}");
    }
    for fine in [r#"lookup(lang, |l| v.get(l))"#, r#"x.enumerate()"#, r#"self.ess"#] {
        assert!(!re.is_match(fine), "{fine}");
    }
    Ok(())
}

/// Every text a renderer keeps in code or in a catalog with fixed language fields, by name: each
/// names the languages of `langs` it lacks. A new table adds one line here.
fn text_tables(langs: &[&str]) -> anyhow::Result<Vec<(&'static str, Vec<String>)>> {
    let pack = load_pack()?;
    Ok(vec![
        (
            "family catalog",
            ownhealth_core::family::catalog()?.missing_langs(langs),
        ),
        ("analyte names", ownhealth_normalize::tables::missing_langs(langs)),
        (
            "wearables",
            ownhealth_ingest_structured::wearables::missing_langs(langs),
        ),
        ("countries", ownhealth_rules::glance::missing_langs(langs)),
        ("numbers", ownhealth_rules::num::missing_langs(langs)),
        ("calculators", ownhealth_rules::calc::missing_langs(langs)),
        ("vaccines", ownhealth_rules::prevention::missing_langs(&pack, langs)?),
        ("timeline", ownhealth_api::routes::timeline::missing_langs(langs)),
        ("calendar feed", ownhealth_api::routes::ics::missing_langs(langs)),
        (
            "imaging viewer",
            ownhealth_api::routes::imaging_proxy::missing_langs(langs),
        ),
        ("trend", ownhealth_api::routes::observations::missing_langs(langs)),
    ])
}

#[test]
fn every_text_table_has_every_registry_language() -> R {
    let gaps: Vec<String> = text_tables(&LANGS)?.into_iter().flat_map(|(_, m)| m).collect();
    assert_eq!(gaps, Vec::<String>::new());
    Ok(())
}

/// The control: a dummy language is missing from every table, so a language PR that adds its code
/// to the registry without the texts goes red here, naming each table.
#[test]
fn a_dummy_language_is_missing_from_every_text_table() -> R {
    let mut langs = LANGS.to_vec();
    langs.push("xx");
    for (name, missing) in text_tables(&langs)? {
        assert!(!missing.is_empty(), "{name} does not see the dummy language");
        assert!(missing.iter().all(|m| m.contains("xx")), "{name}: {missing:?}");
    }
    Ok(())
}
