"""End-to-end lab PDF import on the database: routing, provenance, identity, pending documents,
draw grouping and amended reports, duplicates, injection, page access (ingestion.md 5-11).

These run with the catalog-based stand-in normalizer, so they test lane B's pipeline alone;
`test_pdf_eval.py` repeats the exit criteria with lane A's real normalizer.
"""

from __future__ import annotations

import io
from dataclasses import replace
from uuid import UUID

import evaluate
import generate
import pytest
from PIL import Image
from sqlalchemy import text

from ownhealth.core.review import accept
from ownhealth.ingest.pdf import ground_value, import_pdf, page_words, render_page_png

pytestmark = pytest.mark.usefixtures("fake_normalizer")


def _items(conn, doc, kind=None):
    q = "select * from review_item where document_id = :d" + (" and kind = :k" if kind else "") + " order by created_at"
    return conn.execute(text(q), {"d": doc, "k": kind}).mappings().all()


def _obs(conn, doc):
    return (
        conn.execute(
            text(
                "select o.*, s.page_no as src_page, s.bbox as src_bbox, s.kind as src_kind from observation o"
                " join src s on s.id = o.src_id where o.document_id = :d"
            ),
            {"d": doc},
        )
        .mappings()
        .all()
    )


def test_labnorte_3_page_report_meets_the_exit_criterion(conn, blobs, store, slips):
    pdf, truth = slips["es-labnorte-3p"]
    doc = store(pdf)
    summary = import_pdf(conn, blobs, document_id=doc)
    res = evaluate.score_slip(conn, "es-labnorte-3p", truth, doc, summary)
    assert res.auto / res.truth >= 0.98, summary.as_dict()
    assert res.false_auto == [] and res.dropped == []
    assert res.auto_with_bbox == res.auto  # every committed row cites its page and a bbox around it
    for o in _obs(conn, doc):
        assert o["src_kind"] == "document_region" and o["src_page"] in (1, 2, 3) and len(o["src_bbox"]) == 4
        assert o["accepted_by"] == "auto" and o["report_key"].startswith("es/labnorte:2026-09-14:")
    d = conn.execute(text("select * from document where id = :d"), {"d": doc}).mappings().one()
    assert (d["doc_type"], d["language"], d["provider_name"], d["report_status"]) == (
        "lab_report",
        "es",
        "LabNorte",
        "final",
    )
    assert d["identity_match"] == "matched" and d["pages"] == 3
    pages = conn.execute(
        text(
            "select page_no, text_layer_ok, words_blob is not null as w from document_page"
            " where document_id = :d order by page_no"
        ),
        {"d": doc},
    ).all()
    assert pages == [(1, True, True), (2, True, True), (3, True, True)]
    pending = conn.execute(
        text("select printed_name, reason from expected_test where report_document_id = :d"), {"d": doc}
    ).all()
    assert [tuple(p) for p in pending] == [(truth["pending"][0]["name"], "Pendiente")]


def test_reimport_is_skipped_unless_forced(conn, blobs, store, slips):
    doc = store(slips["es-01-basic"][0])
    first = import_pdf(conn, blobs, document_id=doc)
    again = import_pdf(conn, blobs, document_id=doc)
    assert first.accepted == 12 and again.reasons == {"already_imported": 1} and again.accepted == 0
    assert len(_obs(conn, doc)) == 12


def test_report_for_another_family_member_never_auto_accepts(conn, blobs, store, slips, family):
    pdf, truth = slips["es-08-range-first"]  # printed for Lucía
    assert truth["person"] == "lucia"
    doc = store(pdf, slug="alex")
    summary = import_pdf(conn, blobs, document_id=doc)
    assert summary.accepted == 0 and _obs(conn, doc) == []
    rows = _items(conn, doc, "candidate_row")
    assert len(rows) == len(truth["rows"]) and all("identity_mismatch" in r["reasons"] for r in rows)
    (ident,) = _items(conn, doc, "document_identity")
    assert ident["payload"]["suggested_profile_id"] == str(family.profiles["lucia"])

    accept(conn, ident["id"], family.users["alex"])  # the person confirms the suggestion
    d = conn.execute(text("select profile_id from document where id = :d"), {"d": doc}).scalar_one()
    assert d == family.profiles["lucia"]
    assert all(r["status"] == "dismissed" for r in _items(conn, doc, "candidate_row"))
    job = conn.execute(
        text("select payload from job where kind = 'import.pdf' and payload->>'document_id' = :d"), {"d": str(doc)}
    ).scalar_one()
    assert job["force"] is True
    again = import_pdf(conn, blobs, document_id=doc, force=True)
    assert again.accepted == len(truth["rows"])
    assert {o["profile_id"] for o in _obs(conn, doc)} == {family.profiles["lucia"]}


def test_scanned_pdf_and_image_wait_as_pending_documents(conn, blobs, store, special):
    scan = store(special["es-scan"][0])
    s = import_pdf(conn, blobs, document_id=scan)
    (item,) = _items(conn, scan)
    assert (item["kind"], item["payload"]["why"], item["payload"]["pages"]) == (
        "pending_document",
        "no_text_layer",
        [1],
    )
    assert s.queued == 1 and s.accepted == 0
    assert (
        conn.execute(text("select text_layer_ok from document_page where document_id = :d"), {"d": scan}).scalar()
        is False
    )
    assert conn.execute(text("select count(*) from extraction_run where document_id = :d"), {"d": scan}).scalar() == 0

    buf = io.BytesIO()
    Image.new("RGB", (40, 20), "white").save(buf, format="PNG")
    img = store(buf.getvalue(), mime="image/png", filename="photo.png")
    import_pdf(conn, blobs, document_id=img)
    (item,) = _items(conn, img)
    assert (item["kind"], item["payload"]["why"]) == ("pending_document", "image")
    import_pdf(conn, blobs, document_id=img, force=True)
    assert len(_items(conn, img)) == 1  # one open pending item per document, not one per run


def test_amended_edition_links_confirms_and_queues_changed_values(conn, blobs, store, slips, family):
    pdf_a, truth_a = slips["es-amend-a"]
    pdf_b, truth_b = slips["es-amend-b"]
    a = store(pdf_a, filename="a.pdf")
    sa = import_pdf(conn, blobs, document_id=a)
    assert sa.accepted == 8
    assert {o["status"] for o in _obs(conn, a)} == {"provisional"}  # the first edition said "provisional"

    b = store(pdf_b, filename="b.pdf")
    sb = import_pdf(conn, blobs, document_id=b)
    assert conn.execute(text("select supersedes_document from document where id = :d"), {"d": b}).scalar() == a
    assert sb.reasons == {"identical_in_earlier_edition": 7, "amended_value": 1}
    assert sb.accepted == 1  # AST is new in the amended edition
    old = {o["alias_group"]: o for o in _obs(conn, a)}
    assert old["glucose"]["status"] == "provisional"
    assert {g: o["status"] for g, o in old.items() if g != "glucose"} == dict.fromkeys(
        set(old) - {"glucose"}, "current"
    )
    (amended,) = _items(conn, b, "amended_report")
    assert amended["payload"]["old_printed_value"] != "142" and amended["observation_id"] == old["glucose"]["id"]
    assert "amended_value" in amended["reasons"]

    (new_id,) = accept(conn, amended["id"], family.users["alex"])
    old_g = conn.execute(text("select status from observation where id = :i"), {"i": old["glucose"]["id"]}).scalar()
    new = conn.execute(text("select * from observation where id = :i"), {"i": new_id}).mappings().one()
    assert old_g == "superseded" and new["supersedes"] == old["glucose"]["id"]
    assert (new["printed_value"], new["document_id"], new["accepted_by"]) == ("142", b, "person")
    # every row of edition b is accounted for: linked, queued or committed
    runs = conn.execute(
        text(
            "select c.state, count(*) from candidate_row c join extraction_run r"
            " on r.id = c.extraction_run_id where r.document_id = :d group by 1"
        ),
        {"d": b},
    ).all()
    assert dict(runs) == {"confirmed": 8, "auto_accepted": 1}
    assert len(truth_b["rows"]) == 9 and len(truth_a["rows"]) == 8


def test_same_report_in_another_file_is_linked_not_duplicated(conn, blobs, store, slips):
    pdf, _ = slips["es-01-basic"]
    lay = next(x for x in generate.layouts() if x.id == "es-01-basic")
    reprint, _ = generate.render(replace(lay, meta={"Subject": "reimpresión"}))
    assert reprint != pdf
    first = store(pdf, filename="a.pdf")
    import_pdf(conn, blobs, document_id=first)
    second = store(reprint, filename="b.pdf")
    s = import_pdf(conn, blobs, document_id=second)
    # same sample id and day: the reprint is read as an edition of the first, with nothing changed
    assert s.accepted == 0 and s.queued == 0 and s.skipped == 12
    assert _obs(conn, second) == []


def test_conflicting_value_from_another_lab_goes_to_review(conn, blobs, store, slips):
    lay = next(x for x in generate.layouts() if x.id == "en-01-basic")
    a, _ = generate.render(lay)
    b, _ = generate.render(
        replace(lay, lab="Demo Diagnostics", sample="OTHER-1", analytes=("glucose",), changes={"glucose": "97"})
    )
    import_pdf(conn, blobs, document_id=store(a, filename="a.pdf"))
    doc_b = store(b, filename="b.pdf")
    s = import_pdf(conn, blobs, document_id=doc_b)
    assert s.accepted == 0 and s.reasons.get("conflict") == 1


def test_injection_fixtures_are_flagged_and_hidden_values_never_read(conn, blobs, store, special):
    for name in ("inj-white-on-white", "inj-hidden-ai-instruction", "inj-url-in-table", "inj-metadata"):
        pdf, truth = special[name]
        doc = store(pdf, filename=f"{name}.pdf")
        import_pdf(conn, blobs, document_id=doc)
        flags = conn.execute(text("select flags from document where id = :d"), {"d": doc}).scalar_one()
        exp = truth["expect"]
        if "possible_injection" in exp:
            assert set(exp["possible_injection"]) <= set(flags["possible_injection"]["pages"]), name
        if "possible_injection_metadata" in exp:
            assert set(exp["possible_injection_metadata"]) <= set(flags["possible_injection"]["metadata"])
            assert "System prompt" in flags["metadata_text"]["Subject"]
        assert ("hidden_text" in flags) is exp["hidden_text"], name
        values = {o["printed_value"] for o in _obs(conn, doc)}
        assert "45" not in values and not any("http" in v for v in values)
        if name == "inj-url-in-table":
            (url_item,) = [
                i for i in _items(conn, doc, "candidate_row") if "http" in i["payload"]["cells"]["value"]["text"]
            ]
            assert "possible_injection" in url_item["reasons"]


def test_page_words_ground_value_and_render(conn, blobs, store, slips, special):
    pdf, truth = slips["es-labnorte-3p"]
    doc = store(pdf)
    import_pdf(conn, blobs, document_id=doc)
    row = truth["rows"][2]  # a value with a decimal comma
    words = page_words(conn, blobs, document_id=doc, page_no=row["page"])
    assert any(w.text == row["value"] for w in words)
    dotted = row["value"].replace(",", ".")
    box = ground_value(conn, blobs, document_id=doc, page_no=row["page"], name=row["name"], value=dotted)
    assert box is not None and evaluate.match.inside(row["bbox"][:2] + [row["bbox"][0] + 10, row["bbox"][3]], box)
    assert ground_value(conn, blobs, document_id=doc, page_no=1, name="Glucosa", value="9999") is None
    assert page_words(conn, blobs, document_id=doc, page_no=9) == []

    inj = store(special["inj-white-on-white"][0], filename="inj.pdf")
    import_pdf(conn, blobs, document_id=inj)
    hidden = [w for w in page_words(conn, blobs, document_id=inj, page_no=1) if w.hidden]
    assert hidden and {w.hidden for w in hidden} == {"white_on_white"}
    assert ground_value(conn, blobs, document_id=inj, page_no=1, name="Glucosa", value="45") is None

    sha = conn.execute(text("select encode(blob_sha256, 'hex') from document where id = :d"), {"d": doc}).scalar()
    png = render_page_png(blobs, blob_sha256=sha, page_no=3, scale=1.0)
    w, h = Image.open(io.BytesIO(png)).size
    assert png.startswith(b"\x89PNG") and abs(w - 595) <= 1 and abs(h - 842) <= 1


def test_import_pdf_job_runs_through_the_queue(engine, settings, blobs, committed_family, slips):
    from ownhealth import jobs
    from ownhealth.core.documents import StoredFile, dispatch_upload

    jobs.load_handlers()
    pdf, truth = slips["de-01-basic"]
    sha = blobs.put_bytes(pdf)
    with engine.begin() as c:
        out = dispatch_upload(
            c,
            blobs,
            household_id=committed_family.household_id,
            profile_id=committed_family.profiles["alex"],
            files=[StoredFile("befund.pdf", sha, len(pdf))],
        )
    (item,) = out["items"]
    ctx = jobs.JobContext(engine=engine, settings=settings, blobs=blobs)
    for _ in range(20):
        if jobs.run_one(ctx, kinds=["import.pdf"]) is None:
            break
    else:
        pytest.fail("the import.pdf queue did not drain in 20 runs")
    with engine.connect() as c:
        state, result = c.execute(text("select state, result from job where id = :j"), {"j": item["job_id"]}).one()
        n = c.execute(
            text("select count(*) from observation where document_id = :d"), {"d": UUID(item["document_id"])}
        ).scalar()
    assert state == "done" and result["accepted"] + result["queued"] == len(truth["rows"]) == n + result["queued"]


def test_bad_payload_is_a_permanent_job_failure(engine, settings, blobs):
    from ownhealth import jobs

    jobs.load_handlers()
    with engine.begin() as c:
        jid = jobs.enqueue(c, "import.pdf", {"filename": "x.pdf"}, dedupe_key="import.pdf:bad-payload-test")
    ctx = jobs.JobContext(engine=engine, settings=settings, blobs=blobs)
    for _ in range(20):
        job = jobs.run_one(ctx, kinds=["import.pdf"])
        if job is None or job.id == jid:
            break
    with engine.connect() as c:
        assert c.execute(text("select state, attempts from job where id = :j"), {"j": jid}).one() == ("dead", 1)


def test_lab_values_carry_a_provenance_label(conn, blobs, store, slips):
    pdf, _ = slips["es-labnorte-3p"]
    doc = store(pdf, filename="LabNorte_2026-09-14.pdf")
    import_pdf(conn, blobs, document_id=doc)
    labels = conn.execute(
        text(
            "select s.page_no, s.label from observation o join src s on s.id = o.src_id"
            " where o.document_id = :d order by s.page_no"
        ),
        {"d": doc},
    ).all()
    assert labels and labels[0] == (1, "LabNorte · 2026-09-14 · p1")
    assert all(label == f"LabNorte · 2026-09-14 · p{page}" for page, label in labels)
