"""Write python-reference.json: what the Python normalizer (backend/, 0.1.1) answers for many inputs.

tests/parity.rs reads it and checks the Rust port gives the same answers. Synthetic inputs only:
the shipped alias tables and the labslips catalog. Run from the repo root with the backend venv:

    backend/.venv/bin/python -I server/crates/ownhealth-normalize/tests/fixtures/gen_python.py
"""

from __future__ import annotations

import json
import sys
from decimal import Decimal
from pathlib import Path

ROOT = Path(__file__).resolve().parents[5]
sys.path[:0] = [str(ROOT / "backend" / "src"), str(ROOT / "fixtures" / "labslips")]

from catalog import ANALYTES  # noqa: E402

from ownhealth import normalize as N  # noqa: E402
from ownhealth.normalize import _tables as T  # noqa: E402
from ownhealth.normalize import _text  # noqa: E402

OUT = Path(__file__).resolve().parent / "python-reference.json"
LOCALES = ["es", "en", "de", "fr"]


def dec(v):
    return None if v is None else str(v)


def rng(r):
    if r is None:
        return None
    return [dec(r.low), dec(r.high), r.text, r.applies_to, r.low_inclusive, r.high_inclusive, r.text_code]


def resolution(r):
    return [
        r.alias_group,
        r.loinc,
        r.property,
        r.method,
        r.refused,
        r.reasons,
        r.confidence,
        [[c.alias_group, c.loinc, c.method, c.confidence, c.matched, c.unit_ok, c.specimen_ok, c.partial] for c in r.candidates[:3]],
    ]


def main() -> None:
    names = sorted({a.alias for rows in T.aliases().values() for a in rows})
    panel_names = [ln.split("\t")[0] for ln in (T.DATA / "aliases" / "panels.tsv").read_text().splitlines()[3:] if ln]
    extra_names = [
        "Índice aterogénico", "Cortisol (tarde)", "GPT (ALT)", "Bilirubin, direkt", "LDL-Cholesterin (berechnet)",
        "Colesterol LDL (calculado)", "S-Glucosa", "Creatinina en orina", "Proteínas en orina", "Glucosa en orina",
        "Filtrado glomerular (CKD-EPI 2021)", "  Hemoglobina  : ", "U-Albúmina", "P-Glucosa", "Coles LDL calc.",
        "Hemoglobina glicada (HbA1c)", "ALT (GPT)", "Vitamina D (25-OH)", "Ferritina sérica", "", "***",
    ]
    units = set()
    for g in T.groups().values():
        units |= {g.canonical, g.si, g.conventional}
    for c in T.loinc_codes().values():
        units.add(c.example_ucum)
    for a in ANALYTES.values():
        units |= set(a[3].values())
    units |= {
        "mg/dl", "mg%", "MG/DL", "x10^3/µL", "x10E3/uL", "10^9/L", "x 10⁹/l", "miles/µL", "/mm3", "UI/L", "IE/l",
        "U/l", "mUI/mL", "µUI/mL", "μIU/mL", "mU/L", "mL/min/1.73m2", "mL/min/1,73 m²", "mmol/l", "µmol/L",
        "mcg/dL", "fl", "pg", "%", "mmol/mol", "mEq/L", "mOsm/kg", "seg", "mm/1ªh", "mmHg", "mg/g creat",
        "mg/mmol", "g/24h", "cel/campo", "ng/ml", "10*3/uL", "m[IU]/L", "", "-", "xyz", "mg/xyz", "ver nota",
        "G/L", "T/L", "g/L", "mU/l", "U/mL", "ug/dL", "mg/g de creatinina", "x10*3/ul", "10exp3/ul", "/µl",
        "mmol/24h", "µg/L", "nmol/L", "pmol/L", "kU/L", "ukat/L", "mL/s/{1.73_m2}", "mg/(24.h)", "mg/",
        "mg.(", "Giga/l", "mm/hr", "%.", "x10³/µL", "10^12/L", "/nl", "/pl", "mg{FEU}/L", "s-1", "m2", "1/min",
    }
    units.discard(None)
    units = sorted(units)

    out: dict = {"derivation_version": N.DERIVATION_VERSION, "tables_hash": T.tables_hash()}
    out["fold"] = {n: T.fold(n) for n in [*names, *panel_names, *extra_names]}
    out["to_ucum"] = {u: N.to_ucum(u) for u in units}

    values = [
        "13,5", "13.5", "1.234,5", "1,234.5", "1,200", "6.800", "1.200", "250.000", "1.234.567", "1,2,3", "< 0,5",
        "≤5", "inferior a 5", "kleiner als 0,1", "> 90", "163 H", "24 *", "Negativo", "nicht nachweisbar", "Négatif",
        "Trazas", "+++", "ver nota", "0", "-3", "+4,5", ".5", ",5", "5.", "5,", "1 234", "1'234,5", "12 h", "12 (h)",
        "12 ↑", "<=0.1", "=>5", "greater than 3", "menor que 1", "superieur a 2", "(Negativo)", "Positivo", "-",
        "+", "no detectado", "ausencia", "1.2.3", "1,234,567.89", "1.234.567,89", "12,345.6.7", "0,0001", "99999,999",
        "70 - 110", "5 a 10", "1–2", "", "   ", "abc", "<", "< ", "12,5 L", "3.4**", "1.000", "10.000.000",
    ]
    vunits = [None, "mg/dL", "/mm3", "x10^3/µL", "10*9/L", "/µL", "%"]
    out["parse_value"] = []
    for v in values:
        for loc in LOCALES:
            for u in vunits:
                p = N.parse_value(v, locale=loc, unit=u)
                out["parse_value"].append([v, loc, u, dec(p.value), p.comparator, p.text_code, p.reason])
    out["looks_like_range"] = {v: _text.looks_like_range(v) for v in values}

    ranges = [
        "13.5 - 17.5", "0,70 - 1,20", "< 130", "Hasta 40", "> 90", "Negativo", "150.000 - 450.000", "ver nota",
        "Hombres: 13.5-17.5 Mujeres: 12,0-16,0", "Männer: 0,7-1,2; Frauen: 0,5-0,9", "18-49 años: 1.0-2.0; 50-99 años: 2.0-3.0",
        "Mujeres: 12-16; 3er trimestre: 11-14", "13-17; 12-16", "0-17 años: 5-10\n18-99 años: 3-8", "10-20\n30-40",
        "Hombres:\n13-17\nMujeres:\n12-16", "0-17 años:\n5-10", "< 5", "> 60", "<= 5", ">= 3", "up to 5", "jusqu'a 3",
        "desde 2", "Adultos: 3-5 Niños: 4-6", "mayores de 50 años: < 8", "menores de 18 años: 1-2", "over 65 years 3 to 9",
        "Embarazo: 1-2", "1 bis 2", "1 a 2", "1 to 2", "superior a 30", "No detectado", "(negativo)", "", "  ",
        "Women 12-15 | Men 13-17", "Frau: 1-2, Mann: 2-3", "3 - 1", "1,5 - 2,5 mg/dL", "4.0 - 11.0", "4,0 - 11,0",
        "< 3,36", "<130", "30 - 100", "7 - 41", "136 - 145", "> 90 mL/min",
    ]
    out["parse_range"] = []
    for r in ranges:
        for loc in LOCALES:
            for u in [None, "/mm3", "mg/dL"]:
                parsed = N.parse_range(r, locale=loc, unit=u)
                picks = []
                for sex in ["male", "female", None]:
                    for age in [5, 17, 18, 40, 71, None]:
                        got = N.pick_range(parsed, sex=sex, age_years=age)
                        picks.append(None if got is None else next(i for i, x in enumerate(parsed) if x is got))
                out["parse_range"].append([r, loc, u, [rng(x) for x in parsed], picks])

    interp = []
    for r in ["13.5 - 17.5", "< 130", "<= 130", "> 90", ">= 90", "Negativo", "1 - 3", "< 5", "> 60"]:
        [ref] = N.parse_range(r, locale="es")
        for v in [None, "0.5", "1", "2", "3", "5", "12", "13.5", "17.5", "60", "89", "90", "129", "130", "200"]:
            for tc in [None, "negative", "not_detected", "positive", "trace"]:
                for cmp in [None, "<", "<=", ">", ">="]:
                    got = N.interpret(Decimal(v) if v else None, tc, ref, comparator=cmp)
                    interp.append([r, v, tc, cmp, got])
    out["interpret"] = interp

    flags = ["H", "h", "L", "HH", "LL", "*", "**", "A", "N", "(H)", "alto", "Bajo", "↑", "↓", "↑↑", "<<", ">>", "x", "", " ",
             "+", "-", "critico alto", "Crítico bajo", "élevé", "normal", "anormal", "pathologisch", "(+)"]
    out["printed_flag_code"] = {f: _text.printed_flag_code(f) for f in flags}

    conv = []
    for gname, g in T.groups().items():
        us = [u for u in (g.canonical, g.si, g.conventional) if u]
        for a in us:
            for b in us:
                for v in ["97.3", "6.5"] if a != b else ["1"]:
                    got = N.convert(Decimal(v), a, b, alias_group=gname)
                    conv.append([gname, a, b, v, dec(got)])
    for gname in T.molar():
        for a, b in [("mg/dL", "mmol/L"), ("g/L", "mol/L"), ("ug/dL", "umol/L"), ("meq/L", "mmol/L"), ("mg/dL", "umol/L"),
                     ("ng/mL", "nmol/L"), ("pg/mL", "pmol/L"), ("mg/mmol", "mg/g"), ("mmol/L", "mg/dL")]:
            conv.append([gname, a, b, "1", dec(N.convert(Decimal(1), a, b, alias_group=gname))])
    for a, b in [("[IU]/L", "U/L"), ("m[IU]/L", "mU/L"), ("U/L", "ukat/L"), ("10*3/uL", "10*9/L"), ("/uL", "10*3/uL"),
                 ("mL/s/{1.73_m2}", "mL/min/{1.73_m2}"), ("s", "h"), ("mm[Hg]", "kg/m/s2"), ("%", "mmol/mol")]:
        for v in ["1", "6800", "0.001", "123456.789"]:
            conv.append(["any", a, b, v, dec(N.convert(Decimal(v), a, b, alias_group="any"))])
    out["convert"] = conv

    res = []
    for i, n in enumerate([*names, *extra_names, *panel_names[:20]]):
        langs = sorted({a.lang for a in T.aliases().get(T.fold(n), [])})[:1] or ["es"]
        groups = {a.alias_group for a in T.aliases().get(T.fold(n), [])}
        us = {None}
        for gname in groups:
            g = T.groups().get(gname)
            if g:
                us |= {g.canonical or None, g.si or None} if i % 3 == 0 else {g.canonical or None}
        for lang in langs:
            for u in sorted(us, key=lambda x: x or ""):
                for spec, tr in [(None, False)] + ([("orina", False), ("blood", False), (None, True)] if u is None and i % 4 == 0 else []):
                    r = N.resolve_analyte(None, n, lang=lang, unit=u, specimen=spec, provider="LabNorte", text_result=tr)
                    res.append([n, lang, u, spec, tr, resolution(r)])
    out["resolve"] = res

    rows = []
    for a in ANALYTES.values():
        group, loinc, pnames, punits, _ucum, decimals, interval, specimen = a
        if interval is None:
            continue
        lo, hi = interval
        for lang in LOCALES:
            name, unit = pnames.get(lang), punits.get(lang)
            if not name:
                continue
            for value, flag, given in [(lo, None, None), ((lo + hi) / 2, "H", loinc), (hi * 1.5, "H", None), (hi * 1000, None, None)]:
                text = f"{value:.{decimals}f}"
                rtext = f"{lo:.{decimals}f} - {hi:.{decimals}f}"
                if lang != "en":
                    text, rtext = text.replace(".", ","), rtext.replace(".", ",")
                if True:
                    if True:
                        pv = N.parse_value(text, locale=lang, unit=unit)
                        parsed = N.parse_range(rtext, locale=lang, unit=unit)
                        ref = N.pick_range(parsed, sex="male", age_years=47)
                        d, r, checks = N.derive(
                            printed_name=name, printed_unit=unit, value=pv.value, comparator=pv.comparator,
                            text_code=pv.text_code, ref=ref, printed_flag=flag, lang=lang, specimen=specimen,
                            method=None, provider="LabNorte", loinc=given, conn=None, household_id=None,
                        )
                        rows.append([
                            name, text, unit, rtext, flag, lang, specimen, given,
                            {k: dec(v) if isinstance(v, Decimal) else v for k, v in d.items()},
                            [[c.name, c.ok, c.reason, c.detail] for c in checks],
                        ])
    for name, text, unit, rtext, flag, given in [
        ("Hemoglobina glicada (HbA1c)", "5,6", "mg/dL", "4,0 - 5,6", None, None),
        ("Hierro", "162", "mg/dL", "60 - 160", None, None),
        ("Lipoproteína (a)", "45", "mg/dL", "< 30", None, None),
        ("Potasio", "7,1", "mmol/L", "3,5 - 5,1", "HH", None),
        ("Potasio", "4,0", "mmol/L", "3,5 - 5,1", "HH", None),
        ("Nitritos", "Negativo", None, "Negativo", None, None),
        ("Glucosa", "Negativo", None, None, None, None),
        ("LDL", "97", "mg/dL", None, None, "99999-9"),
        ("LDL", "2.5", "mmol/mol", None, None, "13457-7"),
        ("Índice aterogénico", "3,1", "", "< 4,5", None, None),
        ("Perfil lipídico", "", None, None, None, None),
        ("Colesterol LDL", "97", "ng/mL", "< 130", None, None),
        ("Glucosa", "92", "mg/dL", "70 - 110", "H", None),
        ("TSH", "2,10", "ng/dL", None, None, None),
        ("Sodio", "141", "%", None, None, None),
        ("Cortisol (tarde)", "12", "ug/dL", "5 - 25", None, None),
        ("Filtrado glomerular estimado", "88", "mL/min/1.73m2", "> 60", None, None),
    ]:
        pv = N.parse_value(text, locale="es", unit=unit)
        parsed = N.parse_range(rtext, locale="es", unit=unit) if rtext else []
        ref = N.pick_range(parsed, sex="male", age_years=47) if parsed else None
        d, r, checks = N.derive(
            printed_name=name, printed_unit=unit, value=pv.value, comparator=pv.comparator, text_code=pv.text_code,
            ref=ref, printed_flag=flag, lang="es", specimen=None, method=None, provider="LabNorte", loinc=given,
            conn=None, household_id=None,
        )
        rows.append([
            name, text, unit, rtext, flag, "es", None, given,
            {k: dec(v) if isinstance(v, Decimal) else v for k, v in d.items()},
            [[c.name, c.ok, c.reason, c.detail] for c in checks],
        ])
    out["derive"] = rows
    OUT.write_text(json.dumps(out, ensure_ascii=False, separators=(",", ":")) + "\n")
    print(OUT, OUT.stat().st_size, {k: len(v) for k, v in out.items() if isinstance(v, list | dict)})


if __name__ == "__main__":
    main()
