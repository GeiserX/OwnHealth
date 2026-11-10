//! Lane E (v0.2): what was in the agent's context for a proposal (security-privacy.md 4.2 #4,
//! ingestion.md 7.2): the documents and src ids the proposing token read in the hour before, from
//! the audit log. Stored on the review item (`context_documents`, `context_src_ids`) when `propose`
//! files it, and shown in the review queue.
//!
//! A document counts as read when a call returned one of its pages (`get_document_page` audits
//! `resource_type = 'document'`) or a src id that cites it (`get_source`, search results).
//!
//! Only the proposal's own profile counts: what the same token read on another profile is left out,
//! so the review item never hands that profile's reviewer ids from someone else's record.

use chrono::{DateTime, Duration, Utc};
use ownhealth_core::Result;
use serde::Serialize;
use sqlx::PgConnection;
use uuid::Uuid;

/// The look-back window.
pub const WINDOW_SECONDS: i64 = 3600;
/// At most this many src ids are kept on one review item.
pub const MAX_SRC_IDS: usize = 500;

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct AgentContext {
    /// First read first.
    pub documents: Vec<Uuid>,
    pub src_ids: Vec<String>,
}

pub async fn agent_context(
    conn: &mut PgConnection,
    token_id: Uuid,
    profile_id: Uuid,
    at: DateTime<Utc>,
) -> Result<AgentContext> {
    let from = at - Duration::seconds(WINDOW_SECONDS);
    let src_ids: Vec<String> = sqlx::query_scalar(
        "select s from (select s, min(e.id) as first from audit_event e, unnest(e.src_ids) as s \
         join src p on p.id = s \
         where e.token_id = $1 and e.ts > $2 and e.ts <= $3 and e.result_kind in ('ok', 'budget_cut') \
         and p.profile_id = $5 group by s) x order by first, s limit $4",
    )
    .bind(token_id)
    .bind(from)
    .bind(at)
    .bind(MAX_SRC_IDS as i64)
    .bind(profile_id)
    .fetch_all(&mut *conn)
    .await?;
    let documents: Vec<Uuid> = sqlx::query_scalar(
        "select d from ( \
           select r::uuid as d, e.id as at_id from audit_event e, unnest(e.resource_ids) as r \
           where e.token_id = $1 and e.ts > $2 and e.ts <= $3 and e.resource_type = 'document' \
             and e.result_kind in ('ok', 'budget_cut') and r ~ '^[0-9a-f-]{36}$' \
           union all \
           select s.document_id, e.id from audit_event e, unnest(e.src_ids) as x join src s on s.id = x \
           where e.token_id = $1 and e.ts > $2 and e.ts <= $3 and s.document_id is not null \
             and e.result_kind in ('ok', 'budget_cut') \
         ) y join document doc on doc.id = y.d where doc.profile_id = $4 group by d order by min(at_id), d",
    )
    .bind(token_id)
    .bind(from)
    .bind(at)
    .bind(profile_id)
    .fetch_all(&mut *conn)
    .await?;
    Ok(AgentContext { documents, src_ids })
}
