//! Cited data files: versioned YAML outside the rule pack (`data/calculators`, `data/prevention`,
//! `data/family`, `data/timeline`, `data/summary`, ...). The shape every such file shares:
//!
//! ```yaml
//! version: "prevention 0.1.0"
//! rows:                         # optional; every row names its source
//!   - id: example.one
//!     cite: https://...         # https, never http
//! templates:                    # anywhere in the file: a mapping with a language key is a template
//!   example.one: {en: "...", es: "..."}
//! ```
//!
//! `load` refuses a file without a version, a row without an https `cite`, a template missing the
//! text of a registry language (`ownhealth_core::langs::LANGS`), and a template the wording lint
//! flags. Area loaders add their own checks on top.
//!
//! A language key is any code of `ownhealth_core::langs::LOCALES`, so the planned `de`, `fr`, `ca`,
//! `it` and `pt` count too, and so does a key that ends in `_<code>`. A data key such as `ca`
//! (calcium) or `pt` (prothrombin time), or one like `ratio_it` or `dose_pt`, reads as a language: its
//! mapping becomes a template that must carry every registry language, and the file is refused. Name
//! such keys another way (`calcium`, `pt_inr`).

use std::collections::BTreeSet;
use std::path::Path;

use ownhealth_core::langs::{LANGS, LOCALES};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

use crate::wording::imperative_phrases;

/// A file that breaks the rules above.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct CitedError(pub String);

impl From<CitedError> for ownhealth_core::Error {
    fn from(e: CitedError) -> Self {
        ownhealth_core::Error::Other(anyhow::Error::new(e))
    }
}

/// One loaded file: `sha256` is the hex hash of its bytes, `doc` the whole parsed file (header
/// fields and templates for the area loader).
#[derive(Debug, Clone)]
pub struct CitedFile {
    pub version: String,
    pub sha256: String,
    pub rows: Vec<Map<String, Value>>,
    pub doc: Value,
}

/// Read and check one cited file.
pub fn load(path: &Path) -> Result<CitedFile, CitedError> {
    let bytes = std::fs::read(path).map_err(|e| CitedError(format!("{}: {e}", path.display())))?;
    load_bytes(path, &bytes)
}

/// Check one file whose bytes are already in hand (a file compiled into the binary); `path` names
/// it in errors and picks YAML or JSON by its extension.
pub fn load_bytes(path: &Path, bytes: &[u8]) -> Result<CitedFile, CitedError> {
    let name = path.display();
    let doc = parse(path, bytes)?;
    let version = doc
        .get("version")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .ok_or_else(|| CitedError(format!("{name}: no `version`")))?
        .to_string();
    let rows = match doc.get("rows") {
        None => Vec::new(),
        Some(Value::Array(rows)) => rows
            .iter()
            .enumerate()
            .map(|(i, r)| {
                let row = r
                    .as_object()
                    .ok_or_else(|| CitedError(format!("{name}: rows[{i}] is not a mapping")))?;
                if !row.get("cite").and_then(Value::as_str).is_some_and(is_https) {
                    return Err(CitedError(format!("{name}: rows[{i}] has no https `cite`")));
                }
                Ok(row.clone())
            })
            .collect::<Result<_, _>>()?,
        Some(_) => return Err(CitedError(format!("{name}: `rows` is not a list"))),
    };
    check_templates(&doc, "$", &name.to_string())?;
    Ok(CitedFile {
        version,
        sha256: hex::encode(Sha256::digest(bytes)),
        rows,
        doc,
    })
}

/// Parse a YAML or JSON data file (by extension) into JSON values.
pub fn read_doc(path: &Path) -> Result<Value, CitedError> {
    let bytes = std::fs::read(path).map_err(|e| CitedError(format!("{}: {e}", path.display())))?;
    parse(path, &bytes)
}

/// Every string that is the text of a language in a mapping (`en: ...`, or `short_en: ...`), with its
/// json path. Every code in the registry or planned for it counts, so the wording lint reads a new
/// language from its first string.
pub fn localized(doc: &Value) -> Vec<(String, String)> {
    let mut out = Vec::new();
    walk_localized(doc, "$", &mut out);
    out
}

/// Every text a registry language lacks: for each mapping with a language key (`en`) or a
/// language-suffixed key (`short_en`), one line per language of `langs` whose text is missing or
/// empty, `"<path> has no `<key>` text"`. Pass `ownhealth_core::langs::LANGS`; a test passes a
/// dummy language to prove the check sees every mapping.
pub fn missing_langs(doc: &Value, langs: &[&str]) -> Vec<String> {
    let mut out = Vec::new();
    walk_missing(doc, "$", langs, &mut out);
    out
}

/// A key that names a language: a code in the registry, planned for it, or in `extra`.
fn is_code(key: &str, extra: &[&str]) -> bool {
    LANGS.contains(&key) || LOCALES.contains(&key) || extra.contains(&key)
}

/// `short` for `short_en`.
fn suffix_stem<'a>(key: &'a str, extra: &[&str]) -> Option<&'a str> {
    key.rsplit_once('_')
        .filter(|(stem, code)| !stem.is_empty() && is_code(code, extra))
        .map(|(stem, _)| stem)
}

fn walk_missing(v: &Value, path: &str, langs: &[&str], out: &mut Vec<String>) {
    match v {
        Value::Object(m) => {
            let has = |key: &str| m.get(key).and_then(Value::as_str).is_some_and(|t| !t.trim().is_empty());
            let stems: BTreeSet<&str> = m.keys().filter_map(|k| suffix_stem(k, langs)).collect();
            let mut wanted: Vec<String> = Vec::new();
            if m.keys().any(|k| is_code(k, langs)) {
                wanted.extend(langs.iter().map(|l| l.to_string()));
            }
            for stem in stems {
                wanted.extend(langs.iter().map(|l| format!("{stem}_{l}")));
            }
            out.extend(
                wanted
                    .into_iter()
                    .filter(|k| !has(k))
                    .map(|k| format!("{path} has no `{k}` text")),
            );
            for (k, v) in m {
                walk_missing(v, &format!("{path}.{k}"), langs, out);
            }
        }
        Value::Array(a) => {
            for (i, v) in a.iter().enumerate() {
                walk_missing(v, &format!("{path}[{i}]"), langs, out);
            }
        }
        _ => {}
    }
}

fn parse(path: &Path, bytes: &[u8]) -> Result<Value, CitedError> {
    let err = |e: String| CitedError(format!("{}: {e}", path.display()));
    if path.extension().and_then(|e| e.to_str()) == Some("json") {
        serde_json::from_slice(bytes).map_err(|e| err(e.to_string()))
    } else {
        serde_yaml::from_slice(bytes).map_err(|e| err(e.to_string()))
    }
}

/// `https://` followed by a host, with no whitespace.
fn is_https(cite: &str) -> bool {
    cite.strip_prefix("https://")
        .is_some_and(|rest| !rest.is_empty() && !rest.starts_with('/') && !rest.contains(char::is_whitespace))
}

fn walk_localized(v: &Value, path: &str, out: &mut Vec<(String, String)>) {
    match v {
        Value::Object(m) => {
            for (k, v) in m {
                let at = format!("{path}.{k}");
                match v {
                    Value::String(s) if is_code(k, &[]) || suffix_stem(k, &[]).is_some() => out.push((at, s.clone())),
                    _ => walk_localized(v, &at, out),
                }
            }
        }
        Value::Array(a) => {
            for (i, v) in a.iter().enumerate() {
                walk_localized(v, &format!("{path}[{i}]"), out);
            }
        }
        _ => {}
    }
}

fn check_templates(v: &Value, path: &str, name: &str) -> Result<(), CitedError> {
    match v {
        Value::Object(m) => {
            if m.keys().any(|k| is_code(k, &[])) {
                for lang in LANGS {
                    let text = m
                        .get(lang)
                        .and_then(Value::as_str)
                        .filter(|t| !t.trim().is_empty())
                        .ok_or_else(|| CitedError(format!("{name}: {path} has no `{lang}` text")))?;
                    if let Some(hit) = imperative_phrases(text).first() {
                        return Err(CitedError(format!(
                            "{name}: {path}.{lang} reads as advice ({hit:?}); write information, not instructions"
                        )));
                    }
                }
            }
            for (k, v) in m {
                check_templates(v, &format!("{path}.{k}"), name)?;
            }
        }
        Value::Array(a) => {
            for (i, v) in a.iter().enumerate() {
                check_templates(v, &format!("{path}[{i}]"), name)?;
            }
        }
        _ => {}
    }
    Ok(())
}
