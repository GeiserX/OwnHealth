//! Vendor scores (migration 1220): readiness, recovery, sleep and activity scores, stress, strain and
//! Body Battery are the vendor's numbers, computed with its own method. They are stored apart from
//! measurements, in `vendor_score`, tagged `computed_by = 'provider'` with the provider's name, and
//! never written into `wearable_sample` (a CHECK there refuses a key `is_score_key` names).
//!
//! The rule pack, the calculators and MCP never read them; only the Wearables page shows them.

use chrono::{DateTime, Utc};
use ownhealth_core::Result;
use serde::Serialize;
use serde_json::Value;
use sqlx::PgConnection;
use uuid::Uuid;

/// The `vendor_score.category` CHECK, in the order the Wearables page lists them.
pub const CATEGORIES: [&str; 7] = [
    "readiness",
    "recovery",
    "sleep",
    "activity",
    "stress",
    "strain",
    "body_battery",
];

/// Words that mark a series key as a vendor score, wherever they stand in the key.
const SCORE_WORDS: [&str; 5] = ["readiness", "battery", "stress", "strain", "score"];

/// True when `metric` names a vendor score, so it belongs in `vendor_score`, never `wearable_sample`:
/// one of its words (split on '_') is in `SCORE_WORDS`, or its first word is `recovery`
/// (`hr_recovery` is a measurement). The same rule as the CHECK `wearable_sample_not_a_vendor_score`.
pub fn is_score_key(metric: &str) -> bool {
    metric.split('_').any(|w| SCORE_WORDS.contains(&w)) || metric.split('_').next() == Some("recovery")
}

/// One score as a connector maps it.
#[derive(Debug, Clone, PartialEq)]
pub struct Score {
    /// The vendor, as `wearable_source.provider` names it (`oura`, `garmin`, `whoop`).
    pub provider: String,
    /// One of `CATEGORIES`.
    pub category: String,
    pub t: DateTime<Utc>,
    pub value: f64,
    /// The vendor's own word for the score ("Optimal").
    pub qualifier: Option<String>,
    /// The vendor's contributors to the score, as a JSON object.
    pub components: Option<Value>,
}

/// A stored score, as the read route sends it.
#[derive(Debug, Clone, PartialEq, Serialize, sqlx::FromRow)]
pub struct Stored {
    pub provider: String,
    pub category: String,
    pub t: DateTime<Utc>,
    pub value: f64,
    pub qualifier: Option<String>,
    pub components: Option<Value>,
    pub computed_by: String,
}

/// Store `scores` for one profile, brought by the connector `source_id`. A score already stored for
/// the same provider, category and time takes the new value, qualifier and components. Returns the
/// rows inserted or changed.
pub async fn write(
    conn: &mut PgConnection,
    profile_id: Uuid,
    source_id: Option<Uuid>,
    scores: &[Score],
) -> Result<u64> {
    let mut n = 0;
    for s in scores.iter().filter(|s| s.value.is_finite()) {
        n += sqlx::query(
            "insert into vendor_score (profile_id, source_id, provider, category, t, value, qualifier, components) \
             values ($1, $2, $3, $4, $5, $6, $7, $8) on conflict (profile_id, provider, category, t) do update \
             set value = excluded.value, qualifier = excluded.qualifier, components = excluded.components, \
             source_id = coalesce(excluded.source_id, vendor_score.source_id) \
             where (vendor_score.value, vendor_score.qualifier, vendor_score.components) \
             is distinct from (excluded.value, excluded.qualifier, excluded.components)",
        )
        .bind(profile_id)
        .bind(source_id)
        .bind(&s.provider)
        .bind(&s.category)
        .bind(s.t)
        .bind(s.value)
        .bind(&s.qualifier)
        .bind(&s.components)
        .execute(&mut *conn)
        .await?
        .rows_affected();
    }
    Ok(n)
}

/// The profile's scores from `since` on, by provider, category and time.
pub async fn list(conn: &mut PgConnection, profile_id: Uuid, since: DateTime<Utc>) -> Result<Vec<Stored>> {
    Ok(sqlx::query_as(
        "select provider, category, t, value, qualifier, components, computed_by from vendor_score \
         where profile_id = $1 and t >= $2 order by provider, category, t",
    )
    .bind(profile_id)
    .bind(since)
    .fetch_all(conn)
    .await?)
}
