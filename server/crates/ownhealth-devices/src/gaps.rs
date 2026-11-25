//! Wearable sync-gap windows and the review text for a connection that falls behind.

use std::{collections::BTreeMap, sync::LazyLock};

use chrono::{DateTime, Duration, Utc};
use ownhealth_core::{langs, sources};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Deserialize)]
struct Provider {
    name: String,
    stale_hours: i64,
    /// `stale_hours` as a duration, checked when the policy is read.
    #[serde(skip)]
    window: Duration,
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
/// Every alert text needs every language of the registry.
fn parse(text: &str) -> Result<Policy, String> {
    let mut policy: Policy = serde_json::from_str(text).map_err(|e| format!("{FILE}: {e}"))?;
    let no_window = |key: &str| format!("{FILE}: {key} has no freshness window");
    for key in super::PROVIDERS {
        let provider = policy.providers.get_mut(key).ok_or_else(|| no_window(key))?;
        // a window chrono cannot hold is refused here, not at the first gap check
        provider.window = Duration::try_hours(provider.stale_hours)
            .filter(|window| *window > Duration::zero())
            .ok_or_else(|| no_window(key))?;
    }
    for (section, texts) in [
        ("openwearables", &policy.openwearables),
        ("openwearables_down", &policy.openwearables_down),
        ("dexcom", &policy.dexcom),
    ] {
        let have: Vec<&str> = texts
            .iter()
            .filter(|(_, text)| !text.trim().is_empty())
            .map(|(lang, _)| lang.as_str())
            .collect();
        if let Some(lang) = langs::missing(&have, &langs::LANGS).first() {
            return Err(format!("{FILE}: {section} has no {lang} text"));
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
    /// The last poll whose report included this connection. When no newer report says anything about
    /// it, its gap is judged as of this poll, not as of a later one. OwnHealth sets it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_at: Option<DateTime<Utc>>,
}

impl ConnectionHealth {
    fn is_gap(self, provider: &Provider, now: DateTime<Utc>) -> bool {
        match self.status {
            Status::Expired | Status::Revoked => true,
            // Open Wearables may report a recent sync while OwnHealth cannot read that vendor's data.
            Status::Active => [self.last_synced_at.or(self.seen_since), self.failing_since]
                .into_iter()
                .flatten()
                .any(|since| now - since > provider.window),
            Status::Unknown => false,
        }
    }
}

pub(crate) const CURSOR_KEY: &str = "connections";
/// The cursor key that holds when the connection report first failed, in the current streak of polls
/// whose report failed. A poll whose report reads removes it.
pub(crate) const REPORT_FAILED_KEY: &str = "report_failed_at";
pub(crate) type Connections = BTreeMap<String, ConnectionHealth>;

/// Whether a vendor was in a gap when a report last included it.
pub(crate) fn in_gap_when_observed(vendor: &str, health: ConnectionHealth) -> bool {
    let provider = policy().and_then(|p| p.providers.get(vendor));
    match (provider, health.observed_at) {
        (Some(provider), Some(at)) => health.is_gap(provider, at),
        _ => false,
    }
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
    // While the connection report fails, nothing new is known about any vendor. Judge each one as of the
    // last report that included it, so a service outage does not age into a vendor's gap.
    let report_failed = cursor.is_some_and(|c| c.get(REPORT_FAILED_KEY).is_some());
    let providers: Vec<_> = super::PROVIDERS
        .iter()
        .filter_map(|key| {
            let provider = policy.providers.get(*key)?;
            let health = connections.get(*key)?;
            let at = if report_failed { health.observed_at? } else { now };
            health.is_gap(provider, at).then_some(provider.name.as_str())
        })
        .collect();
    if !providers.is_empty() {
        Some(ow_message(policy, &providers.join(", ")))
    } else {
        // OwnHealth's pull itself fails (the sidecar does not answer, or no vendor reads): reconnecting
        // accounts would not help.
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

    /// A window chrono cannot hold would panic at the first gap check; the policy is refused instead.
    #[test]
    fn a_window_too_long_to_measure_is_refused() {
        for hours in [i64::MAX, i64::MAX / 3_600_000 + 1] {
            let mut policy: Value = serde_json::from_str(SHIPPED).expect("the shipped policy is JSON");
            policy["providers"]["polar"]["stale_hours"] = json!(hours);
            let err = parse(&policy.to_string()).err().unwrap_or_default();
            assert!(
                err.starts_with(FILE) && err.contains("polar has no freshness window"),
                "{hours}: {err}"
            );
        }
    }

    /// Every alert text exists in every registry language, so no alert opens without its guidance.
    #[test]
    fn every_alert_text_needs_every_registry_language() {
        for section in ["openwearables", "openwearables_down", "dexcom"] {
            for lang in langs::LANGS {
                let shipped: Value = serde_json::from_str(SHIPPED).expect("the shipped policy is JSON");
                let mut missing = shipped.clone();
                missing[section]
                    .as_object_mut()
                    .expect("a message map")
                    .shift_remove(lang);
                let mut blank = shipped;
                blank[section][lang] = json!("  ");
                for broken in [missing, blank] {
                    let err = parse(&broken.to_string()).err().unwrap_or_default();
                    assert!(
                        err.starts_with(FILE) && err.contains(&format!("{section} has no {lang} text")),
                        "{section} {lang}: {err}"
                    );
                }
            }
        }
    }
}
