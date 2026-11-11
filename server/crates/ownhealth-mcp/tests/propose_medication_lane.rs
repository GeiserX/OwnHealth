//! The medication lane through `propose`: the schedule an agent proposes reaches the record, a
//! supplement is a supplement, and a change keeps what the old version had (codes, prescriber).

mod common;

use common::*;
use ownhealth_core::review;
use serde_json::{Value, json};
use uuid::Uuid;

async fn accept(w: &World, answer: &Value) -> anyhow::Result<Vec<Uuid>> {
    let handle = answer["data"]["proposal_id"].as_str().unwrap_or_default();
    let id: Uuid = sqlx::query_scalar("select id from review_item where replace(id::text, '-', '') like $1")
        .bind(format!("{}%", &handle[5..]))
        .fetch_one(&w.db.pool)
        .await?;
    let mut conn = w.conn().await?;
    Ok(review::accept(&mut conn, id, w.user("alex"), None).await?)
}

async fn row(w: &World, id: Uuid) -> anyhow::Result<Value> {
    Ok(
        sqlx::query_scalar("select to_jsonb(m) from medication_statement m where id = $1")
            .bind(id)
            .fetch_one(&w.db.pool)
            .await?,
    )
}

#[tokio::test]
async fn start_with_schedule_keeps_the_schedule() -> R {
    let w = world().await?;
    let c = w.connect(&w.tok().await?).await?;
    let p = |payload: Value| json!({"profile": "alex", "kind": "medication", "payload": payload});
    let started = c
        .call_tool(
            "propose",
            p(
                json!({"action": "start", "name": "Metformin 850 mg", "dose": "1 tablet",
                     "schedule": "twice daily", "prescriber": "Dr. Synthetic", "date": "2026-11-01"}),
            ),
        )
        .await?
        .ok();
    let ids = accept(&w, &started).await?;
    let m = row(&w, ids[0]).await?;
    assert_eq!(
        (&m["dose_text"], &m["schedule_text"], &m["prescriber"], &m["kind"]),
        (
            &json!("1 tablet"),
            &json!("twice daily"),
            &json!("Dr. Synthetic"),
            &json!("medication")
        )
    );

    let supplement = c
        .call_tool(
            "propose",
            p(
                json!({"action": "start", "kind": "supplement", "name": "Vitamin D3 1000 IU",
                     "schedule": "once a day", "date": "2026-11-01"}),
            ),
        )
        .await?
        .ok();
    let m = row(&w, accept(&w, &supplement).await?[0]).await?;
    assert_eq!(
        (&m["kind"], &m["schedule_text"]),
        (&json!("supplement"), &json!("once a day"))
    );

    let e = c
        .call_tool(
            "propose",
            p(json!({"action": "start", "kind": "vitamin", "name": "X", "date": "2026-11-01"})),
        )
        .await?
        .err();
    assert_eq!(e["code"], "bad_input");
    Ok(())
}

#[tokio::test]
async fn change_sets_the_schedule_and_keeps_the_rest() -> R {
    let w = world().await?;
    let alex = w.pid("alex");
    let mut conn = w.conn().await?;
    let codes = json!([{"system": "http://www.whocc.no/atc", "code": "A02BC01"}]);
    add_record_json(
        &mut conn,
        "medication",
        json!({"profile_id": alex, "name": "Omeprazole", "strength": "20 mg", "started": "2026-04-01",
               "prescriber": "Dr. Synthetic", "codes": codes, "sensitivity": ["sexual_health"],
               "effective_at": "2026-04-01T09:00:00+00:00"}),
        alex,
    )
    .await?;
    let old_id: Uuid =
        sqlx::query_scalar("select id from medication_statement where profile_id = $1 and name = 'Omeprazole'")
            .bind(alex)
            .fetch_one(&mut *conn)
            .await?;
    drop(conn);
    let c = w.connect(&w.tok().await?).await?;
    let changed = c
        .call_tool(
            "propose",
            json!({"profile": "alex", "kind": "medication",
                   "payload": {"action": "change", "name": "Omeprazole", "schedule": "before breakfast", "date": "2026-11-01",
                               "reason": "moved to the morning"}}),
        )
        .await?
        .ok();
    assert_eq!(changed["data"]["checks"][0]["ok"], true, "{changed}");
    let m = row(&w, accept(&w, &changed).await?[0]).await?;
    assert_eq!(
        (&m["schedule_text"], &m["prescriber"], &m["codes"], &m["strength"]),
        (
            &json!("before breakfast"),
            &json!("Dr. Synthetic"),
            &codes,
            &json!("20 mg")
        )
    );
    assert_eq!(
        m["sensitivity"],
        json!(["sexual_health"]),
        "a change keeps the sensitivity flag"
    );
    assert!(!m["effective_at"].is_null(), "a change keeps effective_at");
    let old = row(&w, old_id).await?;
    assert_eq!(old["status_reason"], "agent proposal change: moved to the morning");

    let stopped = c
        .call_tool(
            "propose",
            json!({"profile": "alex", "kind": "medication",
                   "payload": {"action": "stop", "name": "Omeprazole", "date": "2026-11-09",
                               "reason": "symptoms resolved"}}),
        )
        .await?
        .ok();
    let after = accept(&w, &stopped).await?[0];
    let m = row(&w, after).await?;
    assert_eq!(
        (&m["med_status"], &m["sensitivity"]),
        (&json!("stopped"), &json!(["sexual_health"]))
    );
    let prev: Value = sqlx::query_scalar("select to_jsonb(m) from medication_statement m where id = (select supersedes from medication_statement where id = $1)")
        .bind(after)
        .fetch_one(&w.db.pool)
        .await?;
    assert_eq!(prev["status_reason"], "agent proposal stop: symptoms resolved");
    Ok(())
}
