mod common;

use common::*;
use serde_json::{Value, json};

const AGENT: &str = "SyntheticAgent/1";

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

/// A tools/call that sends a User-Agent header, which the test client never does.
async fn call_with_agent(w: &World, token: &str, name: &str, args: Value) -> R {
    let body = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {
        "name": name,
        "arguments": args,
        "_meta": {"io.modelcontextprotocol/protocolVersion": "2026-07-28",
                  "io.modelcontextprotocol/clientCapabilities": {},
                  "io.modelcontextprotocol/clientInfo": {"name": "ownhealth-tests", "version": "1"}},
    }});
    let auth = format!("Bearer {token}");
    let headers = [
        ("mcp-protocol-version", "2026-07-28"),
        ("mcp-method", "tools/call"),
        ("mcp-name", name),
        ("authorization", auth.as_str()),
        ("user-agent", AGENT),
    ];
    let r = send(&w.app(), post(&body, &headers)?).await?;
    anyhow::ensure!(r.status.as_u16() == 200, "{}", String::from_utf8_lossy(&r.body));
    Ok(())
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
    let token = w.tok().await?;
    let c = w.connect(&token).await?;
    let args = json!({"profile": "alex"});

    // Control: with consent off the row keeps the user agent, so the check below can fail.
    call_with_agent(&w, &token, "get_genome_findings", args.clone()).await?;
    set_consent(&w, true).await?;

    call_with_agent(&w, &token, "get_genome_findings", args.clone()).await?;
    // The shipped drug check leaves several items unassessed for a drug with no genotyped gene.
    let drug = json!({"profile": "alex", "drug": "warfarin"});
    let env = w
        .connect_real(&token)
        .await?
        .call("check_drug", drug.clone())
        .await?
        .ok();
    let n = env["not_assessed"].as_array().map_or(0, Vec::len);
    assert!(n >= 2, "a fixture with several unassessed items: {n}");

    let invalid =
        json!({"profile": "SyntheticProfileName", "SyntheticArgKey": "Preguntañ", "query": "SyntheticQuestion"});
    c.call("SyntheticToolName", invalid.clone()).await?.err();

    set_consent(&w, false).await?;
    c.call("get_genome_findings", args.clone()).await?.ok();

    let log = rows(&w, &token).await?;
    assert_eq!(log.len(), 5);
    assert_eq!(log[0]["user_agent"], AGENT);
    assert_eq!(log[0]["not_assessed_n"], Value::Null);
    assert_eq!(log[1]["arg_bytes"], args.to_string().len());
    assert_eq!(log[2]["not_assessed_n"], n);
    assert_eq!(log[2]["arg_bytes"], drug.to_string().len());
    assert_eq!(log[3]["not_assessed_n"], 0);
    assert_eq!(log[3]["arg_bytes"], invalid.to_string().len());
    assert_eq!(log[3]["tool"], "unknown");
    assert_eq!(log[4]["not_assessed_n"], Value::Null);
    assert_eq!(log[4]["arg_bytes"], Value::Null);
    for row in &log[1..4] {
        assert_eq!(row["user_agent"], Value::Null);
        assert!(!row.to_string().contains("Synthetic"), "{row}");
        assert!(!row.to_string().contains("Pregunta"), "{row}");
    }
    Ok(())
}

#[tokio::test]
async fn opted_in_rows_keep_the_audit_detail_of_the_schema_arguments() -> R {
    let w = world().await?;
    let token = w.tok().await?;
    let c = w.connect(&token).await?;
    c.call_tool("get_genome_findings", json!({"profile": "carmen"}))
        .await?
        .err();
    set_consent(&w, true).await?;
    let denied = c
        .call_tool("get_genome_findings", json!({"profile": "carmen"}))
        .await?
        .err();
    assert_eq!(denied["code"], "profile_not_granted");
    c.call_tool(
        "get_genome_findings",
        json!({"profile": "alex", "SyntheticArgKey": "Preguntañ"}),
    )
    .await?;

    let log = rows(&w, &token).await?;
    assert_eq!(log.len(), 3);
    // The same detail with consent off and on: the audit log still says which profile was refused.
    assert_eq!(log[0]["args_summary"], "profile=carmen");
    assert_eq!(log[1]["args_summary"], "profile=carmen");
    assert_eq!(log[1]["result_kind"], "denied");
    assert_eq!(log[0]["client"], log[1]["client"]);
    assert_eq!(log[1]["client"], "Claude Code · test");
    // A key the tool's schema does not name is caller text, so an opted-in row leaves it out.
    assert_eq!(log[2]["args_summary"], "profile=alex");
    assert!(!log[2].to_string().contains("Synthetic"), "{}", log[2]);
    assert!(!log[2].to_string().contains("Pregunta"), "{}", log[2]);
    Ok(())
}
