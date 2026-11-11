//! Read a printed sheet back the way an outside reader would (PDFium), for the tests of everything
//! `ownhealth_core::pdfwrite` prints: the wording lint and the check that every number names its source.

use crate::textlayer::read_pdf;

/// Run `check` on every visible line of `pdf` and panic with every line it refused. A file with hidden
/// text or no readable text layer fails too. Returns the lines, for further asserts.
pub fn assert_text_layer(pdf: &[u8], mut check: impl FnMut(&str) -> Result<(), String>) -> Vec<String> {
    let model = match read_pdf(pdf) {
        Ok(m) => m,
        Err(e) => panic!("the PDF has no readable text layer: {e}"),
    };
    assert!(
        !model.flags.contains_key("hidden_text"),
        "the PDF hides text: {:?}",
        model.flags
    );
    let lines: Vec<String> = model
        .pages
        .iter()
        .flat_map(|p| p.text(true).lines().map(String::from).collect::<Vec<_>>())
        .collect();
    assert!(!lines.is_empty(), "the PDF has no text");
    let refused: Vec<String> = lines
        .iter()
        .filter_map(|l| check(l).err().map(|why| format!("{l:?}: {why}")))
        .collect();
    assert!(refused.is_empty(), "lines the check refused:\n{}", refused.join("\n"));
    lines
}
