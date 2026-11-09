//! Lane E (v0.2): what was in the agent's context for a proposal (security-privacy.md 4.2 #4,
//! ingestion.md 7.2): the documents and src ids the proposing token read in the hour before, from
//! the audit log. Stored on the review item (`context_documents`, `context_src_ids`) when `propose`
//! files it, and shown in the review queue.

use chrono::{DateTime, Utc};
use ownhealth_core::{Error, Result};
use serde::Serialize;
use sqlx::PgConnection;
use uuid::Uuid;

/// The look-back window.
pub const WINDOW_SECONDS: i64 = 3600;

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct AgentContext {
    pub documents: Vec<Uuid>,
    pub src_ids: Vec<String>,
}

pub async fn agent_context(_conn: &mut PgConnection, _token_id: Uuid, _at: DateTime<Utc>) -> Result<AgentContext> {
    Err(Error::NotImplemented("mcp::context::agent_context"))
}
