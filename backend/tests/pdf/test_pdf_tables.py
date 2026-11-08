"""Table extraction against the synthetic ground truth, and vendor templates (ingestion.md 7, 13)."""

from __future__ import annotations

from collections import Counter

import match
import pytest

from ownhealth.ingest.pdf.extract import extract
from ownhealth.ingest.pdf.tables import GENERIC
from ownhealth.ingest.pdf.templates import load_templates, match_template, parse_template


def _rows(ext):
    return [
        {
            "page": r.page_no,
            "name": r.text("name"),
            "value": r.text("value"),
            "unit": r.text("unit"),
            "bbox": r.bbox,
            "row": r,
        }
        for _, r in ext.results()
    ]


def test_eval_set_covers_30_layouts_in_four_languages(slips):
    layouts = [t["layout"] for _, t in slips.values() if "-amend-" not in t["slip"] and "labnorte-3p" not in t["slip"]]
    assert len(layouts) >= 30
    per_lang = Counter(lay["lang"] for lay in layouts)
    assert set(per_lang) == {"es", "en", "de", "fr"} and min(per_lang.values()) >= 6
    pages3 = slips["es-labnorte-3p"][1]
    assert pages3["pages"] == 3 and pages3["lab"] == "LabNorte" and pages3["lang"] == "es"


LAYOUTS = [
    f"{lang}-{n}"
    for lang in ("es", "en", "de", "fr")
    for n in (
        "01-basic",
        "02-asterisk",
        "03-bold-sections",
        "04-wrapped",
        "05-two-column",
        "06-multipage",
        "07-unit-in-value",
        "08-range-first",
    )
] + ["es-labnorte-3p"]


@pytest.mark.parametrize("name", LAYOUTS)
def test_every_truth_row_is_read_with_its_page_and_bbox(slips, name):
    pdf, truth = slips[name]
    ext = extract(pdf)
    s = match.score_rows(name, truth["rows"], _rows(ext))
    assert s.missing == [] and s.extra == []
    assert s.tp == s.unit_ok == s.bbox_ok == len(truth["rows"])
    assert [r.text("name") for r in ext.pending()] == [p["name"] for p in truth["pending"]]


def test_labnorte_reads_through_its_template_with_flags_split_from_ranges(slips):
    pdf, truth = slips["es-labnorte-3p"]
    ext = extract(pdf)
    assert ext.template is not None and ext.template.id == "es/labnorte"
    flags = {(t["page"], t["name"]): t["flag"] for t in truth["rows"]}
    flagged = 0
    for spec, r in ext.results():
        assert spec.prior == 1.0 and r.conf(spec.prior) == 1.0, r.text("name")
        assert r.text("flag") == flags[(r.page_no, r.text("name"))]
        assert r.text("flag") not in r.text("ref_range").split()
        flagged += bool(r.text("flag"))
    assert flagged >= 3
    urine = [r for _, r in ext.results() if r.specimen == "urine"]
    assert {r.text("name") for r in urine} == {"Glucosa en orina", "Proteínas en orina"}


def test_wrapped_names_are_joined_and_cost_confidence(slips):
    ext = extract(slips["en-04-wrapped"][0])
    wrapped = [r for _, r in ext.results() if r.cells["name"].conf < 1.0]
    assert wrapped and all(len(r.text("name").split()) >= 2 for r in wrapped)
    assert all(r.conf(GENERIC.prior) < 0.95 for r in wrapped)  # generic rules + a join: review, not auto


def test_space_thousands_stay_one_value_and_go_to_review(slips):
    ext = extract(slips["fr-07-unit-in-value"][0])
    plt = next(r for _, r in ext.results() if r.text("name") == "Plaquettes")
    assert (plt.text("value"), plt.text("unit")) == ("353 000", "/µL")
    assert plt.cells["value"].conf < 0.95


def test_glued_asterisk_flag_is_split_from_value_and_unit(slips):
    ext = extract(slips["fr-07-unit-in-value"][0])
    hb = next(r for _, r in ext.results() if r.text("name") == "Hémoglobine")
    assert (hb.text("value"), hb.text("unit"), hb.text("flag")) == ("18,8", "g/dL", "*")


def test_injected_url_row_is_kept_but_never_confident(special):
    ext = extract(special["inj-url-in-table"][0])
    row = next(r for _, r in ext.results() if "http" in r.text("value"))
    assert row.conf(1.0) <= 0.5


def test_hidden_rows_are_never_extracted(special):
    ext = extract(special["inj-white-on-white"][0])
    assert "45" not in [r.text("value") for _, r in ext.results()]
    assert len(ext.results()) == len(special["inj-white-on-white"][1]["rows"])


def test_no_text_layer_pages_are_pending(special):
    for name in ("es-scan", "es-broken-tounicode"):
        ext = extract(special[name][0])
        assert ext.pending_pages == [1] and ext.results() == []


def test_template_fingerprint_needs_the_provider_and_its_headers():
    tpl = load_templates()
    assert [t.id for t in tpl] == ["es/labnorte"]
    assert match_template("LabNorte\nPrueba Resultado Unidades Valores de referencia\nFecha de toma: 1/1/2026")[0]
    assert match_template("Example Lab\nTest Result Units Reference range")[0] is None
    assert match_template("LabNorte newsletter")[0] is None  # provider named, no table headers


def test_template_without_name_or_value_headers_is_refused():
    with pytest.raises(ValueError):
        parse_template(
            "id: x\nversion: '1'\nprovider_name: X\nlocale: es\nfingerprint: {all: [x]}\ncolumns: {unit: [u]}"
        )


def test_a_row_after_a_wide_gap_is_kept_and_a_footer_is_not():
    from dataclasses import replace

    import generate

    lay = next(x for x in generate.layouts() if x.id == "en-01-basic")

    def late(w, page, y):
        w.text(50, y + 200, "Ferritin")
        w.text(250, y + 200, "88")
        w.text(320, y + 200, "ng/mL")
        w.text(400, y + 200, "30 - 400")
        w.text(50, y + 260, "Laboratory director: Dr. Example. Accredited laboratory 0000.")

    pdf, truth = generate.render(replace(lay, id="gap"), hook=late)
    got = [(r.text("name"), r.text("value")) for _, r in extract(pdf).results()]
    assert got[-1] == ("Ferritin", "88") and len(got) == len(truth["rows"]) + 1
