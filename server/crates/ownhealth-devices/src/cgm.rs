//! CGM (lane D): readings and the HL7 CGM IG summary. Interstitial glucose readings are LOINC 99504-3;
//! the summary follows the HL7 CGM IG profiles: mean glucose (97507-8), GMI, and the times-in-ranges
//! panel (106793-3: very low < 54, low 54-69, in range 70-180, high 181-250, very high > 250 mg/dL).
//! A summary is valid only with >= 70% sensor wear over 14 days (the international time-in-range
//! consensus); otherwise it says why it is not valid instead of a number. Information, not diagnosis.

use chrono::{DateTime, Utc};
use ownhealth_core::error::{Error, Result};
use serde::Serialize;
use sqlx::PgConnection;
use uuid::Uuid;

/// The `wearable_sample.metric` of a CGM reading (mg/dL).
pub const METRIC: &str = "glucose_cgm";
pub const LOINC_READING: &str = "99504-3";
pub const LOINC_MEAN_GLUCOSE: &str = "97507-8";
pub const LOINC_TIR_PANEL: &str = "106793-3";
pub const VALID_WEAR_FRACTION: f64 = 0.70;
pub const DEFAULT_DAYS: i64 = 14;

/// One summary over `[start, end)`. Percentages are 0..100 of the readings' time.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CgmSummary {
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
    pub readings: i64,
    pub wear_fraction: f64,
    pub valid: bool,
    /// Why not valid (`insufficient_wear`, `no_readings`), when not.
    pub invalid_reason: Option<String>,
    pub mean_mg_dl: Option<f64>,
    pub gmi_percent: Option<f64>,
    pub very_low_pct: Option<f64>,
    pub low_pct: Option<f64>,
    pub in_range_pct: Option<f64>,
    pub high_pct: Option<f64>,
    pub very_high_pct: Option<f64>,
}

/// The summary of `readings` (time, mg/dL) over `[start, end)` with the sensor's reading interval
/// (5 min for most sensors, 15 min for some Libre setups). Pure.
pub fn summarize(
    _readings: &[(DateTime<Utc>, f64)],
    _start: DateTime<Utc>,
    _end: DateTime<Utc>,
    _interval_minutes: f64,
) -> Result<CgmSummary> {
    Err(Error::NotImplemented("cgm::summarize"))
}

/// The summary of a profile's stored readings over the last `days` days before `end`.
pub async fn cgm_summary(
    _conn: &mut PgConnection,
    _profile_id: Uuid,
    _end: DateTime<Utc>,
    _days: i64,
) -> Result<CgmSummary> {
    Err(Error::NotImplemented("cgm::cgm_summary"))
}
