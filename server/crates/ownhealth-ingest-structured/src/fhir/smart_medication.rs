use std::collections::HashMap;

use serde_json::Value;
use url::Url;

use super::content_sha;

fn fhir_url(text: &str) -> Option<Url> {
    if text
        .bytes()
        .any(|b| b.is_ascii_whitespace() || b.is_ascii_control() || b == b'\\')
    {
        return None;
    }
    let url = Url::parse(text).ok()?;
    (url.username().is_empty() && url.password().is_none() && url.query().is_none() && url.fragment().is_none())
        .then_some(url)
}

/// The FHIR base and ID in an absolute RESTful resource URL.
fn restful<'a>(url: &'a str, kind: &str) -> Option<(&'a str, &'a str)> {
    if !url.starts_with("http://") && !url.starts_with("https://") {
        return None;
    }
    let (prefix, id) = url.rsplit_once('/')?;
    let (base, resource_type) = prefix.rsplit_once('/')?;
    (resource_type == kind && super::fhir_id(id)).then_some((base, id))
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct Reference {
    full_url: String,
    version: Option<String>,
}

impl Reference {
    /// FHIR R4 Bundle references (section 2.36.4.1). A relative reference takes the base of
    /// its own order. An absolute reference can also identify an included entry by URN.
    pub(crate) fn in_order(raw: &str, order_url: Option<&str>, order: &Value) -> Option<Self> {
        let absolute = if raw.starts_with("Medication/") {
            let order_url = fhir_url(order_url?)?;
            let (base, id) = restful(order_url.as_str(), "MedicationRequest")?;
            if order["id"] != id {
                return None;
            }
            let path = raw.strip_prefix("Medication/")?;
            let id = path.split_once("/_history/").map_or(path, |(id, _)| id);
            if !super::fhir_id(id) {
                return None;
            }
            fhir_url(&format!("{base}/{raw}"))?
        } else {
            fhir_url(raw)?
        };
        let (full_url, version) = absolute
            .as_str()
            .rsplit_once("/_history/")
            .filter(|(full, _)| restful(full, "Medication").is_some())
            .map_or((absolute.as_str(), None), |(full, version)| (full, Some(version)));
        if matches!(absolute.scheme(), "http" | "https") {
            restful(full_url, "Medication")?;
            if version.is_some_and(|v| !super::fhir_id(v)) {
                return None;
            }
        }
        Some(Self {
            full_url: full_url.into(),
            version: version.map(String::from),
        })
    }

    /// Only the authorized base can be read. Foreign targets must be in the bundle.
    pub(crate) fn read_url(&self, authorized_base: &Url) -> Option<(&str, String)> {
        let (root, id) = restful(&self.full_url, "Medication")?;
        (root == authorized_base.as_str().trim_end_matches('/')).then(|| {
            let url = match &self.version {
                Some(version) => format!("{}/_history/{version}", self.full_url),
                None => self.full_url.clone(),
            };
            (id, url)
        })
    }

    pub(crate) fn matches_version(&self, resource: &Value) -> bool {
        self.version
            .as_deref()
            .is_none_or(|version| resource["meta"]["versionId"] == version)
    }

    pub(crate) fn into_full_url(self) -> String {
        self.full_url
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
    pub(crate) fn insert(&mut self, full_url: Option<&str>, resource: &'a Value) {
        let Some(url) = full_url.and_then(fhir_url) else {
            return;
        };
        if matches!(url.scheme(), "http" | "https")
            && restful(url.as_str(), "Medication").is_none_or(|(_, id)| resource["id"] != id)
        {
            return;
        }
        let hash = content_sha(resource);
        let version = resource["meta"]["versionId"].as_str();
        let mut store = |version: Option<&str>| {
            let key = Reference {
                full_url: url.as_str().into(),
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
        store(None);
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
