//! Synthetic lab reports for the demo seed and the tests, drawn with `pdfwrite` and byte for byte the
//! output of `demo/pdf.py`. It knows where it drew each row, so the bboxes it returns match the page
//! (PDF points, origin top-left).

use crate::afm;
pub use crate::pdfwrite::{PAGE_H, PAGE_W};
use crate::pdfwrite::{assemble, show};
/// name, value, unit, range, flag
pub const COLS: [f64; 5] = [56.0, 300.0, 352.0, 430.0, 515.0];
pub const ROW_H: f64 = 16.0;
pub const FONT_SIZE: f64 = 9.0;

pub type BBox = [f64; 4];

#[derive(Debug, Clone, PartialEq, Default)]
pub struct LabRow {
    pub name: String,
    pub value: String,
    pub unit: String,
    pub r#ref: String,
    pub flag: String,
}

impl LabRow {
    pub fn new(name: &str, value: &str, unit: &str, r#ref: &str) -> LabRow {
        LabRow {
            name: name.into(),
            value: value.into(),
            unit: unit.into(),
            r#ref: r#ref.into(),
            flag: String::new(),
        }
    }

    pub fn flagged(mut self, flag: &str) -> LabRow {
        self.flag = flag.into();
        self
    }

    fn cells(&self) -> [&str; 5] {
        [&self.name, &self.value, &self.unit, &self.r#ref, &self.flag]
    }
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct LabReport<'a> {
    pub lab: &'a str,
    pub title: &'a str,
    pub patient: &'a str,
    pub birth: &'a str,
    pub collected: &'a str,
    pub report_id: &'a str,
    pub pages: Vec<Vec<LabRow>>,
}

/// Python `s.encode("cp1252", errors="replace")`: the demo never folds, so its bytes stay Python's.
fn text(out: &mut Vec<u8>, x: f64, y: f64, s: &str, font: &str, size: f64) {
    let bytes: Vec<u8> = s.chars().map(|c| afm::cp1252(c).unwrap_or(b'?')).collect();
    show(out, x, y, &bytes, font, size);
}

fn width(s: &str) -> f64 {
    s.chars().count() as f64 * FONT_SIZE * 0.52
}

/// Python `round(x, 2)`.
fn round2(x: f64) -> f64 {
    format!("{x:.2}").parse().unwrap_or(x)
}

/// Render a report. Returns (pdf bytes, bboxes per page per row, top-left origin).
pub fn lab_report_pdf(r: &LabReport<'_>) -> (Vec<u8>, Vec<Vec<BBox>>) {
    let n = r.pages.len();
    let mut streams = Vec::new();
    let mut boxes = Vec::new();
    for (i, rows) in r.pages.iter().enumerate() {
        let page_no = i + 1;
        let mut out = Vec::new();
        text(&mut out, 56.0, 790.0, r.lab, "F2", 14.0);
        text(&mut out, 56.0, 772.0, r.title, "F1", 10.0);
        text(
            &mut out,
            56.0,
            748.0,
            &format!("Paciente: {}    F. nacimiento: {}", r.patient, r.birth),
            "F1",
            9.0,
        );
        text(
            &mut out,
            56.0,
            734.0,
            &format!(
                "Fecha de toma: {}    Informe: {}    Pagina {page_no}/{n}",
                r.collected, r.report_id
            ),
            "F1",
            9.0,
        );
        let headers = ["Prueba", "Resultado", "Unidades", "Valores de referencia", ""];
        for (x, h) in COLS.iter().zip(headers) {
            if !h.is_empty() {
                text(&mut out, *x, 706.0, h, "F2", 8.5);
            }
        }
        out.extend_from_slice(b"0.6 w 56 700 m 539 700 l S\n");
        let mut page_boxes = Vec::new();
        let mut y = 684.0;
        for row in rows {
            let cells = row.cells();
            for (j, (x, cell)) in COLS.iter().zip(cells).enumerate() {
                if !cell.is_empty() {
                    text(&mut out, *x, y, cell, if j == 4 { "F2" } else { "F1" }, FONT_SIZE);
                }
            }
            let right = COLS
                .iter()
                .zip(cells)
                .filter(|(_, c)| !c.is_empty())
                .map(|(x, c)| x + width(c))
                .fold(f64::NEG_INFINITY, f64::max);
            let top = PAGE_H - (y + FONT_SIZE * 0.78);
            let bottom = PAGE_H - (y - FONT_SIZE * 0.22);
            page_boxes.push([COLS[0], round2(top), round2(right), round2(bottom)]);
            y -= ROW_H;
        }
        text(
            &mut out,
            56.0,
            60.0,
            "Documento sintetico de prueba. No corresponde a ningun paciente real.",
            "F1",
            7.0,
        );
        streams.push(out);
        boxes.push(page_boxes);
    }
    (assemble(&streams, false), boxes)
}
