//! Vendor scores are not measurements (migration 1220): a score-like series key never reaches
//! wearable_sample, and scores live in vendor_score, tagged computed_by 'provider'. Synthetic data for
//! Sam Rivera.

mod common;

use chrono::{Datelike, TimeZone, Utc};
use common::{Env, R};
use ownhealth_core::demo::create_rivera_family;
use ownhealth_core::sources::{self, NewSource, SourcePatch};
use ownhealth_devices::scores::{self, CATEGORIES, Score};
use ownhealth_devices::store::{self, Conflict, Sample};
use ownhealth_ingest_structured::wearables;
use serde_json::json;

/// Keys a connector could use for a vendor's score: each one is refused by wearable_sample.
const SCORE_LIKE: [&str; 30] = [
    "body_battery",
    "bodybattery",
    "BodyBattery",
    "body_Battery",
    "bodyBattery",
    "body-battery",
    "stresslevel",
    "stress.level",
    "STRESS",
    "readinessScore",
    "Readiness",
    "sleepScore",
    "nightly_recharge",
    "ans_charge",
    "resilience",
    "hrv_status",
    "training_status",
    "sleep_performance",
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

/// Keys that hold no score word but are not plain (a capital, a '-', a '.', a space, a leading digit or
/// '_'): only the plain-key rule refuses them.
const NOT_PLAIN: [&str; 7] = ["Hr", "HeartRate", "hr-rest", "hr.rest", "hr rest", "1hr", "_hr"];

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
    for key in NOT_PLAIN {
        let err = store::write(&mut c, sam, None, vec![sample(key, src)], Conflict::Keep).await;
        let msg = format!("{err:?}");
        assert!(
            err.is_err() && msg.contains("wearable_sample_not_a_vendor_score"),
            "{key} is not a plain key, yet it reached wearable_sample: {msg}"
        );
    }
    let n = env
        .count("select count(*) from wearable_sample where profile_id = $1", sam)
        .await?;
    assert_eq!(
        n, 0,
        "no vendor score and no key that is not plain is stored as a sample"
    );
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
    // no score word in them, so only the plain-key rule keeps them out
    for key in NOT_PLAIN {
        assert!(!scores::is_score_key(key), "{key} holds a score word");
        assert!(!scores::is_sample_key(key), "{key} is not a plain key");
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
    let src = env.source("gadgetbridge", Some("sam"), json!({}), None).await?.id;
    let mut c = env.db.pool.acquire().await?;
    let batch = vec![
        score("readiness", 1, 82.0, Some("Good")),
        score("readiness", 2, 74.0, None),
        score("sleep", 2, 88.0, Some("Optimal")),
    ];
    assert_eq!(scores::write(&mut c, sam, src, &batch).await?.written, 3);
    // the same pull again changes nothing; a new value for one morning replaces it
    assert_eq!(scores::write(&mut c, sam, src, &batch).await?.written, 0);
    let again = vec![score("readiness", 2, 76.0, Some("Fair"))];
    assert_eq!(scores::write(&mut c, sam, src, &again).await?.written, 1);
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
    assert!(scores::write(&mut c, sam, src, &bad).await.is_err());
    Ok(())
}

fn garmin_morning(value: f64) -> Score {
    Score {
        provider: "garmin".into(),
        category: "body_battery".into(),
        t: Utc.with_ymd_and_hms(2026, 11, 20, 8, 0, 0).single().unwrap_or_default(),
        value,
        qualifier: None,
        components: None,
    }
}

/// Two connections that both bring Garmin's Body Battery for the same moment keep a row each, and the
/// read names the connection of every row.
#[tokio::test]
async fn two_connections_with_the_same_score_keep_a_row_each() -> R {
    let env = Env::new().await?;
    let sam = env.fam.profile("sam");
    let gb = env.source("gadgetbridge", Some("sam"), json!({}), None).await?;
    let hae = env.source("health_auto_export", Some("sam"), json!({}), None).await?;
    let mut c = env.db.pool.acquire().await?;
    scores::write(&mut c, sam, gb.id, &[garmin_morning(62.0)]).await?;
    scores::write(&mut c, sam, hae.id, &[garmin_morning(64.0)]).await?;
    let rows: Vec<(Option<uuid::Uuid>, f64)> =
        sqlx::query_as("select source_id, value from vendor_score where profile_id = $1 order by value")
            .bind(sam)
            .fetch_all(&mut *c)
            .await?;
    assert_eq!(rows, vec![(Some(gb.id), 62.0), (Some(hae.id), 64.0)]);
    Ok(())
}

/// A vendor quirk in one score never fails the batch: a value that is not a number is dropped, an
/// empty or non-object `components` is left out, a long qualifier is cut.
#[tokio::test]
async fn one_vendor_quirk_does_not_fail_the_write() -> R {
    let env = Env::new().await?;
    let sam = env.fam.profile("sam");
    let src = env.source("gadgetbridge", Some("sam"), json!({}), None).await?;
    let mut c = env.db.pool.acquire().await?;
    let mut nan = score("readiness", 1, f64::NAN, None);
    nan.t = Utc.with_ymd_and_hms(2026, 11, 1, 5, 0, 0).single().unwrap_or_default();
    let mut null_parts = score("readiness", 2, 74.0, None);
    null_parts.components = Some(serde_json::Value::Null);
    let mut list_parts = score("sleep", 2, 80.0, None);
    list_parts.components = Some(json!([1, 2]));
    let long = score("activity", 2, 90.0, Some(&"Very good ".repeat(10)));
    // a finite number past the table's ±1e9 range
    let huge = score("strain", 2, 1e10, None);
    let batch = vec![
        nan,
        null_parts,
        list_parts,
        long,
        huge,
        score("sleep", 3, 88.0, Some("Optimal")),
    ];
    let w = scores::write(&mut c, sam, src.id, &batch).await;
    assert!(w.is_ok(), "{w:?}");
    assert_eq!(
        w?,
        scores::Written { written: 4, dropped: 2 },
        "the value that is not a number and the value past ±1e9 are dropped and counted"
    );
    let rows: Vec<(String, f64, Option<serde_json::Value>, Option<String>)> = sqlx::query_as(
        "select category, value, components, qualifier from vendor_score where profile_id = $1 order by t, category",
    )
    .bind(sam)
    .fetch_all(&mut *c)
    .await?;
    let got: Vec<(&str, f64, bool, usize)> = rows
        .iter()
        .map(|(cat, v, comp, q)| {
            (
                cat.as_str(),
                *v,
                comp.is_some(),
                q.as_deref().map_or(0, |q| q.chars().count()),
            )
        })
        .collect();
    assert_eq!(
        got,
        vec![
            ("activity", 90.0, true, 80),
            ("readiness", 74.0, false, 0),
            ("sleep", 80.0, false, 0),
            ("sleep", 88.0, true, 7)
        ]
    );
    Ok(())
}

/// A U+0000 that Postgres cannot store, in a qualifier or anywhere in `components`, drops that score and
/// counts it; the rest of the batch is written.
#[tokio::test]
async fn a_nul_character_drops_its_score_not_the_batch() -> R {
    let env = Env::new().await?;
    let sam = env.fam.profile("sam");
    let src = env.source("gadgetbridge", Some("sam"), json!({}), None).await?;
    let mut c = env.db.pool.acquire().await?;
    let nul_word = score("readiness", 1, 80.0, Some("Go\u{0}od"));
    let mut nul_value = score("readiness", 2, 81.0, None);
    nul_value.components = Some(json!({"hrv_balance": {"note": "x\u{0}"}}));
    let mut nul_key = score("readiness", 3, 82.0, None);
    nul_key.components = Some(json!({"hrv\u{0}balance": 78}));
    let mut nul_in_list = score("readiness", 4, 83.0, None);
    nul_in_list.components = Some(json!({"tags": ["a", "\u{0}"]}));
    let batch = vec![
        nul_word,
        nul_value,
        nul_key,
        nul_in_list,
        score("sleep", 5, 88.0, Some("Optimal")),
    ];
    let w = scores::write(&mut c, sam, src.id, &batch).await;
    assert!(w.is_ok(), "{w:?}");
    assert_eq!(w?, scores::Written { written: 1, dropped: 4 });
    let rows: Vec<(String, f64)> =
        sqlx::query_as("select category, value from vendor_score where profile_id = $1 order by t")
            .bind(sam)
            .fetch_all(&mut *c)
            .await?;
    assert_eq!(rows, vec![("sleep".to_string(), 88.0)]);
    Ok(())
}

/// One write is one transaction: a mapping bug in the middle of a batch keeps none of it.
#[tokio::test]
async fn a_mapping_error_keeps_none_of_the_batch() -> R {
    let env = Env::new().await?;
    let sam = env.fam.profile("sam");
    let src = env.source("gadgetbridge", Some("sam"), json!({}), None).await?;
    let mut c = env.db.pool.acquire().await?;
    let batch = vec![
        score("readiness", 1, 82.0, Some("Good")),
        score("sleep", 1, 88.0, None),
        score("mood", 1, 5.0, None),
        score("activity", 1, 90.0, None),
    ];
    assert!(scores::write(&mut c, sam, src.id, &batch).await.is_err());
    let n = env
        .count("select count(*) from vendor_score where profile_id = $1", sam)
        .await?;
    assert_eq!(n, 0, "the scores before the bad one are not kept");
    // a mapping error fails the batch even on a score that a quirk would drop anyway
    let mut bad_provider = score("sleep", 2, f64::NAN, None);
    bad_provider.provider = "Oura Ring".into();
    let mut nul_mood = score("mood", 2, 5.0, Some("Go\u{0}od"));
    nul_mood.components = Some(json!({"x\u{0}": 1}));
    for (bad, what) in [
        (score("mood", 2, f64::NAN, None), "an unknown category with a NaN"),
        (score("mood", 2, 1e10, None), "an unknown category past ±1e9"),
        (nul_mood, "an unknown category with a NUL"),
        (bad_provider, "a malformed provider with a NaN"),
    ] {
        let w = scores::write(&mut c, sam, src.id, &[batch[0].clone(), bad]).await;
        assert!(w.is_err(), "{what} was dropped as a quirk: {w:?}");
    }
    let n = env
        .count("select count(*) from vendor_score where profile_id = $1", sam)
        .await?;
    assert_eq!(n, 0, "no batch with a mapping error keeps a score");
    // the connection is still usable after the failed batches
    assert_eq!(scores::write(&mut c, sam, src.id, &batch[..2]).await?.written, 2);
    Ok(())
}

/// One write is one transaction, also when Postgres itself refuses a score after the others were
/// inserted: here the last score's time is before 4713 BC, past the range of a timestamptz.
#[tokio::test]
async fn a_database_error_on_the_last_score_keeps_none_of_the_batch() -> R {
    let env = Env::new().await?;
    let sam = env.fam.profile("sam");
    let src = env.source("gadgetbridge", Some("sam"), json!({}), None).await?;
    let mut c = env.db.pool.acquire().await?;
    let mut too_old = score("activity", 1, 90.0, None);
    too_old.t = Utc.with_ymd_and_hms(-5000, 1, 1, 0, 0, 0).single().unwrap_or_default();
    assert_eq!(too_old.t.year(), -5000, "the test time is really out of range");
    let batch = vec![
        score("readiness", 1, 82.0, Some("Good")),
        score("sleep", 1, 88.0, None),
        too_old,
    ];
    let w = scores::write(&mut c, sam, src.id, &batch).await;
    assert!(w.is_err(), "Postgres took a time from 5000 BC: {w:?}");
    let n = env
        .count("select count(*) from vendor_score where profile_id = $1", sam)
        .await?;
    assert_eq!(n, 0, "the scores inserted before the refused one are not kept");
    Ok(())
}

/// A score names a connection of its own person, or a household connection of its own household,
/// never another person's or another household's.
#[tokio::test]
async fn a_score_never_names_another_persons_or_households_connection() -> R {
    let env = Env::new().await?;
    let sam = env.fam.profile("sam");
    let mut c = env.db.pool.acquire().await?;
    let ortega = create_rivera_family(&mut c, &format!("Ortega {}", ownhealth_core::ids::token_hex(3))).await?;
    let new_source = |household, profile: Option<uuid::Uuid>, label: &'static str| NewSource {
        profile_id: profile,
        ..NewSource::new(household, "gadgetbridge", label)
    };
    let lucias = sources::create_source(
        &mut c,
        &new_source(env.fam.household_id, Some(env.fam.profile("lucia")), "Lucía's"),
    )
    .await?;
    let other_house = sources::create_source(&mut c, &new_source(ortega.household_id, None, "Other house")).await?;
    let other_sams = sources::create_source(
        &mut c,
        &new_source(ortega.household_id, Some(ortega.profile("sam")), "Other Sam"),
    )
    .await?;
    for (src, what) in [
        (lucias, "Lucía's connection"),
        (other_house, "another household's connection"),
        (other_sams, "another household's Sam's connection"),
    ] {
        let w = scores::write(&mut c, sam, src, &[garmin_morning(62.0)]).await;
        assert!(
            matches!(w, Err(ownhealth_core::Error::Provenance(_))),
            "Sam's score was written with {what}: {w:?}"
        );
    }
    let n = env
        .count("select count(*) from vendor_score where profile_id = $1", sam)
        .await?;
    assert_eq!(n, 0, "no score names a connection that is not Sam's");
    // Sam's own connection, and a household connection of Sam's household, are fine
    let own = env.source("gadgetbridge", Some("sam"), json!({}), None).await?.id;
    let household = env.source("gadgetbridge", None, json!({}), None).await?.id;
    assert_eq!(
        scores::write(&mut c, sam, own, &[garmin_morning(62.0)]).await?.written,
        1
    );
    assert_eq!(
        scores::write(&mut c, sam, household, &[garmin_morning(63.0)])
            .await?
            .written,
        1
    );
    Ok(())
}

/// Even a row that names another household's connection (written past `write`) never shows that
/// connection's label or id.
#[tokio::test]
async fn the_read_never_shows_another_households_connection() -> R {
    let env = Env::new().await?;
    let sam = env.fam.profile("sam");
    let mut c = env.db.pool.acquire().await?;
    let ortega = create_rivera_family(&mut c, &format!("Ortega {}", ownhealth_core::ids::token_hex(3))).await?;
    let theirs = sources::create_source(
        &mut c,
        &NewSource::new(
            ortega.household_id,
            "gadgetbridge",
            "Carmen Ortega's watch (other house)",
        ),
    )
    .await?;
    let own = env.source("gadgetbridge", Some("sam"), json!({}), None).await?.id;
    for (src, v) in [(theirs, 40.0), (own, 62.0)] {
        sqlx::query(
            "insert into vendor_score (profile_id, source_id, provider, category, t, value) \
             values ($1, $2, 'garmin', 'body_battery', now(), $3)",
        )
        .bind(sam)
        .bind(src)
        .bind(v)
        .execute(&mut *c)
        .await?;
    }
    let since = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).single().unwrap_or_default();
    let got: Vec<(f64, Option<serde_json::Value>)> = scores::list(&mut c, sam, since)
        .await?
        .into_iter()
        .map(|r| (r.value, r.source))
        .collect();
    assert_eq!(
        got,
        vec![(62.0, Some(json!({"id": own, "label": "test inbox"}))), (40.0, None)],
        "the other household's connection is not named"
    );
    Ok(())
}

/// A connection later moved to another person of the household no longer names the scores it brought
/// before: they stay, but they never show the new person's label or id. A connection that becomes a
/// household one still names them.
#[tokio::test]
async fn a_connection_moved_to_another_person_no_longer_names_the_old_scores() -> R {
    let env = Env::new().await?;
    let (sam, lucia) = (env.fam.profile("sam"), env.fam.profile("lucia"));
    let mut c = env.db.pool.acquire().await?;
    let sams = env.source("gadgetbridge", Some("sam"), json!({}), None).await?.id;
    let household = env.source("gadgetbridge", None, json!({}), None).await?.id;
    let to_household = env.source("gadgetbridge", Some("sam"), json!({}), None).await?.id;
    let mut morning = garmin_morning(62.0);
    for src in [sams, household, to_household] {
        scores::write(&mut c, sam, src, std::slice::from_ref(&morning)).await?;
        morning.t += chrono::TimeDelta::hours(1);
    }
    let move_to = |profile: Option<uuid::Uuid>, label: &str| SourcePatch {
        label: Some(label.into()),
        profile_id: Some(profile),
        ..SourcePatch::default()
    };
    sources::update_source(&mut c, sams, &move_to(Some(lucia), "Lucía's watch")).await?;
    sources::update_source(&mut c, household, &move_to(Some(lucia), "Lucía's ring")).await?;
    sources::update_source(&mut c, to_household, &move_to(None, "Hall scale")).await?;
    let since = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).single().unwrap_or_default();
    let got: Vec<Option<serde_json::Value>> = scores::list(&mut c, sam, since)
        .await?
        .into_iter()
        .map(|r| r.source)
        .collect();
    assert_eq!(
        got,
        vec![Some(json!({"id": to_household, "label": "Hall scale"})), None, None],
        "Sam's scores never name the connections that are now Lucía's"
    );
    // the moved connection can no longer bring Sam a score
    let w = scores::write(&mut c, sam, sams, &[garmin_morning(63.0)]).await;
    assert!(matches!(w, Err(ownhealth_core::Error::Provenance(_))), "{w:?}");
    Ok(())
}

/// The read is one person's: Sam never sees Lucía's scores, and Lucía never sees Sam's.
#[tokio::test]
async fn the_read_returns_only_the_profiles_own_scores() -> R {
    let env = Env::new().await?;
    let (sam, lucia) = (env.fam.profile("sam"), env.fam.profile("lucia"));
    let mut c = env.db.pool.acquire().await?;
    let gb = env.source("gadgetbridge", Some("sam"), json!({}), None).await?;
    let gl = env.source("gadgetbridge", Some("lucia"), json!({}), None).await?;
    scores::write(&mut c, sam, gb.id, &[garmin_morning(62.0)]).await?;
    scores::write(
        &mut c,
        lucia,
        gl.id,
        &[garmin_morning(35.0), score("readiness", 2, 70.0, None)],
    )
    .await?;
    let since = Utc.with_ymd_and_hms(2026, 10, 1, 0, 0, 0).single().unwrap_or_default();
    let values = |rows: Vec<scores::Stored>| rows.iter().map(|r| r.value).collect::<Vec<_>>();
    assert_eq!(values(scores::list(&mut c, sam, since).await?), vec![62.0]);
    assert_eq!(values(scores::list(&mut c, lucia, since).await?), vec![35.0, 70.0]);
    Ok(())
}

/// Removing a connection keeps the scores it brought; they no longer name it.
#[tokio::test]
async fn removing_the_connection_keeps_its_scores() -> R {
    let env = Env::new().await?;
    let sam = env.fam.profile("sam");
    let src = env.source("gadgetbridge", Some("sam"), json!({}), None).await?;
    let mut c = env.db.pool.acquire().await?;
    scores::write(&mut c, sam, src.id, &[garmin_morning(62.0)]).await?;
    assert!(ownhealth_core::sources::delete_source(&mut c, src.id).await?);
    let rows: Vec<(Option<uuid::Uuid>, f64)> =
        sqlx::query_as("select source_id, value from vendor_score where profile_id = $1")
            .bind(sam)
            .fetch_all(&mut *c)
            .await?;
    assert_eq!(rows, vec![(None, 62.0)]);
    Ok(())
}

/// The CHECK on wearable_sample, as Postgres prints it.
async fn guard(env: &Env) -> anyhow::Result<String> {
    Ok(sqlx::query_scalar(
        "select pg_get_constraintdef(oid) from pg_constraint where conrelid = 'wearable_sample'::regclass \
         and conname = 'wearable_sample_not_a_vendor_score'",
    )
    .fetch_one(&env.db.pool)
    .await?)
}

#[tokio::test]
async fn the_guard_lists_exactly_the_rust_score_words() -> R {
    let env = Env::new().await?;
    let def = guard(&env).await?;
    let group = def
        .split('\'')
        .find(|p| p.starts_with('(') && p.contains('|'))
        .unwrap_or_default();
    let mut listed: Vec<&str> = group.trim_matches(|c| c == '(' || c == ')').split('|').collect();
    listed.sort_unstable();
    let mut ours = scores::SCORE_WORDS.to_vec();
    ours.sort_unstable();
    assert_eq!(listed, ours, "{def}");
    Ok(())
}

/// The CHECK and `is_sample_key` give the same verdict on one shared list of names: every key the
/// slice knows, each score word plain, inside a key, at either end, in capitals and joined, and keys
/// that are not plain.
#[tokio::test]
async fn the_guard_and_is_sample_key_agree_on_every_name() -> R {
    let env = Env::new().await?;
    let def = guard(&env).await?;
    let expr = def.strip_prefix("CHECK ").unwrap_or(&def);
    let mut names: Vec<String> = SCORE_LIKE
        .iter()
        .chain(&MEASUREMENTS)
        .copied()
        .chain(wearables::METRICS.iter().map(|m| m.0))
        .map(String::from)
        .collect();
    for w in scores::SCORE_WORDS.iter().chain(&["recovery"]) {
        let cap = format!("{}{}", w[..1].to_uppercase(), &w[1..]);
        names.extend([
            w.to_string(),
            format!("x_{w}"),
            format!("{w}_x"),
            format!("x{w}"),
            format!("{w}2"),
            w.to_uppercase(),
            format!("day{cap}"),
            format!("day-{w}"),
        ]);
    }
    names.extend(
        [
            "",
            "Hr",
            "1hr",
            "_hr",
            "hr-rest",
            "hr.rest",
            "hr rest",
            "hr_é",
            "hr_recovery",
            "heart_rate_recovery",
        ]
        .map(String::from),
    );
    let q = format!("select {expr} from (select $1::text as metric) s");
    for name in &names {
        let db: bool = sqlx::query_scalar(sqlx::AssertSqlSafe(q.clone()))
            .bind(name)
            .fetch_one(&env.db.pool)
            .await?;
        assert_eq!(db, scores::is_sample_key(name), "{name:?}: the CHECK says {db}");
    }
    // and the list holds both verdicts
    assert!(names.iter().any(|n| scores::is_sample_key(n)) && names.iter().any(|n| !scores::is_sample_key(n)));
    Ok(())
}
