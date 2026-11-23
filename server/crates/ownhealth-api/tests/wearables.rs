//! Lane C routes (`tests/imports/test_records_api.py`, the wearables cases): the summary and one
//! metric's series, on a synthetic Apple Health export of Lucía's.

use std::collections::BTreeSet;
use std::io::Cursor;

use axum::body::Body;
use chrono::{DateTime, NaiveDate, TimeZone, Utc};
use http::{Request, StatusCode, header};
use ownhealth_auth::sessions::{COOKIE, create_session};
use ownhealth_core::Runtime;
use ownhealth_core::demo::{Family, create_rivera_family};
use ownhealth_core::documents::{NewDocument, create_document};
use ownhealth_core::testing::TestDb;
use ownhealth_ingest_structured::apple_health::import_apple_health;
use ownhealth_ingest_structured::apple_health::synthetic::{ExportSpec, write_export_to};
use serde_json::Value;
use tower::ServiceExt;

type R = anyhow::Result<()>;

struct App {
    _db: TestDb,
    _dir: tempfile::TempDir,
    rt: Runtime,
    family: Family,
}

async fn app() -> anyhow::Result<App> {
    let db = TestDb::new().await?;
    let dir = tempfile::tempdir()?;
    let rt = Runtime::with_pool(db.settings(dir.path())?, db.pool.clone())?;
    let mut c = db.pool.acquire().await?;
    let family = create_rivera_family(&mut c, &format!("Rivera {}", ownhealth_core::ids::token_hex(3))).await?;
    Ok(App {
        _db: db,
        _dir: dir,
        rt,
        family,
    })
}

async fn get(a: &App, who: &str, uri: &str) -> anyhow::Result<(StatusCode, Value)> {
    let mut c = a.rt.pool.acquire().await?;
    let secret = create_session(&mut c, a.family.user(who), None, None, "passkey").await?;
    let req = Request::get(uri)
        .header(header::COOKIE, format!("{COOKIE}={secret}"))
        .body(Body::empty())?;
    let res = ownhealth_api::router().with_state(a.rt.clone()).oneshot(req).await?;
    let status = res.status();
    let bytes = axum::body::to_bytes(res.into_body(), 1 << 26).await?;
    Ok((status, serde_json::from_slice(&bytes).unwrap_or(Value::Null)))
}

async fn import_lucia(a: &App) -> R {
    let spec = ExportSpec {
        days: 40,
        end: NaiveDate::from_ymd_opt(2026, 11, 7).unwrap_or_default(),
        birth_date: NaiveDate::from_ymd_opt(2017, 6, 21).unwrap_or_default(),
        first_name: "Lucía".into(),
        ..ExportSpec::default()
    };
    let mut buf = Cursor::new(Vec::new());
    write_export_to(&mut buf, &spec)?;
    let data = buf.into_inner();
    let sha =
        a.rt.blobs
            .put_bytes(ownhealth_core::blobs::BlobOwner::Reference, &data)?;
    let mut tx = a.rt.pool.begin().await?;
    let doc = create_document(
        &mut tx,
        &NewDocument {
            filename: Some("export.zip"),
            ..NewDocument::upload(
                a.family.household_id,
                Some(a.family.profile("lucia")),
                &sha,
                "application/zip",
                data.len() as i64,
            )
        },
    )
    .await?;
    import_apple_health(&mut tx, &a.rt.blobs, doc.id).await?;
    tx.commit().await?;
    Ok(())
}

fn round1(x: f64) -> f64 {
    format!("{x:.1}").parse().unwrap_or(x)
}

#[tokio::test]
async fn wearables_summary_and_series() -> R {
    let a = app().await?;
    import_lucia(&a).await?;
    let (s, body) = get(
        &a,
        "sam",
        "/profiles/lucia/wearables/summary?as_of=2026-11-07&days=30&lang=en",
    )
    .await?;
    assert_eq!(s, StatusCode::OK, "{body}");
    let metrics = body["metrics"].as_array().cloned().unwrap_or_default();
    let names: Vec<&str> = metrics.iter().filter_map(|m| m["metric"].as_str()).collect();
    assert_eq!(names[..4], ["sleep_asleep", "hrv_sdnn", "hr_resting", "steps"]);
    let by = |k: &str| metrics.iter().find(|m| m["metric"] == k).cloned().unwrap_or_default();
    let steps = by("steps");
    assert_eq!(
        (&steps["label"], &steps["source"]),
        (&Value::from("Steps"), &Value::from("Apple Watch"))
    );
    let series = steps["series"].as_array().cloned().unwrap_or_default();
    assert_eq!(series.len(), 30);
    assert_eq!(series[29]["date"], "2026-11-07");
    assert_eq!(steps["latest"], series[29]["value"]);
    let total: f64 = series.iter().map(|p| p["value"].as_f64().unwrap_or_default()).sum();
    assert_eq!(steps["avg_30d"].as_f64(), Some(round1(total / 30.0)));
    assert_eq!(by("hr_resting")["coverage_pct"].as_f64(), Some(100.0));
    let devices: BTreeSet<&str> = body["devices"]
        .as_array()
        .map(|d| d.iter().filter_map(|x| x["device"].as_str()).collect())
        .unwrap_or_default();
    assert_eq!(devices, BTreeSet::from(["Apple Watch", "iPhone"]));
    let (_, es) = get(&a, "sam", "/profiles/lucia/wearables/summary?as_of=2026-11-07").await?;
    assert_eq!(es["metrics"][0]["label"], "Duración del sueño");
    let (_, r) = get(&a, "sam", "/profiles/lucia/wearables/steps?window=P7D&as_of=2026-11-07").await?;
    let pts = r["points"].as_array().cloned().unwrap_or_default();
    let devs: BTreeSet<&str> = pts.iter().filter_map(|p| p["device"].as_str()).collect();
    assert_eq!(pts.len(), 7);
    assert_eq!(devs, BTreeSet::from(["Apple Watch"]));
    assert_eq!(r["priority"].as_array().map(Vec::len), Some(2));
    let (_, hourly) = get(
        &a,
        "sam",
        "/profiles/lucia/wearables/steps?window=P1D&resolution=hour&as_of=2026-11-07",
    )
    .await?;
    let hourly = hourly["points"].as_array().cloned().unwrap_or_default();
    assert_eq!(hourly.len(), 16);
    let t0 = DateTime::parse_from_rfc3339(hourly[0]["t"].as_str().unwrap_or_default())?;
    assert_eq!(
        t0,
        Utc.with_ymd_and_hms(2026, 11, 7, 6, 0, 0).single().unwrap_or_default()
    );
    let (s, _) = get(&a, "sam", "/profiles/lucia/wearables/steps?window=P1Y&resolution=hour").await?;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
    // a viewer without read:wearables
    let (s, _) = get(&a, "alex", "/profiles/sam/wearables/summary").await?;
    assert_eq!(s, StatusCode::FORBIDDEN);
    let n: i64 =
        sqlx::query_scalar("select count(*) from audit_event where profile_id = $1 and tool like 'wearables.%'")
            .bind(a.family.profile("lucia"))
            .fetch_one(&a.rt.pool)
            .await?;
    assert_eq!(n, 4); // the refused hourly request read nothing
    Ok(())
}

/// A source with no samples left (its export retracted) is not a device the person still has data from.
#[tokio::test]
async fn a_source_with_no_samples_is_not_listed() -> R {
    let a = app().await?;
    import_lucia(&a).await?;
    // a source with no samples left (its export retracted) is not listed
    sqlx::query("insert into wearable_source (profile_id, provider, device) values ($1, 'gadgetbridge', 'Band')")
        .bind(a.family.profile("lucia"))
        .execute(&a.rt.pool)
        .await?;
    let (s, again) = get(
        &a,
        "sam",
        "/profiles/lucia/wearables/summary?as_of=2026-11-07&days=30&lang=en",
    )
    .await?;
    assert_eq!(s, StatusCode::OK, "{again}");
    assert_eq!(
        again["devices"].as_array().map(Vec::len),
        Some(2),
        "{}",
        again["devices"]
    );
    Ok(())
}

#[tokio::test]
async fn a_wearables_window_too_large_for_the_calendar_is_capped() -> R {
    let a = app().await?;
    let (s, body) = get(
        &a,
        "sam",
        "/profiles/lucia/wearables/steps?window=P9999Y&as_of=2026-11-07",
    )
    .await?;
    assert_eq!(s, StatusCode::OK, "{body}");
    let (s, body) = get(
        &a,
        "sam",
        "/profiles/lucia/wearables/steps?window=P30D&as_of=0001-01-02",
    )
    .await?;
    assert_eq!(
        (s, &body["detail"]["code"]),
        (StatusCode::UNPROCESSABLE_ENTITY, &Value::from("bad_input")),
        "{body}"
    );
    Ok(())
}

/// One wearable_source of Lucía's (made once) with daily rollups of `metric` on the given days of
/// November 2026.
async fn rollup_days(a: &App, provider: &str, device: &str, metric: &str, days: &[u32], v: f32) -> anyhow::Result<i16> {
    let pid = a.family.profile("lucia");
    let sid: i16 = sqlx::query_scalar(
        "with s as (insert into wearable_source (profile_id, provider, device) values ($1, $2, $3) \
         on conflict do nothing returning id) select id from s union all \
         select id from wearable_source where profile_id = $1 and provider = $2 and device = $3 limit 1",
    )
    .bind(pid)
    .bind(provider)
    .bind(device)
    .fetch_one(&a.rt.pool)
    .await?;
    for d in days {
        sqlx::query(
            "insert into wearable_rollup (profile_id, metric, source_id, bucket, t, n, v_min, v_max, v_avg, v_sum, \
             v_p10, v_p50, v_p90, coverage) values ($1, $2, $3, 'day', $4, 1, $5, $5, $5, $5, $5, $5, $5, 1)",
        )
        .bind(pid)
        .bind(metric)
        .bind(sid)
        .bind(Utc.with_ymd_and_hms(2026, 11, *d, 0, 0, 0).single().unwrap_or_default())
        .bind(v)
        .execute(&a.rt.pool)
        .await?;
    }
    Ok(sid)
}

fn devices_of(body: &Value) -> Vec<String> {
    body["points"]
        .as_array()
        .map(|p| {
            p.iter()
                .filter_map(|x| x["device"].as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

/// Two vendors measure rMSSD on alternate days: the series is one vendor's (the same count of days,
/// so the one with the newest day), and names the other.
#[tokio::test]
async fn an_hrv_series_takes_one_vendor_and_names_the_rest() -> R {
    let a = app().await?;
    let odd = [1, 3, 5, 7, 9];
    let even = [2, 4, 6, 8, 10];
    let oura = rollup_days(&a, "oura", "Oura Ring", "hrv_rmssd", &odd, 48.0).await?;
    rollup_days(&a, "whoop", "Whoop 4.0", "hrv_rmssd", &even, 71.0).await?;
    let uri = "/profiles/lucia/wearables/hrv_rmssd?window=P30D&as_of=2026-11-12";
    let (s, body) = get(&a, "sam", &format!("{uri}&lang=en")).await?;
    assert_eq!(s, StatusCode::OK, "{body}");
    assert_eq!(devices_of(&body), vec!["Whoop 4.0"; 5], "{body}");
    let note = body["limitations"][0].as_str().unwrap_or_default();
    assert!(note.contains("Oura Ring") && note.contains("Whoop 4.0"), "{body}");
    let (_, es) = get(&a, "sam", &format!("{uri}&lang=es")).await?;
    let nota = es["limitations"][0].as_str().unwrap_or_default();
    assert!(nota.contains("VFC") && nota.contains("Oura Ring"), "{es}");
    // the person's own order picks the vendor
    let mut c = a.rt.pool.acquire().await?;
    ownhealth_ingest_structured::wearables::set_priority(&mut c, a.family.profile("lucia"), "hrv_rmssd", &[oura])
        .await?;
    let (_, body) = get(&a, "sam", uri).await?;
    assert_eq!(devices_of(&body), vec!["Oura Ring"; 5], "{body}");
    Ok(())
}

/// Other metrics still fill each day from the best source that has it, with no note.
#[tokio::test]
async fn other_metrics_still_mix_sources_day_by_day() -> R {
    let a = app().await?;
    rollup_days(&a, "oura", "Oura Ring", "hr_resting", &[1, 3], 55.0).await?;
    rollup_days(&a, "whoop", "Whoop 4.0", "hr_resting", &[2, 4], 58.0).await?;
    let (s, body) = get(
        &a,
        "sam",
        "/profiles/lucia/wearables/hr_resting?window=P30D&as_of=2026-11-12",
    )
    .await?;
    assert_eq!(s, StatusCode::OK, "{body}");
    assert_eq!(
        devices_of(&body),
        ["Oura Ring", "Whoop 4.0", "Oura Ring", "Whoop 4.0"],
        "{body}"
    );
    assert!(body.get("limitations").is_none(), "{body}");
    Ok(())
}

/// Two vendors of the same default rank: the one with more points in the window wins, not the one
/// connected first. An explicit order still wins.
#[tokio::test]
async fn an_hrv_tie_goes_to_the_source_with_more_points() -> R {
    let a = app().await?;
    let oura = rollup_days(&a, "oura", "Oura Ring", "hrv_rmssd", &[1], 48.0).await?;
    let rest: Vec<u32> = (2..=30).collect();
    rollup_days(&a, "whoop", "Whoop 4.0", "hrv_rmssd", &rest, 71.0).await?;
    let uri = "/profiles/lucia/wearables/hrv_rmssd?window=P30D&as_of=2026-11-30&lang=en";
    let (s, body) = get(&a, "sam", uri).await?;
    assert_eq!(s, StatusCode::OK, "{body}");
    assert_eq!(devices_of(&body), vec!["Whoop 4.0"; 29], "{body}");
    let mut c = a.rt.pool.acquire().await?;
    ownhealth_ingest_structured::wearables::set_priority(&mut c, a.family.profile("lucia"), "hrv_rmssd", &[oura])
        .await?;
    let (_, body) = get(&a, "sam", uri).await?;
    assert_eq!(devices_of(&body), vec!["Oura Ring"], "{body}");
    Ok(())
}

/// The summary the Wearables page reads carries the same note on the HRV entry, and only there.
#[tokio::test]
async fn the_summary_names_the_hrv_vendor_left_out() -> R {
    let a = app().await?;
    rollup_days(&a, "oura", "Oura Ring", "hrv_rmssd", &[1, 3, 5, 7, 9], 48.0).await?;
    rollup_days(&a, "whoop", "Whoop 4.0", "hrv_rmssd", &[2, 4, 6, 8, 10], 71.0).await?;
    rollup_days(&a, "oura", "Oura Ring", "hr_resting", &[1, 3], 55.0).await?;
    rollup_days(&a, "whoop", "Whoop 4.0", "hr_resting", &[2, 4], 58.0).await?;
    let (s, body) = get(
        &a,
        "sam",
        "/profiles/lucia/wearables/summary?days=30&as_of=2026-11-12&lang=en",
    )
    .await?;
    assert_eq!(s, StatusCode::OK, "{body}");
    let entry = |m: &str| {
        body["metrics"]
            .as_array()
            .and_then(|a| a.iter().find(|x| x["metric"] == m))
            .cloned()
            .unwrap_or_default()
    };
    let note = entry("hrv_rmssd")["limitations"][0]
        .as_str()
        .unwrap_or_default()
        .to_string();
    assert!(note.contains("Oura Ring") && note.contains("Whoop 4.0"), "{body}");
    assert!(entry("hr_resting").get("limitations").is_none(), "{body}");
    Ok(())
}

/// A `lang` the registry does not hold is refused; an empty one counts as absent, as on every REST
/// route.
#[tokio::test]
async fn a_lang_outside_the_registry_is_refused_and_an_empty_one_reads_the_default() -> R {
    let a = app().await?;
    for uri in [
        "/profiles/lucia/wearables/summary?as_of=2026-11-07",
        "/profiles/lucia/wearables/steps?window=P30D&as_of=2026-11-07",
    ] {
        let (s, b) = get(&a, "sam", &format!("{uri}&lang=fr")).await?;
        assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{uri}: {b}");
        assert_eq!(b["detail"][0]["type"], "literal_error", "{b}");
        let (s, b) = get(&a, "sam", &format!("{uri}&lang=")).await?;
        assert_eq!(s, StatusCode::OK, "{uri}: {b}");
    }
    Ok(())
}

/// The provider policy keeps Oura and a native Dexcom sensor from agents, the rule pack and the
/// calculators (data/connectors/providers.yaml), never from the person: the Wearables page's summary
/// and series still read every provider.
#[tokio::test]
async fn the_wearables_page_reads_the_providers_agents_never_see() -> R {
    let a = app().await?;
    let days: Vec<u32> = (1..=10).collect();
    rollup_days(&a, "oura", "Oura Ring", "hr_resting", &days, 48.0).await?;
    rollup_days(&a, "dexcom", "Dexcom G7", "glucose_cgm", &days, 140.0).await?;
    let (s, body) = get(&a, "sam", "/profiles/lucia/wearables/summary?days=30&as_of=2026-11-12").await?;
    assert_eq!(s, StatusCode::OK, "{body}");
    let entry = |m: &str| {
        body["metrics"]
            .as_array()
            .and_then(|a| a.iter().find(|e| e["metric"] == m))
            .cloned()
            .unwrap_or(Value::Null)
    };
    assert_eq!(entry("hr_resting")["source"], "Oura Ring", "{body}");
    assert_eq!(entry("hr_resting")["latest"], 48.0, "{body}");
    assert_eq!(entry("glucose_cgm")["source"], "Dexcom G7", "{body}");
    for (metric, device) in [("hr_resting", "Oura Ring"), ("glucose_cgm", "Dexcom G7")] {
        let (s, one) = get(
            &a,
            "sam",
            &format!("/profiles/lucia/wearables/{metric}?window=P30D&as_of=2026-11-12"),
        )
        .await?;
        assert_eq!(s, StatusCode::OK, "{one}");
        assert_eq!(devices_of(&one), vec![device; 10], "{one}");
    }
    Ok(())
}
