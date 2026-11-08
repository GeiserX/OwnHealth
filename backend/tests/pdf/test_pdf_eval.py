"""The v0.1 lab-import exit criteria on the whole synthetic eval set (ROADMAP v0.1, ingestion.md 13).

Two runs: with the catalog stand-in normalizer (lane B's pipeline alone; every gate strict), and with
lane A's real normalizer (the system as it ships; no false auto-accept allowed).
"""

from __future__ import annotations

import evaluate
import fake_normalize

from ownhealth import normalize


def _run(conn, blobs, slips):
    results = evaluate.evaluate(conn, blobs, [(n, pdf, t) for n, (pdf, t) in slips.items()])
    return results, evaluate.totals(results)


def _labnorte(results):
    return next(r for r in results if r.slip == "es-labnorte-3p")


def test_exit_criteria_with_the_stand_in_normalizer(conn, blobs, slips, monkeypatch):
    monkeypatch.setattr(normalize, "normalize_row", fake_normalize.normalize_row)
    results, tot = _run(conn, blobs, slips)
    assert tot["row_f1"] >= 0.97
    assert tot["false_auto_accepts"] == 0, [r.false_auto for r in results if r.false_auto]
    assert tot["dropped"] == 0, [r.dropped for r in results if r.dropped]
    assert tot["auto_with_page_bbox"] == tot["auto_accepted"]
    ln = _labnorte(results)
    assert ln.auto / ln.truth >= 0.98 and ln.auto_with_bbox == ln.auto


def test_exit_criteria_with_lane_a(conn, blobs, slips):
    results, tot = _run(conn, blobs, slips)
    assert tot["row_f1"] >= 0.97 and tot["dropped"] == 0
    assert tot["false_auto_accepts"] == 0, [(r.slip, r.false_auto) for r in results if r.false_auto]
    assert tot["auto_with_page_bbox"] == tot["auto_accepted"]
    ln = _labnorte(results)
    assert ln.auto / ln.truth >= 0.98 and ln.auto_with_bbox == ln.auto
