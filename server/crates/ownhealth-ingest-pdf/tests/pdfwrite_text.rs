//! What `pdfwrite` prints reads back through PDFium as the same lines, so the sheets of later slices
//! can be linted and source-checked on their real text layer.

use ownhealth_core::pdfwrite::{Column, Font, Writer};
use ownhealth_ingest_pdf::textcheck::assert_text_layer;

fn sheet() -> Vec<u8> {
    let mut w = Writer::new();
    w.paragraph("Resumen de Michał Rivera", Font::Bold, 14.0, 18.0);
    w.paragraph(
        "Valores de los últimos análisis de LabNorte. Cada número dice de qué documento sale.",
        Font::Regular,
        10.0,
        13.0,
    );
    let cols = [
        Column::new(56.0, 150.0, Font::Regular),
        Column::new(210.0, 80.0, Font::Bold),
        Column::new(300.0, 239.0, Font::Regular),
    ];
    let rows = vec![
        vec![
            "Colesterol LDL".into(),
            "97 mg/dL".into(),
            "LabNorte 14/09/2026, pág. 1".into(),
        ],
        vec![
            "Vitamina D".into(),
            "24 ng/mL".into(),
            "LabNorte 14/09/2026, pág. 2".into(),
        ],
    ];
    w.table(&cols, Some(&["Prueba", "Valor", "Fuente"]), &rows, 9.0, 12.0);
    w.finish().pdf
}

/// Every line with a unit also names where the number came from.
fn names_its_source(line: &str) -> Result<(), String> {
    if (line.contains("mg/dL") || line.contains("ng/mL")) && !line.contains("LabNorte") {
        return Err("a number without its source".into());
    }
    Ok(())
}

#[test]
fn the_printed_lines_read_back_in_order() {
    let lines = assert_text_layer(&sheet(), names_its_source);
    assert_eq!(lines[0], "Resumen de Michal Rivera", "the folded name prints as Michal");
    assert!(lines.iter().any(|l| l.contains("últimos análisis")), "{lines:#?}");
    assert!(lines.contains(&"Prueba Valor Fuente".to_string()), "{lines:#?}");
    assert!(
        lines.contains(&"Colesterol LDL 97 mg/dL LabNorte 14/09/2026, pág. 1".to_string()),
        "{lines:#?}"
    );
}

#[test]
#[should_panic(expected = "a number without its source")]
fn a_line_the_check_refuses_fails_the_test() {
    let mut w = Writer::new();
    w.paragraph("Vitamina D 24 ng/mL", Font::Regular, 10.0, 13.0);
    assert_text_layer(&w.finish().pdf, names_its_source);
}
