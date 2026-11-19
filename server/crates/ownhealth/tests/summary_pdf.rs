//! The printed doctor summary (`ownhealth_rules::summary` + `ownhealth_ops::summary_pdf`), read back
//! the way a reader would (PDFium): the Rivera family and a profile with five years of monthly labs,
//! for every specialty in es and en, fit on two A4 pages in under 5 s; every line that prints a value
//! names its src id; two renders give the same bytes; the text layer is information, not advice. The
//! slots (calculated values, due items, family patterns) print with the id of the formula or guideline
//! row beside each number, and a recorded decision on a medicine prints without its reason.

#[path = "../../ownhealth-rules/tests/common/mod.rs"]
mod common;

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::Path;
use std::time::Instant;

use chrono::{NaiveDate, TimeZone, Utc};
use common::{Seeded, as_of, seeded};
use ownhealth_core::access::{Principal, ProfileIn, Scopes, all_scopes, create_household, create_profile, scope_set};
use ownhealth_core::family::{FamilyCondition, MemberIn};
use ownhealth_core::observations::{CommitOptions, ObservationIn, commit_observation};
use ownhealth_core::provenance::{SrcIn, SrcRef, create_src};
use ownhealth_core::records::{RecordIn, RecordOptions, add_record};
use ownhealth_ingest_pdf::textcheck::assert_text_layer;
use ownhealth_ingest_pdf::textlayer::read_pdf;
use ownhealth_ops::summary_pdf::{MAX_PAGES, render};
use ownhealth_rules::summary::{self, LANGS, Options, SPECIALTIES};
use ownhealth_rules::wording::{SKIP_KEYS, imperative_phrases, lint};
use ownhealth_rules::{cited, load_pack};
use serde_json::{Value, json};
use uuid::Uuid;

type R = anyhow::Result<()>;

const DATA: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../../data/summary");
const SLUGS: [&str; 4] = ["alex", "sam", "lucia", "carmen"];
const BUDGET_S: f64 = 5.0;

fn opts(specialty: &str, lang: &str) -> Options {
    Options {
        lang: lang.into(),
        specialty: specialty.into(),
        sensitive_open: false,
        as_of: as_of(),
    }
}

/// A reason the person wrote for a decision; no sheet may print it.
const REASON: &str = "Decision reason marker 41c9";

/// A browser session of one user of the Rivera household.
async fn session(s: &Seeded, slug: &str) -> anyhow::Result<Principal> {
    let household: Uuid = sqlx::query_scalar("select household_id from profile where id = $1")
        .bind(s.pid(slug))
        .fetch_one(s.pool())
        .await?;
    Ok(Principal::session(s.user(slug), household))
}

/// The sheet as Alex prints it (he owns his profile, is Lucía's guardian and Carmen's caregiver).
async fn model(s: &Seeded, pid: Uuid, scopes: &Scopes, o: &Options) -> anyhow::Result<Value> {
    model_as(s, "alex", pid, scopes, o).await
}

async fn model_as(s: &Seeded, viewer: &str, pid: Uuid, scopes: &Scopes, o: &Options) -> anyhow::Result<Value> {
    let viewer = session(s, viewer).await?;
    let mut tx = s.pool().begin().await?;
    let m = summary::build_for(&mut tx, Some(&viewer), pid, scopes, o).await?;
    tx.commit().await?;
    Ok(m)
}

fn condition(kind: &str, onset_age: i32) -> FamilyCondition {
    FamilyCondition {
        kind: kind.into(),
        text: None,
        code_system: None,
        code: None,
        onset_age: Some(onset_age),
        onset_approx: false,
    }
}

/// One relative of `slug`, entered by hand by `by`.
async fn relative(
    s: &Seeded,
    slug: &str,
    by: &str,
    relationship: &str,
    conditions: Vec<FamilyCondition>,
    link: Option<&str>,
) -> anyhow::Result<()> {
    let mut m = MemberIn::new(s.pid(slug), relationship);
    m.conditions = conditions;
    let src = SrcRef::New(SrcIn {
        profile_id: Some(s.pid(slug)),
        ..SrcIn::manual(Some(s.user(by)))
    });
    let mut tx = s.pool().begin().await?;
    let made = ownhealth_core::family::add(&mut tx, &m, &src, "person", None, link.map(|l| s.pid(l))).await?;
    // the sheets are printed as of the fixture's day, so the store knew the relative by then (a
    // fixture edit, past the trigger that keeps clinical rows immutable)
    sqlx::query("set local session_replication_role = replica")
        .execute(&mut *tx)
        .await?;
    sqlx::query("update family_member set recorded_at = $2 where id = $1")
        .bind(made.id)
        .bind(as_of().and_hms_opt(9, 0, 0).map(|t| t.and_utc()))
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

/// What fills the slots beyond the seed: Alex's father with a heart attack at 52 and his mother
/// (Carmen) with type 2 diabetes, as the demo has them, and a vitamin D he takes with his recorded
/// decision to keep taking it.
async fn fill_slots(s: &Seeded) -> anyhow::Result<()> {
    relative(s, "alex", "alex", "FTH", vec![condition("mi", 52)], None).await?;
    relative(s, "alex", "alex", "MTH", vec![condition("t2d", 69)], Some("carmen")).await?;
    let d3 = RecordIn::from_json(
        "medication",
        json!({
            "profile_id": s.pid("alex"), "kind": "supplement", "name": "Vitamina D3", "strength": "2000 UI",
            "dose_text": "1 perla", "schedule_text": "con el desayuno", "started": "2026-01-10",
            "decision": "keep", "decision_reason": REASON, "decision_on": "2026-04-02",
        }),
    )?;
    let src = SrcRef::New(SrcIn {
        profile_id: Some(s.pid("alex")),
        ..SrcIn::manual(Some(s.user("alex")))
    });
    let mut tx = s.pool().begin().await?;
    add_record(&mut tx, &d3, &src, RecordOptions::new("person")).await?;
    tx.commit().await?;
    Ok(())
}

/// Every slot row prints the id of its formula or guideline row on the line that prints its number:
/// a calculated value with its date, a due item with its due date (or its last date), a family
/// pattern with its first src id. The citations appendix maps each id to its source.
fn slot_check(model: &Value) -> impl Fn(&[String]) -> Result<(), String> + use<> {
    let slot = |k: &str| model["slots"][k].as_array().cloned().unwrap_or_default();
    let mut want: Vec<(String, Vec<String>)> = Vec::new();
    for c in slot("calculators") {
        let mut parts = vec![c["value"].as_str().unwrap_or_default().to_string()];
        parts.extend(c["date"].as_str().map(str::to_string));
        want.push((c["rule"].as_str().unwrap_or_default().to_string(), parts));
    }
    for d in slot("due") {
        let date = d["due"].as_str().or(d["last"]["date"].as_str());
        want.push((
            d["rule"].as_str().unwrap_or_default().to_string(),
            date.map(str::to_string).into_iter().collect(),
        ));
    }
    for f in slot("family") {
        want.push((
            f["rule"].as_str().unwrap_or_default().to_string(),
            f["src"][0].as_str().map(str::to_string).into_iter().collect(),
        ));
    }
    move |lines: &[String]| {
        for (rule, parts) in &want {
            let on_line = lines
                .iter()
                .any(|l| l.split_whitespace().any(|w| w == rule) && parts.iter().all(|p| l.contains(p.as_str())));
            if !on_line {
                return Err(format!("{rule}: no line prints {parts:?} beside its rule id"));
            }
            // the appendix lists the id after its citation
            if lines.iter().filter(|l| l.split_whitespace().any(|w| w == rule)).count() < 2 {
                return Err(format!("{rule}: not in the citations appendix"));
            }
        }
        Ok(())
    }
}

/// Every row that prints a lab value or a ranked flag names its own src id on its first line: a
/// marker row starts with the series name, a flag row with its rank, each read in its own section (a
/// due item or a calculated value may start with a series name too). Every line is information.
fn src_check(model: &Value) -> impl FnMut(&str) -> Result<(), String> + use<> {
    let headings: Vec<(String, String)> = model["labels"]
        .as_object()
        .into_iter()
        .flatten()
        .filter(|(k, _)| k.starts_with("sec."))
        .map(|(k, v)| (v.as_str().unwrap_or_default().to_string(), k.clone()))
        .collect();
    let mut section = String::new();
    let mut rows: Vec<(String, Vec<String>)> = Vec::new();
    for m in model["markers"].as_array().into_iter().flatten() {
        if m.get("not_measured").is_none() {
            let mut ids = vec![m["src"].as_str().unwrap_or("src_missing").to_string()];
            ids.extend(m["previous"]["src"].as_str().map(str::to_string));
            rows.push((format!("sec.markers {} ", m["name"].as_str().unwrap_or_default()), ids));
        }
    }
    for f in model["flags"].as_array().into_iter().flatten() {
        let first = f["src"][0].as_str().unwrap_or("src_missing").to_string();
        rows.push((format!("sec.flags {} ", f["rank"]), vec![first]));
    }
    move |line: &str| {
        if let Some((_, key)) = headings.iter().find(|(h, _)| h == line) {
            section = key.clone();
        }
        let keyed = format!("{section} {line}");
        for (start, ids) in &rows {
            if keyed.starts_with(start.as_str()) && ids.iter().any(|id| !line.contains(id.as_str())) {
                return Err(format!("a value without its src id ({ids:?})"));
            }
        }
        if let Some(hit) = imperative_phrases(line).first() {
            return Err(format!("reads as advice: {hit}"));
        }
        Ok(())
    }
}

fn pages(pdf: &[u8]) -> usize {
    read_pdf(pdf).map_or(0, |m| m.pages.len())
}

/// Print one model and hold it to every rule; returns the text lines.
fn check_sheet(model: &Value, what: &str) -> Vec<String> {
    let sheet = render(model);
    assert!(sheet.pages <= MAX_PAGES, "{what}: {} pages", sheet.pages);
    assert_eq!(
        pages(&sheet.pdf),
        sheet.pages,
        "{what}: PDFium reads another page count"
    );
    assert_eq!(render(model).pdf, sheet.pdf, "{what}: two renders differ");
    let lines = assert_text_layer(&sheet.pdf, src_check(model));
    let footer = format!("1/{}", sheet.pages);
    assert!(
        lines.iter().any(|l| l.contains(&footer)),
        "{what}: no footer {footer}: {lines:#?}"
    );
    let hash = model["state_hash"].as_str().unwrap_or_default();
    let prefix: String = hash.split_once(':').map_or(hash, |(_, h)| h).chars().take(12).collect();
    assert!(
        lines.iter().any(|l| l.contains(&prefix)),
        "{what}: the footer has no state hash"
    );
    lines
}

#[test]
fn the_data_files_load_through_the_cited_loader() -> R {
    let mut versions = Vec::new();
    for (name, _) in summary::FILES {
        let f = cited::load(&Path::new(DATA).join(name))?;
        versions.push(f.version);
    }
    assert_eq!(versions.join(", "), summary::version()?);
    let spec = cited::load(&Path::new(DATA).join("specialties.yaml"))?;
    let ids: Vec<&str> = spec.rows.iter().filter_map(|r| r["id"].as_str()).collect();
    assert_eq!(ids, SPECIALTIES);
    Ok(())
}

/// Status words come from the engine only: every series a specialty lists is a marker of a pack system.
#[test]
fn every_specialty_series_is_a_pack_marker() -> R {
    let pack = load_pack()?;
    let markers: Vec<String> = pack
        .systems
        .iter()
        .flat_map(|s| s.markers.iter().map(ownhealth_rules::pack::System::marker_key))
        .collect();
    for (id, list) in summary::specialty_markers()? {
        assert!(!list.is_empty(), "{id}");
        for g in list {
            assert!(pack.analytes.contains_key(&g), "{id}: {g} is not an analyte");
            assert!(markers.contains(&g), "{id}: {g} is in no system");
        }
    }
    Ok(())
}

#[tokio::test]
async fn the_rivera_family_prints_on_two_pages_with_every_value_cited() -> R {
    let s = seeded().await?;
    fill_slots(&s).await?;
    let scopes = all_scopes();
    for slug in SLUGS {
        for specialty in SPECIALTIES {
            for lang in LANGS {
                let what = format!("{slug} {specialty} {lang}");
                let t = Instant::now();
                let m = model(&s, s.pid(slug), &scopes, &opts(specialty, lang)).await?;
                render(&m);
                let took = t.elapsed().as_secs_f64();
                assert!(took < BUDGET_S, "{what}: {took:.2} s");
                let lines = check_sheet(&m, &what);
                assert_eq!(lint(&m, &SKIP_KEYS), Vec::new(), "{what}");
                if let Err(e) = slot_check(&m)(&lines) {
                    panic!("{what}: {e}: {lines:#?}");
                }
                // every marker either prints a value with its range, date and src, or reads "not measured"
                for r in m["markers"].as_array().into_iter().flatten() {
                    if r.get("not_measured").is_some() {
                        assert!(r.get("value").is_none(), "{what}");
                        continue;
                    }
                    for k in ["value", "date", "src"] {
                        assert!(r[k].is_string(), "{what}: {k} of {r}");
                    }
                    assert!(r.get("range").is_some(), "{what}: range of {r}");
                    if r["previous"].is_object() {
                        assert!(r["previous"]["src"].is_string(), "{what}: previous src of {r}");
                    }
                }
                let gap = if lang == "es" { "no medido" } else { "not measured" };
                let gaps = m["markers"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter(|r| r.get("not_measured").is_some());
                if gaps.count() > 0 {
                    assert!(lines.iter().any(|l| l.contains(gap)), "{what}: no gap line");
                }
            }
        }
    }
    Ok(())
}

#[tokio::test]
async fn alex_shows_values_ranges_previous_values_and_flags() -> R {
    let s = seeded().await?;
    let m = model(&s, s.pid("alex"), &all_scopes(), &opts("cardiology", "es")).await?;
    let ldl = m["markers"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|r| r["group"] == "ldl_c")
        .cloned()
        .unwrap_or(Value::Null);
    assert!(ldl["previous"].is_object(), "{ldl}");
    assert!(m["flags"].as_array().is_some_and(|f| !f.is_empty()), "{m}");
    let lines = check_sheet(&m, "alex cardiology es");
    assert!(
        lines
            .iter()
            .any(|l| l.starts_with("Resumen para el médico · Cardiología")),
        "{lines:#?}"
    );
    // the appendix maps each printed id to its document, page and date
    let cited = summary::cited_srcs(&m);
    for id in &cited {
        assert!(lines.iter().any(|l| l.contains(id.as_str())), "{id} not printed");
    }
    assert!(lines.iter().any(|l| l.contains("LabNorte · ")), "{lines:#?}");
    Ok(())
}

/// The appendix names a hand-entered source in words with its date, and lists documents newest first
/// with the pages of one report together; a Spanish sheet writes its lab ranges with a decimal comma.
#[tokio::test]
async fn the_appendix_reads_in_words_by_date_and_ranges_use_the_sheets_decimals() -> R {
    let s = seeded().await?;
    for (lang, by_hand, sep) in [("es", "introducido a mano · ", ','), ("en", "entered by hand · ", '.')] {
        let m = model(&s, s.pid("alex"), &all_scopes(), &opts("hematology", lang)).await?;
        let lines = check_sheet(&m, lang);
        let at = lines
            .iter()
            .rposition(|l| l == "Fuentes" || l == "Sources")
            .ok_or_else(|| anyhow::anyhow!("no appendix"))?;
        let appendix = &lines[at..];
        assert!(!appendix.iter().any(|l| l.starts_with("manual ")), "{appendix:#?}");
        assert!(appendix.iter().any(|l| l.starts_with(by_hand)), "{lang}: {appendix:#?}");
        // LabNorte · 2026-09-14 · p1 ... : dates never go up, pages go up within a date
        let docs: Vec<(String, String)> = appendix
            .iter()
            .filter_map(|l| l.strip_prefix("LabNorte · "))
            .filter_map(|l| {
                let mut p = l.split(" · ");
                Some((p.next()?.to_string(), p.next()?.split(' ').next()?.to_string()))
            })
            .collect();
        assert!(docs.len() > 1, "{appendix:#?}");
        for w in docs.windows(2) {
            assert!(
                w[0].0 > w[1].0 || (w[0].0 == w[1].0 && w[0].1 < w[1].1),
                "{lang}: {docs:?}"
            );
        }
        let other = if sep == ',' { '.' } else { ',' };
        let mut ranges = 0;
        for r in m["markers"].as_array().into_iter().flatten() {
            let Some(range) = r["range"].as_str() else { continue };
            let c: Vec<char> = range.chars().collect();
            let mixed = c
                .windows(3)
                .any(|w| w[0].is_ascii_digit() && w[1] == other && w[2].is_ascii_digit());
            assert!(!mixed, "{lang}: {} range {range}", r["name"]);
            ranges += 1;
        }
        assert!(ranges > 0, "{lang}: no ranges");
    }
    Ok(())
}

/// A sheet dated in the past leaves out an allergy recorded as starting after that date.
#[tokio::test]
async fn an_allergy_that_starts_after_the_print_date_is_left_out() -> R {
    let s = seeded().await?;
    let names = |m: &Value| -> Vec<String> {
        m["allergies"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|a| a["substance"].as_str().map(str::to_string))
            .collect()
    };
    let before = model(&s, s.pid("sam"), &all_scopes(), &opts("gp", "en")).await?;
    let first = names(&before)
        .first()
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("sam has no allergy"))?;
    // a fixture edit, past the trigger that keeps clinical rows immutable
    let mut tx = s.pool().begin().await?;
    sqlx::query("set local session_replication_role = replica")
        .execute(&mut *tx)
        .await?;
    sqlx::query("update allergy set effective_at = $3 where profile_id = $1 and substance = $2")
        .bind(s.pid("sam"))
        .bind(&first)
        .bind(
            (as_of() + chrono::Duration::days(1))
                .and_hms_opt(9, 0, 0)
                .map(|t| t.and_utc()),
        )
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    let after = model(&s, s.pid("sam"), &all_scopes(), &opts("gp", "en")).await?;
    assert!(!names(&after).contains(&first), "{first} printed before it started");
    assert_eq!(names(&after).len() + 1, names(&before).len());
    Ok(())
}

/// The check above is not one that cannot fail: a marker printed without its src id is refused.
#[tokio::test]
async fn a_value_without_its_src_fails_the_check() -> R {
    let s = seeded().await?;
    let mut m = model(&s, s.pid("alex"), &all_scopes(), &opts("gp", "en")).await?;
    let clean = m.clone();
    let row = m["markers"]
        .as_array_mut()
        .and_then(|a| {
            a.iter_mut()
                .find(|r| r.get("not_measured").is_none() && r["flag_rank"].is_null())
        })
        .ok_or_else(|| anyhow::anyhow!("no unflagged measured marker"))?;
    row["src"] = Value::Null;
    let pdf = render(&m).pdf;
    let refused = catch_unwind(AssertUnwindSafe(|| assert_text_layer(&pdf, src_check(&clean))));
    assert!(refused.is_err(), "a value printed without its src id passed");
    Ok(())
}

/// The PDF font is WinAnsi: a name in Chinese or Arabic letters cannot print, and each such letter
/// shows as '?'. The sheet says so beside the name, so the paper never shows a name that looks whole.
#[tokio::test]
async fn a_name_the_font_cannot_print_is_marked_beside_it() -> R {
    let s = seeded().await?;
    for lang in LANGS {
        let mut m = model(&s, s.pid("alex"), &all_scopes(), &opts("gp", lang)).await?;
        let says = if lang == "es" {
            "esta hoja no puede imprimir"
        } else {
            "this sheet cannot print"
        };
        let lines = check_sheet(&m, "latin name");
        assert!(!lines.iter().any(|l| l.contains(says)), "{lines:#?}");
        m["person"]["name"] = json!("李明 Li Ming");
        let lines = check_sheet(&m, "chinese name");
        let at = lines
            .iter()
            .position(|l| l.contains("?? Li Ming"))
            .ok_or_else(|| anyhow::anyhow!("no name line: {lines:#?}"))?;
        assert!(lines[at + 1].contains(says), "{lines:#?}");
    }
    Ok(())
}

#[tokio::test]
async fn a_viewer_without_lab_records_or_genome_scopes_gets_them_withheld() -> R {
    let s = seeded().await?;
    for lang in LANGS {
        let m = model(&s, s.pid("alex"), &scope_set(["read:summary"]), &opts("gp", lang)).await?;
        assert_eq!(m["withheld"]["by_scope"], json!(["labs", "records", "genome"]));
        for k in [
            "markers",
            "flags",
            "conditions",
            "medications",
            "allergies",
            "vaccines",
            "genome",
        ] {
            assert_eq!(m[k], json!([]), "{k}");
        }
        let lines = check_sheet(&m, "viewer");
        // a withheld line may wrap, so its words are read across the lines
        let text = lines.join(" ");
        let (says, labs, records) = if lang == "es" {
            (
                "esta cuenta no puede leer",
                "Valores de laboratorio y valores calculados: no se muestran",
                "cribados pendientes y antecedentes familiares: no se muestran",
            )
        } else {
            (
                "this account cannot read",
                "Lab values and calculated values: not shown",
                "screenings due and family history: not shown",
            )
        };
        assert_eq!(text.matches(says).count(), 3, "{lines:#?}");
        // the withheld lines name the slots they leave out too
        assert!(text.contains(labs) && text.contains(records), "{lines:#?}");
        assert!(!lines.iter().any(|l| l.contains("mg/dL")), "{lines:#?}");
    }
    Ok(())
}

/// The slots of each view: cardiology prints the cardiovascular calculators, eGFR and the family's heart
/// pattern; general practice prints eGFR, FIB-4, the due items and every family pattern; hematology
/// and the IPS print none.
#[tokio::test]
async fn each_view_prints_the_slots_it_names() -> R {
    let s = seeded().await?;
    fill_slots(&s).await?;
    let ids = |m: &Value, k: &str| -> Vec<String> {
        m["slots"][k]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|r| r["rule"].as_str().map(str::to_string))
            .collect()
    };
    for (lang, father, ckd) in [("en", "father", "eGFR"), ("es", "padre", "FGe")] {
        let m = model(&s, s.pid("alex"), &all_scopes(), &opts("cardiology", lang)).await?;
        // SCORE2-OP is for 70 and over: the calculators document has no item for Alex at 47
        assert_eq!(
            ids(&m, "calculators"),
            ["score2", "prevent_cvd", "prevent_ascvd", "ckd_epi_2021_cr"],
            "{lang}"
        );
        assert_eq!(ids(&m, "family"), ["fam.ascvd"], "{lang}");
        assert_eq!(ids(&m, "due"), Vec::<String>::new(), "{lang}");
        let fam = &m["slots"]["family"][0];
        assert_eq!(fam["status"], "present", "{lang}");
        assert_eq!(fam["relatives"], father, "{lang}");
        let egfr = &m["slots"]["calculators"][3];
        assert_eq!(egfr["status"], "ok", "{egfr}");
        assert!(egfr["value"].is_string() && egfr["date"].is_string(), "{egfr}");
        assert!(egfr["src"].as_array().is_some_and(|a| !a.is_empty()), "{egfr}");
        // a refusal carries its reason and neither a value nor a date
        let score2 = &m["slots"]["calculators"][0];
        assert_ne!(score2["status"], "ok", "{score2}");
        assert!(score2["value"].is_null() && score2["date"].is_null(), "{score2}");
        assert!(score2["text"].as_str().is_some_and(|t| !t.is_empty()), "{score2}");
        let lines = check_sheet(&m, lang);
        assert!(lines.iter().any(|l| l.starts_with(ckd)), "{lines:#?}");
        assert!(
            lines
                .iter()
                .any(|l| l.contains("https://doi.org/10.1161/CIR.0000000000001423")),
            "{lang}: the family pattern's citation: {lines:#?}"
        );
        assert!(
            lines
                .iter()
                .any(|l| l.contains("https://pubmed.ncbi.nlm.nih.gov/34554658/")),
            "{lang}: the eGFR formula's citation: {lines:#?}"
        );

        let gp = model(&s, s.pid("alex"), &all_scopes(), &opts("gp", lang)).await?;
        assert_eq!(
            ids(&gp, "calculators"),
            ["ckd_epi_2021_cr", "ckd_epi_2021_cr_cys", "fib4"]
        );
        assert_eq!(ids(&gp, "family"), ["fam.ascvd"]);
        // Alex's open items are lab gaps; the record's document gaps never print
        let due = gp["slots"]["due"].as_array().cloned().unwrap_or_default();
        assert!(!due.is_empty(), "{lang}: {gp}");
        for d in &due {
            assert!(
                ["overdue", "due", "upcoming"].contains(&d["status"].as_str().unwrap_or_default()),
                "{d}"
            );
            assert!(
                ["screening", "vaccine", "lab"].contains(&d["area"].as_str().unwrap_or_default()),
                "{d}"
            );
            // every row names its guideline beside the link, a rule pack lab item the pack
            assert!(d["source"].as_str().is_some_and(|s| !s.is_empty()), "{d}");
            // a never-recorded item has no due date: the date its age window opened is no deadline
            if d["last"].is_null() && d["status"] != "upcoming" {
                assert!(d["due"].is_null(), "{d}");
            }
        }
        check_sheet(&gp, lang);
        for view in ["hematology", "ips"] {
            let m = model(&s, s.pid("alex"), &all_scopes(), &opts(view, lang)).await?;
            assert_eq!(
                m["slots"],
                json!({"calculators": [], "due": [], "family": []}),
                "{view}"
            );
        }
    }
    // Lucía at 9: CKD-EPI is for adults and FIB-4 from 35, so the sheet prints no calculated value
    let m = model(&s, s.pid("lucia"), &all_scopes(), &opts("gp", "en")).await?;
    assert_eq!(ids(&m, "calculators"), Vec::<String>::new(), "{m}");
    let lines = check_sheet(&m, "lucia gp");
    assert!(!lines.iter().any(|l| l == "Calculated values"), "{lines:#?}");
    // Carmen at 71: the guideline's screenings and vaccines are open, each with its rule's source
    let m = model(&s, s.pid("carmen"), &all_scopes(), &opts("gp", "en")).await?;
    let areas: Vec<&str> = m["slots"]["due"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|d| d["rule"].as_str())
        .collect();
    assert!(areas.iter().any(|r| r.starts_with("es.vac.")), "{areas:?}");
    assert!(areas.iter().any(|r| r.starts_with("es.cancer.")), "{areas:?}");
    // her record's document gaps (requests to her providers) are not for the sheet
    assert!(
        !m["slots"]["due"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|d| d["area"] == "document"),
        "{m}"
    );
    let lines = check_sheet(&m, "carmen gp");
    assert!(lines.iter().any(|l| l.contains("none on record")), "{lines:#?}");
    Ok(())
}

/// The check of the slot rows can fail: a row printed without its formula or guideline id is refused.
#[tokio::test]
async fn a_slot_row_without_its_rule_id_fails_the_check() -> R {
    let s = seeded().await?;
    fill_slots(&s).await?;
    for (view, k) in [("cardiology", "calculators"), ("cardiology", "family"), ("gp", "due")] {
        let clean = model(&s, s.pid("alex"), &all_scopes(), &opts(view, "en")).await?;
        let lines = assert_text_layer(&render(&clean).pdf, |_| Ok(()));
        assert!(slot_check(&clean)(&lines).is_ok(), "{view} {k}: the clean sheet fails");
        let mut m = clean.clone();
        let row = m["slots"][k]
            .as_array_mut()
            .and_then(|a| a.last_mut())
            .ok_or_else(|| anyhow::anyhow!("{view}: no {k} row"))?;
        row["rule"] = json!("");
        let lines = assert_text_layer(&render(&m).pdf, |_| Ok(()));
        assert!(
            slot_check(&clean)(&lines).is_err(),
            "{view} {k}: a row without its rule id passed"
        );
    }
    Ok(())
}

/// A recorded decision on a supplement prints in words with its day, in its own column; the reason the
/// person wrote never prints.
#[tokio::test]
async fn a_recorded_decision_prints_beside_its_medicine_without_its_reason() -> R {
    let s = seeded().await?;
    fill_slots(&s).await?;
    for (lang, said, column) in [
        ("en", "decided to keep taking it on 2026-04-02", "Recorded decision"),
        ("es", "decidió seguir tomándolo el 2026-04-02", "Decisión registrada"),
    ] {
        let m = model(&s, s.pid("alex"), &all_scopes(), &opts("gp", lang)).await?;
        let d3 = m["medications"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|x| x["name"] == "Vitamina D3")
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("no vitamin D3 row"))?;
        assert_eq!(d3["decision"], said, "{lang}");
        assert_eq!(d3["schedule"], "1 perla · con el desayuno", "{lang}");
        assert!(!m.to_string().contains(REASON), "{lang}: the reason is in the model");
        let lines = check_sheet(&m, lang);
        let text = lines.join("\n");
        assert!(text.contains(column), "{lang}: {lines:#?}");
        // the cell wraps, so its words are read across the lines
        let words: Vec<&str> = text.split_whitespace().collect();
        let said_words: Vec<&str> = said.split_whitespace().collect();
        assert!(said_words.iter().all(|w| words.contains(w)), "{lang}: {lines:#?}");
        assert!(!text.contains(REASON), "{lang}: the reason printed");
    }
    // a sheet whose medicines carry no decision keeps the five columns
    let m = model(&s, s.pid("sam"), &all_scopes(), &opts("gp", "en")).await?;
    let lines = check_sheet(&m, "sam");
    assert!(!lines.iter().any(|l| l.contains("Recorded decision")), "{lines:#?}");
    Ok(())
}

/// A family pattern on the sheet counts only the relatives the viewer may read, as on the Family
/// screen: Lucía's father is linked to Alex's profile, which Sam may not read beyond its summary. A
/// build with no viewer prints no family slot.
#[tokio::test]
async fn a_relative_linked_to_a_profile_the_viewer_cannot_read_feeds_no_pattern() -> R {
    let s = seeded().await?;
    relative(&s, "lucia", "alex", "FTH", vec![condition("mi", 50)], Some("alex")).await?;
    let rules = |m: &Value| -> Vec<String> {
        m["slots"]["family"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|r| r["rule"].as_str().map(str::to_string))
            .collect()
    };
    let o = opts("gp", "en");
    let by_alex = model_as(&s, "alex", s.pid("lucia"), &all_scopes(), &o).await?;
    assert_eq!(rules(&by_alex), ["fam.ascvd"]);
    let by_sam = model_as(&s, "sam", s.pid("lucia"), &all_scopes(), &o).await?;
    assert_eq!(rules(&by_sam), Vec::<String>::new());
    let mut tx = s.pool().begin().await?;
    let none = summary::build(&mut tx, s.pid("lucia"), &all_scopes(), &o).await?;
    tx.commit().await?;
    assert_eq!(rules(&none), Vec::<String>::new());
    // without read:records there is no family or due slot; without read:labs no calculators
    let labs_only = model(&s, s.pid("alex"), &scope_set(["read:summary", "read:labs"]), &o).await?;
    assert_eq!(labs_only["slots"]["family"], json!([]));
    assert_eq!(labs_only["slots"]["due"], json!([]));
    let records_only = model(&s, s.pid("alex"), &scope_set(["read:summary", "read:records"]), &o).await?;
    assert_eq!(records_only["slots"]["calculators"], json!([]));
    check_sheet(&labs_only, "labs only");
    check_sheet(&records_only, "records only");
    Ok(())
}

/// A calculated value prints with what qualifies it: HOMA-IR from a draw not labelled fasting, with
/// insulin read from pmol/L, says both beside the number.
#[tokio::test]
async fn a_calculated_value_prints_with_its_qualifications() -> R {
    let s = seeded().await?;
    let pid = s.pid("alex");
    let mut tx = s.pool().begin().await?;
    let src = SrcIn {
        profile_id: Some(pid),
        label: Some("LabNorte · 2026-06-10 · p1".into()),
        ..SrcIn::new("manual")
    };
    let src = create_src(&mut tx, &src).await?;
    for (g, v, unit, canonical, canonical_unit) in [
        ("glucose", "5.4", "mmol/L", "97", "mg/dL"),
        ("insulin", "60", "pmol/L", "10", "u[IU]/mL"),
    ] {
        let obs = ObservationIn::from_json(json!({
            "profile_id": pid, "printed_name": g, "printed_value": v, "printed_unit": unit,
            "value_num": v, "alias_group": g, "unit_ucum": unit, "value_canonical": canonical,
            "unit_canonical": canonical_unit, "derivation_version": "summary",
            "effective_at": "2026-06-10T08:00:00Z",
        }))?;
        commit_observation(&mut tx, &obs, &SrcRef::Id(src.clone()), CommitOptions::new("auto")).await?;
    }
    tx.commit().await?;
    for (lang, fasting, factor) in [
        (
            "en",
            "the record does not say whether this draw was fasting",
            "factor 6.00",
        ),
        ("es", "el registro no dice si esta extracción lo fue", "factor 6,00"),
    ] {
        let m = model(&s, pid, &all_scopes(), &opts("endocrinology", lang)).await?;
        let homa = m["slots"]["calculators"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|c| c["rule"] == "homa_ir")
            .cloned()
            .unwrap_or_default();
        assert_eq!(homa["status"], "ok", "{lang}: {m}");
        assert_eq!(homa["notes"].as_array().map_or(0, Vec::len), 2, "{lang}: {homa}");
        let lines = check_sheet(&m, lang);
        // the result cell wraps, so its words are read across the lines
        let text = lines.join(" ");
        assert!(text.contains(fasting) && text.contains(factor), "{lang}: {lines:#?}");
    }
    Ok(())
}

/// A relative tagged sensitive feeds no family pattern on the sheet, as the person's own sensitive
/// records never print: not for an owner who may read sensitive facts, and not with the tick, which
/// covers genome findings only.
#[tokio::test]
async fn a_relative_tagged_sensitive_feeds_no_pattern_on_the_sheet() -> R {
    let s = seeded().await?;
    relative(&s, "alex", "alex", "FTH", vec![condition("mi", 50)], None).await?;
    let o = opts("cardiology", "en");
    let present = |m: &Value| -> Vec<String> {
        m["slots"]["family"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|r| r["status"] == "present")
            .filter_map(|r| r["rule"].as_str().map(str::to_string))
            .collect()
    };
    // the control: untagged, the father's heart attack makes the pattern
    assert_eq!(
        present(&model(&s, s.pid("alex"), &all_scopes(), &o).await?),
        ["fam.ascvd"]
    );
    let src: String = sqlx::query_scalar("select src_id from family_member where profile_id = $1")
        .bind(s.pid("alex"))
        .fetch_one(s.pool())
        .await?;
    // a fixture edit, past the trigger that keeps clinical rows immutable
    let mut tx = s.pool().begin().await?;
    sqlx::query("set local session_replication_role = replica")
        .execute(&mut *tx)
        .await?;
    sqlx::query("update family_member set sensitivity = '{mental_health}' where profile_id = $1")
        .bind(s.pid("alex"))
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    for sensitive_open in [false, true] {
        let o = Options {
            sensitive_open,
            ..opts("cardiology", "en")
        };
        let m = model(&s, s.pid("alex"), &all_scopes(), &o).await?;
        assert_eq!(present(&m), Vec::<String>::new(), "tick {sensitive_open}");
        assert!(
            !m.to_string().contains(&src),
            "tick {sensitive_open}: {src} is on the sheet"
        );
        check_sheet(&m, "sensitive relative");
    }
    Ok(())
}

/// Carmen Ortega with five years of monthly draws of 40 series, the newest all above range.
#[tokio::test]
async fn five_years_of_labs_print_on_two_pages_in_time() -> R {
    let s = seeded().await?;
    let pid = five_year_profile(&s).await?;
    for specialty in SPECIALTIES {
        for lang in LANGS {
            let what = format!("five years {specialty} {lang}");
            let t = Instant::now();
            let m = model(&s, pid, &all_scopes(), &opts(specialty, lang)).await?;
            render(&m);
            let took = t.elapsed().as_secs_f64();
            assert!(took < BUDGET_S, "{what}: {took:.2} s");
            assert!(
                m["flags"].as_array().map_or(0, Vec::len) >= 15,
                "{what}: the fixture must flag"
            );
            let lines = check_sheet(&m, &what);
            assert!(lines.iter().any(|l| l.starts_with("1 ")), "{what}: flag 1 is missing");
            if let Err(e) = slot_check(&m)(&lines) {
                panic!("{what}: {e}: {lines:#?}");
            }
        }
    }
    Ok(())
}

/// More than two pages of content: rows are cut from the bottom of the ranking, the cut section
/// says how many more the record holds, and the top of the ranking stays.
#[tokio::test]
async fn an_overflow_is_cut_by_rank_with_a_count_of_what_is_left_out() -> R {
    let s = seeded().await?;
    for lang in LANGS {
        let mut m = model(&s, s.pid("alex"), &all_scopes(), &opts("gp", lang)).await?;
        let flags = m["flags"].as_array().cloned().unwrap_or_default();
        let first = flags
            .first()
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("alex has no flags"))?;
        let many: Vec<Value> = (1..=150)
            .map(|rank| {
                let mut f = flags[(rank - 1) % flags.len()].clone();
                f["rank"] = json!(rank);
                f
            })
            .collect();
        m["flags"] = json!(many);
        let sheet = render(&m);
        assert_eq!(sheet.pages, MAX_PAGES, "{lang}");
        assert!(sheet.cut > 0, "{lang}: nothing was cut");
        let lines = check_sheet(&m, lang);
        let more = if lang == "es" {
            "más en el registro"
        } else {
            "more in the record"
        };
        assert!(lines.iter().any(|l| l.contains(more)), "{lang}: {lines:#?}");
        let top = format!("1 {}", first["text"].as_str().unwrap_or_default());
        let top: String = top.chars().take(30).collect();
        assert!(
            lines.iter().any(|l| l.starts_with(&top)),
            "{lang}: flag 1 was cut: {lines:#?}"
        );
        assert!(
            !lines.iter().any(|l| l.starts_with("150 ")),
            "{lang}: the last flag printed"
        );
    }
    Ok(())
}

/// Conditions, allergies and medicines alone can fill more than two pages: their tails are cut too,
/// each with its count, and the src ids the sheet reports (for the audit row) are exactly the ones
/// on the paper, never those of a cut row.
#[tokio::test]
async fn long_record_lists_are_cut_too_and_only_printed_ids_are_reported() -> R {
    let s = seeded().await?;
    for lang in LANGS {
        let mut m = model(&s, s.pid("alex"), &all_scopes(), &opts("gp", lang)).await?;
        let rows = |kind: &str, n: usize| -> Vec<Value> {
            (0..n)
                .map(|i| match kind {
                    "medications" => json!({"name": format!("Medicine {i:03}"), "schedule": "once a day",
                        "since": "2024-01-01", "src": format!("src_fake_med_{i:03}")}),
                    "allergies" => json!({"substance": format!("Substance {i:03}"), "criticality": "low",
                        "reaction": "rash", "src": format!("src_fake_alg_{i:03}")}),
                    _ => json!({"name": format!("Condition {i:03}"), "since": "2020-01-01",
                        "src": format!("src_fake_cnd_{i:03}")}),
                })
                .collect()
        };
        m["medications"] = json!(rows("medications", 130));
        m["allergies"] = json!(rows("allergies", 60));
        m["conditions"] = json!(rows("conditions", 60));
        let sheet = render(&m);
        assert!(sheet.pages <= MAX_PAGES, "{lang}: {} pages", sheet.pages);
        let lines = check_sheet(&m, lang);
        let more = if lang == "es" {
            "más en el registro"
        } else {
            "more in the record"
        };
        assert!(lines.iter().any(|l| l.contains(more)), "{lang}: {lines:#?}");
        let on_paper: Vec<&str> = lines
            .iter()
            .flat_map(|l| l.split_whitespace())
            .filter(|w| w.starts_with("src_"))
            .collect();
        for id in &sheet.printed {
            assert!(on_paper.contains(&id.as_str()), "{lang}: {id} reported but not printed");
        }
        for id in &on_paper {
            assert!(
                sheet.printed.iter().any(|p| p == id),
                "{lang}: {id} printed but not reported"
            );
        }
        assert!(
            !sheet.printed.iter().any(|p| p == "src_fake_med_129"),
            "{lang}: a cut medicine's src is reported"
        );
    }
    Ok(())
}

async fn five_year_profile(s: &Seeded) -> anyhow::Result<Uuid> {
    let mut groups: Vec<(String, Option<String>)> = load_pack()?
        .analytes
        .iter()
        .map(|(g, a)| (g.clone(), a["loinc"].get(0).map(ownhealth_rules::pack::text)))
        .collect();
    let n = groups.len();
    groups.extend((0..40 - n).map(|i| (format!("extra_{i:02}"), None)));
    let mut tx = s.pool().begin().await?;
    let h = create_household(&mut tx, "Ortega (summary)").await?;
    let pid = create_profile(
        &mut tx,
        &ProfileIn {
            household_id: h,
            slug: "carmen5".into(),
            display_name: "Carmen Ortega".into(),
            relationship: Some("self".into()),
            birth_date: NaiveDate::from_ymd_opt(1955, 1, 30),
            sex_at_birth: Some("female".into()),
            country: Some("ES".into()),
            user_id: None,
            unit_pref: None,
        },
    )
    .await?;
    for m in 0..60 {
        let (y, mo) = ((2021 * 12 + 10 + m) / 12, (2021 * 12 + 10 + m) % 12); // 2021-11 .. 2026-10
        let day = Utc
            .with_ymd_and_hms(y, mo as u32 + 1, 3, 8, 0, 0)
            .single()
            .unwrap_or_default();
        let src = SrcIn {
            profile_id: Some(pid),
            label: Some(format!("LabNorte · {} · p1", day.date_naive())),
            ..SrcIn::new("manual")
        };
        let src = create_src(&mut tx, &src).await?;
        for (i, (g, loinc)) in groups.iter().enumerate() {
            let v = if m == 59 {
                90 + i
            } else {
                50 + (m as usize * 7 + i * 13) % 40
            };
            let obs = ObservationIn::from_json(json!({
                "profile_id": pid, "printed_name": g, "printed_value": v.to_string(), "printed_unit": "U/L",
                "printed_ref_range": "40-80", "value_num": v.to_string(), "ref_low": "40", "ref_high": "80",
                "loinc": loinc, "alias_group": g, "unit_ucum": "U/L", "value_canonical": v.to_string(),
                "unit_canonical": "U/L", "interpretation": if v > 80 { "H" } else { "N" },
                "derivation_version": "summary", "effective_at": day.to_rfc3339(),
            }))?;
            commit_observation(&mut tx, &obs, &SrcRef::Id(src.clone()), CommitOptions::new("auto")).await?;
        }
    }
    tx.commit().await?;
    Ok(pid)
}
