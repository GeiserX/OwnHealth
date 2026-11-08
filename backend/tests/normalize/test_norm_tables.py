"""The shipped tables hang together: every alias has a group, every group converts, LOINC has its notice."""

from decimal import Decimal

import pytest

from ownhealth.normalize import DERIVATION_VERSION, _resolve, _tables, convert
from ownhealth.normalize._units import parse_ucum


def test_loinc_subset_size_and_notice():
    codes = _tables.loinc_codes()
    assert len(codes) >= 200
    assert all(c.status for c in codes.values())
    notice = (_tables.DATA / "loinc" / "NOTICE.md").read_text()
    assert "Regenstrief" in notice and "loinc.org/license" in notice


def test_every_alias_and_code_points_at_a_group():
    groups = _tables.groups()
    assert {a.alias_group for rows in _tables.aliases().values() for a in rows} <= set(groups)
    assert set(_tables.codes_by_group()) == set(groups)
    assert set(_tables.molar()) <= set(groups)
    assert {t for t in _tables.specimen_twins().values()} <= set(groups)


@pytest.mark.parametrize("code", sorted(_tables.loinc_codes()))
def test_every_quantitative_property_has_a_dimension_rule(code):
    c = _tables.loinc_codes()[code]
    if c.scale in ("Qn", "SemiQn"):
        assert c.property in _resolve.PROPERTY_DIMS, c


@pytest.mark.parametrize("group", sorted(g for g, v in _tables.groups().items() if v.canonical))
def test_group_units_parse_fit_a_code_and_convert(group):
    g = _tables.groups()[group]
    for u in (g.canonical, g.si, g.conventional):
        parse_ucum(u)
        assert convert(Decimal(1), u, g.canonical, alias_group=group) is not None, (group, u)
    assert any(
        _resolve.unit_fits(c.property, g.canonical) for c in _tables.codes_by_group()[group] if c.scale != "Ord"
    ), group


def test_every_language_has_aliases_for_the_core_panel():
    core = {"hemoglobin", "glucose", "creatinine", "ldl_c", "hdl_c", "triglycerides", "tsh", "hba1c", "ferritin"}
    for lang in _tables.LANGS:
        have = {a.alias_group for rows in _tables.aliases().values() for a in rows if a.lang == lang}
        assert core <= have, (lang, core - have)


def test_derivation_version_names_code_and_tables(tmp_path, monkeypatch):
    assert DERIVATION_VERSION.startswith("norm-0.1.0+t") and len(DERIVATION_VERSION.split("+t")[1]) == 8
    before = _tables.tables_hash()
    copy = tmp_path / "data"
    import shutil

    shutil.copytree(_tables.DATA, copy)
    (copy / "aliases" / "es.tsv").write_text(
        (copy / "aliases" / "es.tsv").read_text() + "Ferritina sérica\tferritin\t\tcurated\t2026-11-08\n"
    )
    monkeypatch.setattr(_tables, "DATA", copy)
    _tables.tables_hash.cache_clear()
    try:
        assert _tables.tables_hash() != before  # a table edit changes the version, so rederive runs
    finally:
        monkeypatch.undo()
        _tables.tables_hash.cache_clear()


def test_tables_are_not_git_ignored():
    """An ignored table never reaches the wheel or the image (the root .gitignore ignores data/)."""
    import shutil
    import subprocess

    if shutil.which("git") is None:
        pytest.skip("git not installed")
    files = [str(p) for p in _tables.DATA.rglob("*") if p.is_file() and "__pycache__" not in p.parts]
    r = subprocess.run(["git", "check-ignore", *files], cwd=_tables.DATA, capture_output=True, text=True)
    if r.returncode == 128:
        pytest.skip("not a git checkout")
    assert r.stdout == "", r.stdout
