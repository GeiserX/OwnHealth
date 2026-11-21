//! The printed doctor summary, from the UI only (never MCP): two A4 pages as a PDF.
//!
//! ```text
//! POST /api/profiles/{p}/summary.pdf  {lang, specialty, include_sensitive?, as_of? (not after today)}
//! ```
//!
//! Needs step-up and `read:summary` on the profile, so a caregiver can print it; what each part
//! shows follows the caller's scopes (a part left out is named on the sheet). Sensitive genome
//! findings print only when ticked and the caller's UI gate is open, never for a minor. One audit
//! row (`via = export`) names the state hash and every src id printed.

use axum::Router;
use axum::extract::{Path, State};
use axum::http::{HeaderValue, header};
use axum::response::IntoResponse;
use axum::routing::post;
use chrono::{DateTime, NaiveDate, Utc};
use ownhealth_core::Runtime;
use ownhealth_core::access::{majority, today_utc};
use ownhealth_core::audit::write_audit_at;
use ownhealth_rules::summary::{self, LANGS, Options, SPECIALTIES};
use serde::Deserialize;
use serde_json::json;

use super::auth::{Body, Check, Meta, R, StepUpCaller};
use super::status::event;
use crate::extract::Caller;

#[derive(Debug, Deserialize)]
struct SummaryIn {
    lang: Option<String>,
    #[serde(default = "gp")]
    specialty: String,
    #[serde(default)]
    include_sensitive: bool,
    as_of: Option<NaiveDate>,
}

fn gp() -> String {
    "gp".into()
}

async fn summary_pdf(
    State(rt): State<Runtime>,
    StepUpCaller(p): StepUpCaller,
    meta: Meta,
    Path(profile): Path<String>,
    Body(body): Body<SummaryIn>,
) -> R {
    let lang = body.lang.clone().unwrap_or_else(|| p.lang.clone());
    Check::body()
        .literal("lang", Some(&lang), &LANGS)
        .literal("specialty", Some(&body.specialty), &SPECIALTIES)
        .done()?;
    let today = today_utc();
    let as_of = body.as_of.unwrap_or(today);
    if as_of > today {
        // a date on which a minor is an adult would lift the minor rule
        Check::body()
            .push(
                "value_error",
                "as_of",
                "Value error, as_of cannot be after today",
                json!(as_of),
            )
            .done()?;
    }
    // every read of the sheet from one snapshot, which ends by storing the state it read
    let mut tx = summary::begin_read(&rt.pool).await?;
    // the start of the read (now() is the transaction's start): the time the audit row carries
    let read_at: DateTime<Utc> = sqlx::query_scalar("select now()").fetch_one(&mut *tx).await?;
    let perm = Caller(p.clone())
        .require(&mut tx, &profile, Some("read:summary"), None)
        .await?;
    // the minor rule is judged on today's date, as the genome page does
    let gate = ownhealth_genome::gate_for(&perm, "ui", today, majority(&mut tx, perm.profile.household_id).await?)?;
    let opts = Options {
        lang,
        specialty: body.specialty.clone(),
        sensitive_open: body.include_sensitive && gate.sensitive && !gate.minor,
        as_of,
    };
    let (model, state) = summary::read_for(&mut tx, Some(&p), perm.profile.id, &perm.scopes, &opts).await?;
    ownhealth_rules::store_read_snapshot(&mut tx, perm.profile.id, &state).await?;
    tx.commit().await?;
    let mut sheet = ownhealth_ops::summary_pdf::render(&model);
    // the ids the paper shows, not those of rows cut to fit two pages
    let srcs = std::mem::take(&mut sheet.printed);
    let mut e = event(&p, perm.profile.id, "POST /api/profiles/{p}/summary.pdf");
    e.via = "export".into();
    e.action = "export".into();
    e.resource_type = Some("summary".into());
    e.resource_ids = Some(vec![model["state_hash"].as_str().unwrap_or_default().to_string()]);
    e.rows_out = Some(i32::try_from(srcs.len()).unwrap_or(i32::MAX));
    e.src_ids = Some(srcs);
    e.bytes_out = Some(i32::try_from(sheet.pdf.len()).unwrap_or(i32::MAX));
    e.args_summary = Some(format!(
        "specialty={};lang={};sensitive={};pages={};cut={}",
        opts.specialty,
        opts.lang,
        u8::from(opts.sensitive_open),
        sheet.pages,
        sheet.cut
    ));
    e.withheld = Some(json!({
        "by_scope": model["withheld"]["by_scope"],
        "sensitive": model["withheld"]["sensitive"],
        "minor_rule": model["withheld"]["minor_rule"],
    }));
    e.ip = meta.ip.clone();
    // the audit row commits before a byte goes out, in a READ COMMITTED transaction of its own (the
    // audit chain links a row only from one), stamped with the start of the read
    let mut tx = rt.pool.begin().await?;
    write_audit_at(&mut tx, &e, Some(read_at)).await?;
    tx.commit().await?;
    let filename = format!(
        "ownhealth-summary-{}-{}-{}-{}.pdf",
        perm.profile.slug,
        opts.specialty,
        opts.lang,
        as_of.format("%Y-%m-%d")
    );
    let mut res = sheet.pdf.into_response();
    let h = res.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/pdf"));
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    if let Ok(v) = HeaderValue::from_str(&format!("inline; filename=\"{filename}\"")) {
        h.insert(header::CONTENT_DISPOSITION, v);
    }
    Ok(res)
}

pub fn router() -> Router<Runtime> {
    Router::new().route("/profiles/{p}/summary.pdf", post(summary_pdf))
}
