//! The Due screen: the ownhealth.prevention/v1 document of one profile in one language.
//!
//! ```text
//! GET /api/profiles/{p}/due?as_of=&lang=    read:summary
//! ```
//!
//! Every item is a record, so a caller without read:records gets an empty list, and an item a lab
//! value shows also needs read:labs. What the scopes leave out is counted in `withheld`. The
//! `prevention_hash` covers the whole document and is the same for every caller and language.

use std::collections::HashMap;

use axum::Router;
use axum::extract::{Path, Query, State};
use axum::routing::get;
use ownhealth_core::Runtime;
use ownhealth_core::access::{Principal, today_utc};
use ownhealth_core::audit::write_audit;
use serde_json::Value;

use super::auth::{Check, R, ok};
use super::status::{event, parse_date};
use crate::extract::Caller;

pub async fn due(
    rt: &Runtime,
    principal: &Principal,
    p: &str,
    as_of: Option<chrono::NaiveDate>,
    lang: Option<&str>,
) -> R<Value> {
    let mut tx = rt.pool.begin().await?;
    let perm = Caller(principal.clone())
        .require(&mut tx, p, Some("read:summary"), None)
        .await?;
    let doc =
        ownhealth_rules::prevention::build_document(&mut tx, perm.profile.id, as_of.unwrap_or_else(today_utc)).await?;
    let mut e = event(principal, perm.profile.id, "GET /api/profiles/{p}/due");
    e.resource_type = Some("prevention".into());
    e.resource_ids = Some(vec![doc["prevention_hash"].as_str().unwrap_or_default().to_string()]);
    write_audit(&mut tx, &e).await?;
    tx.commit().await?;
    let pack = ownhealth_rules::load_pack().map_err(ownhealth_core::Error::from)?;
    Ok(ownhealth_rules::prevention::render(
        &doc,
        &pack,
        lang.unwrap_or(&principal.lang),
        &perm.scopes,
    )?)
}

async fn get_due(
    State(rt): State<Runtime>,
    Caller(c): Caller,
    Path(p): Path<String>,
    Query(q): Query<HashMap<String, String>>,
) -> R {
    let mut check = Check::query();
    let as_of = parse_date(&q, &mut check);
    check.done()?;
    let lang = q.get("lang").filter(|v| !v.is_empty()).map(String::as_str);
    ok(due(&rt, &c, &p, as_of, lang).await?)
}

pub fn router() -> Router<Runtime> {
    Router::new().route("/profiles/{p}/due", get(get_due))
}
