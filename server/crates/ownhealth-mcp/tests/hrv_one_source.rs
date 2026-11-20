//! An agent reading HRV over MCP gets one vendor's series, never a day-by-day mix of two vendors.

mod common;

use common::*;
use serde_json::{Value, json};

/// One wearable_source of Alex's (made once) with daily `metric` rollups on the given days of November 2026.
async fn days(w: &World, provider: &str, device: &str, metric: &str, days: &[u32], v: f32) -> anyhow::Result<()> {
    let pid = w.pid("alex");
    let sid: i16 = sqlx::query_scalar(
        "with s as (insert into wearable_source (profile_id, provider, device) values ($1, $2, $3) \
         on conflict do nothing returning id) select id from s union all \
         select id from wearable_source where profile_id = $1 and provider = $2 and device = $3 limit 1",
    )
    .bind(pid)
    .bind(provider)
    .bind(device)
    .fetch_one(&w.db.pool)
    .await?;
    for d in days {
        sqlx::query(
            "insert into wearable_rollup (profile_id, metric, source_id, bucket, t, n, v_min, v_max, v_avg, v_sum, \
             v_p10, v_p50, v_p90, coverage) values ($1, $2, $3, 'day', $4, 1, $5, $5, $5, $5, $5, $5, $5, 1)",
        )
        .bind(pid)
        .bind(metric)
        .bind(sid)
        .bind(format!("2026-11-{d:02}T00:00:00Z").parse::<chrono::DateTime<chrono::Utc>>()?)
        .bind(v)
        .execute(&w.db.pool)
        .await?;
    }
    Ok(())
}

fn values(env: &Value, list: &str) -> Vec<Option<f64>> {
    env["data"][list]
        .as_array()
        .map(|a| a.iter().map(|p| p["value"].as_f64()).collect())
        .unwrap_or_default()
}

/// The envelope says which vendor the series shows and which it left out.
fn names_left_out(env: &Value) -> bool {
    env["limitations"].as_array().is_some_and(|l| {
        l.iter()
            .filter_map(Value::as_str)
            .any(|n| n.contains("Oura Ring") && n.contains("Whoop 4.0"))
    })
}

#[tokio::test]
async fn an_agent_sees_one_hrv_vendor_and_mixed_resting_hr() -> R {
    let w = world().await?;
    days(&w, "oura", "Oura Ring", "hrv_rmssd", &[1, 3, 5, 7, 9], 48.0).await?;
    days(&w, "whoop", "Whoop 4.0", "hrv_rmssd", &[2, 4, 6, 8, 10], 71.0).await?;
    days(&w, "oura", "Oura Ring", "hr_resting", &[1, 3], 55.0).await?;
    days(&w, "whoop", "Whoop 4.0", "hr_resting", &[2, 4], 58.0).await?;
    let c = w.connect_real(&w.tok().await?).await?;
    let trend = |code: &str| json!({"profile": "alex", "code": code, "window": "P30D", "as_of": "2026-11-12"});
    let hrv = c.call("get_trend", trend("hrv_rmssd")).await?.ok();
    // the same count of days each: the vendor with the newest day
    assert_eq!(values(&hrv, "points"), vec![Some(71.0); 5], "{hrv}");
    assert!(names_left_out(&hrv), "{hrv}");
    let found = c
        .call(
            "search_observations",
            json!({"profile": "alex", "query": "hrv_rmssd", "category": "wearable", "detail": "full",
                   "from": "2026-11-01", "to": "2026-11-12"}),
        )
        .await?
        .ok();
    assert_eq!(values(&found, "results"), vec![Some(71.0); 5], "{found}");
    assert!(names_left_out(&found), "{found}");
    // resting heart rate still takes each day from whichever source has it
    let hr = c.call("get_trend", trend("hr_resting")).await?.ok();
    assert_eq!(
        values(&hr, "points"),
        [Some(55.0), Some(58.0), Some(55.0), Some(58.0)],
        "{hr}"
    );
    assert!(!names_left_out(&hr), "{hr}");
    Ok(())
}
