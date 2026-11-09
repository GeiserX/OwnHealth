//! Which pages need OCR (lane C, ingestion.md 6): no usable text layer (fewer than 150 characters,
//! under half of the 3+ letter tokens in the word lists, `(cid:NN)` runs or 5% or more Private Use
//! Area characters), every image, and nothing else: a page with a good text layer never goes to OCR.

/// True when a page with this text (and text-layer verdict) must be read by OCR.
pub fn needs_ocr(text_layer_ok: bool, _text: &str) -> bool {
    !text_layer_ok
}
