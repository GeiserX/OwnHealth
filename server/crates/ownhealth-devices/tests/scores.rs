//! Vendor scores are not measurements (migration 1220): a score-like series key never reaches
//! wearable_sample, and scores live in vendor_score, tagged computed_by 'provider'. Synthetic data for
//! Sam Rivera.

mod common;

use chrono::{TimeZone, Utc};
use common::{Env, R};
use ownhealth_devices::scores::{self, CATEGORIES, Score};
use ownhealth_devices::store::{self, Conflict, Sample};
use ownhealth_ingest_structured::wearables;
use serde_json::json;

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

#[test]
fn is_score_key_is_the_same_rule_as_the_check() {
    for key in SCORE_LIKE {
        assert!(scores::is_score_key(key), "{key}");
    }
    for key in wearables::METRICS.iter().map(|m| m.0).chain(MEASUREMENTS) {
        assert!(!scores::is_score_key(key), "{key}");
    }
}

#[tokio::test]
async fn the_category_check_lists_exactly_the_rust_categories() -> R {
    let env = Env::new().await?;
    let def: String = sqlx::query_scalar(
        "select pg_get_constraintdef(oid) from pg_constraint where conrelid = 'vendor_score'::regclass \
         and contype = 'c' and pg_get_constraintdef(oid) like '%category%'",
    )
    .fetch_one(&env.db.pool)
    .await?;
    let mut listed: Vec<&str> = def.split('\'').skip(1).step_by(2).collect();
    listed.sort_unstable();
    let mut ours = CATEGORIES.to_vec();
    ours.sort_unstable();
    assert_eq!(listed, ours, "{def}");
    Ok(())
}

fn score(category: &str, day: u32, value: f64, qualifier: Option<&str>) -> Score {
    Score {
        provider: "oura".into(),
        category: category.into(),
        t: Utc
            .with_ymd_and_hms(2026, 11, day, 6, 30, 0)
            .single()
            .unwrap_or_default(),
        value,
        qualifier: qualifier.map(String::from),
        components: Some(json!({"hrv_balance": 78, "resting_heart_rate": 91})),
    }
}

#[tokio::test]
async fn scores_are_stored_apart_tagged_by_provider_and_a_repull_updates_in_place() -> R {
    let env = Env::new().await?;
    let sam = env.fam.profile("sam");
    let mut c = env.db.pool.acquire().await?;
    let batch = vec![
        score("readiness", 1, 82.0, Some("Good")),
        score("readiness", 2, 74.0, None),
        score("sleep", 2, 88.0, Some("Optimal")),
    ];
    assert_eq!(scores::write(&mut c, sam, None, &batch).await?, 3);
    // the same pull again changes nothing; a new value for one morning replaces it
    assert_eq!(scores::write(&mut c, sam, None, &batch).await?, 0);
    let again = vec![score("readiness", 2, 76.0, Some("Fair"))];
    assert_eq!(scores::write(&mut c, sam, None, &again).await?, 1);
    let since = Utc.with_ymd_and_hms(2026, 10, 1, 0, 0, 0).single().unwrap_or_default();
    let rows = scores::list(&mut c, sam, since).await?;
    let got: Vec<(&str, f64, Option<&str>, &str)> = rows
        .iter()
        .map(|r| {
            (
                r.category.as_str(),
                r.value,
                r.qualifier.as_deref(),
                r.computed_by.as_str(),
            )
        })
        .collect();
    assert_eq!(
        got,
        vec![
            ("readiness", 82.0, Some("Good"), "provider"),
            ("readiness", 76.0, Some("Fair"), "provider"),
            ("sleep", 88.0, Some("Optimal"), "provider"),
        ]
    );
    assert!(rows.iter().all(|r| r.provider == "oura"));
    // nothing went into the measurements
    let n = env
        .count("select count(*) from wearable_sample where profile_id = $1", sam)
        .await?;
    assert_eq!(n, 0);
    // an unknown category is refused
    let bad = vec![score("mood", 3, 5.0, None)];
    assert!(scores::write(&mut c, sam, None, &bad).await.is_err());
    Ok(())
}
