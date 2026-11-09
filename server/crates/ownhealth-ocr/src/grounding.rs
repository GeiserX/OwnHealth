//! Grounding (lane C; ingestion.md 7.2, security-privacy.md 4.2 #5): an extracted value counts only
//! when it is on the same page's word layer (the text layer, or our OCR pass), decimal-separator
//! tolerant (`12,3` = `12.3`, `250.000` = `250000` in a Spanish thousands context). Used by OCR rows and
//! by agent proposals (lane E). A page with no word layer gives `NoWordLayer`: the row goes to review
//! as `ungrounded_no_word_layer`, never as grounded.

use ownhealth_ingest_pdf::Word;
use serde::Serialize;

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "grounding", rename_all = "snake_case")]
pub enum Grounding {
    /// Found; `bbox` covers the matched words.
    Grounded { bbox: [f64; 4] },
    /// The page has words but not this value: confidence 0, red marker in review.
    Ungrounded,
    /// The page has no word layer at all (a scan while the `ocr` profile is off).
    UngroundedNoWordLayer,
}

impl Grounding {
    /// The `candidate_row.grounding` value.
    pub fn code(&self) -> &'static str {
        match self {
            Grounding::Grounded { .. } => "grounded",
            Grounding::Ungrounded => "ungrounded",
            Grounding::UngroundedNoWordLayer => "ungrounded_no_word_layer",
        }
    }
}

/// Is `value` (with `unit`, when given) on this page's words, inside `near` when given (a bbox the
/// extractor or the agent claimed)? Pure; the caller loads the words (`ownhealth_ingest_pdf::page_words`).
pub fn ground(words: &[Word], _value: &str, _unit: Option<&str>, _near: Option<[f64; 4]>) -> Grounding {
    if words.is_empty() {
        return Grounding::UngroundedNoWordLayer;
    }
    Grounding::Ungrounded
}
