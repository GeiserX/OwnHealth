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
    openwearables_down: Localized,
    dexcom: Localized,
}

const FILE: &str = "data/connectors/sync_gaps.json";
const SHIPPED: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../../data/connectors/sync_gaps.json"
));

/// Read a sync-gap policy. Every Open Wearables vendor needs a window, and every window a vendor.
fn parse(text: &str) -> Result<Policy, String> {
    let policy: Policy = serde_json::from_str(text).map_err(|e| format!("{FILE}: {e}"))?;
    for key in super::PROVIDERS {
        if policy.providers.get(key).is_none_or(|p| p.stale_hours <= 0) {
            return Err(format!("{FILE}: {key} has no freshness window"));
        }
    }
    if let Some(key) = policy
        .providers
        .keys()
        .find(|k| !super::PROVIDERS.contains(&k.as_str()))
    {
        return Err(format!("{FILE}: {key} has no Open Wearables poller"));
    }
    Ok(policy)
}

/// The shipped policy, or None when it does not read: the error is logged once and no gap check runs.
/// The tests read the shipped file, so a broken one fails CI before it ships.
fn policy() -> Option<&'static Policy> {
    static POLICY: LazyLock<Option<Policy>> = LazyLock::new(|| {
        parse(SHIPPED)
            .inspect_err(|e| tracing::error!("{e}: sync-gap alerts are off"))
            .ok()
    });
    POLICY.as_ref()
}

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
    /// The first poll that saw this connection active without any sync time. Its staleness counts
    /// from here, so a vendor that never sends data still raises an alert. OwnHealth sets it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seen_since: Option<DateTime<Utc>>,
    /// The first poll whose read of this vendor's new days failed, in the current streak. A failed
    /// re-read of days already stored does not count. OwnHealth sets it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failing_since: Option<DateTime<Utc>>,
}

impl ConnectionHealth {
    fn is_gap(self, provider: &Provider, now: DateTime<Utc>) -> bool {
        match self.status {
            Status::Expired | Status::Revoked => true,
            // Open Wearables may report a recent sync while OwnHealth cannot read that vendor's data.
            Status::Active => [self.last_synced_at.or(self.seen_since), self.failing_since]
                .into_iter()
                .flatten()
                .any(|since| now - since > Duration::hours(provider.stale_hours)),
            Status::Unknown => false,
        }
    }
}

pub(crate) const CURSOR_KEY: &str = "connections";
pub(crate) type Connections = BTreeMap<String, ConnectionHealth>;

/// Whether a vendor's last known health is a gap at `now`.
pub(crate) fn in_gap(vendor: &str, health: ConnectionHealth, now: DateTime<Utc>) -> bool {
    policy()
        .and_then(|p| p.providers.get(vendor))
        .is_some_and(|provider| health.is_gap(provider, now))
}

pub(crate) fn observed_connections(cursor: Option<&Value>) -> Connections {
    cursor
        .and_then(|c| c.get(CURSOR_KEY))
        .and_then(|c| serde_json::from_value(c.clone()).ok())
        .unwrap_or_default()
}

fn openwearables_gap(cursor: Option<&Value>, pull_failed: bool, now: DateTime<Utc>) -> Option<Value> {
    let policy = policy()?;
    let connections = observed_connections(cursor);
    let providers: Vec<_> = super::PROVIDERS
        .iter()
        .filter_map(|key| {
            let provider = policy.providers.get(*key)?;
            connections
                .get(*key)?
                .is_gap(provider, now)
                .then_some(provider.name.as_str())
        })
        .collect();
    if !providers.is_empty() {
        Some(ow_message(policy, &providers.join(", ")))
    } else {
        // The sidecar itself does not answer: reconnecting accounts would not help.
        pull_failed.then(|| json!({"message": policy.openwearables_down}))
    }
}

fn ow_message(policy: &Policy, names: &str) -> Value {
    let message: BTreeMap<_, _> = policy
        .openwearables
        .iter()
        .map(|(lang, text)| (lang, text.replace("{providers}", names)))
        .collect();
    json!({"message": message})
}

// Only a failing Dexcom pull is a gap for now. Dexcom holds CGM readings back for 1 h in the US and
// 3 h elsewhere, so a freshness window must allow for both; the native connector (slice 38) adds that
// check with its own window.
fn dexcom_gap(_: Option<&Value>, pull_failed: bool, _: DateTime<Utc>) -> Option<Value> {
    let policy = policy()?;
    pull_failed.then(|| json!({"message": policy.dexcom}))
}

/// Register the gap checks at start. A policy that does not read is logged here, not at a gap check.
pub(crate) fn register() {
    if policy().is_none() {
        return;
    }
    sources::register_gap_check(super::KIND, openwearables_gap);
    sources::register_gap_check("dexcom", dexcom_gap);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_shipped_policy_reads() {
        assert!(parse(SHIPPED).is_ok(), "{:?}", parse(SHIPPED).err());
        assert!(policy().is_some());
    }

    #[test]
    fn a_broken_policy_is_refused_with_the_file_and_what_is_wrong() {
        let mut shipped: Value = serde_json::from_str(SHIPPED).expect("the shipped policy is JSON");
        let mut drop_field = shipped.clone();
        drop_field["providers"]["polar"]
            .as_object_mut()
            .expect("polar")
            .shift_remove("stale_hours");
        let mut no_window = shipped.clone();
        no_window["providers"]
            .as_object_mut()
            .expect("providers")
            .shift_remove("oura");
        shipped["providers"]["dexcom"] = json!({"name": "Dexcom", "stale_hours": 3});
        for (broken, wanted) in [
            (drop_field, "stale_hours"),
            (no_window, "oura has no freshness window"),
            (shipped, "dexcom has no Open Wearables poller"),
        ] {
            let err = parse(&broken.to_string()).err().unwrap_or_default();
            assert!(err.starts_with(FILE) && err.contains(wanted), "{err}");
        }
    }
}
