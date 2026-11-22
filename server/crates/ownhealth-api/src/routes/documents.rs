//! Lane B: `api/documents.py`. The document vault: list and search documents, the original file,
//! page renders and word layers for the review split view, and the two corrections a person makes:
//! assign the document to another profile, or retract it.
//!
//! ```text
//! GET  /api/profiles/{p}/documents              ?q=&type=&limit=100
//! GET  /api/documents/{id}
//! GET  /api/documents/{id}/file
//! GET  /api/documents/{id}/pages/{n}.png        ?scale=2.0
//! GET  /api/documents/{id}/pages/{n}/words
//! POST /api/documents/{id}/assign               {"profile": "lucia"}
//! POST /api/documents/{id}/retract              {"reason": "..."}
//! ```
//!
//! Lab reports need `read:labs`, a raw wearables export (`doc_type` `wearables`: a FIT activity file,
//! a Gadgetbridge database) `read:wearables`, other documents `read:records`; page text and words also
//! need `read:documents_text`. A document on a profile the caller cannot read answers 404 like an unknown
//! id. Every read writes one audit row. Filenames and page text are untrusted and go out marked as
//! such.

use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use ownhealth_core::access::{Permission, Principal, ProfileRef, effective_permission, require};
use ownhealth_core::audit::{AuditEvent, write_audit};
use ownhealth_core::blobs::{BlobError, BlobOwner};
use ownhealth_core::documents::{
    WEARABLES_DOC_TYPE, document_is_sensitive, record_document_sql, sensitive_document_sql,
};
use ownhealth_core::pyjson::{parse_datetime, pydantic_datetime, row_to_json};
use ownhealth_core::{Error, Runtime};
use serde_json::{Map, Value, json};
use sqlx::postgres::PgRow;
use sqlx::{PgConnection, Row as _};
use uuid::Uuid;

use crate::Caller;
use crate::error::ApiError;
use crate::routes::auth::{Fail, R};
use crate::validate::{Errors, Params, body_str, json_object, param, uuid_path};

pub fn router() -> Router<Runtime> {
    Router::new()
        .route("/profiles/{p}/documents", get(list_documents))
        .route("/documents/{document_id}", get(get_document))
        .route("/documents/{document_id}/file", get(document_file))
        .route("/documents/{document_id}/pages/{page}", get(page_png))
        .route("/documents/{document_id}/pages/{page_no}/words", get(page_word_layer))
        .route("/documents/{document_id}/assign", post(assign))
        .route("/documents/{document_id}/retract", post(retract))
}

const COLS: &str = "d.id, d.profile_id, d.filename, d.doc_type, d.provider_name, d.collected_at, d.pages, d.status, \
                    d.flags, d.mime, d.size_bytes, d.created_at, \
                    (select count(*) from review_item r where r.document_id = d.id and r.status = 'open') as open_review";

const FTS_CFG: &str = "cast(case d.language when 'es' then 'spanish' when 'en' then 'english' when 'de' then 'german' \
                       when 'fr' then 'french' else 'simple' end as regconfig)";

fn untrusted(s: Option<&str>, origin: &str) -> Value {
    match s.filter(|s| !s.is_empty()) {
        Some(s) => json!({"trust": "untrusted_text", "origin": origin, "text": s}),
        None => Value::Null,
    }
}

fn scope_for(doc_type: Option<&str>) -> &'static str {
    match doc_type {
        Some("lab_report") => "read:labs",
        Some(WEARABLES_DOC_TYPE) => "read:wearables",
        _ => "read:records",
    }
}

fn unknown(message: &str) -> ApiError {
    ApiError::new(StatusCode::NOT_FOUND, "unknown", message)
}

/// A document as the list and the detail show it. `wearables`: the caller reads wearables on its
/// profile; without it, a CGM export says what it is (`flags.cgm_csv.vendor`) and not what it brought
/// (its counts, sensors and import summary).
fn summary(row: &Map<String, Value>, snippet: Option<&str>, wearables: bool) -> Map<String, Value> {
    let mut flags: Map<String, Value> = row
        .get("flags")
        .and_then(Value::as_object)
        .map(|f| {
            f.iter()
                .filter(|(k, _)| *k != "metadata_text")
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect()
        })
        .unwrap_or_default();
    if !wearables && let Some(cgm) = flags.get("cgm_csv").cloned() {
        flags.shift_remove("import");
        flags.insert(
            "cgm_csv".into(),
            json!({"vendor": cgm.get("vendor").cloned().unwrap_or(Value::Null)}),
        );
    }
    let mut out = Map::new();
    for k in ["id", "profile_id"] {
        out.insert(k.into(), row.get(k).cloned().unwrap_or(Value::Null));
    }
    out.insert(
        "filename".into(),
        untrusted(row.get("filename").and_then(Value::as_str), "filename"),
    );
    for k in ["doc_type", "provider_name", "collected_at", "pages", "status"] {
        out.insert(k.into(), row.get(k).cloned().unwrap_or(Value::Null));
    }
    // the routes are annotated `-> dict`: pydantic writes the datetime with `Z`
    if let Some(Value::String(s)) = out.get_mut("collected_at")
        && let Ok(dt) = parse_datetime(s)
    {
        *s = pydantic_datetime(&dt);
    }
    out.insert("flags".into(), Value::Object(flags));
    out.insert(
        "open_review".into(),
        row.get("open_review").cloned().unwrap_or(Value::Null),
    );
    out.insert("snippet".into(), untrusted(snippet, "pdf_text_layer"));
    out
}

async fn audit(
    conn: &mut PgConnection,
    principal: &Principal,
    profile_id: Uuid,
    tool: &str,
    ids: Vec<String>,
    action: &str,
) -> ownhealth_core::Result<()> {
    let via = if principal.token_id.is_none() { "ui" } else { "api" };
    let mut e = AuditEvent::new(principal.actor_kind(), via, action);
    e.actor_id = principal.user_id;
    e.token_id = principal.token_id;
    e.tool = Some(tool.into());
    e.profile_id = Some(profile_id);
    e.resource_type = Some("document".into());
    e.rows_out = Some(i32::try_from(ids.len()).unwrap_or(i32::MAX));
    e.resource_ids = Some(ids);
    write_audit(conn, &e).await?;
    Ok(())
}

/// A visible document: its row (as JSON), its blob hash and the caller's permission on its profile.
pub struct Doc {
    pub row: Map<String, Value>,
    pub blob_sha256: String,
    pub perm: Permission,
}

impl Doc {
    fn str(&self, k: &str) -> Option<&str> {
        self.row.get(k).and_then(Value::as_str)
    }
}

/// The document row and the caller's permission on its profile; 404 when it is not visible.
/// `content` (the file, page renders, words) also needs `read:sensitive` when a value read from the
/// document is sensitive.
pub async fn visible_document(
    conn: &mut PgConnection,
    principal: &Principal,
    document_id: Uuid,
    scope_extra: Option<&str>,
    content: bool,
) -> Result<Doc, ApiError> {
    let sql = format!("select {COLS}, d.blob_sha256 from document d where d.id = $1");
    let row: Option<PgRow> = sqlx::query(sqlx::AssertSqlSafe(sql))
        .bind(document_id)
        .fetch_optional(&mut *conn)
        .await?;
    let unknown_doc = || unknown("Unknown document.");
    let Some(row) = row else { return Err(unknown_doc()) };
    let Some(profile_id) = row.try_get::<Option<Uuid>, _>("profile_id")? else {
        return Err(unknown_doc());
    };
    let sha: Vec<u8> = row.try_get("blob_sha256")?;
    let mut json_row = row_to_json(&row)?;
    json_row.shift_remove("blob_sha256");
    let perm = match effective_permission(&mut *conn, principal, profile_id).await {
        Ok(p) => p,
        Err(Error::AccessDenied { .. }) => return Err(unknown_doc()),
        Err(e) => return Err(e.into()),
    };
    let doc_type = json_row.get("doc_type").and_then(Value::as_str);
    for scope in [Some(scope_for(doc_type)), scope_extra].into_iter().flatten() {
        if !perm.scopes.contains(scope) {
            return Err(Error::access_denied("insufficient_scope", format!("This needs {scope}.")).into());
        }
    }
    if content && !perm.has("read:sensitive") && document_is_sensitive(&mut *conn, document_id).await? {
        return Err(Error::access_denied(
            "insufficient_scope",
            "This document holds sensitive results; it needs read:sensitive.",
        )
        .into());
    }
    Ok(Doc {
        row: json_row,
        blob_sha256: hex(&sha),
        perm,
    })
}

/// Whether `GET /documents/{id}/file` serves this document to `perm` (the caller's permission on its
/// profile): the scope its type needs, and read:sensitive when it is sensitive, as
/// [`visible_document`] decides. A review item that only points at its document (a pull's
/// `fhir_unmapped_summary`) shows on the same terms.
pub async fn file_opens(conn: &mut PgConnection, perm: &Permission, document_id: Uuid) -> Result<bool, ApiError> {
    let doc_type: Option<Option<String>> = sqlx::query_scalar("select doc_type from document where id = $1")
        .bind(document_id)
        .fetch_optional(&mut *conn)
        .await?;
    let Some(doc_type) = doc_type else {
        return Ok(false);
    };
    if !perm.scopes.contains(scope_for(doc_type.as_deref())) {
        return Ok(false);
    }
    Ok(perm.has("read:sensitive") || !document_is_sensitive(conn, document_id).await?)
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// When a document counts as `done` evidence: its collection time, else its report time, the rule
/// prevention::evidence applies.
const EVIDENCE_AT: &str = "coalesce(d.collected_at, d.reported_at)";

/// What a document a FHIR import read carries: the issuer of its resources, written by every FHIR
/// file import and connector pull. An Apple Health export has its own tile.
const FHIR_IMPORT: &str = "(d.flags->'fhir_issuer' is not null and d.flags->'apple_health' is null)";

/// `GET /profiles/{p}/documents` for a resolved caller. `fhir_newest` (`latest=fhir`) lists only the
/// documents a FHIR import read, the newest stored first: a connector pull carries no date, so the
/// evidence order would put it behind every dated document.
pub async fn list_for(
    conn: &mut PgConnection,
    principal: &Principal,
    p: &str,
    q: Option<&str>,
    doc_type: Option<&str>,
    limit: i64,
    fhir_newest: bool,
) -> Result<Value, ApiError> {
    let r = match Uuid::parse_str(p) {
        Ok(id) => ProfileRef::Id(id),
        Err(_) => ProfileRef::Name(p),
    };
    let perm = require(&mut *conn, principal, r, None, None).await?;
    let (labs, records, wearables) = (
        perm.has("read:labs"),
        perm.has("read:records"),
        perm.has("read:wearables"),
    );
    if !labs && !records && !wearables {
        return Err(Error::access_denied(
            "insufficient_scope",
            "This needs read:labs, read:records or read:wearables.",
        )
        .into());
    }
    let q = q.filter(|q| !q.is_empty());
    let with_text = q.is_some() && perm.has("read:documents_text");
    // without read:sensitive, the page text of a sensitive document neither matches nor is quoted,
    // the rule visible_document applies to its file, renders and words
    let text_ok = if perm.has("read:sensitive") {
        "true".to_string()
    } else {
        format!("not {}", sensitive_document_sql("d.id"))
    };
    let mut wh = vec!["d.profile_id = $1".to_string()];
    // each type under the scope it needs (scope_for): a wearables export only with read:wearables
    let mut types = Vec::new();
    if labs {
        types.push("d.doc_type = 'lab_report'".to_string());
    }
    if records {
        types.push(format!(
            "(d.doc_type is distinct from 'lab_report' and {})",
            record_document_sql("d")
        ));
    }
    if wearables {
        types.push(format!("d.doc_type = '{WEARABLES_DOC_TYPE}'"));
    }
    wh.push(format!("({})", types.join(" or ")));
    wh.push("($3::text is null or d.doc_type = $3)".into());
    if fhir_newest {
        wh.push(FHIR_IMPORT.into());
    }
    if q.is_some() {
        // page text is searchable only with read:documents_text; the filename always is
        wh.push(if with_text {
            format!(
                "(d.filename ilike '%' || $4 || '%' or ({text_ok} and exists (select 1 from document_page pg \
                 where pg.document_id = d.id and pg.fts @@ websearch_to_tsquery({FTS_CFG}, $4))))"
            )
        } else {
            "d.filename ilike '%' || $4 || '%'".into()
        });
    }
    let snippet = if with_text {
        format!(
            ", case when {text_ok} then (select ts_headline({FTS_CFG}, pg.text_content, \
             websearch_to_tsquery({FTS_CFG}, $4), 'MaxWords=18, MinWords=6') from document_page pg \
             where pg.document_id = d.id \
             order by (pg.fts @@ websearch_to_tsquery({FTS_CFG}, $4)) desc, pg.page_no limit 1) end as snippet"
        )
    } else {
        String::new()
    };
    // one rule for the evidence day and the order, so the newest evidence comes first and a report
    // dated only by its report date is never cut by the limit behind older collection-dated files
    let order = if fhir_newest {
        "d.created_at desc, d.id".to_string()
    } else {
        format!("{EVIDENCE_AT} desc nulls last, d.created_at desc")
    };
    let sql = format!(
        "select {COLS}, {EVIDENCE_AT}::date as evidence_day{snippet} \
         from document d where {} and ($4::text is null or true) \
         order by {order} limit $2",
        wh.join(" and ")
    );
    let rows: Vec<PgRow> = sqlx::query(sqlx::AssertSqlSafe(sql))
        .bind(perm.profile.id)
        .bind(limit)
        .bind(doc_type)
        .bind(q)
        .fetch_all(&mut *conn)
        .await?;
    let mut items = Vec::new();
    let mut ids = Vec::new();
    for row in &rows {
        let j = row_to_json(row)?;
        ids.push(j.get("id").and_then(Value::as_str).unwrap_or("").to_string());
        let snip = if with_text {
            j.get("snippet").and_then(Value::as_str)
        } else {
            None
        };
        let mut item = summary(&j, snip, perm.has("read:wearables"));
        // the day this document counts as `done` evidence from: its collection day, else its report
        // day, the rule prevention::evidence applies
        item.insert(
            "evidence_day".into(),
            j.get("evidence_day").cloned().unwrap_or(Value::Null),
        );
        items.push(Value::Object(item));
    }
    let action = if q.is_some() { "search" } else { "read" };
    audit(
        conn,
        principal,
        perm.profile.id,
        "GET /api/profiles/{p}/documents",
        ids,
        action,
    )
    .await?;
    Ok(json!({ "items": items }))
}

async fn list_documents(
    State(rt): State<Runtime>,
    Caller(principal): Caller,
    Path(p): Path<String>,
    Query(qs): Query<Params>,
) -> R<Json<Value>> {
    let mut e = Errors::new();
    let q = e.str_max(&qs, "q", 200);
    let limit = e.int("query", &qs, "limit", 100, Some(1), Some(500));
    let latest = param(&qs, "latest");
    if let Some(v) = latest.filter(|v| *v != "fhir") {
        e.push(
            "literal_error",
            vec![json!("query"), json!("latest")],
            "Input should be 'fhir'",
            json!(v),
            Some(json!({"expected": "'fhir'"})),
        );
    }
    e.done()?;
    let doc_type = param(&qs, "type");
    let mut tx = rt.pool.begin().await?;
    let out = list_for(&mut tx, &principal, &p, q, doc_type, limit, latest.is_some()).await?;
    tx.commit().await?;
    Ok(Json(out))
}

async fn get_document(
    State(rt): State<Runtime>,
    Caller(principal): Caller,
    Path(document_id): Path<String>,
) -> R<Json<Value>> {
    let document_id = uuid_path("document_id", &document_id)?;
    let mut tx = rt.pool.begin().await?;
    let doc = visible_document(&mut tx, &principal, document_id, None, false).await?;
    let pages: Vec<PgRow> = sqlx::query(
        "select page_no, width_pt, height_pt, text_layer_ok, flags from document_page where document_id = $1 \
         order by page_no",
    )
    .bind(document_id)
    .fetch_all(&mut *tx)
    .await?;
    let expected: Vec<(Option<String>, Option<String>, Option<String>)> =
        sqlx::query_as("select printed_name, alias_group, reason from expected_test where report_document_id = $1")
            .bind(document_id)
            .fetch_all(&mut *tx)
            .await?;
    audit(
        &mut tx,
        &principal,
        doc.perm.profile.id,
        "GET /api/documents/{id}",
        vec![document_id.to_string()],
        "read",
    )
    .await?;
    tx.commit().await?;
    let mut out = summary(&doc.row, None, doc.perm.has("read:wearables"));
    out.insert("mime".into(), doc.row.get("mime").cloned().unwrap_or(Value::Null));
    out.insert(
        "size_bytes".into(),
        doc.row.get("size_bytes").cloned().unwrap_or(Value::Null),
    );
    let pages_info = pages
        .iter()
        .map(|p| row_to_json(p).map(Value::Object))
        .collect::<Result<Vec<_>, _>>()?;
    out.insert("pages_info".into(), Value::Array(pages_info));
    let tests: Vec<Value> = expected
        .iter()
        .map(|(name, group, reason)| {
            json!({
                "printed_name": untrusted(name.as_deref(), "pdf_text_layer"),
                "alias_group": group,
                "reason": untrusted(reason.as_deref(), "pdf_text_layer"),
            })
        })
        .collect();
    out.insert("expected_tests".into(), Value::Array(tests));
    Ok(Json(Value::Object(out)))
}

fn file_headers(mime: &str) -> [(header::HeaderName, String); 4] {
    [
        (header::CONTENT_TYPE, mime.to_string()),
        (header::X_CONTENT_TYPE_OPTIONS, "nosniff".into()),
        (header::CONTENT_SECURITY_POLICY, "sandbox".into()),
        (header::CACHE_CONTROL, "private, no-store".into()),
    ]
}

async fn document_file(
    State(rt): State<Runtime>,
    Caller(principal): Caller,
    Path(document_id): Path<String>,
) -> R<Response> {
    let document_id = uuid_path("document_id", &document_id)?;
    let mut tx = rt.pool.begin().await?;
    let doc = visible_document(&mut tx, &principal, document_id, None, true).await?;
    audit(
        &mut tx,
        &principal,
        doc.perm.profile.id,
        "GET /api/documents/{id}/file",
        vec![document_id.to_string()],
        "read",
    )
    .await?;
    // an Apple Health zip runs to hundreds of MB: stream it from disk, never hold it in memory
    let owner = BlobOwner::Profile(doc.perm.profile.id);
    let opened = rt.blobs.open_async(owner, &doc.blob_sha256).await;
    let (file, size) = opened.map_err(|e| match e {
        // the row is there but its file is not: say so without the hash
        BlobError::NotFound(_) => Error::NotFound("This document's file is not stored any more.".into()),
        e => Error::from(e),
    })?;
    tx.commit().await?;
    let mime = doc.str("mime").unwrap_or("application/octet-stream");
    // the file opened (open_async checks a sealed file's first and last record), so a record that
    // fails later can only end the stream: the status went out, so the log is where it shows
    let stream = futures::TryStreamExt::inspect_err(tokio_util::io::ReaderStream::new(file), move |e| {
        tracing::error!(document = %document_id, error = %e, "a document's file stopped part way through");
    });
    let body = axum::body::Body::from_stream(stream);
    Ok((
        file_headers(mime),
        [
            (header::CONTENT_DISPOSITION, "inline".to_string()),
            (header::CONTENT_LENGTH, size.to_string()),
        ],
        body,
    )
        .into_response())
}

/// FastAPI's `page_no: int` path parameter.
fn page_param(raw: &str) -> R<i32> {
    let mut e = Errors::new();
    let n = e.int(
        "path",
        &[("page_no".to_string(), raw.to_string())],
        "page_no",
        0,
        None,
        None,
    );
    e.done()?;
    Ok(i32::try_from(n).unwrap_or(0))
}

async fn page_png(
    State(rt): State<Runtime>,
    Caller(principal): Caller,
    Path((document_id, page)): Path<(String, String)>,
    Query(qs): Query<Params>,
) -> R<Response> {
    let Some(page) = page.strip_suffix(".png") else {
        return Err(ApiError::not_found().into());
    };
    let document_id = uuid_path("document_id", &document_id)?;
    let page_no = page_param(page)?;
    let mut e = Errors::new();
    let scale = e.float(&qs, "scale", 2.0, 0.5, 4.0);
    e.done()?;
    let mut tx = rt.pool.begin().await?;
    let doc = visible_document(&mut tx, &principal, document_id, None, true).await?;
    let pages = doc.row.get("pages").and_then(Value::as_i64).unwrap_or(0);
    if doc.str("mime") != Some("application/pdf") || !(1..=pages).contains(&i64::from(page_no)) {
        return Err(unknown("No such page.").into());
    }
    let (blobs, sha, owner) = (
        rt.blobs.clone(),
        doc.blob_sha256.clone(),
        BlobOwner::Profile(doc.perm.profile.id),
    );
    let png =
        tokio::task::spawn_blocking(move || ownhealth_ingest_pdf::render_page_png(&blobs, owner, &sha, page_no, scale))
            .await
            .map_err(|e| Error::Other(e.into()))??;
    audit(
        &mut tx,
        &principal,
        doc.perm.profile.id,
        "GET /api/documents/{id}/pages/{n}.png",
        vec![document_id.to_string()],
        "read",
    )
    .await?;
    tx.commit().await?;
    Ok((file_headers("image/png"), png).into_response())
}

/// `GET /documents/{id}/pages/{n}/words` for a resolved caller.
pub async fn words_for(
    conn: &mut PgConnection,
    blobs: &ownhealth_core::blobs::BlobStore,
    principal: &Principal,
    document_id: Uuid,
    page_no: i32,
) -> Result<Value, ApiError> {
    let doc = visible_document(&mut *conn, principal, document_id, Some("read:documents_text"), true).await?;
    let page: Option<PgRow> =
        sqlx::query("select width_pt, height_pt, flags from document_page where document_id = $1 and page_no = $2")
            .bind(document_id)
            .bind(page_no)
            .fetch_optional(&mut *conn)
            .await?;
    let Some(page) = page else {
        return Err(unknown("No such page."));
    };
    let page = row_to_json(&page)?;
    let words = ownhealth_ingest_pdf::page_words(&mut *conn, blobs, document_id, page_no).await?;
    audit(
        conn,
        principal,
        doc.perm.profile.id,
        "GET /api/documents/{id}/pages/{n}/words",
        vec![document_id.to_string()],
        "read",
    )
    .await?;
    let words: Vec<Value> = words
        .iter()
        .map(|w| json!({"text": w.text, "bbox": w.bbox, "bold": w.bold, "hidden": w.hidden}))
        .collect();
    Ok(json!({
        "trust": "untrusted_text",
        "origin": "pdf_text_layer",
        "page_no": page_no,
        "width_pt": page.get("width_pt"),
        "height_pt": page.get("height_pt"),
        "flags": page.get("flags"),
        "words": words,
    }))
}

async fn page_word_layer(
    State(rt): State<Runtime>,
    Caller(principal): Caller,
    Path((document_id, page_no)): Path<(String, String)>,
) -> R<Json<Value>> {
    let document_id = uuid_path("document_id", &document_id)?;
    let page_no = page_param(&page_no)?;
    let mut tx = rt.pool.begin().await?;
    let out = words_for(&mut tx, &rt.blobs, &principal, document_id, page_no).await?;
    tx.commit().await?;
    Ok(Json(out))
}

async fn assign(
    State(rt): State<Runtime>,
    caller: Caller,
    Path(document_id): Path<String>,
    headers: HeaderMap,
    bytes: Bytes,
) -> R<Json<Value>> {
    let principal = &caller.0;
    let document_id = uuid_path("document_id", &document_id)?;
    let body = json_object(&headers, &bytes, true)?.unwrap_or_default();
    let mut e = Errors::new();
    let profile = body_str(&mut e, &body, "profile");
    e.done()?;
    let profile = profile.unwrap_or_default();
    let mut tx = rt.pool.begin().await?;
    // A CGM export's lock before its row: a running import of it holds that lock and then writes the row.
    ownhealth_ingest_structured::retract::lock_export_of(&mut tx, document_id).await?;
    // The row lock first: a move running at the same time commits before the owner is read here, so
    // the permission check and the re-key (in assign_document) both see the owner it left.
    ownhealth_core::documents::lock_owner(&mut tx, document_id).await?;
    let doc = visible_document(&mut tx, principal, document_id, None, false).await?;
    if !doc.perm.can("import") {
        let msg = format!("The role {} cannot import on this profile.", doc.perm.role);
        return Err(Error::access_denied("forbidden", msg).into());
    }
    let target = caller.require(&mut tx, &profile, None, Some("import")).await?;
    // it re-keys the files and queues the removal of the old copies in this transaction
    let job_id = ownhealth_ingest_pdf::assign_document(
        &mut tx,
        &rt.blobs,
        document_id,
        target.profile.id,
        principal.user_id.unwrap_or_default(),
        None,
    )
    .await?;
    audit(
        &mut tx,
        principal,
        target.profile.id,
        "POST /api/documents/{id}/assign",
        vec![document_id.to_string()],
        "import",
    )
    .await?;
    tx.commit().await?;
    Ok(Json(
        json!({"document_id": document_id, "profile": target.profile.slug, "job_id": job_id}),
    ))
}

/// Soft delete with a reason: the document leaves the vault, its values are retracted, its open
/// review items dismissed. The blob stays until an explicit purge.
async fn retract(
    State(rt): State<Runtime>,
    Caller(principal): Caller,
    Path(document_id): Path<String>,
    headers: HeaderMap,
    bytes: Bytes,
) -> R<Json<Value>> {
    let document_id = uuid_path("document_id", &document_id)?;
    let body = json_object(&headers, &bytes, true)?.unwrap_or_default();
    let mut e = Errors::new();
    let reason = body_str(&mut e, &body, "reason");
    e.done()?;
    let mut tx = rt.pool.begin().await?;
    let doc = visible_document(&mut tx, &principal, document_id, None, false).await?;
    if !doc.perm.can("write") {
        let msg = format!("The role {} cannot change this profile.", doc.perm.role);
        return Err(Error::access_denied("forbidden", msg).into());
    }
    let reason = reason.unwrap_or_default().trim().to_string();
    if reason.is_empty() {
        return Err(Fail::Api(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "bad_input",
            "A retraction needs a reason.",
        )));
    }
    if doc.str("status") == Some("retracted") {
        return Err(ApiError::new(StatusCode::CONFLICT, "conflict", "The document is already retracted.").into());
    }
    let why = format!("document retracted: {reason}");
    let out = ownhealth_ingest_structured::retract::retract_document_facts(&mut tx, document_id, &why).await?;
    // A ClinVar alert of this file moves to another current file of the person that shows the same
    // change; only an alert no other file stands for is dismissed below.
    ownhealth_genome::clinvar::hand_over_alerts(&mut tx, document_id).await?;
    let open: Vec<Uuid> = sqlx::query_scalar("select id from review_item where document_id = $1 and status = 'open'")
        .bind(document_id)
        .fetch_all(&mut *tx)
        .await?;
    for item_id in open {
        ownhealth_core::review::dismiss(
            &mut tx,
            item_id,
            principal.user_id.unwrap_or_default(),
            Some("document retracted"),
        )
        .await?;
    }
    sqlx::query("update document set status = 'retracted', status_reason = $1 where id = $2")
        .bind(&reason)
        .bind(document_id)
        .execute(&mut *tx)
        .await?;
    audit(
        &mut tx,
        &principal,
        doc.perm.profile.id,
        "POST /api/documents/{id}/retract",
        vec![document_id.to_string()],
        "delete",
    )
    .await?;
    tx.commit().await?;
    Ok(Json(json!({
        "document_id": document_id,
        "status": "retracted",
        "retracted_observations": out.get("observations").cloned().unwrap_or_else(|| json!([])),
        "retracted_records": out.get("records").cloned().unwrap_or_else(|| json!({})),
        "deleted_wearable_samples": out.get("wearable_samples").cloned().unwrap_or_else(|| json!(0)),
    })))
}
