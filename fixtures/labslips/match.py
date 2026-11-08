"""Match extracted rows against ground truth: row F1 and dropped rows (shared by eval.py and the tests)."""

from __future__ import annotations

import unicodedata
from dataclasses import dataclass, field


def norm(s: str | None) -> str:
    s = unicodedata.normalize("NFKD", s or "")
    s = "".join(c for c in s if not unicodedata.combining(c)).lower()
    return " ".join(s.replace("µ", "u").replace("μ", "u").split())


def key(page: int, name: str, value: str) -> tuple:
    return (page, norm(name), norm(value))


def inside(inner: list[float], outer: list[float] | tuple, pad: float = 4.0) -> bool:
    cx, cy = (inner[0] + inner[2]) / 2, (inner[1] + inner[3]) / 2
    return outer[0] - pad <= cx <= outer[2] + pad and outer[1] - pad <= cy <= outer[3] + pad


@dataclass
class SlipScore:
    slip: str
    truth: int
    extracted: int
    tp: int
    unit_ok: int
    missing: list[tuple] = field(default_factory=list)
    extra: list[tuple] = field(default_factory=list)
    bbox_ok: int = 0


def score_rows(slip: str, truth_rows: list[dict], extracted: list[dict]) -> SlipScore:
    """extracted: [{page, name, value, unit, bbox}]. A truth row matches on page, name and value."""
    want = {}
    for t in truth_rows:
        want.setdefault(key(t["page"], t["name"], t["value"]), []).append(t)
    s = SlipScore(slip, len(truth_rows), len(extracted), 0, 0)
    for e in extracted:
        k = key(e["page"], e["name"], e["value"])
        if want.get(k):
            t = want[k].pop()
            s.tp += 1
            if norm(e.get("unit")) == norm(t["unit"]):
                s.unit_ok += 1
            if e.get("bbox") and inside(t["bbox"], e["bbox"]) and inside(e["bbox"], t["bbox"]):
                s.bbox_ok += 1
        else:
            s.extra.append(k + (e.get("unit"),))
    s.missing = [k for k, v in want.items() for _ in v]
    return s


def f1(scores: list[SlipScore]) -> float:
    tp = sum(s.tp for s in scores)
    n_truth = sum(s.truth for s in scores)
    n_ext = sum(s.extracted for s in scores)
    if not tp:
        return 0.0
    p, r = tp / n_ext, tp / n_truth
    return 2 * p * r / (p + r)
