//! The one PDF writer: the same input gives the same bytes, text wraps by the AFM widths, tables
//! break across pages, and a character WinAnsi cannot show is folded and reported.

use ownhealth_core::pdfwrite::{Column, Folded, Font, Writer, text_width, wrap};
use sha2::{Digest, Sha256};

const PARAGRAPH: &str = "Carmen Ortega trajo a la consulta los análisis de LabNorte del 14/09/2026. \
     El colesterol LDL fue 97 mg/dL (valores de referencia < 130), la vitamina D 24 ng/mL \
     (30 - 100) y el ácido úrico 5,1 mg/dL. Cada número de esta hoja cita el documento y la \
     página de los que sale, para que cualquiera pueda comprobarlo.";

fn sha(b: &[u8]) -> String {
    hex::encode(Sha256::digest(b))
}

fn paragraph_pdf() -> Vec<u8> {
    let mut w = Writer::new();
    w.paragraph("Resumen para la consulta", Font::Bold, 14.0, 18.0);
    w.paragraph(PARAGRAPH, Font::Regular, 10.0, 13.0);
    w.y -= 4.0;
    w.rule(56.0, 539.0, 0.6);
    w.polyline(&[(56.0, 600.0), (80.0, 610.0), (104.0, 604.0), (128.0, 620.0)], 1.0);
    w.finish().pdf
}

fn table_pdf() -> Vec<u8> {
    let cols = [
        Column::new(56.0, 180.0, Font::Regular),
        Column::new(240.0, 70.0, Font::Regular),
        Column::new(314.0, 225.0, Font::Regular),
    ];
    let rows: Vec<Vec<String>> = (0..70)
        .map(|i| {
            vec![
                format!("Prueba {i} con un nombre largo que no cabe en una línea"),
                format!("{},{} mg/dL", 90 + i, i % 10),
                format!("LabNorte_2026-09-14.pdf, página {}", 1 + i % 3),
            ]
        })
        .collect();
    let mut w = Writer::new().on_new_page(|w, page| {
        w.text(56.0, 800.0, &format!("Rivera, Alex · hoja {page}"), Font::Regular, 8.0);
        w.y = 780.0;
    });
    w.table(&cols, Some(&["Prueba", "Resultado", "Fuente"]), &rows, 9.0, 12.0);
    w.finish().pdf
}

#[test]
fn a_wrapped_paragraph_is_byte_stable() {
    let a = paragraph_pdf();
    assert_eq!(a, paragraph_pdf(), "two runs gave different bytes");
    assert_eq!(
        sha(&a),
        "521efcc3f5cc57697c1325b0a1e94e5eee67395617f121262e70cbbc9a54f2fe",
        "the paragraph golden moved; re-record only on purpose"
    );
}

#[test]
fn a_table_is_byte_stable_and_breaks_pages() {
    let a = table_pdf();
    assert_eq!(a, table_pdf(), "two runs gave different bytes");
    let s = String::from_utf8_lossy(&a);
    assert!(s.contains("/Count 3 "), "70 wrapped rows should fill three pages");
    assert_eq!(s.matches(" hoja ").count(), 3, "the page hook runs once per page");
    assert_eq!(s.matches("(Prueba) Tj").count(), 3, "the header repeats on every page");
    assert_eq!(
        sha(&a),
        "dfcd50cd66a78e63f7ae6f3c7cd1575c9ba4108df931b1ae1637502d91613840",
        "the table golden moved; re-record only on purpose"
    );
}

#[test]
fn the_id_is_a_content_hash_and_there_is_no_date() {
    let a = paragraph_pdf();
    let s = String::from_utf8_lossy(&a);
    assert!(!s.contains("CreationDate") && !s.contains("ModDate") && !s.contains("/Info"));
    let xref = a.windows(6).rposition(|w| w == b"\nxref\n").expect("xref") + 1;
    let id = &sha(&a[..xref])[..32];
    assert!(
        s.contains(&format!("/ID [<{id}> <{id}>]")),
        "the /ID is the body's hash"
    );
    let mut other = Writer::new();
    other.paragraph("Otro texto", Font::Regular, 10.0, 13.0);
    assert!(!String::from_utf8_lossy(&other.finish().pdf).contains(id));
}

#[test]
fn wrapping_follows_the_afm_widths() {
    let lines = wrap(PARAGRAPH, Font::Regular, 10.0, 200.0);
    assert!(lines.len() > 3);
    for l in &lines {
        assert!(
            text_width(l, Font::Regular, 10.0) <= 200.0,
            "{l:?} is wider than 200 pt"
        );
    }
    assert_eq!(lines.join(" "), PARAGRAPH, "wrapping only splits at spaces");
    // AFM, not a per-character guess: "WWW" is wider than "iii".
    assert!(text_width("WWW", Font::Bold, 10.0) > 3.0 * text_width("iii", Font::Bold, 10.0));
    assert_eq!(
        text_width("Hola", Font::Regular, 10.0),
        (722.0 + 556.0 + 222.0 + 556.0) / 100.0
    );
    // A word longer than the line is cut, never drawn past the edge.
    let long = wrap("Supercalifragilisticoespialidoso", Font::Regular, 10.0, 40.0);
    assert!(long.len() > 1 && long.iter().all(|l| text_width(l, Font::Regular, 10.0) <= 40.0));
}

#[test]
fn a_character_winansi_lacks_is_folded_and_reported() {
    let mut w = Writer::new();
    w.paragraph("Paciente: Michał Rivera Núñez", Font::Regular, 10.0, 13.0);
    w.paragraph("Michał otra vez", Font::Regular, 10.0, 13.0);
    let out = w.finish();
    assert_eq!(
        out.folded,
        vec![Folded {
            ch: 'ł',
            to: "l".into()
        }]
    );
    let s = String::from_utf8_lossy(&out.pdf);
    assert!(
        s.contains("(Paciente: Michal Rivera N\u{FFFD}\u{FFFD}ez) Tj"),
        "ñ and ú stay WinAnsi"
    );
    let mut w = Writer::new();
    w.paragraph("ﬁn ŝ ☃", Font::Regular, 10.0, 13.0);
    let out = w.finish();
    assert_eq!(
        out.folded,
        vec![
            Folded {
                ch: 'ﬁ',
                to: "fi".into()
            },
            Folded {
                ch: 'ŝ',
                to: "s".into()
            },
            Folded {
                ch: '☃',
                to: "?".into()
            },
        ]
    );
    assert!(String::from_utf8_lossy(&out.pdf).contains("(fin s ?) Tj"));
}

#[test]
fn an_empty_writer_still_makes_one_page() {
    let pdf = Writer::new().finish().pdf;
    assert!(String::from_utf8_lossy(&pdf).contains("/Count 1 "));
}

#[test]
fn a_fold_never_turns_into_a_different_number() {
    let mut w = Writer::new();
    w.paragraph(
        "Leucocitos 7,2 x10⁹/L · ⅓ comprimido · 12 μmol/L · −0,4",
        Font::Regular,
        10.0,
        13.0,
    );
    let out = w.finish();
    let s = String::from_utf8_lossy(&out.pdf);
    assert!(
        s.contains("(Leucocitos 7,2 x10?/L \u{FFFD} ? comprimido \u{FFFD} 12 \u{FFFD}mol/L \u{FFFD} -0,4) Tj"),
        "{s}"
    );
    let to = |c: char| out.folded.iter().find(|f| f.ch == c).map(|f| f.to.clone());
    assert_eq!(to('⁹').as_deref(), Some("?"), "a superscript nine is not a nine");
    assert_eq!(to('⅓').as_deref(), Some("?"), "a third is not thirteen");
    assert_eq!(to('μ').as_deref(), Some("µ"), "Greek mu prints as the micro sign");
    assert_eq!(to('−').as_deref(), Some("-"), "the minus sign stays a minus");

    let mut w = Writer::new();
    w.paragraph("50 ㎍ · 10 ㎛ · 5 ㎕ · ㎓", Font::Regular, 10.0, 13.0);
    let out = w.finish();
    let to = |c: char| out.folded.iter().find(|f| f.ch == c).map(|f| f.to.clone());
    assert_eq!(to('㎍').as_deref(), Some("µg"), "microgram stays micro");
    assert_eq!(to('㎛').as_deref(), Some("µm"), "micrometre stays micro");
    assert_eq!(to('㎕').as_deref(), Some("µl"), "microlitre stays micro");
    assert_eq!(to('㎓').as_deref(), Some("GHz"), "a fold of plain letters still works");
}

#[test]
fn a_fold_never_drops_part_of_a_character() {
    // A character with a part that has no WinAnsi byte prints as `?`, never as the other parts:
    // ㏀ is k + Greek omega, and "k" alone would be a different unit.
    let mut w = Writer::new();
    w.paragraph("㎆ ㏀", Font::Regular, 10.0, 13.0);
    let out = w.finish();
    let to = |c: char| out.folded.iter().find(|f| f.ch == c).map(|f| f.to.clone());
    assert_eq!(to('㎆').as_deref(), Some("MB"));
    assert_eq!(
        to('㏀').as_deref(),
        Some("?"),
        "kilo-ohm loses its omega, so it is refused"
    );
}

#[test]
fn line_breaks_start_new_lines_and_other_controls_are_reported() {
    assert_eq!(
        wrap("Tomar con comida\nNotas: ninguna\r\n\nFin", Font::Regular, 10.0, 400.0),
        vec!["Tomar con comida", "Notas: ninguna", "", "Fin"]
    );
    let mut w = Writer::new();
    w.paragraph("a\tb\nc", Font::Regular, 10.0, 13.0);
    let out = w.finish();
    assert_eq!(
        out.folded,
        vec![Folded {
            ch: '\t',
            to: " ".into()
        }]
    );
    let s = String::from_utf8_lossy(&out.pdf);
    assert!(s.contains("(a b) Tj") && s.contains("(c) Tj"), "{s}");
}

/// The y of `BT /F1 9.0 Tf x y Td (...) Tj ET`.
fn td_y(line: &str) -> Option<f64> {
    line.split(" Td ").next()?.rsplit(' ').next()?.parse().ok()
}

#[test]
fn a_row_taller_than_a_page_continues_on_the_next_and_loses_nothing() {
    let cols = [
        Column::new(56.0, 60.0, Font::Regular),
        Column::new(120.0, 40.0, Font::Regular),
    ];
    let rows = vec![
        vec!["corta".to_string(), "1".to_string()],
        vec!["nota".to_string(), "palabra ".repeat(300)],
        vec!["después".to_string(), "2".to_string()],
    ];
    let mut w = Writer::new();
    w.table(&cols, Some(&["Campo", "Texto"]), &rows, 9.0, 12.0);
    let out = w.finish();
    let s = String::from_utf8_lossy(&out.pdf);
    assert_eq!(s.matches("palabra").count(), 300, "every word is printed");
    for line in s.lines().filter(|l| l.contains(" Td ")) {
        let y = td_y(line).expect("a Td line has a y");
        assert!(y >= 56.0, "drawn below the bottom margin: {line}");
    }
    let pages: usize = s
        .split("/Count ")
        .nth(1)
        .and_then(|r| r.split(' ').next()?.parse().ok())
        .expect("a page count");
    assert!(pages >= 3, "300 lines need several pages");
    assert_eq!(
        s.matches("(Texto) Tj").count(),
        pages,
        "the header repeats on every page"
    );
    assert!(s.contains("(corta) Tj") && s.contains("(despu\u{FFFD}s) Tj"));
}
