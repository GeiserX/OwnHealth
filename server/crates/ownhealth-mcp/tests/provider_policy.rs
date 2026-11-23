//! The provider policy (`data/connectors/providers.yaml`) at every MCP path: an Oura ring and a native
//! Dexcom sensor (withheld until their connector pages quote the vendors' terms) never reach an agent,
//! while an Apple Watch (served) does. search_observations and get_trend count what they left out in
//! `withheld.vendor_terms`; the status, its resource, the calculators and the freshness date change
//! nothing at all.

mod common;

use chrono::{Duration, NaiveDate, Utc};
use common::*;
use serde_json::{Value, json};

fn today() -> NaiveDate {
    Utc::now().date_naive()
}

/// One wearable_source of Alex's with daily `metric` rollups `ago` days before today.
async fn days(w: &World, provider: &str, device: &str, metric: &str, ago: &[i64], v: f32) -> anyhow::Result<()> {
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
    for d in ago {
        let t = (today() - Duration::days(*d)).and_hms_opt(0, 0, 0).map(|t| t.and_utc());
        sqlx::query(
            "insert into wearable_rollup (profile_id, metric, source_id, bucket, t, n, v_min, v_max, v_avg, v_sum, \
             v_p10, v_p50, v_p90, coverage) values ($1, $2, $3, 'day', $4, 1, $5, $5, $5, $5, $5, $5, $5, 1)",
        )
        .bind(pid)
        .bind(metric)
        .bind(sid)
        .bind(t)
        .bind(v)
        .execute(&w.db.pool)
        .await?;
    }
    Ok(())
}

/// The Apple Watch: resting heart rate 5 to 9 days ago, systolic pressure on the last 3 days.
async fn served(w: &World) -> R {
    days(w, "apple_health", "Apple Watch", "hr_resting", &[5, 6, 7, 8, 9], 55.0).await?;
    days(w, "apple_health", "Apple Watch", "steps", &[5, 6, 7, 8, 9], 8000.0).await
}

/// The Oura ring and the native Dexcom sensor: newer days, more of them, and metrics the watch lacks.
async fn withheld(w: &World) -> R {
    let recent: Vec<i64> = (0..20).collect();
    days(w, "oura", "Oura Ring", "hr_resting", &recent, 48.0).await?;
    days(w, "oura", "Oura Ring", "hrv_rmssd", &recent, 61.0).await?;
    days(w, "oura", "Oura Ring", "sleep_asleep", &recent, 450.0).await?;
    days(w, "dexcom", "Dexcom G7", "glucose_cgm", &recent, 140.0).await?;
    // a cuff behind a withheld vendor: the calculators would read its 30-day mean
    days(w, "withings", "BPM Connect", "bp_systolic", &[0, 1, 2], 150.0).await
}

fn values(env: &Value, list: &str) -> Vec<Option<f64>> {
    env["data"][list]
        .as_array()
        .map(|a| a.iter().map(|p| p["value"].as_f64()).collect())
        .unwrap_or_default()
}

fn vendor_terms(env: &Value) -> i64 {
    env["withheld"]["vendor_terms"].as_i64().unwrap_or(0)
}

fn says_vendor_terms(env: &Value) -> bool {
    env["limitations"]
        .as_array()
        .is_some_and(|l| l.iter().any(|x| x.as_str() == Some(VENDOR_TERMS)))
}

const VENDOR_TERMS: &str = ownhealth_ingest_structured::wearables::VENDOR_TERMS_SENTENCE;

#[tokio::test]
async fn search_observations_leaves_out_a_withheld_vendor_and_counts_it() -> R {
    let w = world().await?;
    served(&w).await?;
    withheld(&w).await?;
    let c = w.connect_real(&w.tok().await?).await?;
    let search = |q: &str| {
        json!({"profile": "alex", "query": q, "category": "wearable", "detail": "full",
               "from": (today() - Duration::days(30)).to_string(), "to": today().to_string()})
    };
    let res = c.call("search_observations", search("hr_resting")).await?;
    let hr = res.ok();
    // the text block says it too
    assert!(res.text.contains("vendor_terms=20"), "{}", res.text);
    // only the watch's five days; the ring's twenty are counted, never shown
    assert_eq!(values(&hr, "results"), vec![Some(55.0); 5], "{hr}");
    assert_eq!(vendor_terms(&hr), 20, "{hr}");
    assert!(says_vendor_terms(&hr), "{hr}");
    assert!(!dumps(&hr).contains("Oura"), "{hr}");
    // a metric only the withheld vendors hold: nothing shown, and the count says why. The person did
    // measure it, so the answer never says it was not measured.
    let cgm = c.call("search_observations", search("glucose_cgm")).await?.ok();
    assert_eq!(values(&cgm, "results"), Vec::<Option<f64>>::new(), "{cgm}");
    assert_eq!(vendor_terms(&cgm), 20, "{cgm}");
    assert!(says_vendor_terms(&cgm), "{cgm}");
    assert!(cgm["data"].get("not_measured").is_none(), "{cgm}");
    assert!(!dumps(&cgm["limitations"]).contains("Not measured"), "{cgm}");
    assert!(!dumps(&cgm).contains("Dexcom"), "{cgm}");
    // with no withheld day either, the empty answer still says not measured
    let none = c.call("search_observations", search("spo2")).await?.ok();
    assert_eq!(none["data"]["not_measured"], json!(true), "{none}");
    assert!(none["withheld"].get("vendor_terms").is_none(), "{none}");
    // a served metric with no withheld day keeps the envelope it always had
    let steps = c.call("search_observations", search("steps")).await?.ok();
    assert_eq!(values(&steps, "results"), vec![Some(8000.0); 5], "{steps}");
    assert!(steps["withheld"].get("vendor_terms").is_none(), "{steps}");
    assert!(!says_vendor_terms(&steps), "{steps}");
    Ok(())
}

#[tokio::test]
async fn get_trend_leaves_out_a_withheld_vendor_and_counts_it() -> R {
    let w = world().await?;
    served(&w).await?;
    withheld(&w).await?;
    let c = w.connect_real(&w.tok().await?).await?;
    let trend = |code: &str| json!({"profile": "alex", "code": code, "window": "P30D", "resolution": "day"});
    let hr = c.call("get_trend", trend("hr_resting")).await?.ok();
    assert_eq!(values(&hr, "points"), vec![Some(55.0); 5], "{hr}");
    assert_eq!(vendor_terms(&hr), 20, "{hr}");
    assert!(says_vendor_terms(&hr), "{hr}");
    for code in ["hrv_rmssd", "glucose_cgm", "sleep_asleep"] {
        let env = c.call("get_trend", trend(code)).await?.ok();
        assert_eq!(values(&env, "points"), Vec::<Option<f64>>::new(), "{code}: {env}");
        assert_eq!(vendor_terms(&env), 20, "{code}: {env}");
        assert!(
            !dumps(&env).contains("Oura") && !dumps(&env).contains("Dexcom"),
            "{env}"
        );
    }
    // the CGM by its general name: still nothing from the sensor
    let glucose = c.call("get_trend", trend("glucosa")).await?.ok();
    assert_eq!(values(&glucose, "points"), Vec::<Option<f64>>::new(), "{glucose}");
    Ok(())
}

/// What the status tool, the status resource, the calculators and the profiles list say for Alex.
async fn agent_view(w: &World) -> anyhow::Result<Vec<Value>> {
    let c = w.connect_real(&w.tok().await?).await?;
    let mut out = Vec::new();
    let status = c.call("get_health_status", json!({"profile": "alex"})).await?;
    let env = status.ok();
    assert!(dumps(&env).len() < 2048, "{}", dumps(&env).len());
    out.push(env["data"].clone());
    let res: Value = serde_json::from_str(&c.read_text("ownhealth://profile/alex/status").await?)?;
    out.push(res["data"].clone());
    let calc: Value = serde_json::from_str(&c.read_text("ownhealth://profile/alex/calculators").await?)?;
    out.push(calc["items"].clone());
    out.push(calc["calculators_hash"].clone());
    let profiles: Value = serde_json::from_str(&c.read_text("ownhealth://profiles").await?)?;
    out.push(json!(
        profiles["profiles"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|p| (p["slug"].clone(), p["data_freshness"].clone()))
            .collect::<Vec<_>>()
    ));
    Ok(out)
}

#[tokio::test]
async fn status_calculators_and_freshness_never_see_a_withheld_vendor() -> R {
    let w = world().await?;
    served(&w).await?;
    let before = agent_view(&w).await?;
    // the watch is served: its newest day is the freshness date
    let fresh = before[4].as_array().cloned().unwrap_or_default();
    let alex = fresh.iter().find(|p| p[0] == "alex").cloned().unwrap_or(Value::Null);
    assert_eq!(
        alex[1]["wearables"],
        json!((today() - Duration::days(5)).to_string()),
        "{alex}"
    );
    withheld(&w).await?;
    let after = agent_view(&w).await?;
    for (i, (b, a)) in before.iter().zip(&after).enumerate() {
        assert_eq!(b, a, "view {i} moved with a withheld vendor");
    }
    Ok(())
}

/// A served vendor's data does reach the same paths: the gate is the policy, not the metric.
#[tokio::test]
async fn a_served_vendor_reaches_the_status_and_the_freshness() -> R {
    let w = world().await?;
    let before = agent_view(&w).await?;
    served(&w).await?;
    let after = agent_view(&w).await?;
    assert_ne!(before[0], after[0], "the status reads the watch");
    assert_ne!(before[4], after[4], "the freshness reads the watch");
    Ok(())
}

/// The file imports are the person's own exports, so their providers are served: FIT activity heart
/// rate and both CGM CSV vendors (LibreView, Dexcom Clarity; the provider names their importers write,
/// pinned by ownhealth-devices tests/providers.rs) reach an agent with nothing withheld,
/// while the same sensor through the native Dexcom API does not.
#[tokio::test]
async fn fit_and_cgm_csv_imports_reach_an_agent() -> R {
    let w = world().await?;
    days(&w, "fit", "Forerunner 265", "hr_resting", &[1, 2, 3], 52.0).await?;
    days(&w, "libreview", "FreeStyle Libre 3", "glucose_cgm", &[1, 2], 101.0).await?;
    days(&w, "dexcom_clarity", "Dexcom G7", "glucose_cgm", &[3, 4], 121.0).await?;
    days(&w, "dexcom", "Dexcom G7", "glucose_cgm", &[5, 6, 7], 140.0).await?;
    let c = w.connect_real(&w.tok().await?).await?;
    let trend = |code: &str| json!({"profile": "alex", "code": code, "window": "P30D", "resolution": "day"});
    let hr = c.call("get_trend", trend("hr_resting")).await?.ok();
    assert_eq!(values(&hr, "points"), vec![Some(52.0); 3], "{hr}");
    assert!(hr["withheld"].get("vendor_terms").is_none(), "{hr}");
    let cgm = c.call("get_trend", trend("glucose_cgm")).await?.ok();
    let mut got = values(&cgm, "points");
    got.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    assert_eq!(got, vec![Some(101.0), Some(101.0), Some(121.0), Some(121.0)], "{cgm}");
    // the native API sensor's three days are counted, never shown
    assert_eq!(vendor_terms(&cgm), 3, "{cgm}");
    Ok(())
}

/// An HRV series shows one source. A withheld vendor is never that source and never a name in the
/// answer, however many and however new its days, and even first in the person's priority: the choice
/// and the left-out note are made among the served sources only.
#[tokio::test]
async fn an_hrv_series_never_chooses_or_names_a_withheld_vendor() -> R {
    let w = world().await?;
    let recent: Vec<i64> = (0..20).collect();
    days(&w, "oura", "Oura Ring", "hrv_rmssd", &recent, 61.0).await?;
    days(&w, "gadgetbridge", "Amazfit Band", "hrv_rmssd", &[5, 6, 7], 44.0).await?;
    days(&w, "fit", "Forerunner 265", "hrv_rmssd", &[8], 50.0).await?;
    let check = |res: &ToolRes| {
        let env = res.ok();
        assert_eq!(values(&env, "points"), vec![Some(44.0); 3], "{env}");
        assert_eq!(vendor_terms(&env), 20, "{env}");
        assert!(says_vendor_terms(&env), "{env}");
        assert!(dumps(&env).contains("Forerunner 265"), "the served source left out is named: {env}");
        assert!(!dumps(&env).contains("Oura") && !res.text.contains("Oura"), "{env}\n{}", res.text);
    };
    let trend = json!({"profile": "alex", "code": "hrv_rmssd", "window": "P30D", "resolution": "day"});
    let c = w.connect_real(&w.tok().await?).await?;
    check(&c.call("get_trend", trend.clone()).await?);
    // the ring first in Alex's own priority for HRV changes nothing
    let ring: i16 = sqlx::query_scalar("select id from wearable_source where provider = 'oura'")
        .fetch_one(&w.db.pool)
        .await?;
    let mut conn = w.db.pool.acquire().await?;
    ownhealth_ingest_structured::wearables::set_priority(&mut conn, w.pid("alex"), "hrv_rmssd", &[ring]).await?;
    check(&c.call("get_trend", trend).await?);
    Ok(())
}

/// The tools' outputSchema names `withheld.vendor_terms`, so a client that reads the schema learns it.
#[tokio::test]
async fn the_output_schema_names_vendor_terms() -> R {
    let w = world().await?;
    let c = w.connect(&w.tok().await?).await?;
    for t in c.list_tools().await? {
        let withheld = &t["outputSchema"]["properties"]["withheld"];
        assert_eq!(withheld["properties"]["vendor_terms"]["type"], "integer", "{}", t["name"]);
        // optional: absent when nothing was left out
        let required = withheld["required"].as_array().cloned().unwrap_or_default();
        assert!(!required.contains(&json!("vendor_terms")), "{}", t["name"]);
    }
    Ok(())
}
