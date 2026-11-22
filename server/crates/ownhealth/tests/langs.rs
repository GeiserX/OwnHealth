//! The language registry gates (`ownhealth_core::langs`): every localized string under `data/` has
//! every registry language, every registry language has its own wording-lint patterns, and the UI
//! registry lists the same languages. A missing string fails here instead of falling back to English.
//! The controls run each check with a dummy language and watch it go red.

use std::path::{Path, PathBuf};

use ownhealth_core::langs::LANGS;
use ownhealth_rules::cited;
use ownhealth_rules::wording::missing_patterns;
use serde_json::json;

type R = anyhow::Result<()>;

const DATA: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../../data");
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
