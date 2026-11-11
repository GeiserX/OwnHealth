//! The one PDF writer of the app: A4, Helvetica and Helvetica-Bold with WinAnsi encoding, text layer
//! only. The same input always gives the same bytes: there is no date in the file, and the trailer
//! `/ID` is a hash of the content, so a printed sheet can be pinned in a test.
//!
//! Coordinates are PDF points with the origin at the bottom left. The cursor `y` is the top of the
//! free space on the current page; flowing content (`paragraph`, `table`) moves it down and starts a
//! new page when it would cross the bottom margin. The demo lab reports (`demo_pdf`) share this
//! module's text operator and file assembly.

use sha2::{Digest, Sha256};
use unicode_normalization::UnicodeNormalization;
use unicode_normalization::char::is_combining_mark;

use crate::afm;

pub const PAGE_W: f64 = 595.0;
pub const PAGE_H: f64 = 842.0;
pub const MARGIN: f64 = 56.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Font {
    Regular,
    Bold,
}

impl Font {
    /// The resource name on the page (`/F1`, `/F2`).
    pub(crate) fn res(self) -> &'static str {
        match self {
            Font::Regular => "F1",
            Font::Bold => "F2",
        }
    }

    fn widths(self) -> &'static [u16; 256] {
        match self {
            Font::Regular => &afm::HELVETICA,
            Font::Bold => &afm::HELVETICA_BOLD,
        }
    }
}

/// A character WinAnsi cannot show, and what was printed instead (`?` when nothing fits).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Folded {
    pub ch: char,
    pub to: String,
}

/// Letters NFKD does not split into a base letter and a mark, and signs with a WinAnsi twin.
const STROKES: [(char, char); 9] = [
    ('ł', 'l'),
    ('Ł', 'L'),
    ('đ', 'd'),
    ('Đ', 'D'),
    ('ħ', 'h'),
    ('Ħ', 'H'),
    ('ı', 'i'),
    ('μ', 'µ'),
    ('−', '-'),
];

fn stroke(c: char) -> char {
    STROKES.iter().find(|(from, _)| *from == c).map_or(c, |(_, to)| *to)
}

/// What WinAnsi prints for `c`, which it cannot show: its NFC form when WinAnsi shows that, else
/// its NFKD letters without the marks, or `?`.
/// A fold that would print a digit is refused (`⁹` is not `9`, `⅓` is not `13`), and so is one
/// with a part WinAnsi cannot show (`㏀` is not `k`), so a value on the page is never a different
/// value. A control character prints as a space.
fn fold(c: char) -> String {
    if c.is_control() {
        return " ".into();
    }
    // A canonical twin WinAnsi shows keeps the whole character: the angstrom sign prints as Å.
    let mut nfc = std::iter::once(c).nfc();
    if let (Some(one), None) = (nfc.next(), nfc.next())
        && afm::cp1252(one).is_some()
    {
        return one.into();
    }
    let parts: Vec<char> = c.nfkd().filter(|d| !is_combining_mark(*d)).map(stroke).collect();
    let base: String = parts.iter().collect();
    if parts.is_empty() || parts.iter().any(|d| afm::cp1252(*d).is_none() || d.is_ascii_digit()) {
        "?".into()
    } else {
        base
    }
}

/// A character WinAnsi prints as itself: everything `cp1252` maps except the control characters.
fn printable(c: char) -> Option<u8> {
    if c.is_control() { None } else { afm::cp1252(c) }
}

/// `s` in WinAnsi bytes, and the characters that had to be folded (each once, in order).
pub fn encode(s: &str) -> (Vec<u8>, Vec<Folded>) {
    let mut bytes = Vec::with_capacity(s.len());
    let mut folded: Vec<Folded> = Vec::new();
    for c in s.chars() {
        match printable(c) {
            Some(b) => bytes.push(b),
            None => {
                let to = fold(c);
                bytes.extend(to.chars().filter_map(afm::cp1252));
                if !folded.iter().any(|f| f.ch == c) {
                    folded.push(Folded { ch: c, to });
                }
            }
        }
    }
    (bytes, folded)
}

fn bytes_width(bytes: &[u8], font: Font, size: f64) -> f64 {
    let table = font.widths();
    bytes.iter().map(|b| f64::from(table[*b as usize])).sum::<f64>() * size / 1000.0
}

/// The printed width of `s` in points, by the AFM advance widths.
pub fn text_width(s: &str, font: Font, size: f64) -> f64 {
    bytes_width(&encode(s).0, font, size)
}

/// Split `s` into lines no wider than `max_w`: at each line break (`\n` or `\r\n`), then at spaces;
/// a word longer than a line is cut.
pub fn wrap(s: &str, font: Font, size: f64, max_w: f64) -> Vec<String> {
    s.split('\n')
        .flat_map(|l| wrap_line(l.strip_suffix('\r').unwrap_or(l), font, size, max_w))
        .collect()
}

fn wrap_line(s: &str, font: Font, size: f64, max_w: f64) -> Vec<String> {
    let fits = |t: &str| text_width(t, font, size) <= max_w;
    let mut lines = Vec::new();
    let mut line = String::new();
    for word in s.split(' ') {
        let joined = if line.is_empty() {
            word.to_string()
        } else {
            format!("{line} {word}")
        };
        if fits(&joined) {
            line = joined;
            continue;
        }
        if !line.is_empty() {
            lines.push(std::mem::take(&mut line));
        }
        for c in word.chars() {
            line.push(c);
            if !fits(&line) && line.chars().count() > 1 {
                line.pop();
                lines.push(std::mem::replace(&mut line, c.to_string()));
            }
        }
    }
    if !line.is_empty() || lines.is_empty() {
        lines.push(line);
    }
    lines
}

/// One `BT ... Tj ET` line with the text already in WinAnsi bytes.
pub(crate) fn show(out: &mut Vec<u8>, x: f64, y: f64, bytes: &[u8], font: &str, size: f64) {
    out.extend_from_slice(format!("BT /{font} {size:.1} Tf {x:.2} {y:.2} Td (").as_bytes());
    for &b in bytes {
        if matches!(b, b'\\' | b'(' | b')') {
            out.push(b'\\');
        }
        out.push(b);
    }
    out.extend_from_slice(b") Tj ET\n");
}

/// The file around the page content streams. `with_id` adds the trailer `/ID`, the first 16 bytes of
/// the sha256 of everything before the cross-reference table.
pub(crate) fn assemble(streams: &[Vec<u8>], with_id: bool) -> Vec<u8> {
    let n = streams.len();
    // objects: 1 catalog, 2 pages, 3 font F1, 4 font F2, then per page: page obj, content obj
    let kids = (0..n)
        .map(|i| format!("{} 0 R", 5 + 2 * i))
        .collect::<Vec<_>>()
        .join(" ");
    let mut objs: Vec<Vec<u8>> = vec![
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        format!("<< /Type /Pages /Kids [{kids}] /Count {n} >>").into_bytes(),
        b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /Encoding /WinAnsiEncoding >>".to_vec(),
        b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica-Bold /Encoding /WinAnsiEncoding >>".to_vec(),
    ];
    for (i, stream) in streams.iter().enumerate() {
        let content_ref = 6 + 2 * i;
        objs.push(
            format!(
                "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 {PAGE_W:.0} {PAGE_H:.0}] \
                 /Resources << /Font << /F1 3 0 R /F2 4 0 R >> >> /Contents {content_ref} 0 R >>"
            )
            .into_bytes(),
        );
        let mut body = format!("<< /Length {} >>\nstream\n", stream.len()).into_bytes();
        body.extend_from_slice(stream);
        body.extend_from_slice(b"endstream");
        objs.push(body);
    }
    let mut out: Vec<u8> = b"%PDF-1.4\n%\xe2\xe3\xcf\xd3\n".to_vec();
    let mut offsets = Vec::new();
    for (num, body) in objs.iter().enumerate() {
        offsets.push(out.len());
        out.extend_from_slice(format!("{} 0 obj\n", num + 1).as_bytes());
        out.extend_from_slice(body);
        out.extend_from_slice(b"\nendobj\n");
    }
    let xref = out.len();
    let id = if with_id {
        let h = hex::encode(&Sha256::digest(&out)[..16]);
        format!(" /ID [<{h}> <{h}>]")
    } else {
        String::new()
    };
    out.extend_from_slice(format!("xref\n0 {}\n0000000000 65535 f \n", objs.len() + 1).as_bytes());
    for off in offsets {
        out.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
    }
    out.extend_from_slice(
        format!(
            "trailer\n<< /Size {} /Root 1 0 R{id} >>\nstartxref\n{xref}\n%%EOF\n",
            objs.len() + 1
        )
        .as_bytes(),
    );
    out
}

/// One table column: where it starts, how wide its text may run, and its font.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Column {
    pub x: f64,
    pub width: f64,
    pub font: Font,
}

impl Column {
    pub fn new(x: f64, width: f64, font: Font) -> Column {
        Column { x, width, font }
    }
}

/// The finished file and every character that had to be folded on the way.
#[derive(Debug, Clone, PartialEq)]
pub struct Rendered {
    pub pdf: Vec<u8>,
    pub folded: Vec<Folded>,
}

type PageHook = Box<dyn FnMut(&mut Writer, usize)>;

/// A document being written, page by page.
pub struct Writer {
    pages: Vec<Vec<u8>>,
    open: bool,
    folded: Vec<Folded>,
    hook: Option<PageHook>,
    /// The top of the free space on the current page.
    pub y: f64,
    pub left: f64,
    pub right: f64,
    pub top: f64,
    pub bottom: f64,
}

impl Default for Writer {
    fn default() -> Self {
        Writer::new()
    }
}

impl Writer {
    /// An empty A4 document with 56 pt margins.
    pub fn new() -> Writer {
        Writer {
            pages: Vec::new(),
            open: false,
            folded: Vec::new(),
            hook: None,
            y: PAGE_H - MARGIN,
            left: MARGIN,
            right: PAGE_W - MARGIN,
            top: MARGIN,
            bottom: MARGIN,
        }
    }

    /// Run `hook(writer, page_no)` at the start of every page (1-based), for a header or footer.
    /// The hook may move `y` down to leave room for what it drew.
    pub fn on_new_page(mut self, hook: impl FnMut(&mut Writer, usize) + 'static) -> Writer {
        self.hook = Some(Box::new(hook));
        self
    }

    fn page(&mut self) -> &mut Vec<u8> {
        if !self.open {
            self.new_page();
        }
        let last = self.pages.len() - 1;
        &mut self.pages[last]
    }

    /// Start a new page now and run the page hook.
    pub fn new_page(&mut self) {
        self.pages.push(Vec::new());
        self.open = true;
        self.y = PAGE_H - self.top;
        if let Some(mut hook) = self.hook.take() {
            let n = self.pages.len();
            hook(self, n);
            self.hook = Some(hook);
        }
    }

    /// Start a new page unless `h` points still fit above the bottom margin.
    pub fn ensure(&mut self, h: f64) {
        if !self.open || self.y - h < self.bottom {
            self.new_page();
        }
    }

    /// One line of text with its baseline at (`x`, `y`). The cursor does not move.
    pub fn text(&mut self, x: f64, y: f64, s: &str, font: Font, size: f64) {
        let (bytes, folded) = encode(s);
        for f in folded {
            if !self.folded.iter().any(|g| g.ch == f.ch) {
                self.folded.push(f);
            }
        }
        show(self.page(), x, y, &bytes, font.res(), size);
    }

    /// `s` wrapped to the margins, one line every `leading` points, starting a page when needed.
    pub fn paragraph(&mut self, s: &str, font: Font, size: f64, leading: f64) {
        for line in wrap(s, font, size, self.right - self.left) {
            self.ensure(leading);
            let (x, y) = (self.left, self.y - size);
            self.text(x, y, &line, font, size);
            self.y -= leading;
        }
    }

    /// A horizontal rule across `x0..x1` at the cursor, `w` points thick. The cursor does not move.
    pub fn rule(&mut self, x0: f64, x1: f64, w: f64) {
        let y = self.y;
        self.line(&[(x0, y), (x1, y)], w);
    }

    /// An open path through `points` (a sparkline), `w` points thick.
    pub fn polyline(&mut self, points: &[(f64, f64)], w: f64) {
        self.line(points, w);
    }

    fn line(&mut self, points: &[(f64, f64)], w: f64) {
        let Some(((x0, y0), rest)) = points.split_first() else {
            return;
        };
        let mut op = format!("{w:.2} w {x0:.2} {y0:.2} m");
        for (x, y) in rest {
            op.push_str(&format!(" {x:.2} {y:.2} l"));
        }
        op.push_str(" S\n");
        self.page().extend_from_slice(op.as_bytes());
    }

    fn cells(cols: &[Column], cells: &[&str], bold: bool, size: f64) -> Vec<Vec<String>> {
        cols.iter()
            .zip(cells)
            .map(|(c, s)| wrap(s, if bold { Font::Bold } else { c.font }, size, c.width))
            .collect()
    }

    /// Lines `from..to` of a wrapped row, the first at the cursor; the cursor moves below them.
    #[allow(clippy::too_many_arguments)]
    fn lines(
        &mut self,
        cols: &[Column],
        wrapped: &[Vec<String>],
        from: usize,
        to: usize,
        bold: bool,
        size: f64,
        leading: f64,
    ) {
        let top = self.y;
        for (c, cell) in cols.iter().zip(wrapped) {
            let font = if bold { Font::Bold } else { c.font };
            for (i, line) in cell.iter().enumerate().take(to).skip(from) {
                if !line.is_empty() {
                    self.text(c.x, top - size - (i - from) as f64 * leading, line, font, size);
                }
            }
        }
        self.y = top - (to - from) as f64 * leading;
    }

    fn row(&mut self, cols: &[Column], cells: &[&str], bold: bool, size: f64, leading: f64) {
        let wrapped = Writer::cells(cols, cells, bold, size);
        let n = wrapped.iter().map(Vec::len).max().unwrap_or(1);
        self.lines(cols, &wrapped, 0, n, bold, size, leading);
    }

    /// Rows of cells under an optional bold header; each cell wraps inside its column. A row that
    /// does not fit in the rest of the page starts a new one; a row taller than a whole page
    /// continues on the next page. The header repeats at the top of every page the table reaches.
    pub fn table(&mut self, cols: &[Column], header: Option<&[&str]>, rows: &[Vec<String>], size: f64, leading: f64) {
        let rule_w = 0.6;
        let draw_header = |w: &mut Writer| {
            if let Some(h) = header {
                w.row(cols, h, true, size, leading);
                let (x0, x1) = (
                    cols.first().map_or(w.left, |c| c.x),
                    cols.last().map_or(w.right, |c| c.x + c.width),
                );
                w.y -= 2.0;
                w.rule(x0, x1, rule_w);
                w.y -= 2.0;
            }
        };
        let wrapped: Vec<Vec<Vec<String>>> = rows
            .iter()
            .map(|r| {
                let cells: Vec<&str> = r.iter().map(String::as_str).collect();
                Writer::cells(cols, &cells, false, size)
            })
            .collect();
        let count = |w: &[Vec<String>]| w.iter().map(Vec::len).max().unwrap_or(1);
        let first = wrapped.first().map_or(1, |w| count(w)) as f64 * leading;
        self.ensure(first + if header.is_some() { leading + 4.0 } else { 0.0 });
        draw_header(self);
        let mut on_page = 0;
        for w in &wrapped {
            let n = count(w);
            if on_page > 0 && self.y - n as f64 * leading < self.bottom {
                self.new_page();
                draw_header(self);
                on_page = 0;
            }
            let mut from = 0;
            loop {
                // At least one line per page, so a page hook that leaves no room cannot loop forever.
                let room = (((self.y - self.bottom) / leading).floor() as usize).max(1);
                let to = n.min(from + room);
                self.lines(cols, w, from, to, false, size, leading);
                from = to;
                if from >= n {
                    break;
                }
                self.new_page();
                draw_header(self);
            }
            on_page += 1;
        }
    }

    /// The finished file (at least one page) and the characters that were folded.
    pub fn finish(mut self) -> Rendered {
        if self.pages.is_empty() {
            self.new_page();
        }
        Rendered {
            pdf: assemble(&self.pages, true),
            folded: self.folded,
        }
    }
}
