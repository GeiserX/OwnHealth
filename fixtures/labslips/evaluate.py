"""Eval of the lab PDF import on the synthetic set (ingestion.md 13; ROADMAP v0.1 exit criteria).

    uv run --project backend python fixtures/labslips/evaluate.py [--database-url URL] [--fake-normalizer]

Each slip is imported end to end (`ingest.pdf.import_pdf`) into its own synthetic household, in a
throwaway database that is dropped afterwards. Reported per slip and in total:

  row F1            extracted candidate rows vs ground truth, matched on page, printed name and value
  auto-accept rate  truth rows committed without a click / truth rows
  false auto-accept auto-accepted rows whose value, unit or LOINC code differ from truth (gate: 0)
  dropped           truth rows that reached neither the record nor the review queue (gate: 0)
  page+bbox         auto-accepted rows whose src has the page and a bbox around the truth row

The normalizer is lane A's; while that is a contract stub (or with --fake-normalizer) the stand-in
in `fake_normalize.py` is used and the report says so.
"""

from __future__ import annotations

import argparse
import contextlib
import json
import secrets
import sys
from dataclasses import dataclass, field
from decimal import Decimal
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
import match  # noqa: E402
from sqlalchemy import Connection, text  # noqa: E402

from ownhealth.blobs import BlobStore  # noqa: E402
from ownhealth.core.documents import create_document  # noqa: E402
from ownhealth.demo.family import create_rivera_family  # noqa: E402
from ownhealth.ingest.pdf import import_pdf  # noqa: E402


@dataclass
class SlipResult:
    slip: str
    truth: int
    extracted: int
    tp: int
    auto: int
    false_auto: list[str] = field(default_factory=list)
    dropped: list[str] = field(default_factory=list)
    auto_with_bbox: int = 0
    pending_items: int = 0
    summary: dict = field(default_factory=dict)


def _family_key(slip: str) -> str:
    return slip.rsplit("-amend-", 1)[0] + "-amend" if "-amend-" in slip else slip


def import_slip(conn: Connection, blobs: BlobStore, family, name: str, pdf: bytes, truth: dict):
    sha = blobs.put_bytes(pdf)
    doc = create_document(
        conn,
        household_id=family.household_id,
        profile_id=family.profiles[truth["person"]],
        blob_sha256=sha,
        mime="application/pdf",
        size_bytes=len(pdf),
        filename=f"{name}.pdf",
    )
    summary = import_pdf(conn, blobs, document_id=doc.id)
    return doc.id, summary


def score_slip(conn: Connection, name: str, truth: dict, document_id, summary) -> SlipResult:
    rows = (
        conn.execute(
            text(
                "select c.page_no, c.bbox, c.cells, c.state, o.value_num, o.unit_ucum, o.loinc, o.accepted_by,"
                " s.page_no as src_page, s.bbox as src_bbox from candidate_row c join extraction_run r"
                " on r.id = c.extraction_run_id left join observation o on o.id = c.observation_id"
                " left join src s on s.id = o.src_id where r.document_id = :d"
            ),
            {"d": document_id},
        )
        .mappings()
        .all()
    )
    extracted = [
        {
            "page": r["page_no"],
            "name": r["cells"]["name"]["text"],
            "value": r["cells"]["value"]["text"],
            "unit": r["cells"].get("unit", {}).get("text", ""),
            "bbox": list(r["bbox"]),
            "row": r,
        }
        for r in rows
    ]
    s = match.score_rows(name, truth["rows"], extracted)
    res = SlipResult(name, len(truth["rows"]), len(extracted), s.tp, 0, summary=summary.as_dict())
    by_key = {}
    for t in truth["rows"]:
        by_key.setdefault(match.key(t["page"], t["name"], t["value"]), []).append(t)
    for e in extracted:
        r = e["row"]
        if r["state"] != "auto_accepted":
            continue
        res.auto += 1
        cands = by_key.get(match.key(e["page"], e["name"], e["value"]))
        t = cands[0] if cands else None
        label = f"p{e['page']} {e['name']}={e['value']}"
        if t is None:
            res.false_auto.append(label + " (no such truth row)")
            continue
        want = Decimal(t["value_num"]) if t["value_num"] is not None else None
        got = Decimal(str(r["value_num"])) if r["value_num"] is not None else None
        if got != want or (r["unit_ucum"] or None) != (t["ucum"] or None) or r["loinc"] != t["loinc"]:
            res.false_auto.append(
                f"{label} got {got} {r['unit_ucum']} {r['loinc']}, truth {want} {t['ucum']} {t['loinc']}"
            )
        if r["src_page"] == t["page"] and r["src_bbox"] and match.inside(t["bbox"], r["src_bbox"]):
            res.auto_with_bbox += 1
    for t in truth["rows"]:
        if not any(e["page"] == t["page"] and match.inside(t["bbox"], e["bbox"]) for e in extracted):
            res.dropped.append(f"p{t['page']} {t['name']}={t['value']}")
    res.pending_items = conn.execute(
        text("select count(*) from review_item where document_id = :d and kind = 'pending_document'"),
        {"d": document_id},
    ).scalar_one()
    return res


def evaluate(conn: Connection, blobs: BlobStore, slips) -> list[SlipResult]:
    families: dict[str, object] = {}
    out = []
    for name, pdf, truth in slips:
        fk = _family_key(name)
        if fk not in families:
            families[fk] = create_rivera_family(conn, f"Eval {fk} {secrets.token_hex(2)}")
        doc_id, summary = import_slip(conn, blobs, families[fk], name, pdf, truth)
        out.append(score_slip(conn, name, truth, doc_id, summary))
    return out


def totals(results: list[SlipResult]) -> dict:
    truth = sum(r.truth for r in results)
    ext = sum(r.extracted for r in results)
    tp = sum(r.tp for r in results)
    auto = sum(r.auto for r in results)
    p = tp / ext if ext else 0.0
    rc = tp / truth if truth else 0.0
    return {
        "slips": len(results),
        "truth_rows": truth,
        "extracted_rows": ext,
        "row_f1": round(2 * p * rc / (p + rc), 4) if tp else 0.0,
        "auto_accepted": auto,
        "auto_accept_rate": round(auto / truth, 4) if truth else 0.0,
        "false_auto_accepts": sum(len(r.false_auto) for r in results),
        "dropped": sum(len(r.dropped) for r in results),
        "auto_with_page_bbox": sum(r.auto_with_bbox for r in results),
    }


def use_fake_normalizer_if_stub(force: bool = False) -> bool:
    """Patch lane A's normalize_row with the stand-in when it is still a stub. Returns True if patched."""
    from ownhealth import normalize

    if not force:
        try:
            normalize.normalize_row(
                None,
                profile_id=None,
                printed_name="x",
                printed_value="1",
                printed_unit=None,
                printed_range=None,
                printed_flag=None,
                effective_at=None,
                lang="en",
            )
            return False
        except NotImplementedError:
            pass
        except Exception:  # noqa: BLE001  (the real one ran and choked on the probe: it exists)
            return False
    import fake_normalize

    normalize.normalize_row = fake_normalize.normalize_row
    return True


def main() -> None:
    import os
    import tempfile

    import generate

    from ownhealth.db import make_engine
    from ownhealth.db.migrate import migrate

    ap = argparse.ArgumentParser()
    ap.add_argument(
        "--database-url",
        default=os.environ.get("OWNHEALTH_TEST_ADMIN_URL", "postgresql://postgres:postgres@127.0.0.1:55432/postgres"),
    )
    ap.add_argument("--fake-normalizer", action="store_true")
    ap.add_argument("--json", action="store_true")
    args = ap.parse_args()
    fake = use_fake_normalizer_if_stub(force=args.fake_normalizer)
    admin = make_engine(args.database_url, isolation_level="AUTOCOMMIT")
    db = f"oh_eval_{secrets.token_hex(5)}"
    with admin.connect() as c:
        c.execute(text(f'create database "{db}"'))
    try:
        eng = make_engine(args.database_url.rsplit("/", 1)[0] + "/" + db)
        migrate(eng)
        with tempfile.TemporaryDirectory() as tmp, eng.connect() as conn:
            tx = conn.begin()
            results = evaluate(conn, BlobStore(Path(tmp)), generate.all_slips())
            tx.rollback()
        eng.dispose()
    finally:
        with admin.connect() as c, contextlib.suppress(Exception):
            c.execute(text(f'drop database if exists "{db}" with (force)'))
        admin.dispose()
    tot = totals(results)
    tot["normalizer"] = "stand-in (fake_normalize.py)" if fake else "lane A"
    if args.json:
        print(json.dumps({"totals": tot, "slips": [r.__dict__ for r in results]}, default=str, indent=1))
        return
    for r in results:
        print(
            f"{r.slip:26} truth {r.truth:3} extracted {r.extracted:3} tp {r.tp:3} auto {r.auto:3}"
            f" false_auto {len(r.false_auto)} dropped {len(r.dropped)} pending {r.pending_items}"
        )
        for x in r.false_auto + [f"DROPPED {d}" for d in r.dropped]:
            print("    ", x)
    print(json.dumps(tot, indent=1))


if __name__ == "__main__":
    main()
