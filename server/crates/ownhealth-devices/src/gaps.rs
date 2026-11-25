//! Wearable sync-gap windows and the review text for a connection that falls behind.

use std::{collections::BTreeMap, sync::LazyLock};

use chrono::{DateTime, Duration, Utc};
use ownhealth_core::sources;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Deserialize)]
struct Provider {
    name: String,
    stale_hours: i64,
}

type Localized = BTreeMap<String, String>;

#[derive(Deserialize)]
struct Policy {
    providers: BTreeMap<String, Provider>,
    openwearables: Localized,
    dexcom: Localized,
}

// Dexcom can hold CGM readings for 1 h in the US or 3 h elsewhere. Its policy leaves
// room for both. Slice 38 supplies its native vendor sync status.
static POLICY: LazyLock<Policy> = LazyLock::new(|| {
    serde_json::from_str(include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../../data/connectors/sync_gaps.json"
    )))
    .expect("bundled wearable sync-gap policy")
});

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Status {
    Active,
    Expired,
    Revoked,
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
pub(crate) struct ConnectionHealth {
    pub status: Status,
    pub last_synced_at: Option<DateTime<Utc>>,
}

impl ConnectionHealth {
    fn is_gap(self, provider: &Provider, now: DateTime<Utc>) -> bool {
        match self.status {
            Status::Expired | Status::Revoked => true,
            Status::Active => self
                .last_synced_at
                .is_some_and(|synced| now - synced > Duration::hours(provider.stale_hours)),
            Status::Unknown => false,
        }
    }
}

pub(crate) const CURSOR_KEY: &str = "connections";
pub(crate) type Connections = BTreeMap<String, ConnectionHealth>;

pub(crate) fn observed_connections(cursor: Option<&Value>) -> Connections {
    cursor
        .and_then(|c| c.get(CURSOR_KEY))
        .and_then(|c| serde_json::from_value(c.clone()).ok())
        .unwrap_or_default()
}

fn openwearables_gap(cursor: Option<&Value>, pull_failed: bool, now: DateTime<Utc>) -> Option<Value> {
    let connections = observed_connections(cursor);
    let providers: Vec<_> = super::PROVIDERS
        .iter()
        .filter_map(|key| {
            let provider = POLICY.providers.get(*key)?;
            connections
                .get(*key)?
                .is_gap(provider, now)
                .then_some(provider.name.as_str())
        })
        .collect();
    if !providers.is_empty() {
        Some(ow_message(&providers.join(", ")))
    } else {
        pull_failed.then(|| ow_message("Open Wearables"))
    }
}

fn ow_message(names: &str) -> Value {
    let message: BTreeMap<_, _> = POLICY
        .openwearables
        .iter()
        .map(|(lang, text)| (lang, text.replace("{providers}", names)))
        .collect();
    json!({"message": message})
}

fn dexcom_gap(_: Option<&Value>, pull_failed: bool, _: DateTime<Utc>) -> Option<Value> {
    pull_failed.then(|| json!({"message": POLICY.dexcom}))
}

pub(crate) fn register() {
    sources::register_gap_check(super::KIND, openwearables_gap);
    sources::register_gap_check("dexcom", dexcom_gap);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_open_wearables_provider_has_a_freshness_window() {
        for key in super::super::PROVIDERS {
            assert!(POLICY.providers[key].stale_hours > 0, "{key}");
        }
    }

    #[test]
    fn dexcom_vendor_freshness_allows_both_api_delays() {
        let now = DateTime::<Utc>::UNIX_EPOCH;
        let dexcom = &POLICY.providers["dexcom"];
        for hours in [1, 3] {
            let reading = ConnectionHealth {
                status: Status::Active,
                last_synced_at: Some(now - Duration::hours(hours)),
            };
            assert!(!reading.is_gap(dexcom, now), "{hours} h is an expected delay");
        }
    }
}
