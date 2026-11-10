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
