//! Lane G: `api/export.py`. Full export, from the UI only (never MCP): FHIR R4 NDJSON plus the
//! original files in one zip.
//!
//! ```text
//! POST /api/profiles/{p}/export  {fmt: "zip"}               the full export (manage right)
//! POST /api/profiles/{p}/export  {fmt: "ips", lang?: es|en} the International Patient Summary
//! ```
//!
//! Both need step-up and write one audit row. The zip needs the right to manage the profile (owner,
//! or guardian of an unclaimed profile); it is built in a temporary file under the data directory and
//! streamed, and the file is unlinked once it is open for sending, so no copy stays on the server.
//! The IPS (`ownhealth_ops::ips`, a FHIR document Bundle as JSON) is the printed doctor summary's
//! twin, so it needs what printing does: `read:summary`, each part following the caller's scopes.

use std::fs::{File, OpenOptions};
use std::path::PathBuf;

use axum::Router;
use axum::body::Body as HttpBody;
use axum::extract::{Path, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::IntoResponse;
use axum::routing::post;
use ownhealth_auth::household::require_manage;
use ownhealth_core::Runtime;
use ownhealth_core::access::today_utc;
use ownhealth_core::audit::{AuditEvent, write_audit_at};
use ownhealth_ops::export::{blocking, export_profile};
use ownhealth_rules::summary::LANGS;
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use super::auth::{Body, Check, Fail, Meta, R, StepUpCaller};
use crate::error::ApiError;
use crate::extract::Caller;

#[derive(Debug, Deserialize)]
struct ExportIn {
    #[serde(default = "zip")]
    fmt: String,
    /// The IPS narratives' language; the caller's own by default.
    lang: Option<String>,
}

fn zip() -> String {
    "zip".into()
}

/// Removes the temporary file when dropped, whatever happened.
struct TmpFile(PathBuf);

impl Drop for TmpFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn internal(e: std::io::Error) -> Fail {
    tracing::error!(error = %e, "export file");
    Fail::Api(ApiError::new(
        StatusCode::INTERNAL_SERVER_ERROR,
        "internal",
        "Internal error.",
    ))
}

async fn export(
    State(rt): State<Runtime>,
    StepUpCaller(p): StepUpCaller,
    meta: Meta,
    Path(profile): Path<String>,
    Body(body): Body<ExportIn>,
) -> R {
    Check::body().literal("fmt", Some(&body.fmt), &["zip", "ips"]).done()?;
    if body.fmt == "ips" {
        return ips(rt, p, meta, profile, body.lang).await;
    }
    let mut tx = rt.pool.begin().await?;
    // one snapshot for every query of the export (it must be set before the first query)
    sqlx::query("set transaction isolation level repeatable read")
        .execute(&mut *tx)
        .await?;
    // the start of the read (now() is the transaction's start): the time the audit row carries
    let read_at: chrono::DateTime<chrono::Utc> = sqlx::query_scalar("select now()").fetch_one(&mut *tx).await?;
    let perm = require_manage(Caller(p.clone()).require(&mut tx, &profile, None, None).await?)?;
    let tmp_dir = rt.settings.data_dir.join("tmp");
    let path = tmp_dir.join(format!("export-{}.zip", Uuid::new_v4().simple()));
    let tmp = TmpFile(path.clone());
    // file IO runs off the async worker (`blocking`), as export_profile's own blob copies do
    let out = blocking(|| {
        std::fs::create_dir_all(&tmp_dir)?;
        OpenOptions::new().create_new(true).read(true).write(true).open(&path)
    })
    .map_err(internal)?;
    let manifest = export_profile(&mut tx, &rt.blobs, perm.profile.id, &out, &body.fmt).await?;
    drop(out);
    let size = blocking(|| std::fs::metadata(&path)).map_err(internal)?.len();
    let rows: u64 = manifest
        .get("counts")
        .and_then(Value::as_object)
        .map_or(0, |m| m.values().filter_map(Value::as_u64).sum());
    let files = manifest.get("files").and_then(Value::as_array).map_or(0, Vec::len);
    tx.commit().await?;
    // the audit chain links a row at commit, which a repeatable read snapshot cannot do, so the row
    // goes in its own transaction, still before any byte of the export is sent, stamped with the
    // start of the read
    let mut tx = rt.pool.begin().await?;
    write_audit_at(
        &mut tx,
        &AuditEvent {
            actor_id: p.user_id,
            tool: Some("POST /api/profiles/export".into()),
            profile_id: Some(perm.profile.id),
            resource_type: Some("profile".into()),
            resource_ids: Some(vec![perm.profile.id.to_string()]),
            rows_out: Some(i32::try_from(rows).unwrap_or(i32::MAX)),
            bytes_out: Some(i32::try_from(size).unwrap_or(i32::MAX)),
            args_summary: Some(format!("fmt={};files={files}", body.fmt)),
            ip: meta.ip.clone(),
            ..AuditEvent::new("user", "export", "export")
        },
        Some(read_at),
    )
    .await?;
    tx.commit().await?;
    // open for sending, then unlink: the data lives only in the open file until the body is sent
    let file = blocking(|| File::open(&path)).map_err(internal)?;
    drop(tmp);
    let filename = format!(
        "ownhealth-{}-{}.zip",
        perm.profile.slug,
        super::core::local_today().format("%Y-%m-%d")
    );
    let mut res =
        HttpBody::from_stream(tokio_util::io::ReaderStream::new(tokio::fs::File::from_std(file))).into_response();
    let h = res.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/zip"));
    h.insert(header::CONTENT_LENGTH, HeaderValue::from(size));
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    if let Ok(v) = HeaderValue::from_str(&format!("attachment; filename=\"{filename}\"")) {
        h.insert(header::CONTENT_DISPOSITION, v);
    }
    Ok(res)
}

/// The IPS Bundle of one profile, as the caller may read it today.
async fn ips(
    rt: Runtime,
    p: ownhealth_core::access::Principal,
    meta: Meta,
    profile: String,
    lang: Option<String>,
) -> R {
    // only a lang the caller sent can be refused, as on the summary sheet
    let lang = lang.unwrap_or_else(|| ownhealth_core::langs::lang_or_default(&p.lang).to_string());
    Check::body().literal("lang", Some(&lang), &LANGS).done()?;
    // the summary behind the Bundle reads one snapshot and stores its state, as on the sheet
    let mut tx = ownhealth_rules::summary::begin_read(&rt.pool).await?;
    let read_at: chrono::DateTime<chrono::Utc> = sqlx::query_scalar("select now()").fetch_one(&mut *tx).await?;
    let perm = Caller(p.clone())
        .require(&mut tx, &profile, Some("read:summary"), None)
        .await?;
    let today = today_utc();
    let (out, state) = ownhealth_ops::ips::read(&mut tx, perm.profile.id, &perm.scopes, &lang, today).await?;
    ownhealth_rules::commit_read(tx, perm.profile.id, &state).await?;
    let bytes = out.to_bytes()?;
    let entries = out.bundle["entry"].as_array().map_or(0, Vec::len);
    // the audit row commits before the Bundle goes out, stamped with the start of the read
    let mut tx = rt.pool.begin().await?;
    write_audit_at(
        &mut tx,
        &AuditEvent {
            actor_id: p.user_id,
            tool: Some("POST /api/profiles/export".into()),
            profile_id: Some(perm.profile.id),
            resource_type: Some("ips".into()),
            resource_ids: Some(vec![out.state_hash.clone()]),
            rows_out: Some(i32::try_from(out.srcs.len()).unwrap_or(i32::MAX)),
            src_ids: Some(out.srcs.clone()),
            bytes_out: Some(i32::try_from(bytes.len()).unwrap_or(i32::MAX)),
            args_summary: Some(format!("fmt=ips;lang={lang};entries={entries}")),
            withheld: Some(json!({"by_scope": out.withheld})),
            ip: meta.ip.clone(),
            ..AuditEvent::new("user", "export", "export")
        },
        Some(read_at),
    )
    .await?;
    tx.commit().await?;
    let filename = format!(
        "ownhealth-ips-{}-{lang}-{}.json",
        perm.profile.slug,
        today.format("%Y-%m-%d")
    );
    let mut res = bytes.into_response();
    let h = res.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/fhir+json"));
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    if let Ok(v) = HeaderValue::from_str(&format!("attachment; filename=\"{filename}\"")) {
        h.insert(header::CONTENT_DISPOSITION, v);
    }
    Ok(res)
}

pub fn router() -> Router<Runtime> {
    Router::new().route("/profiles/{p}/export", post(export))
}
