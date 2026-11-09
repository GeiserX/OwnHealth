//! Lane E (v0.2): `get_document_page` (mcp.md 6.2). Scopes `read:documents_text` and `write:propose`;
//! read-only; at most 8k tokens per page and 300 pages per day per token (`page_cap`). Input
//! `{doc_id, page, include: ["words", "image"]}`. Words as lines with bboxes in one `untrusted_text`
//! block (origin `pdf_text_layer` or `ocr`); the image (PNG, 150 dpi, max 1600 px) only in `full`
//! identity mode. The get_source page rule applies.

use super::{Answer, Call, ToolResult};

pub async fn get_document_page(_call: &mut Call<'_>) -> ToolResult<Answer> {
    Err(ownhealth_core::Error::NotImplemented("get_document_page").into())
}
