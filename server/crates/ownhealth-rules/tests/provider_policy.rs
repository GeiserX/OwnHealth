//! The provider policy at the rule engine (`engine::wearables`): the pack markers, the state and its
//! hash, the dashboard and the calculators never read a provider that `data/connectors/providers.yaml`
//! withholds (Oura, native Dexcom, Withings ...), so the UI and MCP get one state_hash. A served
//! provider is read as before.

mod common;

use common::{as_of, seeded};
use ownhealth_core::access::all_scopes;
use ownhealth_rules::calc::cardio::{self, Sbp};
use ownhealth_rules::{build_state, dashboard, state_hash};
use serde_json::Value;
use sqlx::PgConnection;
use uuid::Uuid;

type R = anyhow::Result<()>;

/// The wearable_source of one device of the profile, made once (the seed writes its own ids, so a new
/// one takes the next free id), with daily `metric` rollups `ago` days before the fixed as_of.
async fn days(
    c: &mut PgConnection,
    pid: Uuid,
    provider: &str,
    device: &str,
    metric: &str,
    ago: &[i32],
    v: f32,
) -> anyhow::Result<i16> {
    let sid: i16 = sqlx::query_scalar(
        "with s as (insert into wearable_source (id, profile_id, provider, device) overriding system value \
         values ((select coalesce(max(id), 0) + 1 from wearable_source), $1, $2, $3) \
         on conflict do nothing returning id) select id from s union all \
         select id from wearable_source where profile_id = $1 and provider = $2 and device = $3 limit 1",
    )
    .bind(pid)
    .bind(provider)
    .bind(device)
    .fetch_one(&mut *c)
    .await?;
    for d in ago {
        sqlx::query(
            "insert into wearable_rollup (profile_id, metric, source_id, bucket, t, n, v_avg, v_sum, coverage) \
             values ($1, $2, $3, 'day', $4::date - $5::int, 1, $6, $6, 1)",
        )
        .bind(pid)
        .bind(metric)
        .bind(sid)
        .bind(as_of())
        .bind(d)
        .bind(v)
        .execute(&mut *c)
        .await?;
    }
    Ok(sid)
}

/// The person's own source order for `metric`: this source first.
async fn first(c: &mut PgConnection, pid: Uuid, metric: &str, source: i16) -> R {
    sqlx::query(
        "insert into source_priority (profile_id, metric, source_ids) values ($1, $2, $3) \
         on conflict (profile_id, metric) do update set source_ids = excluded.source_ids",
    )
    .bind(pid)
    .bind(metric)
    .bind(vec![source])
    .execute(&mut *c)
    .await?;
    Ok(())
}

/// An Oura ring with every day of the last 90 for each pack marker, placed first in the person's own
/// source order, and a native Dexcom sensor.
async fn withheld(c: &mut PgConnection, pid: Uuid) -> R {
    let all: Vec<i32> = (0..90).collect();
    for (metric, v) in [
        ("hr_resting", 41.0),
        ("hrv_rmssd", 95.0),
        ("sleep_asleep", 300.0),
        ("steps", 25000.0),
        ("vo2max", 61.0),
    ] {
        let oura = days(c, pid, "oura", "Oura Ring", metric, &all, v).await?;
        first(c, pid, metric, oura).await?;
    }
    days(c, pid, "dexcom", "Dexcom G7", "glucose_cgm", &all, 180.0).await?;
    Ok(())
}

/// The state's hash, the state and the dashboard (without its generated_at) of one profile at the fixed
/// as_of.
async fn state(tx: &mut PgConnection, pid: Uuid) -> anyhow::Result<(String, Value, Value)> {
    let body = build_state(&mut *tx, pid, as_of(), None).await?;
    let mut board = dashboard(&mut *tx, pid, as_of(), "en", &all_scopes()).await?;
    // the clock the dashboard was made at is not data
    if let Some(b) = board.as_object_mut() {
        b.shift_remove("generated_at");
    }
    Ok((state_hash(&body)?, body, board))
}

#[tokio::test]
async fn the_state_and_the_dashboard_never_read_a_withheld_vendor() -> R {
    let s = seeded().await?;
    let mut tx = s.pool().begin().await?;
    let alex = s.pid("alex");
    let before = state(&mut tx, alex).await?;
    withheld(&mut tx, alex).await?;
    let after = state(&mut tx, alex).await?;
    assert_eq!(before.0, after.0, "state_hash moved with a withheld vendor");
    assert_eq!(before.1, after.1);
    assert_eq!(before.2, after.2);
    // a served provider in the same place is read: the gate is the policy, not the data
    let phone = days(
        &mut tx,
        alex,
        "health_auto_export",
        "Phone push",
        "steps",
        &(0..90).collect::<Vec<_>>(),
        25000.0,
    )
    .await?;
    first(&mut tx, alex, "steps", phone).await?;
    let served = state(&mut tx, alex).await?;
    // (the seed's watch already makes the state current, so the served steps show on the dashboard)
    assert_ne!(before.2, served.2, "a served provider changes the dashboard");
    Ok(())
}

#[tokio::test]
async fn the_calculators_never_read_a_withheld_cuff() -> R {
    let s = seeded().await?;
    let mut tx = s.pool().begin().await?;
    let alex = s.pid("alex");
    let cv = cardio::data().expect("the shipped cardiovascular files load");
    let before = cardio::facts(&mut tx, cv, alex, Some("ES"), as_of(), &all_scopes()).await?;
    assert!(!matches!(before.sbp, Some(Sbp::Wearable { .. })), "{:?}", before.sbp);
    days(
        &mut tx,
        alex,
        "withings",
        "BPM Connect",
        "bp_systolic",
        &[0, 1, 2],
        160.0,
    )
    .await?;
    days(&mut tx, alex, "oura", "Oura Ring", "bp_systolic", &[0, 1, 2], 170.0).await?;
    let after = cardio::facts(&mut tx, cv, alex, Some("ES"), as_of(), &all_scopes()).await?;
    assert!(!matches!(after.sbp, Some(Sbp::Wearable { .. })), "{:?}", after.sbp);
    // the same readings from a served cuff are read
    let cuff = days(
        &mut tx,
        alex,
        "health_auto_export",
        "cuff",
        "bp_systolic",
        &[0, 1, 2],
        122.0,
    )
    .await?;
    let served = cardio::facts(&mut tx, cv, alex, Some("ES"), as_of(), &all_scopes()).await?;
    match served.sbp {
        Some(Sbp::Wearable { source, .. }) => assert_eq!(source, cuff),
        other => panic!("want the served cuff's mean, got {other:?}"),
    }
    Ok(())
}
