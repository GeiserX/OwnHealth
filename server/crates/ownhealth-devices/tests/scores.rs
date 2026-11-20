//! Vendor scores are not measurements (migration 1220): a score-like series key never reaches
//! wearable_sample, and scores live in vendor_score, tagged computed_by 'provider'. Synthetic data for
//! Sam Rivera.

mod common;

use chrono::{TimeZone, Utc};
use common::{Env, R};
use ownhealth_devices::store::{self, Conflict, Sample};
use ownhealth_ingest_structured::wearables;

/// Keys a connector could use for a vendor's score: each one is refused by wearable_sample.
const SCORE_LIKE: [&str; 13] = [
    "body_battery",
    "garmin_body_battery",
    "stress",
    "stress_level",
    "readiness",
    "readiness_score",
    "training_readiness",
    "recovery",
    "recovery_score",
    "sleep_score",
    "activity_score",
    "strain",
    "day_strain",
];

/// Measurements that look close: each one is still accepted.
const MEASUREMENTS: [&str; 4] = ["hr_recovery", "sleep_deep", "stride_length", "hrv_rmssd_night"];

fn sample(metric: &'static str, source: i16) -> Sample {
    Sample {
        metric,
        t: Utc.with_ymd_and_hms(2026, 11, 2, 7, 0, 0).single().unwrap_or_default(),
        t_end: None,
        v: 64.0,
        tz: Some(60),
        source,
    }
}

#[tokio::test]
async fn a_score_like_series_key_never_reaches_wearable_sample() -> R {
    let env = Env::new().await?;
    let sam = env.fam.profile("sam");
    let mut c = env.db.pool.acquire().await?;
    let src = store::device_source(&mut c, sam, "garmin", Some("Forerunner"), None, None).await?;
    for key in SCORE_LIKE {
        let err = store::write(&mut c, sam, None, vec![sample(key, src)], Conflict::Keep).await;
        let msg = format!("{err:?}");
        assert!(
            err.is_err() && msg.contains("wearable_sample_not_a_vendor_score"),
            "{key} reached wearable_sample: {msg}"
        );
    }
    let n = env
        .count("select count(*) from wearable_sample where profile_id = $1", sam)
        .await?;
    assert_eq!(n, 0, "no vendor score is stored as a sample");
    let mut keys: Vec<&'static str> = wearables::METRICS.iter().map(|m| m.0).collect();
    keys.extend(MEASUREMENTS);
    for key in &keys {
        store::write(&mut c, sam, None, vec![sample(key, src)], Conflict::Keep).await?;
    }
    let n = env
        .count("select count(*) from wearable_sample where profile_id = $1", sam)
        .await?;
    assert_eq!(n, keys.len() as i64, "every measurement key is still stored");
    Ok(())
}
