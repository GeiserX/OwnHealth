//! The provider policy (`data/connectors/providers.yaml`): per wearable provider
//! (`wearable_source.provider`), whether its samples are stored and whether MCP, the rule-pack markers
//! and the calculators may read them.
//!
//! The shared readers apply it: `ownhealth_rules::engine::wearables` (markers and calculators),
//! `ownhealth_ingest_structured::wearables::points` read as [`Reader::Shared`] (MCP rollups) and the
//! MCP freshness date. Only the Wearables page and its REST routes read every provider. A provider the
//! file does not name is withheld and dropped, so a new importer must add its line before an agent
//! sees its data.

use std::collections::BTreeMap;
use std::sync::OnceLock;

use serde::Deserialize;

const SHIPPED: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../../data/connectors/providers.yaml"
));

/// What happens to a provider's samples when a connector meets them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Ingest {
    Allow,
    Drop,
}

/// Whether agents and the shared readers see a provider's data.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mcp {
    Served,
    Withheld,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Provider {
    pub ingest: Ingest,
    pub mcp: Mcp,
    /// How the data arrives; documentation only.
    pub path: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Policy {
    pub version: u32,
    pub providers: BTreeMap<String, Provider>,
}

/// Who reads wearable data. The Wearables page and its REST routes read every provider; every other
/// reader (MCP, the rule-pack markers, the calculators) only the providers the policy serves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reader {
    WearablesPage,
    Shared,
}

impl Reader {
    /// Whether this reader may see `provider`'s data.
    pub fn sees(self, provider: &str) -> bool {
        self == Reader::WearablesPage || served(provider)
    }
}

/// Parse a policy file. A provider key follows the `wearable_source.provider` form: 1 to 40 lowercase
/// letters, digits or `_`.
pub fn parse(text: &str) -> Result<Policy, String> {
    let p: Policy = serde_yaml::from_str(text).map_err(|e| format!("providers.yaml: {e}"))?;
    if let Some(bad) = p.providers.keys().find(|k| {
        k.is_empty()
            || k.len() > 40
            || !k
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
    }) {
        return Err(format!("providers.yaml: {bad:?} is not a provider key"));
    }
    Ok(p)
}

/// The shipped policy. The file is compiled in and its test parses it, so it cannot fail at run time;
/// if it ever did, every provider would be withheld and dropped.
pub fn policy() -> &'static Policy {
    static P: OnceLock<Policy> = OnceLock::new();
    P.get_or_init(|| {
        parse(SHIPPED).unwrap_or_else(|e| {
            tracing::error!("{e}: every wearable provider is withheld");
            Policy {
                version: 0,
                providers: BTreeMap::new(),
            }
        })
    })
}

/// Whether MCP, the markers and the calculators may read `provider`'s data. Unknown: no.
pub fn served(provider: &str) -> bool {
    policy().providers.get(provider).is_some_and(|p| p.mcp == Mcp::Served)
}

/// Whether `provider`'s samples may be stored. Unknown: no.
pub fn ingest_allowed(provider: &str) -> bool {
    policy()
        .providers
        .get(provider)
        .is_some_and(|p| p.ingest == Ingest::Allow)
}

/// Every provider the policy serves, for a SQL filter (`provider = any($n)`).
pub fn served_providers() -> Vec<String> {
    policy()
        .providers
        .iter()
        .filter(|(_, p)| p.mcp == Mcp::Served)
        .map(|(k, _)| k.clone())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_shipped_file_parses() {
        let p = parse(SHIPPED).expect("providers.yaml");
        assert_eq!(p.version, 1);
        assert_eq!(policy().providers.len(), p.providers.len());
    }

    #[test]
    fn the_open_wearables_vendors_and_native_dexcom_are_withheld() {
        for p in ["oura", "whoop", "polar", "withings", "dexcom"] {
            assert!(!served(p), "{p}");
            assert!(ingest_allowed(p), "{p}");
            assert!(Reader::WearablesPage.sees(p) && !Reader::Shared.sees(p), "{p}");
        }
    }

    #[test]
    fn the_sources_served_before_stay_served() {
        for p in [
            "apple_health",
            "health_auto_export",
            "gadgetbridge",
            "nightscout",
            "strava_export",
            "fit",
            "libreview",
            "dexcom_clarity",
        ] {
            assert!(served(p) && ingest_allowed(p), "{p}");
            assert!(served_providers().contains(&p.to_string()), "{p}");
        }
    }

    #[test]
    fn an_unknown_provider_is_withheld_and_dropped() {
        for p in ["", "Oura", "new_vendor", "strava", "garmin"] {
            assert!(!served(p), "{p}");
            assert!(!Reader::Shared.sees(p), "{p}");
        }
        assert!(!ingest_allowed("new_vendor") && !ingest_allowed("strava") && !ingest_allowed("garmin"));
        assert!(Reader::WearablesPage.sees("new_vendor"));
    }

    #[test]
    fn a_bad_file_is_refused() {
        assert!(parse("version: 1\nproviders:\n  Oura: {ingest: allow, mcp: served, path: x}\n").is_err());
        assert!(parse("version: 1\nproviders:\n  oura: {ingest: allow, mcp: shared, path: x}\n").is_err());
        assert!(parse("version: 1\nproviders:\n  oura: {ingest: allow, mcp: served}\n").is_err());
        assert!(parse("version: 1\nproviders:\n  oura: {ingest: allow, mcp: served, path: x, ai: yes}\n").is_err());
        assert!(parse("version: 1\nproviders:\n  oura: {ingest: allow, mcp: served, path: x}\n").is_ok());
    }
}
