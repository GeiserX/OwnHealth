"""Vendor templates: YAML files in `vendors/<country>/<provider>.yaml`, reviewed like code.

A template names the provider, its fingerprint (words that must and may appear on the first page),
the header words of each column, the value locale, and optional flag, stop and skip words. It holds
anchors only: never values, never a patient block. A template whose fingerprint scores >= 0.8 reads
the document with prior 1.0; otherwise the generic rules read it with prior 0.95.

    id: es/labnorte
    version: "1"
    provider_name: LabNorte
    locale: es
    fingerprint: {all: [labnorte], any: [prueba, resultado, unidades, valores de referencia]}
    columns: {name: [prueba], value: [resultado], unit: [unidades], ref_range: [valores de referencia]}
"""

from __future__ import annotations

import hashlib
import re
from dataclasses import dataclass
from functools import lru_cache
from pathlib import Path

import yaml

from ownhealth.ingest.classify import fold
from ownhealth.ingest.pdf.tables import FLAG_TOKENS, SKIP_PREFIXES, STOP_PREFIXES, TableSpec

VENDORS_DIR = Path(__file__).resolve().parent / "vendors"
MIN_SCORE = 0.8


@dataclass(frozen=True)
class Template:
    id: str
    version: str
    provider_name: str
    locale: str
    all_words: tuple[str, ...]
    any_words: tuple[str, ...]
    spec: TableSpec
    sha256: str

    def score(self, text: str) -> float:
        f = fold(text)

        def has(w: str) -> bool:
            return re.search(r"(?<![a-z0-9])" + re.escape(fold(w)) + r"(?![a-z0-9])", f) is not None

        if not all(has(w) for w in self.all_words):
            return 0.0
        if not self.any_words:
            return 1.0
        return sum(1 for w in self.any_words if has(w)) / len(self.any_words)


def parse_template(raw: str) -> Template:
    d = yaml.safe_load(raw)
    for key in ("id", "version", "provider_name", "locale", "fingerprint", "columns"):
        if key not in d:
            raise ValueError(f"template missing {key}")
    cols = {k: tuple(fold(x) for x in v) for k, v in d["columns"].items()}
    if not cols.get("name") or not cols.get("value"):
        raise ValueError("a template needs name and value header words")
    spec = TableSpec(
        extractor=f"template:{d['id']}",
        version=str(d["version"]),
        prior=1.0,
        headers=cols,
        flag_tokens=frozenset(d.get("flag_tokens") or FLAG_TOKENS),
        stop=tuple(fold(x) for x in d.get("stop", [])) + STOP_PREFIXES,
        skip=tuple(fold(x) for x in d.get("skip", [])) + SKIP_PREFIXES,
        locale=d["locale"],
    )
    fp = d["fingerprint"]
    return Template(
        id=d["id"],
        version=str(d["version"]),
        provider_name=d["provider_name"],
        locale=d["locale"],
        all_words=tuple(fp.get("all", [])),
        any_words=tuple(fp.get("any", [])),
        spec=spec,
        sha256=hashlib.sha256(raw.encode()).hexdigest(),
    )


@lru_cache(maxsize=1)
def load_templates(directory: Path = VENDORS_DIR) -> tuple[Template, ...]:
    out = [parse_template(p.read_text(encoding="utf-8")) for p in sorted(directory.glob("*/*.yaml"))]
    ids = [t.id for t in out]
    if len(set(ids)) != len(ids):
        raise ValueError(f"duplicate template ids in {directory}")
    return tuple(out)


def match_template(
    first_page_text: str, templates: tuple[Template, ...] | None = None
) -> tuple[Template | None, float]:
    best, score = None, 0.0
    for t in templates if templates is not None else load_templates():
        s = t.score(first_page_text)
        if s > score:
            best, score = t, s
    return (best, score) if score >= MIN_SCORE else (None, score)
