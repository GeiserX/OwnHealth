mod common;

use common::*;
use serde_json::{Value, json};

async fn set_consent(w: &World, enabled: bool) -> R {
    sqlx::query(
        "insert into household_setting (household_id, key, value) values ($1, 'mcp_session_report', $2) \
         on conflict (household_id, key) do update set value = excluded.value",
    )
    .bind(w.family.household_id)
    .bind(json!(enabled))
    .execute(&w.db.pool)
    .await?;
    Ok(())
}

async fn rows(w: &World, token: &str) -> anyhow::Result<Vec<Value>> {
    Ok(sqlx::query_scalar(
        "select to_jsonb(e) from audit_event e where token_id = $1 and tool is not null order by e.id",
    )
    .bind(w.token_id(token).await?)
    .fetch_all(&w.db.pool)
    .await?)
}

#[tokio::test]
async fn omitted_and_disabled_consent_leave_report_columns_null() -> R {
    let w = world().await?;
    let token = w.tok().await?;
    let c = w.connect(&token).await?;
    c.call("get_genome_findings", json!({"profile": "alex"})).await?.ok();
    set_consent(&w, false).await?;
    c.call("get_genome_findings", json!({"profile": "alex"})).await?.ok();

    let log = rows(&w, &token).await?;
    assert_eq!(log.len(), 2);
    for row in log {
        assert_eq!(row["not_assessed_n"], Value::Null);
        assert_eq!(row["arg_bytes"], Value::Null);
    }
    Ok(())
}

#[tokio::test]
async fn consent_records_counts_and_bytes_without_free_text_even_for_errors() -> R {
    let w = world().await?;
    let token = w
        .token(Tok {
            label: "SyntheticTokenName",
            ..Tok::default()
        })
        .await?;
    let c = w.connect(&token).await?;
    set_consent(&w, true).await?;

    let args = json!({"profile": "alex"});
    let env = c.call("get_genome_findings", args.clone()).await?.ok();
    let n = env["not_assessed"].as_array().map_or(0, Vec::len);
    assert!(n > 0);

    let invalid =
        json!({"profile": "SyntheticProfileName", "SyntheticArgKey": "Preguntañ", "query": "SyntheticQuestion"});
    c.call("SyntheticToolName", invalid.clone()).await?.err();

    set_consent(&w, false).await?;
    c.call("get_genome_findings", args.clone()).await?.ok();

    let log = rows(&w, &token).await?;
    assert_eq!(log.len(), 3);
    assert_eq!(log[0]["not_assessed_n"], n);
    assert_eq!(log[0]["arg_bytes"], args.to_string().len());
    assert_eq!(log[1]["not_assessed_n"], 0);
    assert_eq!(log[1]["arg_bytes"], invalid.to_string().len());
    assert_eq!(log[1]["tool"], "unknown");
    assert_eq!(log[2]["not_assessed_n"], Value::Null);
    assert_eq!(log[2]["arg_bytes"], Value::Null);
    for row in &log[..2] {
        assert_eq!(row["args_summary"], Value::Null);
        assert_eq!(row["client"], Value::Null);
        assert_eq!(row["user_agent"], Value::Null);
        assert!(!row.to_string().contains("Synthetic"));
        assert!(!row.to_string().contains("Pregunta"));
    }
    Ok(())
}
