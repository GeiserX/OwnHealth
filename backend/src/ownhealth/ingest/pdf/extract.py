"""PDF bytes -> classification and candidate rows, with no database (the eval script uses this too).

Order per page (ingestion.md 7): the vendor template when its fingerprint matched and the page has
its header, the generic table rules otherwise. A page without a usable text layer is `pending`:
no OCR in v0.1, it waits for agent-assisted extraction or manual entry.
"""

from __future__ import annotations

import hashlib
from dataclasses import dataclass, field

from ownhealth.ingest.classify import Classification, classify
from ownhealth.ingest.pdf.tables import GENERIC, Row, TableSpec, extract_page
from ownhealth.ingest.pdf.templates import Template, match_template
from ownhealth.ingest.pdf.textlayer import ENGINE, ENGINE_VERSION, PdfModel, read_pdf

CODE_VERSION = "b-1"  # bump when extraction code changes what it reads; part of the run's config hash


@dataclass
class PageRows:
    page_no: int
    spec: TableSpec
    rows: list[Row]


@dataclass
class Extraction:
    pdf: PdfModel
    cls: Classification
    template: Template | None
    template_score: float
    pages: list[PageRows] = field(default_factory=list)
    pending_pages: list[int] = field(default_factory=list)  # no usable text layer
    no_table_pages: list[int] = field(default_factory=list)  # text layer read, no lab table found

    @property
    def extractor(self) -> str:
        return self.template.spec.extractor if self.template else GENERIC.extractor

    @property
    def extractor_version(self) -> str:
        return self.template.version if self.template else GENERIC.version

    @property
    def config_hash(self) -> bytes:
        key = f"{CODE_VERSION}|{ENGINE}{ENGINE_VERSION}|{self.template.sha256 if self.template else 'generic'}"
        return hashlib.sha256(key.encode()).digest()

    @property
    def locale(self) -> str:
        if self.template:
            return self.template.locale
        return self.cls.language if self.cls.language in ("es", "en", "de", "fr") else "en"

    def results(self) -> list[tuple[TableSpec, Row]]:
        return [(p.spec, r) for p in self.pages for r in p.rows if r.kind == "result"]

    def pending(self) -> list[Row]:
        return [r for p in self.pages for r in p.rows if r.kind == "pending"]


def extract(data: bytes) -> Extraction:
    pdf = read_pdf(data)
    head_lines = [line for p in pdf.pages[:2] for line in p.text(visible_only=True).splitlines()]
    cls = classify(head_lines)
    template, score = match_template("\n".join(head_lines))
    if template and cls.language is None:
        cls.language = template.locale
    ext = Extraction(pdf=pdf, cls=cls, template=template, template_score=round(score, 3))
    for page in pdf.pages:
        if not page.text_layer_ok:
            ext.pending_pages.append(page.page_no)
            continue
        spec = template.spec if template else GENERIC
        result = extract_page(page, spec)
        if template and result.headers == 0:
            spec, result = GENERIC, extract_page(page, GENERIC)
        if result.headers == 0:
            ext.no_table_pages.append(page.page_no)
        ext.pages.append(PageRows(page.page_no, spec, result.rows))
    if any(p.rows for p in ext.pages) and cls.doc_type == "other":
        cls.doc_type = "lab_report"
    return ext
