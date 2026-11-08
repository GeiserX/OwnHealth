"""PDF text layer: words with bboxes and font attributes, the usable-page check, hidden-text flags.

Coordinates are PDF points with the origin top-left, in the page's own space (ingestion.md 7.1).
Pure functions over PDF bytes; nothing here touches the database.
"""

from __future__ import annotations

import io
import re
import unicodedata
from dataclasses import asdict, dataclass, field

import pdfplumber

ENGINE = "pdfplumber"
ENGINE_VERSION = pdfplumber.__version__

MIN_CHARS = 150
MIN_WORDLIKE = 0.5
MAX_PUA = 0.05
HIDDEN_LUMA_DELTA = 0.05
TINY_PT = 2.0

# Instruction patterns (ingestion.md 10.3). A warning, never a filter: the text is kept.
INJECTION_PATTERNS: dict[str, re.Pattern] = {
    "ignore_previous": re.compile(
        r"ignor\w*\s+(\w+\s+){0,3}?(previous|prior|above|instructions?|instrucciones|anteriores|"
        r"pr[ée]c[ée]dentes|vorherigen|anweisungen)",
        re.I,
    ),
    "system_prompt": re.compile(r"system\s*prompt|prompt\s+del\s+sistema|invite\s+syst[èe]me", re.I),
    "you_are_now": re.compile(r"you\s+are\s+now|ahora\s+eres|tu\s+es\s+maintenant|du\s+bist\s+jetzt", re.I),
    "call_tool": re.compile(r"call\s+the\s+tool|use\s+the\s+tool|llama\s+a\s+la\s+herramienta|tool\s*call", re.I),
    "addressed_to_ai": re.compile(
        r"\b(AI|IA|KI)\s+(agent|assistant|asistente|agente|assistent)s?\b|\b(LLM|ChatGPT|Claude)\b", re.I
    ),
    "url": re.compile(r"https?://|www\.", re.I),
}

_VOWELS = set("aeiouyáéíóúàèìòùâêîôûäöüæœ")


@dataclass
class PWord:
    text: str
    x0: float
    top: float
    x1: float
    bottom: float
    font: str = ""
    size: float = 0.0
    color: float = 0.0  # luminance 0 (black) .. 1 (white)
    bold: bool = False
    hidden: str | None = None  # white_on_white | tiny | outside_page

    @property
    def bbox(self) -> tuple[float, float, float, float]:
        return (self.x0, self.top, self.x1, self.bottom)

    @property
    def xc(self) -> float:
        return (self.x0 + self.x1) / 2

    @property
    def yc(self) -> float:
        return (self.top + self.bottom) / 2

    def as_dict(self) -> dict:
        return asdict(self)


@dataclass
class PageModel:
    page_no: int
    width: float
    height: float
    words: list[PWord]
    text_layer_ok: bool
    flags: dict = field(default_factory=dict)

    @property
    def visible(self) -> list[PWord]:
        return [w for w in self.words if not w.hidden]

    def text(self, visible_only: bool = False) -> str:
        return "\n".join(
            " ".join(w.text for w in line) for line in group_lines(self.visible if visible_only else self.words)
        )


@dataclass
class PdfModel:
    pages: list[PageModel]
    metadata: dict[str, str]
    annotations: list[str]
    flags: dict = field(default_factory=dict)


def _luma(color) -> float:
    if color is None or isinstance(color, str):
        return 0.0
    c = list(color) if isinstance(color, (list, tuple)) else [color]
    try:
        c = [float(v) for v in c]
    except (TypeError, ValueError):
        return 0.0
    if len(c) == 1:
        return c[0]
    if len(c) == 3:
        return 0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2]
    if len(c) == 4:  # cmyk
        r, g, b = ((1 - c[i]) * (1 - c[3]) for i in range(3))
        return 0.2126 * r + 0.7152 * g + 0.0722 * b
    return 0.0


def _background(rects: list[dict], x: float, y: float) -> float:
    """Luminance behind a point: the last filled rectangle drawn there, else white paper."""
    bg = 1.0
    for r in rects:
        if r.get("fill") and r["x0"] <= x <= r["x1"] and r["top"] <= y <= r["bottom"]:
            bg = _luma(r.get("non_stroking_color"))
    return bg


def wordlike_share(words: list[str]) -> float:
    tokens = [t.lower() for t in words if len(t) >= 3 and t.isalpha()]
    if not tokens:
        return 0.0
    hits = sum(1 for t in tokens if (_VOWELS & set(t) and not re.search(r"[^aeiouyáéíóúàèìòùâêîôûäöüæœ]{5}", t)))
    return hits / len(tokens)


def usable(words: list[PWord]) -> tuple[bool, dict]:
    """ingestion.md 6: >= 150 chars, >= 50% word-like tokens, no (cid:NN) runs, < 5% Private Use Area."""
    joined = "".join(w.text for w in words)
    n = len(joined)
    pua = sum(1 for ch in joined if 0xE000 <= ord(ch) <= 0xF8FF) / n if n else 0.0
    cid = "(cid:" in joined
    share = wordlike_share([w.text for w in words])
    ok = n >= MIN_CHARS and share >= MIN_WORDLIKE and not cid and pua < MAX_PUA
    return ok, {"chars": n, "wordlike": round(share, 3), "cid": cid, "pua": round(pua, 3)}


def injection_hits(text: str) -> list[str]:
    return sorted(k for k, p in INJECTION_PATTERNS.items() if p.search(text or ""))


def read_pdf(data: bytes) -> PdfModel:
    """Every page's words (hidden ones flagged, not removed), the usable check, metadata and annotations."""
    pages: list[PageModel] = []
    annotations: list[str] = []
    with pdfplumber.open(io.BytesIO(data)) as pdf:
        metadata = {
            str(k): str(v) for k, v in (pdf.metadata or {}).items() if isinstance(v, (str, bytes)) and str(v).strip()
        }
        for i, page in enumerate(pdf.pages, start=1):
            raw = page.extract_words(
                keep_blank_chars=False,
                use_text_flow=False,
                x_tolerance=1.5,
                extra_attrs=["fontname", "size", "non_stroking_color"],
            )
            rects = [r for r in page.rects if r.get("fill")]
            words: list[PWord] = []
            for w in raw:
                text = unicodedata.normalize("NFC", w["text"])
                font = w.get("fontname") or ""
                pw = PWord(
                    text=text,
                    x0=round(float(w["x0"]), 2),
                    top=round(float(w["top"]), 2),
                    x1=round(float(w["x1"]), 2),
                    bottom=round(float(w["bottom"]), 2),
                    font=font.split("+", 1)[-1],
                    size=round(float(w.get("size") or 0), 2),
                    color=round(_luma(w.get("non_stroking_color")), 3),
                    bold=bool(re.search(r"bold|black|heavy|semibold|bd\b|-b$", font, re.I)),
                )
                if pw.size and pw.size < TINY_PT:
                    pw.hidden = "tiny"
                elif pw.x1 < 0 or pw.x0 > page.width or pw.bottom < 0 or pw.top > page.height:
                    pw.hidden = "outside_page"
                elif abs(pw.color - _background(rects, pw.xc, pw.yc)) < HIDDEN_LUMA_DELTA:
                    pw.hidden = "white_on_white"
                words.append(pw)
            visible = [w for w in words if not w.hidden]
            ok, stats = usable(visible)
            flags: dict = {"usable": stats}
            hidden = [w for w in words if w.hidden]
            if hidden:
                flags["hidden_text"] = {
                    "words": len(hidden),
                    "kinds": sorted({w.hidden for w in hidden if w.hidden}),
                }
            hits = injection_hits(" ".join(w.text for w in words))
            if hits:
                flags["possible_injection"] = hits
            for a in page.annots or []:
                for key in ("contents", "title"):
                    val = a.get(key)
                    if isinstance(val, (str, bytes)) and str(val).strip():
                        annotations.append(str(val))
            pages.append(
                PageModel(
                    page_no=i,
                    width=float(page.width),
                    height=float(page.height),
                    words=words,
                    text_layer_ok=ok,
                    flags=flags,
                )
            )
    doc_flags: dict = {}
    meta_hits = injection_hits(" ".join([*metadata.values(), *annotations]))
    page_hits = sorted({h for p in pages for h in p.flags.get("possible_injection", [])})
    if meta_hits or page_hits:
        doc_flags["possible_injection"] = {"pages": page_hits, "metadata": meta_hits}
    if any("hidden_text" in p.flags for p in pages):
        doc_flags["hidden_text"] = [p.page_no for p in pages if "hidden_text" in p.flags]
    return PdfModel(pages=pages, metadata=metadata, annotations=annotations, flags=doc_flags)


# ------------------------------------------------------------------- lines ----


def group_lines(words: list[PWord], tol: float = 0.5) -> list[list[PWord]]:
    """Words whose vertical extents overlap by more than `tol` of the smaller height share a line."""
    lines: list[list[PWord]] = []
    for w in sorted(words, key=lambda w: (w.yc, w.x0)):
        if lines:
            last = lines[-1]
            top = min(x.top for x in last)
            bottom = max(x.bottom for x in last)
            overlap = min(bottom, w.bottom) - max(top, w.top)
            h = min(bottom - top, w.bottom - w.top) or 1.0
            if overlap / h > tol:
                last.append(w)
                continue
        lines.append([w])
    for line in lines:
        line.sort(key=lambda w: w.x0)
    return lines


def union(boxes) -> tuple[float, float, float, float]:
    boxes = list(boxes)
    return (
        round(min(b[0] for b in boxes), 2),
        round(min(b[1] for b in boxes), 2),
        round(max(b[2] for b in boxes), 2),
        round(max(b[3] for b in boxes), 2),
    )
