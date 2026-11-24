use std::collections::HashMap;

use super::{content_sha, fhir_id};
use serde_json::Value;

fn rest_id(id: &str) -> bool {
    fhir_id(id) && !matches!(id, "." | "..")
}

fn under_base<'a>(url: &'a str, base: &str) -> Option<&'a str> {
    url.strip_prefix(base)?.strip_prefix('/')
}

/// A versionless REST identity on the authorized base.
pub(crate) fn entry_id<'a>(url: &'a str, base: &str, kind: &str) -> Option<&'a str> {
    let (resource_type, id) = under_base(url, base)?.split_once('/')?;
    (resource_type == kind && rest_id(id)).then_some(id)
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct Reference {
    id: String,
    version: Option<String>,
}

impl Reference {
    /// Accept only a raw Medication path on the authorized base. A relative path needs an order
    /// fullUrl that identifies that same order. The parser never normalizes a reference.
    pub(crate) fn in_order(raw: &str, base: &str, order_url: Option<&str>, order: &Value) -> Option<Self> {
        let path = if raw.starts_with("Medication/") {
            let kind = order["resourceType"].as_str()?;
            let id = entry_id(order_url?, base, kind)?;
            if order["id"] != id {
                return None;
            }
            raw
        } else {
            under_base(raw, base)?
        };
        let path = path.strip_prefix("Medication/")?;
        let (id, version) = path
            .split_once("/_history/")
            .map_or((path, None), |(id, version)| (id, Some(version)));
        if !rest_id(id) || version.is_some_and(|version| !rest_id(version)) {
            return None;
        }
        Some(Self {
            id: id.into(),
            version: version.map(String::from),
        })
    }

    pub(crate) fn read_url(&self, base: &str) -> (&str, String) {
        let full = self.full_url(base);
        let url = match &self.version {
            Some(version) => format!("{full}/_history/{version}"),
            None => full,
        };
        (&self.id, url)
    }

    pub(crate) fn full_url(&self, base: &str) -> String {
        format!("{base}/Medication/{}", self.id)
    }

    pub(crate) fn matches_version(&self, resource: &Value) -> bool {
        self.version
            .as_deref()
            .is_none_or(|version| resource["meta"]["versionId"] == version)
    }
}

/// A contained reference is local to one order, even when the order has no fullUrl.
pub(crate) fn contained<'a>(order: &'a Value, raw: &str) -> Option<&'a Value> {
    let id = raw.strip_prefix('#').filter(|id| super::fhir_id(id))?;
    let mut matches = order["contained"]
        .as_array()?
        .iter()
        .filter(|r| r["resourceType"] == "Medication" && r["id"] == id);
    let medication = matches.next()?;
    matches.next().is_none().then_some(medication)
}

pub(crate) enum Match<'a> {
    Missing,
    Ambiguous,
    Found(&'a Value),
}

/// A full URL can identify more than one version. Keep that ambiguity for unversioned references
/// and for conflicting copies of a version. Repeated copies of the same resource are harmless.
#[derive(Default)]
pub(crate) struct Index<'a>(HashMap<Reference, Option<(&'a Value, String)>>);

impl<'a> Index<'a> {
    pub(crate) fn insert(&mut self, base: &str, full_url: Option<&str>, resource: &'a Value) {
        let Some(url) = full_url else {
            return;
        };
        // a version read stands for that version only, never for the current Medication
        let (id, only_version) = match entry_id(url, base, "Medication") {
            Some(id) => (id, false),
            None => {
                let Some((id, version)) = under_base(url, base)
                    .and_then(|path| path.strip_prefix("Medication/"))
                    .and_then(|path| path.split_once("/_history/"))
                    .filter(|(id, version)| rest_id(id) && rest_id(version))
                else {
                    return;
                };
                if resource["meta"]["versionId"] != version {
                    return;
                }
                (id, true)
            }
        };
        if resource["id"] != id {
            return;
        }
        let hash = content_sha(resource);
        let version = resource["meta"]["versionId"].as_str();
        let mut store = |version: Option<&str>| {
            let key = Reference {
                id: id.into(),
                version: version.map(String::from),
            };
            self.0
                .entry(key)
                .and_modify(|held| {
                    if let Some((old, digest)) = held
                        && (old["meta"]["versionId"] != resource["meta"]["versionId"] || *digest != hash)
                    {
                        *held = None;
                    }
                })
                .or_insert_with(|| Some((resource, hash.clone())));
        };
        if !only_version {
            store(None);
        }
        if version.is_some() {
            store(version);
        }
    }

    pub(crate) fn lookup(&self, reference: &Reference) -> Match<'a> {
        match self.0.get(reference) {
            None => Match::Missing,
            Some(None) => Match::Ambiguous,
            Some(Some((resource, _))) => Match::Found(resource),
        }
    }
}
