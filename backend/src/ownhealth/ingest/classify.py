"""Rule-based document classifier (ingestion.md 5). No model.

From the text lines of a document's first pages it guesses the document type, the language, the
patient block (name, birth date, sample id), the collection and report dates, and the report
status. Every guess is shown to the person, who confirms or corrects it; nothing here is a fact.
"""

from __future__ import annotations

import re
import unicodedata
from dataclasses import dataclass, field
from datetime import date, datetime, time

DOC_TYPES = ("lab_report", "imaging_report", "discharge", "prescription", "genomics", "other")

_TYPE_WORDS: dict[str, tuple[str, ...]] = {
    "lab_report": (
        "hemograma",
        "bioquimica",
        "laboratorio",
        "valores de referencia",
        "intervalo de referencia",
        "laboratory",
        "lab results",
        "reference range",
        "reference interval",
        "haematology",
        "hematology",
        "laborbefund",
        "referenzbereich",
        "normbereich",
        "blutbild",
        "klinische chemie",
        "laboratoire",
        "valeurs de reference",
        "valeurs usuelles",
        "biochimie",
        "hematologie",
        "analyses",
    ),
    "imaging_report": (
        "radiologia",
        "ecografia",
        "resonancia",
        "tomografia",
        "radiografia",
        "informe radiologico",
        "radiology",
        "ultrasound",
        "mri",
        "ct scan",
        "x-ray",
        "rontgen",
        "sonographie",
        "mrt",
        "radiologie",
        "echographie",
        "irm",
        "scanner",
        "impression:",
        "hallazgos",
        "findings",
    ),
    "discharge": (
        "informe de alta",
        "alta hospitalaria",
        "discharge summary",
        "discharge letter",
        "entlassungsbericht",
        "entlassbrief",
        "compte rendu d'hospitalisation",
        "lettre de sortie",
        "fecha de ingreso",
        "admission",
    ),
    "prescription": (
        "receta",
        "prescripcion",
        "posologia",
        "prescription",
        "rx",
        "dosage",
        "rezept",
        "verordnung",
        "ordonnance",
        "posologie",
    ),
    "genomics": (
        "genotipo",
        "genomica",
        "farmacogenetica",
        "genotype",
        "pharmacogenomic",
        "pharmacogenetic",
        "rsid",
        "vcf",
        "genetik",
        "genotyp",
        "genetique",
        "sequencage",
        "haplotype",
        "diplotype",
    ),
}

_LANG_WORDS: dict[str, tuple[str, ...]] = {
    "es": (
        "de",
        "la",
        "el",
        "los",
        "las",
        "del",
        "y",
        "resultado",
        "paciente",
        "fecha",
        "unidades",
        "prueba",
        "valores",
        "referencia",
        "informe",
        "muestra",
    ),
    "en": (
        "the",
        "and",
        "of",
        "result",
        "results",
        "patient",
        "date",
        "units",
        "test",
        "reference",
        "range",
        "report",
        "sample",
        "collected",
    ),
    "de": (
        "der",
        "die",
        "das",
        "und",
        "ergebnis",
        "einheit",
        "datum",
        "untersuchung",
        "referenzbereich",
        "befund",
        "seite",
        "geb",
    ),
    "fr": (
        "le",
        "les",
        "des",
        "et",
        "du",
        "resultat",
        "unite",
        "valeurs",
        "analyse",
        "patient",
        "date",
        "prelevement",
        "naissance",
        "page",
    ),
}

_LABELS: dict[str, tuple[str, ...]] = {
    "name": ("paciente", "nombre", "patient(in)", "patientin", "patient(e)", "patient", "name", "nom"),
    "birth": (
        "f. nacimiento",
        "fecha de nacimiento",
        "f. nac.",
        "fecha nac.",
        "date of birth",
        "dob",
        "born",
        "geb.-datum",
        "geburtsdatum",
        "geb.",
        "ne(e) le",
        "nee le",
        "ne le",
        "date de naissance",
    ),
    "sample": (
        "n.o muestra",
        "no muestra",
        "n.o de muestra",
        "muestra",
        "sample id",
        "specimen id",
        "sample",
        "accession",
        "probennummer",
        "probe-nr.",
        "auftragsnr.",
        "auftrag",
        "no echantillon",
        "echantillon",
        "dossier",
        "informe",
        "report id",
        "report",
        "befund-nr.",
    ),
    "collected": (
        "fecha de toma",
        "fecha de extraccion",
        "fecha extraccion",
        "f. extraccion",
        "fecha toma",
        "collected",
        "collection date",
        "date collected",
        "entnahmedatum",
        "entnahme",
        "abnahme",
        "date de prelevement",
        "preleve le",
        "prelevement",
    ),
    "reported": (
        "fecha de informe",
        "fecha de emision",
        "fecha validacion",
        "fecha de validacion",
        "reported",
        "report date",
        "befunddatum",
        "ausgestellt",
        "edite le",
        "date d'edition",
        "date du compte rendu",
    ),
}

_STATUS: dict[str, tuple[str, ...]] = {
    "amended": (
        "rectificado",
        "informe corregido",
        "corregido",
        "amended",
        "corrected report",
        "korrigiert",
        "berichtigt",
        "rectifie",
        "compte rendu modifie",
    ),
    "preliminary": ("provisional", "preliminar", "preliminary", "vorlaufig", "provisoire", "partiel"),
}

_MONTHS = {
    "jan": 1,
    "january": 1,
    "enero": 1,
    "ene": 1,
    "januar": 1,
    "janvier": 1,
    "janv": 1,
    "feb": 2,
    "february": 2,
    "febrero": 2,
    "februar": 2,
    "fevrier": 2,
    "fevr": 2,
    "mar": 3,
    "march": 3,
    "marzo": 3,
    "marz": 3,
    "mars": 3,
    "apr": 4,
    "april": 4,
    "abril": 4,
    "abr": 4,
    "avril": 4,
    "avr": 4,
    "may": 5,
    "mayo": 5,
    "mai": 5,
    "jun": 6,
    "june": 6,
    "junio": 6,
    "juni": 6,
    "juin": 6,
    "jul": 7,
    "july": 7,
    "julio": 7,
    "juli": 7,
    "juillet": 7,
    "juil": 7,
    "aug": 8,
    "august": 8,
    "agosto": 8,
    "ago": 8,
    "aout": 8,
    "sep": 9,
    "sept": 9,
    "september": 9,
    "septiembre": 9,
    "septembre": 9,
    "oct": 10,
    "october": 10,
    "octubre": 10,
    "oktober": 10,
    "octobre": 10,
    "okt": 10,
    "nov": 11,
    "november": 11,
    "noviembre": 11,
    "novembre": 11,
    "dec": 12,
    "december": 12,
    "diciembre": 12,
    "dic": 12,
    "dezember": 12,
    "decembre": 12,
    "dez": 12,
}

_DATE_NUM = re.compile(r"\b(\d{1,2})[./-](\d{1,2})[./-](\d{4})\b|\b(\d{4})-(\d{2})-(\d{2})\b")
_DATE_WORD = re.compile(r"\b(\d{1,2})\.?\s+([a-z]{3,10})\.?\s+(\d{4})\b|\b([a-z]{3,10})\.?\s+(\d{1,2}),\s*(\d{4})\b")
_TIME = re.compile(r"\b([01]?\d|2[0-3])[:h]([0-5]\d)\b")


def fold(s: str) -> str:
    """Lowercase, strip accents, normalise the ordinal sign: for matching only."""
    s = s.replace("º", "o").replace("°", "o").replace("ª", "a")
    s = unicodedata.normalize("NFKD", s)
    return "".join(c for c in s if not unicodedata.combining(c)).lower()


def parse_date(s: str, lang: str | None = None) -> date | None:
    f = fold(s)
    m = _DATE_NUM.search(f)
    if m:
        try:
            if m.group(4):
                return date(int(m.group(4)), int(m.group(5)), int(m.group(6)))
            a, b, y = int(m.group(1)), int(m.group(2)), int(m.group(3))
            return date(y, b, a)  # day first: es, de, fr and UK English; US layouts print ISO or month names
        except ValueError:
            return None
    m = _DATE_WORD.search(f)
    if m:
        try:
            if m.group(1):
                mon = _MONTHS.get(m.group(2))
                return date(int(m.group(3)), mon, int(m.group(1))) if mon else None
            mon = _MONTHS.get(m.group(4))
            return date(int(m.group(6)), mon, int(m.group(5))) if mon else None
        except ValueError:
            return None
    return None


def parse_datetime(s: str, lang: str | None = None) -> datetime | None:
    d = parse_date(s, lang)
    if d is None:
        return None
    rest = _DATE_NUM.sub(" ", fold(s))
    m = _TIME.search(rest)
    t = time(int(m.group(1)), int(m.group(2))) if m else time(0, 0)
    return datetime.combine(d, t)


@dataclass
class Classification:
    doc_type: str = "other"
    language: str | None = None
    scores: dict[str, int] = field(default_factory=dict)
    patient_name: str | None = None
    birth_date: date | None = None
    sample_id: str | None = None
    collected_at: datetime | None = None
    reported_at: datetime | None = None
    report_status: str = "final"


def _all_labels() -> list[tuple[str, str]]:
    out = [(kind, lab) for kind, labs in _LABELS.items() for lab in labs]
    return sorted(out, key=lambda kl: -len(kl[1]))


def _label_values(line: str) -> dict[str, str]:
    """`Paciente: RIVERA, ALEX  F. nacimiento: 14/05/1979` -> {name: 'RIVERA, ALEX', birth: '14/05/1979'}."""
    f = fold(line)
    hits: list[tuple[int, int, str]] = []
    taken: list[tuple[int, int]] = []
    for kind, lab in _all_labels():
        for m in re.finditer(r"(?<![a-z])" + re.escape(lab) + r"(?![a-z])\s*:?", f):
            a, b = m.span()
            if any(a < y and x < b for x, y in taken):
                continue
            # a label needs a colon after it, or must start the line, to count (avoids "Patient" in prose)
            if ":" not in f[a:b] and a != 0:
                continue
            taken.append((a, b))
            hits.append((a, b, kind))
    hits.sort()
    out: dict[str, str] = {}
    for i, (_a, b, kind) in enumerate(hits):
        end = hits[i + 1][0] if i + 1 < len(hits) else len(line)
        value = line[b:end].strip(" :|;,\t")
        if value and kind not in out:
            out[kind] = value
    return out


def classify(lines: list[str]) -> Classification:
    """Guess type, language, patient block, dates and status from the first pages' text lines."""
    c = Classification()
    folded = [fold(x) for x in lines]
    blob = "\n".join(folded)
    tokens = re.findall(r"[a-z]+", blob)
    lang_scores = {lang: sum(1 for t in tokens if t in words) for lang, words in _LANG_WORDS.items()}
    if any(lang_scores.values()):
        c.language = max(lang_scores, key=lambda k: lang_scores[k])
    for t, words in _TYPE_WORDS.items():
        c.scores[t] = sum(len(re.findall(r"(?<![a-z])" + re.escape(w) + r"(?![a-z])", blob)) for w in words)
    best = max(c.scores, key=lambda k: c.scores[k])
    c.doc_type = best if c.scores[best] > 0 else "other"
    for line in lines:
        for kind, value in _label_values(line).items():
            if kind == "name" and c.patient_name is None and re.search(r"[A-Za-zÀ-ÿ]{2}", value):
                c.patient_name = value
            elif kind == "birth" and c.birth_date is None:
                c.birth_date = parse_date(value)
            elif kind == "sample" and c.sample_id is None:
                tok = value.split()[0] if value.split() else ""
                if re.search(r"\d", tok):
                    c.sample_id = tok
            elif kind == "collected" and c.collected_at is None:
                c.collected_at = parse_datetime(value)
            elif kind == "reported" and c.reported_at is None:
                c.reported_at = parse_datetime(value)
    for status, words in _STATUS.items():
        if any(re.search(r"(?<![a-z])" + re.escape(w), blob) for w in words):
            c.report_status = status
            break
    return c


# ------------------------------------------------------------------ identity ----


def name_tokens(name: str) -> set[str]:
    return {t for t in re.findall(r"[a-z]+", fold(name)) if len(t) > 1}


def identity_match(
    page_name: str | None, page_birth: date | None, profile_name: str, profile_birth: date | None
) -> str:
    """matched | mismatch | missing | hinted (name or birth date alone matched, the other absent)."""
    if not page_name and not page_birth:
        return "missing"
    name_ok = None
    if page_name:
        got, want = name_tokens(page_name), name_tokens(profile_name)
        name_ok = bool(want) and want <= got
    birth_ok = None
    if page_birth and profile_birth:
        birth_ok = page_birth == profile_birth
    if name_ok is False or birth_ok is False:
        return "mismatch"
    if name_ok and birth_ok:
        return "matched"
    return "hinted"
