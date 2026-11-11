//! The calculators resource (`ownhealth://profile/{id}/calculators`): listed and read with
//! read:labs, the same document the REST route serves, refused like any other resource without it.

mod common;

use common::*;
use serde_json::Value;

const URI: &str = "ownhealth://profile/alex/calculators";

#[tokio::test]
async fn an_agent_with_read_labs_reads_the_calculators() -> R {
    let w = world().await?;
    let c = w
        .connect(&w.tok_scopes(scopes(&["read:summary", "read:labs"])).await?)
        .await?;
    assert!(c.list_resources().await?.iter().any(|u| u == URI));
    let doc: Value = serde_json::from_str(&c.read_text(URI).await?)?;
    assert_eq!(doc["schema"], "ownhealth.calculators/v1");
    assert_eq!(doc["profile"], "alex");
    assert_eq!(doc["lang"], "en");
    assert!(
        doc["calculators_hash"]
            .as_str()
            .is_some_and(|h| h.starts_with("sha256:"))
    );
    let ids: Vec<&str> = doc["items"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|i| i["id"].as_str())
        .collect();
    for id in ["ckd_epi_2021_cr", "ckd_epi_2021_cr_cys", "fib4", "homa_ir"] {
        assert!(ids.contains(&id), "{id} in {ids:?}");
    }
    for it in doc["items"].as_array().into_iter().flatten() {
        assert!(it["cite"].as_str().is_some_and(|u| u.starts_with("https://")), "{it}");
        assert!(it["name"].as_str().is_some_and(|n| !n.is_empty()));
    }
    Ok(())
}

#[tokio::test]
async fn a_token_without_read_labs_neither_sees_nor_reads_it() -> R {
    let w = world().await?;
    let c = w.connect(&w.tok_scopes(without(&["read:labs"])).await?).await?;
    assert!(!c.list_resources().await?.iter().any(|u| u.ends_with("/calculators")));
    let refused = c.read_resource(URI).await.expect_err("needs read:labs");
    let missing = c
        .read_resource("ownhealth://source/src_doesnotexist")
        .await
        .expect_err("missing");
    // one answer for every refusal, as for the other resources
    assert_eq!(refused.code, missing.code);
    assert!(refused.message.contains("scope_missing"), "{}", refused.message);
    Ok(())
}
