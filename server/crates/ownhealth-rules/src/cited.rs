//! Cited data files: versioned YAML outside the rule pack (`data/calculators`, `data/prevention`,
//! `data/family`, `data/timeline`, `data/summary`, ...). The shape every such file shares:
//!
//! ```yaml
//! version: "prevention 0.1.0"
//! rows:                         # optional; every row names its source
//!   - id: example.one
//!     cite: https://...         # https, never http
//! templates:                    # anywhere in the file: a mapping with `en` or `es` is a template
//!   example.one: {en: "...", es: "..."}
//! ```
//!
//! `load` refuses a file without a version, a row without an https `cite`, a template missing its
//! `en` or `es` text, and a template the wording lint flags. Area loaders add their own checks on top.

use std::path::Path;

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
    let name = path.display();
    let bytes = std::fs::read(path).map_err(|e| CitedError(format!("{name}: {e}")))?;
    let doc = parse(path, &bytes)?;
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
        sha256: hex::encode(Sha256::digest(&bytes)),
        rows,
        doc,
    })
}

/// Parse a YAML or JSON data file (by extension) into JSON values.
pub fn read_doc(path: &Path) -> Result<Value, CitedError> {
    let bytes = std::fs::read(path).map_err(|e| CitedError(format!("{}: {e}", path.display())))?;
    parse(path, &bytes)
}

/// Every string that is the `en` or `es` value of a mapping, with its json path.
pub fn localized(doc: &Value) -> Vec<(String, String)> {
    let mut out = Vec::new();
    walk_localized(doc, "$", &mut out);
    out
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
                match (k.as_str(), v) {
                    ("en" | "es", Value::String(s)) => out.push((at, s.clone())),
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
            if m.contains_key("en") || m.contains_key("es") {
                for lang in ["en", "es"] {
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
