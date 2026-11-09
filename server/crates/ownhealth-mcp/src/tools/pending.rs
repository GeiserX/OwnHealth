//! Lane E (v0.2): `list_pending_documents` (mcp.md 6.1). Scope `write:propose`; read-only,
//! idempotent; budget 2k. Input `{profile?, state?: "unparsed"|"needs_review"|"any", cursor?}`. Rows
//! `{doc_id, received_at, kind_guess, pages, pages_pending, text_layer, ocr_state, title}`; the title
//! (from an email subject or a file name) is `untrusted_text`. Documents waiting with the `ocr` profile
//! off are all listed. Registered in `tools::TOOLS` when it lands, not before (the tool list is part
//! of the contract).

use super::{Answer, Call, ToolResult};

pub async fn list_pending_documents(_call: &mut Call<'_>) -> ToolResult<Answer> {
    Err(ownhealth_core::Error::NotImplemented("list_pending_documents").into())
}
