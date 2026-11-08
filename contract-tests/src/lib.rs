//! Lane J: black-box contract tests. They talk only HTTP to a running backend at
//! `OWNHEALTH_CONTRACT_BASE_URL`, started by `run.sh python|rust` on a fresh database with
//! OWNHEALTH_DEMO=1 and OWNHEALTH_DEV_AUTH=1 (REST acts as Alex Rivera, the first owner).
//!
//! Every test is `#[ignore]` so a plain `cargo test` of the workspace stays offline; `run.sh`
//! runs them with `--include-ignored`, and fails when the base URL is missing.

use anyhow::Context;
use serde_json::Value;

pub const BASE_URL_ENV: &str = "OWNHEALTH_CONTRACT_BASE_URL";

/// The backend under test, e.g. `http://127.0.0.1:18080`.
pub fn base_url() -> anyhow::Result<String> {
    let url = std::env::var(BASE_URL_ENV)
        .with_context(|| format!("{BASE_URL_ENV} is not set: run contract-tests/run.sh"))?;
    Ok(url.trim_end_matches('/').to_string())
}

/// Which backend `run.sh` started: `python` or `rust` (for the few documented differences).
pub fn backend() -> String {
    std::env::var("OWNHEALTH_CONTRACT_BACKEND").unwrap_or_default()
}

pub struct Client {
    pub base: String,
    pub http: reqwest::Client,
}

impl Client {
    pub fn new() -> anyhow::Result<Client> {
        Ok(Client {
            base: base_url()?,
            http: reqwest::Client::builder().build()?,
        })
    }

    /// GET a path and return (status, JSON body or Null).
    pub async fn get_json(&self, path: &str) -> anyhow::Result<(u16, Value)> {
        let res = self.http.get(format!("{}{path}", self.base)).send().await?;
        let status = res.status().as_u16();
        let body = res.bytes().await?;
        Ok((status, serde_json::from_slice(&body).unwrap_or(Value::Null)))
    }
}
