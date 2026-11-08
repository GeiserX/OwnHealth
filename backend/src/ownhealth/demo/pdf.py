"""A tiny PDF writer for synthetic lab reports (demo seed and tests). Text layer only, Helvetica.

It knows where it drew each row, so the bboxes it returns match the page (PDF points, origin
top-left). Lane B's synthetic lab-slip generator can replace it for the eval set.
"""

from __future__ import annotations

from dataclasses import dataclass

PAGE_W, PAGE_H = 595.0, 842.0  # A4
COLS = (56.0, 300.0, 352.0, 430.0, 515.0)  # name, value, unit, range, flag
ROW_H = 16.0
FONT_SIZE = 9.0


@dataclass(frozen=True)
class LabRow:
    name: str
    value: str
    unit: str = ""
    ref: str = ""
    flag: str = ""


def _esc(s: str) -> bytes:
    raw = s.encode("cp1252", errors="replace")
    return raw.replace(b"\\", b"\\\\").replace(b"(", b"\\(").replace(b")", b"\\)")


def _text(x: float, y: float, s: str, font: str = "F1", size: float = FONT_SIZE) -> bytes:
    return b"BT /%s %.1f Tf %.2f %.2f Td (%s) Tj ET\n" % (font.encode(), size, x, y, _esc(s))


def _width(s: str, size: float = FONT_SIZE) -> float:
    return len(s) * size * 0.52


def lab_report_pdf(
    *, lab: str, title: str, patient: str, birth: str, collected: str, report_id: str, pages: list[list[LabRow]]
) -> tuple[bytes, list[list[tuple[float, float, float, float]]]]:
    """Render a report. Returns (pdf bytes, bboxes per page per row, top-left origin)."""
    streams: list[bytes] = []
    boxes: list[list[tuple[float, float, float, float]]] = []
    for page_no, rows in enumerate(pages, start=1):
        out = bytearray()
        out += _text(56, 790, lab, "F2", 14)
        out += _text(56, 772, title, "F1", 10)
        out += _text(56, 748, f"Paciente: {patient}    F. nacimiento: {birth}", "F1", 9)
        out += _text(
            56, 734, f"Fecha de toma: {collected}    Informe: {report_id}    Pagina {page_no}/{len(pages)}", "F1", 9
        )
        headers = ("Prueba", "Resultado", "Unidades", "Valores de referencia", "")
        for x, h in zip(COLS, headers, strict=True):
            if h:
                out += _text(x, 706, h, "F2", 8.5)
        out += b"0.6 w 56 700 m 539 700 l S\n"
        page_boxes = []
        y = 684.0
        for row in rows:
            for i, (x, cell) in enumerate(zip(COLS, (row.name, row.value, row.unit, row.ref, row.flag), strict=True)):
                if cell:
                    out += _text(x, y, cell, "F2" if i == 4 else "F1")
            right = max(
                x + _width(c)
                for x, c in zip(COLS, (row.name, row.value, row.unit, row.ref, row.flag), strict=True)
                if c
            )
            top = PAGE_H - (y + FONT_SIZE * 0.78)
            bottom = PAGE_H - (y - FONT_SIZE * 0.22)
            page_boxes.append((COLS[0], round(top, 2), round(right, 2), round(bottom, 2)))
            y -= ROW_H
        out += _text(56, 60, "Documento sintetico de prueba. No corresponde a ningun paciente real.", "F1", 7)
        streams.append(bytes(out))
        boxes.append(page_boxes)
    return _assemble(streams), boxes


def _assemble(streams: list[bytes]) -> bytes:
    n = len(streams)
    # objects: 1 catalog, 2 pages, 3 font F1, 4 font F2, then per page: page obj, content obj
    objs: list[bytes] = []
    kids = " ".join(f"{5 + 2 * i} 0 R" for i in range(n))
    objs.append(b"<< /Type /Catalog /Pages 2 0 R >>")
    objs.append(f"<< /Type /Pages /Kids [{kids}] /Count {n} >>".encode())
    objs.append(b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /Encoding /WinAnsiEncoding >>")
    objs.append(b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica-Bold /Encoding /WinAnsiEncoding >>")
    for i, stream in enumerate(streams):
        content_ref = 6 + 2 * i
        objs.append(
            f"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 {PAGE_W:.0f} {PAGE_H:.0f}]"
            f" /Resources << /Font << /F1 3 0 R /F2 4 0 R >> >> /Contents {content_ref} 0 R >>".encode()
        )
        objs.append(b"<< /Length %d >>\nstream\n" % len(stream) + stream + b"endstream")
    out = bytearray(b"%PDF-1.4\n%\xe2\xe3\xcf\xd3\n")
    offsets = []
    for num, body in enumerate(objs, start=1):
        offsets.append(len(out))
        out += b"%d 0 obj\n" % num + body + b"\nendobj\n"
    xref = len(out)
    out += b"xref\n0 %d\n0000000000 65535 f \n" % (len(objs) + 1)
    for off in offsets:
        out += b"%010d 00000 n \n" % off
    out += b"trailer\n<< /Size %d /Root 1 0 R >>\nstartxref\n%d\n%%%%EOF\n" % (len(objs) + 1, xref)
    return bytes(out)
