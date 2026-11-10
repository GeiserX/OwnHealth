//! A write rebuilds only the rollup buckets its samples can reach, and the result is the same as a full
//! rebuild. Synthetic data for Sam Rivera.

mod common;

use chrono::{DateTime, Duration, TimeZone, Utc};
use common::{Env, R};
use ownhealth_devices::store::{self, Conflict, Sample};
use ownhealth_ingest_structured::wearables;

type Row = (
    String,
    i16,
    String,
    DateTime<Utc>,
    i32,
    Option<f32>,
    Option<f32>,
    Option<f32>,
    f32,
);

async fn rollups(env: &Env, profile: uuid::Uuid) -> anyhow::Result<Vec<Row>> {
    Ok(sqlx::query_as(
        "select metric, source_id, bucket, t, n, v_avg, v_sum, v_p50, coverage from wearable_rollup \
         where profile_id = $1 order by metric, source_id, bucket, t",
    )
    .bind(profile)
    .fetch_all(&env.db.pool)
    .await?)
}

#[tokio::test]
async fn a_write_rebuilds_only_the_buckets_it_reaches_and_matches_a_full_rebuild() -> R {
    let env = Env::new().await?;
    let sam = env.fam.profile("sam");
    let mut c = env.db.pool.acquire().await?;
    let src = store::device_source(&mut c, sam, "health_auto_export", None, Some("rollup-test"), None).await?;
    let t0 = Utc.with_ymd_and_hms(2026, 9, 1, 0, 0, 0).single().unwrap_or_default();
    // 40 days of hourly heart rate and steps west and east of UTC, and a night of sleep stages a day
    let mut history = Vec::new();
    for h in 0..40 * 24i64 {
        let t = t0 + Duration::hours(h);
        let tz = if h % 48 < 24 { -300 } else { 600 };
        history.push(Sample {
            metric: "hr",
            t,
            t_end: None,
            v: (55 + h % 30) as f64,
            tz: Some(tz),
            source: src,
        });
        history.push(Sample {
            metric: "steps",
            t,
            t_end: None,
            v: (h % 500) as f64,
            tz: Some(tz),
            source: src,
        });
        if h % 24 == 23 {
            for k in 0..6 {
                let s = t + Duration::minutes(40 * k);
                history.push(Sample {
                    metric: "sleep_asleep",
                    t: s,
                    t_end: Some(s + Duration::minutes(30)),
                    v: 30.0,
                    tz: Some(tz),
                    source: src,
                });
            }
        }
    }
    store::write(&mut c, sam, None, history, Conflict::Refresh).await?;
    let old_day = t0 + Duration::days(5);
    let xmin_of = |t: DateTime<Utc>| {
        sqlx::query_scalar::<_, String>(
            "select xmin::text from wearable_rollup where profile_id = $1 and metric = 'hr' and bucket = 'day' and t = $2",
        )
        .bind(sam)
        .bind(t)
    };
    let before = xmin_of(old_day).fetch_one(&mut *c).await?;
    // a late push for day 39 and a correction on day 20
    let late = t0 + Duration::days(39) + Duration::minutes(30);
    let fix = t0 + Duration::days(20) + Duration::hours(3);
    store::write(
        &mut c,
        sam,
        None,
        vec![
            Sample {
                metric: "hr",
                t: late,
                t_end: None,
                v: 99.0,
                tz: Some(-300),
                source: src,
            },
            Sample {
                metric: "hr",
                t: fix,
                t_end: None,
                v: 41.0,
                tz: Some(600),
                source: src,
            },
            Sample {
                metric: "sleep_asleep",
                t: late,
                t_end: Some(late + Duration::minutes(20)),
                v: 20.0,
                tz: Some(-300),
                source: src,
            },
        ],
        Conflict::Refresh,
    )
    .await?;
    let after = xmin_of(old_day).fetch_one(&mut *c).await?;
    assert_eq!(before, after, "a bucket 15 days before the write was rewritten");
    let ranged = rollups(&env, sam).await?;
    wearables::recompute_rollups(&mut c, sam, None, None).await?;
    let full = rollups(&env, sam).await?;
    assert_eq!(ranged.len(), full.len());
    for (a, b) in ranged.iter().zip(&full) {
        assert_eq!(a, b);
    }
    Ok(())
}
