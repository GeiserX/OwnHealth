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
               "prescriber": "Dr. Synthetic", "codes": codes}),
        alex,
    )
    .await?;
    drop(conn);
    let c = w.connect(&w.tok().await?).await?;
    let changed = c
        .call_tool(
            "propose",
            json!({"profile": "alex", "kind": "medication",
                   "payload": {"action": "change", "name": "Omeprazole", "schedule": "before breakfast", "date": "2026-11-01"}}),
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
    Ok(())
}
