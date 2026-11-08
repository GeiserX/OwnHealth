"""Per-token identity modes (mcp.md 9.6), applied to the rendered response after `state_hash`.

| Mode         | Names and IDs                          | Providers       | Dates               | Free text |
|--------------|----------------------------------------|-----------------|---------------------|-----------|
| full         | as stored                              | as stored       | exact               | wrapped   |
| pseudonymous | slug; detectors scrub IDs, phones, ... | role ("lab")    | exact               | scrubbed  |
| minimal      | as pseudonymous                        | as pseudonymous | relative to as_of   | dropped   |

Structured values (numbers, units, codes, ranges) are never touched, so trends survive every mode.
"""

from __future__ import annotations

import re
from datetime import date, datetime

from ownhealth.mcp.envelope import is_untrusted

MODES = ("full", "pseudonymous", "minimal")
STRICTNESS = {m: i for i, m in enumerate(MODES)}

DETECTORS: tuple[tuple[str, re.Pattern], ...] = (
    ("email", re.compile(r"[\w.+-]+@[\w-]+\.[\w.-]+")),
    ("iban", re.compile(r"\b[A-Z]{2}\d{2}(?:\s?[A-Z0-9]{4}){3,7}\b")),
    ("nie", re.compile(r"\b[XYZ]-?\d{7}-?[A-Z]\b")),
    ("dni", re.compile(r"\b\d{8}-?[A-Z]\b")),
    ("ssn", re.compile(r"\b\d{3}-\d{2}-\d{4}\b")),
    ("nhs", re.compile(r"\b\d{3}\s\d{3}\s\d{4}\b")),
    ("phone", re.compile(r"(?<![\w.])\+?\d[\d\s().-]{7,}\d(?![\w.])")),
)
PROVIDER_KEYS = {"provider", "provider_name", "lab", "clinician", "performer"}
DROP_KEYS_PSEUDO = {"display_name", "patient_name", "address", "phone", "email", "mrn", "national_id"}
DATE_KEYS = {"date", "since", "until", "last", "on", "due", "started", "stopped", "effective", "accepted_at", "updated"}
ISO_DATE = re.compile(r"^\d{4}-\d{2}-\d{2}(?:[T ][\d:.]+(?:Z|[+-]\d{2}:?\d{2})?)?$")


def stricter(token_mode: str, asked: str | None) -> str:
    """A call may only ask for a stricter mode than its token's."""
    if asked in STRICTNESS and STRICTNESS[asked] > STRICTNESS.get(token_mode, 1):
        return asked
    return token_mode if token_mode in STRICTNESS else "pseudonymous"


def scrub(text: str, names: list[str]) -> str:
    for label, pattern in DETECTORS:
        if label == "phone":  # 9+ digits, so dates and lab values stay
            text = pattern.sub(lambda m: "[phone]" if sum(c.isdigit() for c in m.group()) >= 9 else m.group(), text)
        else:
            text = pattern.sub(f"[{label}]", text)
    for name in names:
        if name:
            text = re.sub(rf"(?<!\w){re.escape(name)}(?!\w)", "[name]", text, flags=re.IGNORECASE)
    return text


def name_dictionary(full_names: list[str]) -> list[str]:
    """Full names and each part of 3+ letters, longest first, so 'Alex Rivera' goes before 'Alex'."""
    parts: set[str] = set()
    for n in full_names:
        if not n:
            continue
        parts.add(n)
        parts.update(p for p in re.split(r"\s+", n) if len(p) >= 3)
    return sorted(parts, key=len, reverse=True)


def relative(value: str, as_of: date) -> str:
    try:
        d = datetime.fromisoformat(value.replace("Z", "+00:00")).date() if "T" in value else date.fromisoformat(value)
    except ValueError:
        return value
    days = (as_of - d).days
    if days == 0:
        return "same day as as_of"
    return f"{days} days before as_of" if days > 0 else f"{-days} days after as_of"


def age_band(age: int | None) -> str | None:
    if age is None:
        return None
    if age < 18:
        return "0-17"
    lo = (age // 10) * 10
    return f"{lo}-{lo + 9}"


def apply(data, mode: str, *, as_of: date, names: list[str]):
    """A redacted copy of `data` (the envelope's data block or any part of a response)."""
    if mode == "full":
        return data

    def go(node, key=None):
        if is_untrusted(node):
            if mode == "minimal":
                return None
            return {**node, "text": scrub(node["text"], names) if isinstance(node["text"], str) else node["text"]}
        if isinstance(node, dict):
            out = {}
            for k, v in node.items():
                if k in DROP_KEYS_PSEUDO:
                    continue
                if k in PROVIDER_KEYS and v is not None and not isinstance(v, list | int | float | bool):
                    out[k] = "lab" if k in ("lab", "provider", "provider_name") else "clinician"
                    continue
                out[k] = go(v, k)
            return out
        if isinstance(node, list):
            return [go(v, key) for v in node]
        if mode == "minimal" and isinstance(node, str) and key in DATE_KEYS and ISO_DATE.match(node):
            return relative(node, as_of)
        return node

    return go(data)
