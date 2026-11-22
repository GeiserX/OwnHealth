//! The audit hash chain (migrations/1250_audit_chain.sql) on the MCP path: every get_health_status
//! call links its audit row at commit, and the status still answers in under 300 ms at p95 while
//! another writer keeps adding audit rows. Synthetic data only.

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use common::*;
use ownhealth_core::audit::{AuditEvent, verify_chain, write_audit};
use serde_json::json;

const RUNS: usize = 20;
const BUDGET_MS: f64 = 300.0;

#[tokio::test]
async fn get_health_status_is_under_300ms_p95_with_the_chain() -> R {
    let w = world().await?;
    let tok = w.tok().await?;
    let c = Client::connect(w.real_app(), Some(&tok), "auto").await?;
    // a second writer contends for the chain head the whole time
    let stop = Arc::new(AtomicBool::new(false));
    let writer = {
        let (pool, stop) = (w.db.pool.clone(), stop.clone());
        tokio::spawn(async move {
            let mut n = 0_u32;
            while !stop.load(Ordering::Relaxed) {
                let mut tx = pool.begin().await?;
                write_audit(&mut tx, &AuditEvent::new("system", "job", "read")).await?;
                tx.commit().await?;
                n += 1;
            }
            anyhow::Ok(n)
        })
    };
    let mut times = Vec::with_capacity(RUNS);
    for _ in 0..RUNS {
        let t = Instant::now();
        let env = c.call_tool("get_health_status", json!({"profile": "alex"})).await?.ok();
        times.push(t.elapsed().as_secs_f64() * 1000.0);
        assert_envelope(&env);
    }
    stop.store(true, Ordering::Relaxed);
    let background = writer.await??;
    times.sort_by(f64::total_cmp);
    let p95 = times[RUNS * 95 / 100 - 1];
    eprintln!(
        "get_health_status p95 {p95:.1} ms, median {:.1} ms over {RUNS} runs, {background} other audit rows",
        times[RUNS / 2]
    );
    assert!(p95 < BUDGET_MS, "p95 {p95:.1} ms");
    let linked: i64 = sqlx::query_scalar(
        "select count(*) from audit_chain c join audit_event e on e.id = c.event_id where e.tool = 'get_health_status'",
    )
    .fetch_one(&w.db.pool)
    .await?;
    assert_eq!(usize::try_from(linked)?, RUNS);
    let check = verify_chain(&mut *w.db.pool.acquire().await?).await?;
    assert!(check.ok(), "{check:?}");
    Ok(())
}
