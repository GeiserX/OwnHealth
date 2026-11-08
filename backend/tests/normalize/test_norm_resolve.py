"""Resolution from name + unit + specimen, never name alone (ingestion.md 8.2)."""

import pytest

from ownhealth.normalize import add_alias, resolve_analyte


@pytest.mark.parametrize(
    ("name", "lang", "unit", "specimen", "group", "loinc"),
    [
        ("Colesterol LDL", "es", "mg/dL", None, "ldl_c", "13457-7"),
        ("Colesterol LDL", "es", "mmol/L", None, "ldl_c", "39469-2"),
        ("Colesterol LDL directo", "es", "mg/dL", None, "ldl_c", "18262-6"),
        ("LDL-Cholesterin", "de", "mg/dL", None, "ldl_c", "2089-1"),
        ("Cholestérol LDL", "fr", "mmol/L", None, "ldl_c", "39469-2"),
        ("Colesterol total", "es", "mg/dL", None, "total_cholesterol", "2093-3"),
        ("Colesterol total", "es", "mmol/L", None, "total_cholesterol", "14647-2"),
        ("Hemoglobina glicada (HbA1c)", "es", "%", "blood", "hba1c", "4548-4"),
        ("HbA1c", "de", "mmol/mol", None, "hba1c", "59261-8"),
        ("Hémoglobine glyquée", "fr", "%", None, "hba1c", "4548-4"),
        ("TSH", "es", "µUI/mL", None, "tsh", "3016-3"),
        ("Thyreotropin", "de", "mIU/L", None, "tsh", "3016-3"),
        ("Ferritina", "es", "ng/mL", None, "ferritin", "2276-4"),
        ("Creatinina", "es", "mg/dL", None, "creatinine", "2160-0"),
        ("Creatinine", "en", "µmol/L", None, "creatinine", "14682-9"),
        ("Créatinine", "fr", "µmol/L", None, "creatinine", "14682-9"),
        ("Creatinina", "es", "mg/dL", "orina", "urine_creatinine", "2161-8"),
        ("Creatinina en orina", "es", "mg/dL", None, "urine_creatinine", "2161-8"),
        ("Neutrófilos", "es", "%", None, "neutrophils_pct", "770-8"),
        ("Neutrófilos", "es", "x10^3/µL", None, "neutrophils_abs", "751-8"),
        ("Plaquetas", "es", "x10^9/L", "blood", "platelets", "777-3"),
        ("S-Glucosa", "es", "mg/dl", None, "glucose", "2345-7"),
        ("Glucosa basal", "es", "mg/dL", None, "glucose", "1558-6"),
        ("Glucosa", "es", "mmol/L", None, "glucose", "14749-6"),
        ("Cociente albúmina/creatinina", "es", "mg/g", "urine", "urine_acr", "9318-7"),
        ("Albumin/creatinine ratio", "en", "mg/mmol", None, "urine_acr", "32294-1"),
        ("CA 125", "es", "U/mL", None, "ca125", "10334-1"),
        ("INR", "es", None, None, "inr", "6301-6"),
        ("ALT (GPT)", "es", "U/L", "serum", "alt", "1742-6"),
        ("GPT (ALAT)", "de", "U/l", None, "alt", "1742-6"),
        ("Vitamina D (25-OH)", "es", "ng/mL", None, "vitamin_d_25oh", "62292-8"),
        ("25-OH-Vitamin D3", "de", "nmol/L", None, "vitamin_d_25oh", "14635-7"),
    ],
)
def test_resolve(name, lang, unit, specimen, group, loinc):
    r = resolve_analyte(None, name, lang=lang, unit=unit, specimen=specimen)
    assert (r.alias_group, r.loinc) == (group, loinc), r
    assert r.reasons in ([], ["method_uncertain"])
    assert 0.8 <= r.confidence <= 1


def test_same_name_different_unit_is_a_different_code():
    """Never name alone: total cholesterol in mg/dL and in mmol/L are different LOINC codes."""
    mass = resolve_analyte(None, "Colesterol total", lang="es", unit="mg/dL")
    molar = resolve_analyte(None, "Colesterol total", lang="es", unit="mmol/L")
    assert mass.alias_group == molar.alias_group
    assert (mass.property, molar.property) == ("MCnc", "SCnc")


def test_panel_names_are_refused():
    for name, lang in [
        ("Perfil lipídico", "es"),
        ("Hemograma completo", "es"),
        ("Blutbild", "de"),
        ("Lipid panel", "en"),
        ("Bilan lipidique", "fr"),
    ]:
        r = resolve_analyte(None, name, lang=lang, unit=None)
        assert r.refused and r.loinc is None, name


def test_unknown_name_is_unmapped_and_scoped_to_the_provider():
    r = resolve_analyte(None, "Índice aterogénico", lang="es", unit=None, provider="LabNorte")
    assert r.loinc is None and r.reasons == ["unmapped"]
    assert r.alias_group == "indice_aterogenico@labnorte"


def test_wrong_unit_keeps_the_group_and_flags_the_dimension():
    r = resolve_analyte(None, "Hemoglobina glicada (HbA1c)", lang="es", unit="mg/dL")
    assert r.alias_group == "hba1c" and "unit_dimension_mismatch" in r.reasons
    assert r.confidence < 0.6


def test_a_printed_u_is_read_by_the_code_property():
    # The units never convert IU to U (test_norm_units); the code decides what the label meant.
    tsh = resolve_analyte(None, "TSH", lang="de", unit="mU/l")  # an ACnc code in [IU]: mU/l is mIU/L
    assert (tsh.loinc, tsh.reasons) == ("3016-3", [])
    alt = resolve_analyte(None, "ALAT", lang="fr", unit="UI/L")  # a CCnc enzyme: UI/L is U/L
    assert alt.alias_group == "alt" and alt.reasons == []
    gram = resolve_analyte(None, "TSH", lang="es", unit="mg/L")  # a mass unit is still wrong
    assert "unit_dimension_mismatch" in gram.reasons


def test_giga_and_tera_per_litre_are_counts_not_grams():
    wbc = resolve_analyte(None, "Leucocytes", lang="fr", unit="G/L")
    assert wbc.alias_group == "wbc" and wbc.reasons == []
    rbc = resolve_analyte(None, "Hématies", lang="fr", unit="T/L")
    assert rbc.alias_group == "rbc" and rbc.reasons == []
    assert "unit_dimension_mismatch" in resolve_analyte(None, "Leucocytes", lang="fr", unit="g/L").reasons


@pytest.mark.parametrize(
    ("name", "lang", "loinc"),
    [
        ("LDL-Cholesterin (berechnet)", "de", "13457-7"),
        ("Colesterol LDL (calculado)", "es", "13457-7"),
        ("LDL-Cholesterin (direkt)", "de", "18262-6"),
        ("LDL (berechnet)", "de", "13457-7"),  # the alias "LDL" names no method: the name does
        ("LDL direkt", "de", "18262-6"),
    ],
)
def test_a_method_word_is_kept_when_the_name_is_reduced(name, lang, loinc):
    r = resolve_analyte(None, name, lang=lang, unit="mg/dL")
    assert (r.loinc, r.reasons) == (loinc, [])


def test_a_name_matched_only_in_part_is_marked():
    r = resolve_analyte(None, "Cortisol (tarde)", lang="es", unit="ug/dL")
    assert r.alias_group == "cortisol" and "name_partial" in r.reasons
    gloss = resolve_analyte(None, "GPT (ALT)", lang="de", unit="U/l")  # both halves name ALT
    assert gloss.alias_group == "alt" and gloss.reasons == []


def test_a_text_result_picks_the_ordinal_code():
    r = resolve_analyte(None, "Proteínas en orina", lang="es", unit=None, text_result=True)
    assert (r.alias_group, r.loinc, r.reasons) == ("urine_protein_strip", "20454-5", [])
    q = resolve_analyte(None, "Proteínas en orina", lang="es", unit="mg/dL")
    assert q.loinc != "20454-5"
    g = resolve_analyte(None, "Glucosa en orina", lang="es", unit="mg/dL", text_result=True)
    assert g.loinc == "25428-4"  # "Negativo mg/dL": the unit beside a text result names no quantity


def test_egfr_without_a_year_is_the_older_code_and_marked():
    r = resolve_analyte(None, "Filtrado glomerular estimado", lang="es", unit="mL/min/1.73m2")
    assert (r.loinc, r.reasons) == ("62238-1", ["method_uncertain"])
    r = resolve_analyte(None, "Filtrado glomerular (CKD-EPI 2021)", lang="es", unit="mL/min/1.73m2")
    assert (r.loinc, r.reasons) == ("98979-8", [])


def test_candidates_are_scored():
    r = resolve_analyte(None, "Creatinina", lang="es", unit="mg/dL")
    assert r.candidates and r.candidates[0].alias_group == "creatinine"
    assert r.candidates == sorted(r.candidates, key=lambda c: -c.confidence)


def test_household_alias_resolves_only_in_its_household(conn, family):
    other = family.household_id
    assert resolve_analyte(conn, "Coles LDL hoja", lang="es", unit="mg/dL", household_id=other).loinc is None
    add_alias(conn, household_id=other, alias="Coles LDL hoja", alias_group="ldl_c", lang="es", added_by=None)
    r = resolve_analyte(conn, "Coles LDL hoja", lang="es", unit="mg/dL", household_id=other)
    assert (r.alias_group, r.loinc) == ("ldl_c", "2089-1")  # no method named: the method-agnostic code
    add_alias(
        conn,
        household_id=other,
        alias="Coles LDL hoja",
        alias_group="ldl_c",
        lang="es",
        added_by=None,
        loinc="13457-7",
    )
    r = resolve_analyte(conn, "Coles LDL hoja", lang="es", unit="mmol/L", household_id=other)
    assert (r.alias_group, r.loinc) == ("ldl_c", "39469-2")  # the alias code says calculated; the unit says molar
    assert resolve_analyte(conn, "Coles LDL hoja", lang="es", unit="mg/dL", household_id=None).loinc is None
    from ownhealth.core.access import create_household

    stranger = create_household(conn, "Ortega (other household)")
    assert resolve_analyte(conn, "Coles LDL hoja", lang="es", unit="mg/dL", household_id=stranger).loinc is None


def test_add_alias_validates(conn, family):
    from ownhealth.normalize import AliasError

    with pytest.raises(AliasError):
        add_alias(
            conn, household_id=family.household_id, alias="x", alias_group="no_such_group", lang="es", added_by=None
        )
    with pytest.raises(AliasError):
        add_alias(
            conn,
            household_id=family.household_id,
            alias="Perfil lipídico",
            alias_group="ldl_c",
            lang="es",
            added_by=None,
        )
    with pytest.raises(AliasError):
        add_alias(
            conn,
            household_id=family.household_id,
            alias="x",
            alias_group="ldl_c",
            lang="es",
            added_by=None,
            loinc="2345-7",
        )
