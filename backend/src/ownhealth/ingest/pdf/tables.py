"""Lab table extraction from a page's word layer (ingestion.md 7).

One engine, two configurations: a vendor template (`templates.py`, prior 1.0) pins the header words,
locale, flag tokens and stop lines of one provider; the generic rules (prior 0.95) use the header
vocabulary of es/en/de/fr. Both find a header line with name and value columns, cut the page into
column ranges from the header word positions, group words into lines by vertical overlap, and turn
each line below the header into a result row, a pending ("ordered but missing") line, a section
title, or a continuation of the row above (wrapped names, two-line ranges). A line inside the table
that does not read cleanly still becomes a low-confidence row for review: nothing is dropped.
"""

from __future__ import annotations

import re
from dataclasses import dataclass, field

from ownhealth.ingest.classify import fold
from ownhealth.ingest.pdf.textlayer import PageModel, PWord, group_lines, union

COLUMN_KINDS = ("name", "value", "unit", "ref_range", "flag")
REQUIRED = ("name", "value", "unit")

GENERIC_HEADERS: dict[str, tuple[str, ...]] = {
    "name": (
        "prueba",
        "pruebas",
        "analisis",
        "determinacion",
        "parametro",
        "magnitud",
        "test",
        "tests",
        "analyte",
        "investigation",
        "parameter",
        "examination",
        "untersuchung",
        "analyt",
        "kenngrosse",
        "analyse",
        "analyses",
        "examen",
        "parametre",
    ),
    "value": (
        "resultado",
        "resultados",
        "result",
        "results",
        "value",
        "ergebnis",
        "wert",
        "messwert",
        "resultat",
        "valeur",
    ),
    "unit": ("unidades", "unidad", "uds", "units", "unit", "einheit", "einheiten", "unite", "unites"),
    "ref_range": (
        "valores de referencia",
        "intervalo de referencia",
        "rango de referencia",
        "v. referencia",
        "referencia",
        "reference range",
        "reference interval",
        "ref. range",
        "reference",
        "range",
        "referenzbereich",
        "normbereich",
        "referenzwerte",
        "referenz",
        "valeurs de reference",
        "valeurs usuelles",
        "intervalle de reference",
        "valeurs normales",
        "normes",
    ),
    "flag": ("indicador", "marca", "flag", "flags", "kennz.", "kennzeichen", "bewertung", "indicateur", "alerte"),
}

FLAG_TOKENS = frozenset(
    {
        "H",
        "L",
        "HH",
        "LL",
        "A",
        "*",
        "**",
        "+",
        "-",
        "++",
        "--",
        "↑",
        "↓",
        "(H)",
        "(L)",
        "▲",
        "▼",
        "h",
        "l",
        "(+)",
        "(-)",
    }
)

TEXT_RESULTS = (
    "negativo",
    "negativa",
    "positivo",
    "positiva",
    "no detectado",
    "no detectable",
    "indetectable",
    "trazas",
    "ausencia",
    "presencia",
    "normal",
    "negative",
    "positive",
    "not detected",
    "undetectable",
    "trace",
    "absent",
    "present",
    "negativ",
    "positiv",
    "nicht nachweisbar",
    "spuren",
    "negatif",
    "positif",
    "non detecte",
    "traces",
    "absence",
    "presence",
)
PENDING = (
    "pendiente",
    "en curso",
    "en proceso",
    "pending",
    "to follow",
    "in progress",
    "folgt",
    "ausstehend",
    "in arbeit",
    "en cours",
    "a suivre",
    "resultat a suivre",
)
SKIP_PREFIXES = (
    "comentario",
    "nota:",
    "observaciones",
    "comment",
    "note:",
    "remark",
    "hinweis",
    "bemerkung",
    "kommentar",
    "remarque",
    "commentaire",
)
STOP_PREFIXES = (
    "documento sintetico",
    "validado por",
    "fin del informe",
    "synthetic document",
    "validated by",
    "end of report",
    "synthetisches dokument",
    "validiert",
    "ende des befundes",
    "document synthetique",
    "valide par",
    "fin du compte rendu",
)
SPECIMENS = {
    "urine": ("orina", "urine", "urin", "urines", "harn"),
    "blood": ("sangre", "blood", "blut", "sang", "hemograma", "haematology", "hematology", "blutbild", "hematologie"),
    "serum": ("suero", "serum", "bioquimica", "biochemistry", "chemistry", "klinische chemie", "biochimie"),
    "stool": ("heces", "stool", "faeces", "stuhl", "selles"),
}

_VALUE_RE = re.compile(r"^[<>≤≥=]{0,2}\s*[-+]?\d")
_NUMBER_RE = re.compile(r"[-+]?\d+(?:[.,']\d+)*")
COL_MARGIN = 6.0
STRADDLE_PT = 2.0
FLAG_GAP = 12.0  # a flag token this far right of the range text sits in its own (unheaded) column
TIGHT = 0.45  # a gap under this share of the line height joins a line to its neighbour


@dataclass
class TableSpec:
    extractor: str  # "template:es/labnorte" or "rules:generic-table"
    version: str
    prior: float
    headers: dict[str, tuple[str, ...]]
    flag_tokens: frozenset[str] = FLAG_TOKENS
    stop: tuple[str, ...] = STOP_PREFIXES
    skip: tuple[str, ...] = SKIP_PREFIXES
    locale: str | None = None


GENERIC = TableSpec(extractor="rules:generic-table", version="1", prior=0.95, headers=GENERIC_HEADERS)


@dataclass
class Cell:
    text: str
    bbox: tuple[float, float, float, float] | None
    conf: float = 1.0
    bold: bool = False

    def as_dict(self) -> dict:
        d = {"text": self.text, "bbox": list(self.bbox) if self.bbox else None, "conf": round(self.conf, 3)}
        if self.bold:
            d["bold"] = True
        return d


@dataclass
class Row:
    page_no: int
    kind: str  # result | pending
    cells: dict[str, Cell]
    words: list[PWord]
    section: str | None = None
    specimen: str | None = None
    notes: list[str] = field(default_factory=list)

    @property
    def bbox(self) -> tuple[float, float, float, float]:
        return union(w.bbox for w in self.words)

    def text(self, kind: str) -> str:
        c = self.cells.get(kind)
        return c.text if c else ""

    def conf(self, prior: float) -> float:
        return round(min(self.cells[k].conf if k in self.cells else 1.0 for k in REQUIRED) * prior, 3)


@dataclass
class Header:
    top: float
    bottom: float
    regions: list[dict[str, float]]  # per table region: column kind -> x0


@dataclass
class PageResult:
    rows: list[Row]
    headers: int
    lines_in_tables: int


# --------------------------------------------------------------- headers ----


def _phrase_hits(words: list[PWord], phrases: tuple[str, ...]) -> list[tuple[int, int]]:
    """(start, end) word index spans where a header phrase appears, longest phrases first, no overlap."""
    folded = [fold(w.text).strip(":") for w in words]
    spans: list[tuple[int, int]] = []
    for phrase in sorted(phrases, key=lambda p: -len(p.split())):
        toks = phrase.split()
        n = len(toks)
        for i in range(len(folded) - n + 1):
            if folded[i : i + n] == toks and not any(i < b and a < i + n for a, b in spans):
                spans.append((i, i + n))
    return sorted(spans)


def find_headers(lines: list[list[PWord]], spec: TableSpec) -> list[tuple[int, Header]]:
    found: list[tuple[int, Header]] = []
    for li, line in enumerate(lines):
        hits: list[tuple[float, str]] = []
        used: set[int] = set()
        for kind in ("ref_range", "name", "value", "unit", "flag"):  # multiword kinds first
            for a, b in _phrase_hits(line, spec.headers.get(kind, ())):
                if used & set(range(a, b)):
                    continue
                used |= set(range(a, b))
                hits.append((line[a].x0, kind))
        kinds = [k for _, k in hits]
        if "name" not in kinds or "value" not in kinds:
            continue
        # every word of a header line is a header word (a result row never is)
        if len(used) < len(line):
            continue
        hits.sort()
        regions: list[dict[str, float]] = []
        for x, kind in hits:
            if kind == "name" or not regions or kind in regions[-1]:
                regions.append({})
            regions[-1][kind] = x
        regions = [r for r in regions if "name" in r and "value" in r]
        if regions:
            found.append((li, Header(min(w.top for w in line), max(w.bottom for w in line), regions)))
    return found


# --------------------------------------------------------------- columns ----


def _assign(words: list[PWord], cols: dict[str, float]) -> dict[str, list[PWord]]:
    order = sorted(cols.items(), key=lambda kv: kv[1])
    out: dict[str, list[PWord]] = {}
    for w in words:
        kind = order[0][0]
        for k, x in order:
            if w.x0 >= x - COL_MARGIN:
                kind = k
        out.setdefault(kind, []).append(w)
    return out


def _straddles(words: list[PWord], kind: str, cols: dict[str, float]) -> bool:
    starts = sorted(x for k, x in cols.items() if x > cols[kind])
    nxt = starts[0] if starts else None
    return nxt is not None and any(w.x1 > nxt - COL_MARGIN + STRADDLE_PT for w in words)


def _text(words: list[PWord]) -> str:
    return " ".join(w.text for w in sorted(words, key=lambda w: (round(w.top), w.x0)))


def is_value(text: str) -> bool:
    f = fold(text).strip()
    return bool(_VALUE_RE.match(f)) or any(f == t or f.startswith(t + " ") for t in TEXT_RESULTS)


def is_pending(text: str) -> bool:
    f = fold(text).strip(" .()")
    return any(f == p or f.startswith(p) for p in PENDING)


def _split_value(words: list[PWord], spec: TableSpec) -> tuple[list, list, list, list]:
    """Split a value cell into (value words, unit words, flag words, spilled name words)."""
    ws = sorted(words, key=lambda w: w.x0)
    flags = []
    while ws and ws[0].text in spec.flag_tokens and len(ws) > 1:
        flags.append(ws.pop(0))
    while ws and ws[-1].text in spec.flag_tokens and len(ws) > 1:
        flags.insert(0, ws.pop())
    # a word before the number that is not part of a value spilled over from the name column
    lead: list[PWord] = []
    while len(ws) > 1 and not is_value(ws[0].text) and is_value(_text(ws[1:])):
        lead.append(ws.pop(0))
    units: list[PWord] = []
    if ws and _VALUE_RE.match(fold(ws[0].text)):
        # comparator split from its number: "<" "0,5"
        head = 1
        if ws[0].text in ("<", ">", "≤", "≥", "<=", ">=") and len(ws) > 1:
            head = 2
        # a flag glued to the number: "6,2*" or "6,2H"
        w = ws[head - 1]
        m = re.match(r"^([<>≤≥]?[-+]?\d[\d.,]*)(\*{1,2}|H|L)$", w.text)
        if m:
            ws[head - 1] = PWord(m.group(1), w.x0, w.top, w.x1, w.bottom, w.font, w.size, w.color, w.bold)
            flags.append(PWord(m.group(2), w.x1, w.top, w.x1, w.bottom, w.font, w.size, w.color, w.bold))
        # digit groups split by a space: "250 000" (kept together, and ambiguous enough for review)
        while head < len(ws) and re.fullmatch(r"\d{3}", ws[head].text):
            head += 1
        rest = ws[head:]
        if rest and not all(_NUMBER_RE.fullmatch(w.text) for w in rest):
            units = rest
            ws = ws[:head]
    return ws, units, flags, lead


def _section_like(words: list[PWord]) -> bool:
    letters = "".join(c for w in words for c in w.text if c.isalpha())
    return (len(letters) >= 4 and letters.isupper()) or all(w.bold for w in words)


def _specimen(section: str | None) -> str | None:
    if not section:
        return None
    f = fold(section)
    for spec, words in SPECIMENS.items():
        if any(re.search(r"(?<![a-z])" + re.escape(w) + r"(?![a-z])", f) for w in words):
            return spec
    return None


# ------------------------------------------------------------------ rows ----


def extract_page(page: PageModel, spec: TableSpec) -> PageResult:
    lines = group_lines(page.visible)
    headers = find_headers(lines, spec)
    rows: list[Row] = []
    in_tables = 0
    for hi, (li, header) in enumerate(headers):
        end = headers[hi + 1][0] if hi + 1 < len(headers) else len(lines)
        body = lines[li + 1 : end]
        starts = [min(r.values()) for r in header.regions]
        for ri, cols in enumerate(header.regions):
            x_lo = starts[ri] - COL_MARGIN if ri > 0 else -1e9
            x_hi = starts[ri + 1] - COL_MARGIN if ri + 1 < len(starts) else 1e9
            segs = [[w for w in line if x_lo <= w.x0 < x_hi] for line in body]
            region_rows, n = _rows_in_region([s for s in segs if s], cols, spec, page.page_no)
            rows.extend(region_rows)
            in_tables += n
    return PageResult(rows=rows, headers=len(headers), lines_in_tables=in_tables)


def _rows_in_region(segs: list[list[PWord]], cols: dict[str, float], spec: TableSpec, page_no: int):
    items: list[tuple[str, list[PWord], dict[str, list[PWord]]]] = []
    last_bottom = None
    pitch = None
    after_gap = False
    for seg in segs:
        txt = fold(_text(seg)).strip()
        if any(txt.startswith(s) for s in spec.stop):
            break
        top = min(w.top for w in seg)
        if last_bottom is not None and pitch and top - last_bottom > 4 * pitch:
            # a wide gap: maybe a footer, maybe more of the table. From here on only lines that read as
            # clean results count, so a footer adds no noise and a late row is still not lost.
            after_gap = True
        if any(txt.startswith(s) for s in spec.skip):
            items.append(("note", seg, {}))
            last_bottom = max(w.bottom for w in seg)
            continue
        parts = _assign(seg, cols)
        value_text = _text(parts.get("value", []))
        has_name = bool(parts.get("name"))
        others = [k for k in parts if k != "name"]
        if has_name and value_text and is_pending(value_text):
            kind = "pending"
        elif has_name and value_text:
            kind = "result"
        elif has_name and not others:
            kind = "name_only"
        elif not has_name and set(others) <= {"ref_range", "unit"}:
            kind = "range_only"
        elif not has_name and value_text:
            kind = "value_only"  # the name sits on the line above (wrapped name, value on the second line)
        else:
            kind = "orphan"
        if after_gap and not (
            kind == "pending" or (kind == "result" and is_value(value_text) and ({"unit", "ref_range"} & set(parts)))
        ):
            continue
        h = max(w.bottom for w in seg) - top
        if last_bottom is not None and kind == "result":
            gap = top - last_bottom
            pitch = gap + h if pitch is None else min(pitch, gap + h)
        items.append((kind, seg, parts))
        last_bottom = max(w.bottom for w in seg)

    rows: list[Row] = []
    section: str | None = None
    pending_prefix: list[PWord] = []
    for i, (kind, seg, parts) in enumerate(items):
        top, bottom = min(w.top for w in seg), max(w.bottom for w in seg)
        h = (bottom - top) or 1.0
        prev = items[i - 1] if i else None
        nxt = items[i + 1] if i + 1 < len(items) else None
        tight_above = prev is not None and top - max(w.bottom for w in prev[1]) < TIGHT * h
        tight_below = nxt is not None and min(w.top for w in nxt[1]) - bottom < TIGHT * h
        if kind == "note":
            if rows:
                rows[-1].notes.append(_text(seg))
            continue
        if kind == "name_only":
            if _section_like(seg) and not (tight_above and prev and prev[0] in ("result", "pending")):
                section = _text(seg)
            elif tight_below and nxt and nxt[0] == "value_only":
                pending_prefix = seg
            elif tight_above and rows and prev and prev[0] in ("result", "pending", "value_only"):
                r = rows[-1]
                r.words.extend(seg)
                old = r.cells["name"]
                r.cells["name"] = Cell(
                    old.text + " " + _text(seg),
                    union([*([old.bbox] if old.bbox else []), union(w.bbox for w in seg)]),
                    min(old.conf, 0.98),
                )
            else:
                section = _text(seg)
            continue
        if kind == "range_only":
            if rows and tight_above:
                r = rows[-1]
                rng = parts.get("ref_range", []) + parts.get("unit", [])
                r.words.extend(seg)
                old = r.cells.get("ref_range")
                r.cells["ref_range"] = Cell(
                    ((old.text + " ") if old else "") + _text(rng),
                    union([*([old.bbox] if old and old.bbox else []), union(w.bbox for w in rng)]),
                    old.conf if old else 1.0,
                )
                continue
            kind = "orphan"
        if kind == "value_only":
            value_text = _text(parts.get("value", []))
            kind = ("pending" if is_pending(value_text) else "result") if pending_prefix else "orphan"
        row = _make_row(kind, seg, parts, cols, spec, page_no, pending_prefix)
        pending_prefix = []
        row.section = section
        row.specimen = _specimen(section)
        rows.append(row)
    return rows, len(items)


def _make_row(kind, seg, parts, cols, spec, page_no, prefix: list[PWord]) -> Row:
    cells: dict[str, Cell] = {}
    words = list(seg) + list(prefix)
    value_words, unit_words, flag_words, spilled = _split_value(parts.get("value", []), spec)
    name_words = list(prefix) + parts.get("name", []) + spilled
    if name_words:
        conf = 0.98 if prefix else 1.0
        if spilled or _straddles(parts.get("name", []), "name", cols):
            conf = min(conf, 0.8)
        cells["name"] = Cell(_text(name_words), union(w.bbox for w in name_words), conf)
    else:
        cells["name"] = Cell("", None, 0.0)
    vtext = _text(value_words)
    if kind == "pending":
        vconf = 1.0
    elif not value_words:
        vconf = 0.0
    elif not is_value(vtext):
        vconf = 0.5
    elif len([w for w in value_words if _NUMBER_RE.search(w.text)]) > 1:
        vconf = 0.6  # two numbers in one value cell: ambiguous
    else:
        vconf = 1.0
    if value_words and _straddles(parts.get("value", []), "value", cols):
        vconf = min(vconf, 0.8)
    if kind == "orphan":
        vconf = min(vconf, 0.5)
    cells["value"] = Cell(
        vtext,
        union(w.bbox for w in value_words) if value_words else None,
        vconf,
        bold=bool(value_words)
        and all(w.bold for w in value_words)
        and not all(w.bold for w in parts.get("name", []) or [PWord("", 0, 0, 0, 0)]),
    )
    unit_col = parts.get("unit", [])
    if unit_col and unit_words:
        ucell = Cell(_text(unit_col), union(w.bbox for w in unit_col), 0.7)  # unit printed twice
    elif unit_col:
        ucell = Cell(_text(unit_col), union(w.bbox for w in unit_col), 1.0)
    elif unit_words:
        ucell = Cell(_text(unit_words), union(w.bbox for w in unit_words), 1.0)
    else:
        numeric = bool(value_words) and bool(_VALUE_RE.match(fold(vtext)))
        ucell = Cell("", None, 0.97 if numeric else 1.0)
    cells["unit"] = ucell
    rng = sorted(parts.get("ref_range", []), key=lambda w: w.x0)
    flag_words = list(flag_words)
    # an unheaded flag column right of the range: "70 - 110      H"
    if "flag" not in cols and len(rng) > 1 and rng[-1].text in spec.flag_tokens and rng[-1].x0 - rng[-2].x1 > FLAG_GAP:
        flag_words.append(rng.pop())
    if rng:
        cells["ref_range"] = Cell(_text(rng), union(w.bbox for w in rng), 1.0)
    flags = parts.get("flag", []) + flag_words
    if flags:
        cells["flag"] = Cell(" ".join(w.text for w in flags), union(w.bbox for w in flags), 1.0)
    return Row(page_no=page_no, kind="pending" if kind == "pending" else "result", cells=cells, words=words)
