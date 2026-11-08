"""Wording lint: information, never advice. Finds imperative or prescriptive phrases (en, es) in any
output we write ourselves. Verbatim CPIC quotes (`quote`) and wrapped untrusted text (person or agent
notes, document text) are not ours and are skipped by default; everything else must have 0 hits.
"""

from __future__ import annotations

import re

# Prescriptive anywhere in a sentence.
ANYWHERE = [
    r"\byou (?:should|must|need to|have to|ought to)\b",
    r"\b(?:should|must)\b",
    r"\bdo not\b",
    r"\bdon'?t\b",
    r"\bnever (?:take|use|stop|start)\b",
    r"\bdeb(?:e|es|en|ería|erías|erían)\b",
    r"\b(?:tiene|tienes|tienen) que\b",
    r"\bhay que\b",
    r"\bno (?:tome|tomes|tomen|use|uses|deje|dejes)\b",
]
# Imperative when a sentence starts with it.
START_VERBS = [
    "take",
    "stop",
    "start",
    "avoid",
    "increase",
    "decrease",
    "reduce",
    "lower",
    "raise",
    "switch",
    "consider",
    "use",
    "skip",
    "discontinue",
    "ask",
    "see",
    "talk",
    "call",
    "discuss",
    "check",
    "recheck",
    "retest",
    "get",
    "keep",
    "continue",
    "try",
    "eat",
    "drink",
    "limit",
    "add",
    "remove",
    "double",
    "halve",
    "measure",
    "schedule",
    "book",
    "contact",
    "go",
    "tome",
    "toma",
    "tomen",
    "deje",
    "deja",
    "evite",
    "evita",
    "suspenda",
    "suspende",
    "empiece",
    "empieza",
    "aumente",
    "aumenta",
    "reduzca",
    "reduce",
    "baje",
    "cambie",
    "cambia",
    "use",
    "usa",
    "consulte",
    "consulta",
    "pregunte",
    "pregunta",
    "hable",
    "habla",
    "coméntalo",
    "comente",
    "comenta",
    "repita",
    "repite",
    "mida",
    "mide",
    "pida",
    "pide",
    "vaya",
    "acuda",
    "llame",
]
_ANY = re.compile("|".join(ANYWHERE), re.IGNORECASE)
_START = re.compile(r"^(?:" + "|".join(START_VERBS) + r")\b", re.IGNORECASE)
_SPLIT = re.compile(r"(?:[.;:!?·()\n]|\s[-–—]\s)+")
SKIP_KEYS = frozenset({"quote"})


def imperative_phrases(text: str) -> list[str]:
    hits = [m.group(0) for m in _ANY.finditer(text)]
    for sentence in _SPLIT.split(text):
        s = sentence.strip().lstrip("\"'¿¡«“ ").strip()
        m = _START.match(s)
        if m:
            hits.append(m.group(0))
    return hits


def lint(obj, *, skip_keys: frozenset[str] = SKIP_KEYS, path: str = "$") -> list[tuple[str, str]]:
    """Every (json path, phrase) hit in the strings of `obj`."""
    out: list[tuple[str, str]] = []
    if isinstance(obj, str):
        out += [(path, h) for h in imperative_phrases(obj)]
    elif isinstance(obj, dict):
        if obj.get("trust") == "untrusted_text" and skip_keys:
            return out
        for k, v in obj.items():
            if k not in skip_keys:
                out += lint(v, skip_keys=skip_keys, path=f"{path}.{k}")
    elif isinstance(obj, list | tuple):
        for i, v in enumerate(obj):
            out += lint(v, skip_keys=skip_keys, path=f"{path}[{i}]")
    return out
