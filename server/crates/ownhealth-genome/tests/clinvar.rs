//! The ClinVar diff (genomics.md 9) against two synthetic mini releases written as files: the first
//! run compares with the facts, the second with the first run's snapshot. All data is synthetic.

mod common;

use std::io::Write;
use std::path::{Path, PathBuf};

use common::{Env, R, env};
use noodles::bgzf;
use ownhealth_genome::clinvar::{
    self, Before, FactAlleles, VcfAlleles, alerts, carried_records, event_kind, is_sf_gene, summary,
};
use ownhealth_genome::sensitive::{acmg_sf_genes, carrier_spans_overlapping, sensitive_gene_at, sensitive_genes};
use ownhealth_genome::synthetic::{self, VcfRow, write_vcf};
use ownhealth_genome::{FindingsQuery, Gate, list_findings};
use serde_json::{Value, json};
use sqlx::PgConnection;
use uuid::Uuid;

/// (chrom, pos, VariationID, REF, ALT, CLNSIG, CLNREVSTAT, gene)
type Cv<'a> = (&'a str, i64, i64, &'a str, &'a str, &'a str, &'a str, &'a str);

const EXPERT: &str = "reviewed_by_expert_panel";
const SINGLE: &str = "criteria_provided,_single_submitter";
const MULTI: &str = "criteria_provided,_multiple_submitters,_no_conflicts";
const NONE: &str = "no_assertion_criteria_provided";

// Alex's carried sites (synthetic.rs alex_facts) plus three this test adds to his calls VCF.
const CFTR: (&str, i64, &str, &str) = ("7", 117559590, "ATCT", "A"); // an indel, VariationID 7105
const MTHFR: (&str, i64, &str, &str) = ("1", 11796321, "G", "A");
const LCT: (&str, i64, &str, &str) = ("2", 135851076, "G", "A");
const CYP2C19: (&str, i64, &str, &str) = ("10", 94781859, "G", "A");
const BRCA2: (&str, i64, &str, &str) = ("13", 32338000, "A", "G");
const MULTI_POS: i64 = 5_000_000; // chr11, REF GT, ALT G,GTT, GT 1/2
const DPYD_REF: (&str, i64, &str, &str) = ("1", 97450058, "C", "T"); // Alex is ref here
const NO_CALL: (&str, i64, &str, &str) = ("3", 1_000_500, "A", "G"); // Alex's calls VCF has ./. here

fn cv<'a>(site: (&'a str, i64, &'a str, &'a str), vid: i64, sig: &'a str, rev: &'a str, gene: &'a str) -> Cv<'a> {
    (site.0, site.1, vid, site.2, site.3, sig, rev, gene)
}

/// Release one (2026-10-05): what the facts already say, plus a planted LP at MTHFR.
fn release_one() -> Vec<Cv<'static>> {
    vec![
        cv(MTHFR, 900030, "Likely_pathogenic", SINGLE, "MTHFR"),
        cv(DPYD_REF, 900040, "Pathogenic", MULTI, "DPYD"),
        cv(LCT, 900020, "Benign", MULTI, "LCT"),
        ("11", MULTI_POS, 900001, "GT", "G", "Benign", SINGLE, "SYNTH1"),
        (
            "11",
            MULTI_POS,
            900002,
            "G",
            "GT",
            "Uncertain_significance",
            SINGLE,
            "SYNTH1",
        ),
        cv(BRCA2, 900010, "Uncertain_significance", SINGLE, "BRCA2"),
        cv(CYP2C19, 12348, "drug_response", EXPERT, "CYP2C19"),
        cv(CFTR, 7105, "Pathogenic", EXPERT, "CFTR"),
        cv(NO_CALL, 900050, "Pathogenic", MULTI, "SYNTH3"),
    ]
}

/// Release two (2026-10-12): one change of every kind.
fn release_two() -> Vec<Cv<'static>> {
    vec![
        cv(MTHFR, 900030, "Likely_pathogenic", MULTI, "MTHFR"), // stars 1 -> 2: silent
        cv(DPYD_REF, 900040, "Pathogenic", MULTI, "DPYD"),
        cv(LCT, 900020, "Likely_pathogenic", SINGLE, "LCT"), // upgrade, 1 star: alert
        ("11", MULTI_POS, 900001, "GT", "G", "Benign", SINGLE, "SYNTH1"),
        ("11", MULTI_POS, 900002, "G", "GT", "Likely_pathogenic", NONE, "SYNTH1"), // upgrade, 0 stars
        cv(BRCA2, 900010, "Likely_benign", SINGLE, "BRCA2"),                       // SF gene: alert
        cv(CYP2C19, 12348, "drug_response", EXPERT, "CYP2C19"),
        cv(CFTR, 7105, "Uncertain_significance", SINGLE, "CFTR"), // downgrade of a shown P: alert
        cv(NO_CALL, 900050, "Likely_pathogenic", EXPERT, "SYNTH3"), // not covered: nothing
    ]
}

fn rank(chrom: &str) -> u8 {
    clinvar::contig_rank(chrom).unwrap_or(99)
}

/// A ClinVar-shaped GRCh38 VCF, sorted as ClinVar sorts it; bgzipped when `bgzip`.
fn write_release(dir: &Path, name: &str, date: &str, rows: &[Cv<'_>], bgzip: bool) -> anyhow::Result<PathBuf> {
    let mut rows = rows.to_vec();
    rows.sort_by_key(|r| (rank(r.0), r.1, r.2));
    let mut text = format!(
        "##fileformat=VCFv4.1\n##fileDate={date}\n##source=ClinVar (synthetic test release)\n\
         #CHROM\tPOS\tID\tREF\tALT\tQUAL\tFILTER\tINFO\n"
    );
    for (chrom, pos, vid, r, a, sig, rev, gene) in rows {
        text.push_str(&format!(
            "{chrom}\t{pos}\t{vid}\t{r}\t{a}\t.\t.\tALLELEID={vid};CLNREVSTAT={rev};CLNSIG={sig};GENEINFO={gene}:1\n"
        ));
    }
    let path = dir.join(name);
    if bgzip {
        let mut w = bgzf::io::Writer::new(std::fs::File::create(&path)?);
        w.write_all(text.as_bytes())?;
        w.finish()?;
    } else {
        std::fs::write(&path, text)?;
    }
    Ok(path)
}

/// Alex's calls VCF as synthetic.rs builds it, plus a BRCA2 site and a multiallelic site.
fn alex_calls() -> Vec<VcfRow> {
    let mut rows = Vec::new();
    for fact in synthetic::alex_facts()["facts"].as_array().into_iter().flatten() {
        for v in fact.get("variants").and_then(Value::as_array).into_iter().flatten() {
            if v["genotype"]["call_state"] == "alt" {
                let s = |k: &str| v[k].as_str().unwrap_or_default().to_string();
                rows.push(VcfRow::new(
                    &s("chrom"),
                    v["pos"].as_i64().unwrap_or_default(),
                    &s("ref"),
                    &s("alt"),
                    "0/1",
                ));
            }
        }
    }
    rows.push(VcfRow::new("chr13", BRCA2.1, BRCA2.2, BRCA2.3, "0/1"));
    rows.push(VcfRow::new("chr11", MULTI_POS, "GT", "G,GTT", "1/2"));
    rows.push(VcfRow::new("chr3", NO_CALL.1, NO_CALL.2, NO_CALL.3, "./."));
    rows
}

async fn seed_alex(e: &Env, c: &mut PgConnection) -> anyhow::Result<()> {
    let raw = vec![(
        "calls_vcf".to_string(),
        write_vcf("AR-01.vcf.gz", &alex_calls(), "AR-01")?,
    )];
    let (facts, att) = synthetic::with_files(&synthetic::alex_facts(), &e.blobs, &raw)?;
    e.seed(c, "alex", &facts, &att).await?;
    Ok(())
}

/// Sam's chip: an LCT call and a DPYD site the array does not probe.
fn sam_chip() -> Value {
    let mut doc = synthetic::sam_facts();
    doc["assay"] = json!({"build": "GRCh38", "type": "chip", "platform": "synthetic array v1"});
    let site = |(chrom, pos, r, a): (&str, i64, &str, &str), state: &str, gt: &str| {
        json!({"chrom": format!("chr{chrom}"), "pos": pos, "ref": r, "alt": a,
               "vrs_id": synthetic::vrs_id(&format!("chr{chrom}"), pos, r, a),
               "genotype": {"call_state": state, "gt": gt}})
    };
    let lct = site(LCT, "alt", "0/1");
    let dpyd = site(DPYD_REF, "not_on_array", "./.");
    doc["facts"] = json!([
        {"id": ownhealth_genome::fact_id("trait", "LCT", lct["vrs_id"].as_str().unwrap_or_default()),
         "category": "trait", "gene": "LCT", "variants": [lct],
         "value": {"trait": "lactase persistence", "call": "likely"},
         "summary": "LCT -13910 C/T: lactase persistence likely.",
         "evidence": {"level": "E2", "native": "replicated association"},
         "confidence": {"level": "medium", "basis": "array call"}, "sensitivity": {"level": "normal"}},
        {"id": ownhealth_genome::fact_id("trait", "DPYD", dpyd["vrs_id"].as_str().unwrap_or_default()),
         "category": "trait", "gene": "DPYD", "variants": [dpyd],
         "value": {"trait": "synthetic marker", "call": "unknown"},
         "summary": "Not on this array.",
         "evidence": {"level": "E3", "native": "synthetic"},
         "confidence": {"level": "low", "basis": "not probed"}, "sensitivity": {"level": "normal"}},
    ]);
    doc
}

async fn seed_family(e: &Env, c: &mut PgConnection) -> anyhow::Result<()> {
    seed_alex(e, c).await?;
    e.seed(c, "lucia", &synthetic::lucia_facts(), &[]).await?;
    e.seed(c, "sam", &sam_chip(), &[]).await?;
    Ok(())
}

#[derive(Debug, Clone, PartialEq, sqlx::FromRow)]
struct Ev {
    slug: String,
    variation_id: i64,
    kind: String,
    alerted: bool,
    release_from: Option<String>,
    release_to: String,
}

async fn events(c: &mut PgConnection) -> anyhow::Result<Vec<Ev>> {
    Ok(sqlx::query_as(
        "select p.slug, e.variation_id, e.kind, e.alerted, e.release_from, e.release_to from genome_event e \
         join profile p on p.id = e.profile_id order by p.slug, e.release_to, e.variation_id",
    )
    .fetch_all(c)
    .await?)
}

fn ev(slug: &str, vid: i64, kind: &str, alerted: bool, from: &str, to: &str) -> Ev {
    Ev {
        slug: slug.into(),
        variation_id: vid,
        kind: kind.into(),
        alerted,
        release_from: Some(from.into()),
        release_to: to.into(),
    }
}

async fn alerts_of(c: &mut PgConnection, profile: Uuid) -> anyhow::Result<Vec<Value>> {
    Ok(sqlx::query_scalar(
        "select payload from review_item where kind = 'genome_reinterpretation' and profile_id = $1 \
         order by payload->>'event_id'",
    )
    .bind(profile)
    .fetch_all(c)
    .await?)
}

/// The review items of one event kind, found through genome_event: the item itself does not say.
async fn alert_for(c: &mut PgConnection, profile: Uuid, kind: &str) -> anyhow::Result<Vec<Value>> {
    Ok(sqlx::query_scalar(
        "select r.payload from review_item r join genome_event g on g.id = r.ref_id \
         where r.kind = 'genome_reinterpretation' and r.profile_id = $1 and g.kind = $2 order by g.release_to",
    )
    .bind(profile)
    .bind(kind)
    .fetch_all(c)
    .await?)
}

/// A review payload names no gene, VariationID, class or the way the class moved: the reader's gate
/// decides that at read time.
fn names_nothing(item: &Value, words: &[&str]) {
    for key in ["gene", "variation_id", "from", "to", "event"] {
        assert!(item.get(key).is_none(), "{key} in {item}");
    }
    let text = item.to_string();
    for w in words.iter().copied().chain(clinvar::EVENT_KINDS) {
        assert!(!text.contains(w), "{w} in {text}");
    }
}

fn date(s: &str) -> chrono::NaiveDate {
    s.parse().unwrap_or_default()
}

#[tokio::test]
async fn two_releases_fire_exactly_on_the_three_alert_classes() -> R {
    let e = env().await?;
    let mut c = e.conn().await?;
    seed_family(&e, &mut c).await?;
    let one = write_release(
        e.dir.path(),
        "clinvar_20261005.vcf.gz",
        "20261005",
        &release_one(),
        true,
    )?;
    let two = write_release(
        e.dir.path(),
        "clinvar_20261012.vcf",
        "2026-10-12",
        &release_two(),
        false,
    )?;

    // First run: the facts are the baseline. Only the planted MTHFR LP (1 star, no fact) is new.
    let r1 = clinvar::diff_release(&e.db.pool, &e.blobs, &one).await?;
    assert_eq!(r1.release.to_string(), "2026-10-05");
    assert_eq!(r1.datasets.len(), 3);
    assert!(r1.datasets.iter().all(|d| d.skipped.is_none()), "{r1:?}");
    assert_eq!(
        events(&mut c).await?,
        vec![ev("alex", 900030, "upgraded_to_plp", true, "2026-09-28", "2026-10-05")]
    );
    let alex = e.profile("alex");
    let planted = alerts_of(&mut c, alex).await?;
    assert_eq!(planted.len(), 1);
    // The review item names nothing: the gene and classes come from the gated events route.
    assert_eq!(planted[0]["withheld"], false);
    assert_eq!(planted[0]["summary"], "A result you carry changed in ClinVar.");
    assert_eq!(
        planted[0]["summary_es"],
        "Ha cambiado en ClinVar un resultado que tienes."
    );
    names_nothing(&planted[0], &["MTHFR", "900030"]);

    // The snapshot holds exactly the carried records: the indel, both alleles of the multiallelic
    // site, and nothing at the ref site (Alex), the no-call site (Alex, 900050) or the not_on_array
    // site (Sam's chip).
    let snap: Vec<(String, i64)> = sqlx::query_as(
        "select p.slug, c.variation_id from clinvar_carried c join genome_dataset d on d.id = c.dataset_id \
         join profile p on p.id = d.profile_id order by 1, 2",
    )
    .fetch_all(&mut *c)
    .await?;
    let want: Vec<(String, i64)> = [
        ("alex", 7105),
        ("alex", 12348),
        ("alex", 900001),
        ("alex", 900002),
        ("alex", 900010),
        ("alex", 900020),
        ("alex", 900030),
        ("lucia", 7105),
        ("sam", 900020),
    ]
    .iter()
    .map(|(s, v)| (s.to_string(), *v))
    .collect();
    assert_eq!(snap, want);

    // Second run: diffed against the snapshot.
    let r2 = clinvar::diff_release(&e.db.pool, &e.blobs, &two).await?;
    assert!(r2.datasets.iter().all(|d| d.skipped.is_none()), "{r2:?}");
    let (a, b) = ("2026-10-05", "2026-10-12");
    assert_eq!(
        events(&mut c).await?,
        vec![
            ev("alex", 900030, "upgraded_to_plp", true, "2026-09-28", a),
            ev("alex", 7105, "downgraded_from_plp", true, a, b),
            ev("alex", 900002, "upgraded_to_plp", false, a, b), // 0 stars: an event, no alert
            ev("alex", 900010, "vus_resolved", true, a, b),     // ACMG SF gene
            ev("alex", 900020, "upgraded_to_plp", true, a, b),
            ev("alex", 900030, "stars_changed", false, a, b),
            ev("lucia", 7105, "downgraded_from_plp", false, a, b), // a minor's carrier fact is not shown
            ev("sam", 900020, "upgraded_to_plp", true, a, b),
        ]
    );

    let items = alerts_of(&mut c, alex).await?;
    assert_eq!(items.len(), 4);
    let sf = alert_for(&mut c, alex, "vus_resolved").await?;
    assert_eq!(sf.len(), 1);
    assert_eq!(sf[0]["withheld"], true);
    assert_eq!(sf[0]["summary"], "A withheld result changed in ClinVar.");
    assert_eq!(
        sf[0]["summary_es"],
        "Ha cambiado en ClinVar un resultado que está oculto."
    );
    let text = sf[0].to_string();
    assert!(!text.contains("BRCA2") && !text.contains("900010"), "{text}");
    let down = alert_for(&mut c, alex, "downgraded_from_plp").await?;
    names_nothing(&down[0], &["CFTR", "7105"]);
    for item in &items {
        names_nothing(item, &["MTHFR", "LCT", "BRCA2", "CFTR"]);
    }
    // The item's reasons are as neutral as its payload.
    let reasons: Vec<Vec<String>> =
        sqlx::query_scalar("select reasons from review_item where kind = 'genome_reinterpretation'")
            .fetch_all(&mut *c)
            .await?;
    assert_eq!(reasons.len(), 5, "{reasons:?}");
    assert!(reasons.iter().all(|r| r == &["clinvar_change"]), "{reasons:?}");

    let sam = alerts_of(&mut c, e.profile("sam")).await?;
    assert_eq!(sam.len(), 1);
    assert_eq!(sam[0]["unconfirmed"], true);
    assert!(
        sam[0]["summary"]
            .as_str()
            .is_some_and(|s| s.ends_with("It comes from a chip and is unconfirmed: worth a clinical test.")),
        "{}",
        sam[0]
    );
    assert!(
        sam[0]["summary_es"]
            .as_str()
            .is_some_and(|s| s.ends_with("merece una prueba clínica."))
    );
    assert!(alerts_of(&mut c, e.profile("lucia")).await?.is_empty());

    // get_genome_findings' clinvar_now: the CFTR finding now reads what release two says.
    let mut found = list_findings(&mut c, alex, &Gate::new(false, false), &FindingsQuery::default()).await?;
    clinvar::attach_now(&mut c, alex, date(b), &Gate::new(false, false), &mut found).await?;
    let cftr_of = |found: &Value| {
        found["findings"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|f| f["gene"] == "CFTR")
            .cloned()
            .unwrap_or_default()
    };
    // Only our own class vocabulary reaches an agent, never the release's free text.
    assert_eq!(
        cftr_of(&found)["clinvar_now"],
        json!({"class": "vus", "stars": 1, "release": b})
    );
    // A carried record ClinVar files under a sensitive gene stays behind the reader's gate, as its
    // event does, even when the finding citing it is not sensitive.
    sqlx::query("update clinvar_carried set gene = 'BRCA2' where variation_id = 7105")
        .execute(&mut *c)
        .await?;
    for (include_sensitive, shown) in [(false, false), (true, true)] {
        let gate = Gate::new(include_sensitive, false);
        let mut found = list_findings(&mut c, alex, &gate, &FindingsQuery::default()).await?;
        clinvar::attach_now(&mut c, alex, date(b), &gate, &mut found).await?;
        assert_eq!(cftr_of(&found).get("clinvar_now").is_some(), shown, "{found}");
    }
    sqlx::query("update clinvar_carried set gene = 'CFTR' where variation_id = 7105")
        .execute(&mut *c)
        .await?;
    // An as_of before the release compared knows nothing of it.
    let mut past = list_findings(&mut c, alex, &Gate::new(false, false), &FindingsQuery::default()).await?;
    clinvar::attach_now(&mut c, alex, date("2026-10-11"), &Gate::new(false, false), &mut past).await?;
    assert!(!past.to_string().contains("clinvar_now"), "{past}");
    // A retracted genome file's snapshot no longer feeds clinvar_now, as it feeds no event.
    sqlx::query(
        "update document set status = 'retracted' where id in (select document_id from genome_dataset where profile_id = $1)",
    )
    .bind(alex)
    .execute(&mut *c)
    .await?;
    assert!(clinvar::now_by_variation(&mut c, alex, date(b)).await?.is_empty());
    sqlx::query(
        "update document set status = 'active' where id in (select document_id from genome_dataset where profile_id = $1)",
    )
    .bind(alex)
    .execute(&mut *c)
    .await?;

    // The same release again, or an older one, compares nothing.
    let again = clinvar::diff_release(&e.db.pool, &e.blobs, &two).await?;
    assert!(
        again.datasets.iter().all(|d| d.skipped == Some("already_compared")),
        "{again:?}"
    );
    let older = clinvar::diff_release(&e.db.pool, &e.blobs, &one).await?;
    assert!(older.datasets.iter().all(|d| d.skipped == Some("already_compared")));
    assert_eq!(events(&mut c).await?.len(), 8);
    Ok(())
}

#[tokio::test]
async fn an_sf_gene_is_named_once_the_sensitive_switch_is_on() -> R {
    let e = env().await?;
    let mut c = e.conn().await?;
    seed_alex(&e, &mut c).await?;
    sqlx::query("update profile set genome_sensitive_ui = true where id = $1")
        .bind(e.profile("alex"))
        .execute(&mut *c)
        .await?;
    let one = write_release(e.dir.path(), "one.vcf", "20261005", &release_one(), false)?;
    let two = write_release(e.dir.path(), "two.vcf", "20261012", &release_two(), false)?;
    clinvar::diff_release(&e.db.pool, &e.blobs, &one).await?;
    clinvar::diff_release(&e.db.pool, &e.blobs, &two).await?;
    let sf = alert_for(&mut c, e.profile("alex"), "vus_resolved").await?;
    assert_eq!(sf[0]["withheld"], false);
    // Even with the switch on the item names nothing: the switch can be turned off later, and the
    // reviewer may be a caregiver without read:genome.
    names_nothing(&sf[0], &["BRCA2", "900010"]);
    Ok(())
}

#[test]
fn the_named_sentence_reads_well_for_every_class() {
    let vus = Before {
        class: "vus",
        stars: Some(1),
    };
    let plp = Before {
        class: "plp",
        stars: Some(3),
    };
    fn named(old: Option<&Before>, new_class: &str, lang: &str) -> String {
        summary(
            &clinvar::EventText {
                gene: Some("BRCA2"),
                old,
                new_class,
                new_stars: 1,
                withheld: false,
                unconfirmed: false,
            },
            lang,
        )
    }
    assert_eq!(
        named(Some(&vus), "blb", "en"),
        "ClinVar's classification of a variant you carry in BRCA2 is now: benign or likely benign (1 star). \
         Before: uncertain significance (1 star)."
    );
    assert_eq!(
        named(Some(&plp), "vus", "es"),
        "La clasificación en ClinVar de una variante que tienes en BRCA2 es ahora: significado incierto \
         (1 estrella). Antes: patogénica o probablemente patogénica (3 estrellas)."
    );
    assert_eq!(
        named(Some(&plp), "conflicting", "en"),
        "ClinVar's classification of a variant you carry in BRCA2 is now: conflicting classifications \
         (1 star). Before: pathogenic or likely pathogenic (3 stars)."
    );
    let plp_one = Before {
        class: "plp",
        stars: Some(1),
    };
    assert_eq!(
        named(Some(&plp_one), "plp", "en"),
        "ClinVar changed its classification of a variant you carry in BRCA2 within the same group. It is \
         now: pathogenic or likely pathogenic (1 star)."
    );
    assert_eq!(
        named(Some(&plp_one), "plp", "es"),
        "ClinVar ha cambiado su clasificación de una variante que tienes en BRCA2 dentro del mismo grupo. \
         Es ahora: patogénica o probablemente patogénica (1 estrella)."
    );
    assert_eq!(
        named(None, "plp", "es"),
        "La clasificación en ClinVar de una variante que tienes en BRCA2 es ahora: patogénica o probablemente \
         patogénica (1 estrella). Antes: sin informar en tu archivo genómico."
    );
}

/// Alex's calls VCF rewritten in text order (chrM first, then chr1, chr10...), as some producers sort.
async fn unsort_alex_calls(e: &Env, c: &mut PgConnection) -> anyhow::Result<()> {
    let sha: String = sqlx::query_scalar(
        "select encode(vcf_blob, 'hex') from genome_dataset where profile_id = $1 and status = 'current'",
    )
    .bind(e.profile("alex"))
    .fetch_one(&mut *c)
    .await?;
    let path = e.blobs.path(&sha)?;
    let mut text = "##fileformat=VCFv4.2\n#CHROM\tPOS\tID\tREF\tALT\tQUAL\tFILTER\tINFO\tFORMAT\tAR-01\n".to_string();
    text.push_str("chrM\t100\t.\tA\tG\t50\tPASS\t.\tGT\t1/1\n");
    for r in alex_calls() {
        text.push_str(&format!(
            "{}\t{}\t.\t{}\t{}\t50\tPASS\t.\tGT\t{}\n",
            r.chrom, r.pos, r.reference, r.alt, r.gt
        ));
    }
    let mut perms = std::fs::metadata(&path)?.permissions();
    #[allow(clippy::permissions_set_readonly_false)]
    perms.set_readonly(false);
    std::fs::set_permissions(&path, perms)?;
    std::fs::write(&path, text)?;
    Ok(())
}

#[tokio::test]
async fn one_unreadable_calls_vcf_does_not_stop_the_other_datasets() -> R {
    let e = env().await?;
    let mut c = e.conn().await?;
    seed_family(&e, &mut c).await?;
    unsort_alex_calls(&e, &mut c).await?;
    let one = write_release(e.dir.path(), "one.vcf", "20261005", &release_one(), false)?;
    let two = write_release(e.dir.path(), "two.vcf", "20261012", &release_two(), false)?;
    let r1 = clinvar::diff_release(&e.db.pool, &e.blobs, &one).await?;
    let alex = r1.datasets.iter().find(|d| d.profile == "alex").cloned();
    let alex = alex.ok_or_else(|| anyhow::anyhow!("no alex in {r1:?}"))?;
    assert_eq!(alex.skipped, Some("unreadable"), "{r1:?}");
    assert!(
        alex.error.as_deref().is_some_and(|m| m.contains("not sorted")),
        "{alex:?}"
    );
    assert_eq!(r1.datasets.len(), 3);
    clinvar::diff_release(&e.db.pool, &e.blobs, &two).await?;
    // Nothing partial for Alex; Sam, compared after him, still gets his LCT alert.
    assert_eq!(
        events(&mut c).await?,
        vec![
            ev("lucia", 7105, "downgraded_from_plp", false, "2026-10-05", "2026-10-12"),
            ev("sam", 900020, "upgraded_to_plp", true, "2026-10-05", "2026-10-12"),
        ]
    );
    let n: i64 = sqlx::query_scalar(
        "select count(*) from clinvar_carried c join genome_dataset d on d.id = c.dataset_id where d.profile_id = $1",
    )
    .bind(e.profile("alex"))
    .fetch_one(&mut *c)
    .await?;
    assert_eq!(n, 0);
    Ok(())
}

#[tokio::test]
async fn a_broken_release_stops_the_run_and_blames_no_genome() -> R {
    let e = env().await?;
    let mut c = e.conn().await?;
    seed_family(&e, &mut c).await?;
    // A release whose last record is out of order, after a chrM record past every allele the family
    // carries: each dataset stops reading before it, so only a full pass over the release sees it.
    let path = write_release(e.dir.path(), "broken.vcf", "20261005", &release_one(), false)?;
    let mut f = std::fs::OpenOptions::new().append(true).open(&path)?;
    f.write_all(b"MT\t16000\t2\tA\tG\t.\t.\tCLNREVSTAT=no_assertion_provided;CLNSIG=Benign\n")?;
    f.write_all(b"1\t1\t1\tA\tG\t.\t.\tCLNREVSTAT=no_assertion_provided;CLNSIG=Benign\n")?;
    drop(f);
    let r = clinvar::diff_release(&e.db.pool, &e.blobs, &path).await;
    let err = match r {
        Ok(report) => anyhow::bail!("a broken release was accepted: {report:?}"),
        Err(err) => err.to_string(),
    };
    assert!(err.contains("broken.vcf") && err.contains("not sorted"), "{err}");
    assert!(events(&mut c).await?.is_empty());
    let n: i64 = sqlx::query_scalar("select count(*) from clinvar_carried")
        .fetch_one(&mut *c)
        .await?;
    assert_eq!(n, 0, "no dataset may take a broken release as its snapshot");
    Ok(())
}

#[tokio::test]
async fn one_change_alerts_once_when_a_profile_has_two_current_datasets() -> R {
    let e = env().await?;
    let mut c = e.conn().await?;
    seed_alex(&e, &mut c).await?;
    // Alex also has a chip (another sample, so both datasets stay current) that carries LCT.
    let mut chip = sam_chip();
    chip["subject"] = synthetic::alex_facts()["subject"].clone();
    chip["subject"]["sample_id"] = json!("AR-CHIP");
    e.seed(&mut c, "alex", &chip, &[]).await?;
    let current: i64 =
        sqlx::query_scalar("select count(*) from genome_dataset where profile_id = $1 and status = 'current'")
            .bind(e.profile("alex"))
            .fetch_one(&mut *c)
            .await?;
    assert_eq!(current, 2);
    let one = write_release(e.dir.path(), "one.vcf", "20261005", &release_one(), false)?;
    let two = write_release(e.dir.path(), "two.vcf", "20261012", &release_two(), false)?;
    clinvar::diff_release(&e.db.pool, &e.blobs, &one).await?;
    clinvar::diff_release(&e.db.pool, &e.blobs, &two).await?;
    // Two LCT events, one per dataset, and one review item: the WGS one, which is confirmed.
    let lct: Vec<Ev> = events(&mut c)
        .await?
        .into_iter()
        .filter(|x| x.variation_id == 900020)
        .collect();
    assert_eq!(lct.len(), 2, "{lct:?}");
    let items = alert_for(&mut c, e.profile("alex"), "upgraded_to_plp").await?;
    let lct_items: Vec<&Value> = items.iter().filter(|p| p["release_to"] == "2026-10-12").collect();
    assert_eq!(lct_items.len(), 1, "{items:?}");
    assert_eq!(lct_items[0]["unconfirmed"], false);
    Ok(())
}

/// Sam's chip filed as Alex's second sample: it carries LCT.
fn alex_chip() -> Value {
    let mut chip = sam_chip();
    chip["subject"] = synthetic::alex_facts()["subject"].clone();
    chip["subject"]["sample_id"] = json!("AR-CHIP");
    chip
}

/// (status, unconfirmed, event's assay) of every LCT item of Alex that release two raised.
async fn lct_items(c: &mut PgConnection, alex: Uuid) -> anyhow::Result<Vec<(String, bool, String)>> {
    Ok(sqlx::query_as(
        "select r.status, (r.payload->>'unconfirmed')::boolean, d.assay from review_item r \
         join genome_event g on g.id = r.ref_id join genome_dataset d on d.id = g.dataset_id \
         where r.kind = 'genome_reinterpretation' and r.profile_id = $1 and g.variation_id = 900020 \
         and g.release_to = '2026-10-12' order by r.created_at",
    )
    .bind(alex)
    .fetch_all(c)
    .await?)
}

/// A dismissed alert stays dismissed: a dataset imported later that sees the same change in the same
/// release raises nothing new. Only when the alert's own genome file is retracted does the other
/// dataset raise it again.
#[tokio::test]
async fn a_dismissed_alert_is_not_raised_again_by_a_later_dataset() -> R {
    let e = env().await?;
    let mut c = e.conn().await?;
    seed_alex(&e, &mut c).await?;
    let alex = e.profile("alex");
    let one = write_release(e.dir.path(), "one.vcf", "20261005", &release_one(), false)?;
    let two = write_release(e.dir.path(), "two.vcf", "20261012", &release_two(), false)?;
    clinvar::diff_release(&e.db.pool, &e.blobs, &one).await?;
    clinvar::diff_release(&e.db.pool, &e.blobs, &two).await?;
    sqlx::query(
        "update review_item set status = 'dismissed', decided_at = now() where kind = 'genome_reinterpretation' \
         and profile_id = $1",
    )
    .bind(alex)
    .execute(&mut *c)
    .await?;
    e.seed(&mut c, "alex", &alex_chip(), &[]).await?;
    clinvar::diff_release(&e.db.pool, &e.blobs, &two).await?;
    assert_eq!(
        lct_items(&mut c, alex).await?,
        vec![("dismissed".to_string(), false, "wgs".to_string())]
    );
    // The WGS file is retracted (its open items would be dismissed), then a new chip sees the change:
    // the retracted file's dismissed alert no longer stands for it.
    sqlx::query(
        "update document set status = 'retracted' where id in \
         (select document_id from genome_dataset where profile_id = $1 and assay = 'wgs')",
    )
    .bind(alex)
    .execute(&mut *c)
    .await?;
    let mut chip2 = alex_chip();
    chip2["subject"]["sample_id"] = json!("AR-CHIP-2");
    e.seed(&mut c, "alex", &chip2, &[]).await?;
    clinvar::diff_release(&e.db.pool, &e.blobs, &two).await?;
    let items = lct_items(&mut c, alex).await?;
    assert_eq!(items.len(), 2, "{items:?}");
    assert_eq!(items[1], ("open".to_string(), true, "chip".to_string()));
    Ok(())
}

/// A chip compared before a genome file of the same person: when the genome file later sees the same
/// change in the same release, the chip's open alert becomes the confirmed one, without the chip
/// wording, instead of staying unconfirmed beside a skipped confirmation.
#[tokio::test]
async fn a_later_genome_file_confirms_an_open_chip_alert() -> R {
    let e = env().await?;
    let mut c = e.conn().await?;
    let alex = e.profile("alex");
    e.seed(&mut c, "alex", &alex_chip(), &[]).await?;
    let one = write_release(e.dir.path(), "one.vcf", "20261005", &release_one(), false)?;
    let two = write_release(e.dir.path(), "two.vcf", "20261012", &release_two(), false)?;
    clinvar::diff_release(&e.db.pool, &e.blobs, &one).await?;
    clinvar::diff_release(&e.db.pool, &e.blobs, &two).await?;
    assert_eq!(
        lct_items(&mut c, alex).await?,
        vec![("open".to_string(), true, "chip".to_string())]
    );
    seed_alex(&e, &mut c).await?;
    clinvar::diff_release(&e.db.pool, &e.blobs, &two).await?;
    assert_eq!(
        lct_items(&mut c, alex).await?,
        vec![("open".to_string(), false, "wgs".to_string())]
    );
    let item: (Value, Option<Uuid>) = sqlx::query_as(
        "select r.payload, r.document_id from review_item r join genome_event g on g.id = r.ref_id \
         where r.profile_id = $1 and g.variation_id = 900020 and g.release_to = '2026-10-12'",
    )
    .bind(alex)
    .fetch_one(&mut *c)
    .await?;
    assert_eq!(item.0["summary"], "A result you carry changed in ClinVar.");
    assert_eq!(item.0["summary_es"], "Ha cambiado en ClinVar un resultado que tienes.");
    let wgs_doc: Option<Uuid> =
        sqlx::query_scalar("select document_id from genome_dataset where profile_id = $1 and assay = 'wgs'")
            .bind(alex)
            .fetch_one(&mut *c)
            .await?;
    assert_eq!(item.1, wgs_doc, "a retraction of the genome file must reach the item");
    Ok(())
}

/// A change inside one class (Pathogenic to Likely_pathogenic, stars unchanged) is an event; in an
/// ACMG SF gene it alerts, as any change there does.
#[tokio::test]
async fn a_change_inside_one_class_is_an_event_and_alerts_in_an_sf_gene() -> R {
    let e = env().await?;
    let mut c = e.conn().await?;
    seed_alex(&e, &mut c).await?;
    let one = write_release(
        e.dir.path(),
        "one.vcf",
        "20261005",
        &[
            cv(MTHFR, 900030, "Pathogenic", MULTI, "MTHFR"),
            cv(BRCA2, 900010, "Pathogenic", MULTI, "BRCA2"),
        ],
        false,
    )?;
    let two = write_release(
        e.dir.path(),
        "two.vcf",
        "20261012",
        &[
            cv(MTHFR, 900030, "Likely_pathogenic", MULTI, "MTHFR"),
            cv(BRCA2, 900010, "Likely_pathogenic", MULTI, "BRCA2"),
        ],
        false,
    )?;
    clinvar::diff_release(&e.db.pool, &e.blobs, &one).await?;
    clinvar::diff_release(&e.db.pool, &e.blobs, &two).await?;
    let (a, b) = ("2026-10-05", "2026-10-12");
    let second: Vec<Ev> = events(&mut c)
        .await?
        .into_iter()
        .filter(|x| x.release_to == b)
        .collect();
    assert_eq!(
        second,
        vec![
            ev("alex", 900010, "significance_changed", true, a, b),
            ev("alex", 900030, "significance_changed", false, a, b),
        ]
    );
    assert_eq!(
        alert_for(&mut c, e.profile("alex"), "significance_changed")
            .await?
            .len(),
        1
    );
    // The same significance written differently is no change.
    let three = write_release(
        e.dir.path(),
        "three.vcf",
        "20261019",
        &[
            cv(MTHFR, 900030, "Likely_Pathogenic", MULTI, "MTHFR"),
            cv(BRCA2, 900010, "Likely_pathogenic", MULTI, "BRCA2"),
        ],
        false,
    )?;
    let r3 = clinvar::diff_release(&e.db.pool, &e.blobs, &three).await?;
    assert_eq!(r3.datasets[0].events, 0, "{r3:?}");
    Ok(())
}

/// A release in which the person carries nothing is still a compared release: a record that appears
/// in the next one is new against it, and the event's release_from is that release, not the facts'.
#[tokio::test]
async fn a_release_where_nothing_is_carried_is_still_the_baseline() -> R {
    let e = env().await?;
    let mut c = e.conn().await?;
    seed_alex(&e, &mut c).await?;
    let one = write_release(
        e.dir.path(),
        "one.vcf",
        "20261005",
        &[cv(DPYD_REF, 900040, "Pathogenic", MULTI, "DPYD")],
        false,
    )?;
    let two = write_release(
        e.dir.path(),
        "two.vcf",
        "20261012",
        &[
            cv(DPYD_REF, 900040, "Pathogenic", MULTI, "DPYD"),
            cv(BRCA2, 900010, "Uncertain_significance", SINGLE, "BRCA2"),
        ],
        false,
    )?;
    let r1 = clinvar::diff_release(&e.db.pool, &e.blobs, &one).await?;
    assert_eq!((r1.datasets[0].carried, r1.datasets[0].events), (0, 0));
    // The same release again is already compared, though nothing was carried in it.
    let again = clinvar::diff_release(&e.db.pool, &e.blobs, &one).await?;
    assert_eq!(again.datasets[0].skipped, Some("already_compared"));
    let r2 = clinvar::diff_release(&e.db.pool, &e.blobs, &two).await?;
    assert_eq!(r2.datasets[0].skipped, None);
    assert_eq!(
        events(&mut c).await?,
        vec![ev(
            "alex",
            900010,
            "new_assertion_on_carried_variant",
            true,
            "2026-10-05",
            "2026-10-12"
        )]
    );
    assert_eq!(
        alert_for(&mut c, e.profile("alex"), "new_assertion_on_carried_variant")
            .await?
            .len(),
        1
    );
    Ok(())
}

/// A minor's finding that carries no sensitive or carrier flag still gets no clinvar_now when its
/// ClinVar record sits in a carrier-panel gene or ClinVar files it under a sensitive gene: the minor
/// rule withholds the record as it withholds its event.
#[tokio::test]
async fn a_minor_sees_no_clinvar_now_for_a_sensitive_or_carrier_record() -> R {
    let e = env().await?;
    let mut c = e.conn().await?;
    // Two plain trait findings, so nothing flags their rows: SYNTH7 inside the CFTR span, SYNTH3 in
    // no carrier or sensitive span.
    let trait_at = |gene: &str, chrom: &str, pos: i64, vid: i64| {
        let site = json!({"chrom": chrom, "pos": pos, "ref": "C", "alt": "T", "clinvar_variation_id": vid,
                          "vrs_id": synthetic::vrs_id(chrom, pos, "C", "T"),
                          "genotype": {"call_state": "alt", "gt": "0/1"}});
        json!({
            "id": ownhealth_genome::fact_id("trait", gene, site["vrs_id"].as_str().unwrap_or_default()),
            "category": "trait", "gene": gene, "variants": [site],
            "value": {"trait": "synthetic marker", "call": "likely"},
            "summary": "A synthetic marker.",
            "evidence": {"level": "E2", "native": "synthetic"},
            "confidence": {"level": "high", "basis": "synthetic"}, "sensitivity": {"level": "normal"}})
    };
    assert!(!carrier_spans_overlapping("chr7", 117_559_000, 117_559_000).is_empty());
    assert!(carrier_spans_overlapping("chr3", 1_000_700, 1_000_700).is_empty());
    assert!(sensitive_gene_at("chr3", 1_000_700).is_none());
    let mut doc = synthetic::lucia_facts();
    if let Some(facts) = doc["facts"].as_array_mut() {
        facts.push(trait_at("SYNTH7", "chr7", 117_559_000, 900070));
        facts.push(trait_at("SYNTH3", "chr3", 1_000_700, 900071));
    }
    e.seed(&mut c, "lucia", &doc, &[]).await?;
    let lucia = e.profile("lucia");
    // SYNTH3's record is filed under BRCA2 by ClinVar.
    sqlx::query(
        "insert into clinvar_carried (dataset_id, release_date, variation_id, gene, significance, stars)          select id, '2026-10-12', v, g, 'Likely_benign', 1 from genome_dataset,          (values (900070::bigint, 'SYNTH7'), (900071, 'BRCA2')) x(v, g) where profile_id = $1",
    )
    .bind(lucia)
    .execute(&mut *c)
    .await?;
    let shown = async |c: &mut PgConnection, gate: Gate| -> anyhow::Result<Vec<(String, bool)>> {
        let mut found = list_findings(&mut *c, lucia, &gate, &FindingsQuery::default()).await?;
        clinvar::attach_now(&mut *c, lucia, date("2026-10-12"), &gate, &mut found).await?;
        Ok(found["findings"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|f| f["gene"] == "SYNTH7" || f["gene"] == "SYNTH3")
            .map(|f| {
                (
                    f["gene"].as_str().unwrap_or_default().to_string(),
                    f.get("clinvar_now").is_some(),
                )
            })
            .collect())
    };
    let both = |s7: bool, s3: bool| vec![("SYNTH3".to_string(), s3), ("SYNTH7".to_string(), s7)];
    let sorted = |mut v: Vec<(String, bool)>| {
        v.sort();
        v
    };
    // Without the minor rule both are shown (the sensitive one needs the switch); under it neither,
    // even with the sensitive switch on.
    assert_eq!(sorted(shown(&mut c, Gate::new(true, false)).await?), both(true, true));
    assert_eq!(sorted(shown(&mut c, Gate::new(false, false)).await?), both(true, false));
    assert_eq!(sorted(shown(&mut c, Gate::new(true, true)).await?), both(false, false));
    assert_eq!(sorted(shown(&mut c, Gate::new(false, true)).await?), both(false, false));
    Ok(())
}

#[tokio::test]
async fn a_release_older_than_the_genome_file_is_not_a_baseline() -> R {
    let e = env().await?;
    let mut c = e.conn().await?;
    seed_alex(&e, &mut c).await?;
    let old = write_release(e.dir.path(), "old.vcf", "20260901", &release_two(), false)?;
    let r = clinvar::diff_release(&e.db.pool, &e.blobs, &old).await?;
    assert_eq!(r.datasets[0].skipped, Some("older_than_genome_file"));
    assert!(events(&mut c).await?.is_empty());
    Ok(())
}

#[test]
fn a_file_without_a_release_date_or_out_of_order_is_refused() -> R {
    let dir = tempfile::tempdir()?;
    let p = dir.path().join("nodate.vcf");
    std::fs::write(
        &p,
        "##fileformat=VCFv4.1\n#CHROM\tPOS\tID\tREF\tALT\tQUAL\tFILTER\tINFO\n",
    )?;
    let err = clinvar::release_date(&p)
        .err()
        .map(|e| e.to_string())
        .unwrap_or_default();
    assert!(err.contains("no ##fileDate"), "{err}");
    let mut rows = release_one();
    rows.sort_by_key(|r| (rank(r.0), r.1));
    rows.reverse();
    let mut text = "##fileDate=20261005\n#CHROM\tPOS\tID\tREF\tALT\tQUAL\tFILTER\tINFO\n".to_string();
    for (chrom, pos, vid, r, a, sig, rev, gene) in rows {
        text.push_str(&format!(
            "{chrom}\t{pos}\t{vid}\t{r}\t{a}\t.\t.\tCLNREVSTAT={rev};CLNSIG={sig};GENEINFO={gene}:1\n"
        ));
    }
    let p = dir.path().join("unsorted.vcf");
    std::fs::write(&p, text)?;
    let mut person = FactAlleles::new(vec![((22, 1), "A".into(), "G".into())]);
    let err = carried_records(&p, &mut person)
        .err()
        .map(|e| e.to_string())
        .unwrap_or_default();
    assert!(err.contains("not sorted"), "{err}");
    Ok(())
}

#[test]
fn a_multiallelic_and_an_indel_site_are_matched_from_a_vcf() -> R {
    let dir = tempfile::tempdir()?;
    let rel = write_release(dir.path(), "r.vcf.gz", "20261005", &release_one(), true)?;
    let calls = write_vcf("AR-01.vcf.gz", &alex_calls(), "AR-01")?;
    let vcf = dir.path().join("calls.vcf.gz");
    std::fs::write(&vcf, &calls.data)?;
    let mut person = VcfAlleles::open(&vcf, "calls")?;
    let (_, carried) = carried_records(&rel, &mut person)?;
    let ids: Vec<i64> = carried.iter().map(|c| c.variation_id).collect();
    assert!(ids.contains(&7105), "the CFTR indel: {ids:?}");
    assert!(
        ids.contains(&900001) && ids.contains(&900002),
        "both alleles of the 1/2 site: {ids:?}"
    );
    assert!(!ids.contains(&900040), "a ref site is not carried: {ids:?}");
    // Only the carried allele: a person 0/1 at the multiallelic site carries GT>G, not G>GT.
    let mut one = calls_with(&dir, &[VcfRow::new("chr11", MULTI_POS, "GT", "G,GTT", "0/1")])?;
    let (_, carried) = carried_records(&rel, &mut one)?;
    assert_eq!(carried.iter().map(|c| c.variation_id).collect::<Vec<_>>(), vec![900001]);
    Ok(())
}

#[test]
fn calls_out_of_order_past_the_release_end_are_still_refused() -> R {
    // The release ends at chr13; the calls go chrM, chrM, then chr7. Reading only up to the release's
    // end would never see the chr7 line, and would miss its carried CFTR allele without a word.
    let dir = tempfile::tempdir()?;
    let rel = write_release(dir.path(), "r.vcf", "20261005", &release_one(), false)?;
    let calls = dir.path().join("calls.vcf");
    std::fs::write(
        &calls,
        format!(
            "##fileformat=VCFv4.2\n#CHROM\tPOS\tID\tREF\tALT\tQUAL\tFILTER\tINFO\tFORMAT\tX\n\
             chrM\t1\t.\tA\tG\t50\tPASS\t.\tGT\t1/1\n\
             chrM\t2\t.\tA\tG\t50\tPASS\t.\tGT\t1/1\n\
             chr7\t{}\t.\t{}\t{}\t50\tPASS\t.\tGT\t0/1\n",
            CFTR.1, CFTR.2, CFTR.3
        ),
    )?;
    let mut person = VcfAlleles::open(&calls, "calls")?;
    let err = carried_records(&rel, &mut person)
        .err()
        .map(|e| e.to_string())
        .unwrap_or_default();
    assert!(err.contains("calls") && err.contains("not sorted"), "{err}");
    Ok(())
}

#[test]
fn a_release_record_with_missing_columns_is_refused() -> R {
    // A row cut after ALT, at a site nobody carries: the full pass over the release must still refuse
    // it, or a later run could take a release with a lost record as its snapshot.
    let dir = tempfile::tempdir()?;
    let p = dir.path().join("partial.vcf");
    std::fs::write(
        &p,
        "##fileDate=20261005\n#CHROM\tPOS\tID\tREF\tALT\tQUAL\tFILTER\tINFO\n\
         1\t100\t1\tA\tG\t.\t.\tCLNREVSTAT=no_assertion_provided;CLNSIG=Benign\n\
         7\t117559590\t7105\tATCT\tA\n",
    )?;
    let mut person = FactAlleles::new(vec![((22, 1), "A".into(), "G".into())]);
    let err = carried_records(&p, &mut person)
        .err()
        .map(|e| e.to_string())
        .unwrap_or_default();
    assert!(err.contains("partial.vcf") && err.contains("columns"), "{err}");
    // A row cut right after a known contig, before POS, is refused too.
    let p = dir.path().join("cut.vcf");
    std::fs::write(
        &p,
        "##fileDate=20261005\n#CHROM\tPOS\tID\tREF\tALT\tQUAL\tFILTER\tINFO\n\
         1\t100\t1\tA\tG\t.\t.\tCLNREVSTAT=no_assertion_provided;CLNSIG=Benign\n\
         7\n",
    )?;
    let mut person = FactAlleles::new(vec![((22, 1), "A".into(), "G".into())]);
    let err = carried_records(&p, &mut person)
        .err()
        .map(|e| e.to_string())
        .unwrap_or_default();
    assert!(err.contains("cut.vcf") && err.contains("columns"), "{err}");
    Ok(())
}

/// Wait until a statement of this test's database waits on a row lock: the run reached its lock.
async fn wait_for_a_lock_wait(pool: &sqlx::PgPool) -> anyhow::Result<()> {
    for _ in 0..600 {
        let waiting: bool = sqlx::query_scalar(
            "select exists (select 1 from pg_stat_activity where datname = current_database() \
             and wait_event_type = 'Lock')",
        )
        .fetch_one(pool)
        .await?;
        if waiting {
            return Ok(());
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    anyhow::bail!("no statement ever waited on a lock")
}

#[tokio::test]
async fn a_dataset_is_compared_by_one_run_at_a_time() -> R {
    let e = env().await?;
    let mut c = e.conn().await?;
    seed_alex(&e, &mut c).await?;
    let one = write_release(e.dir.path(), "one.vcf", "20261005", &release_one(), false)?;
    // Another run holds Alex's dataset and is about to commit its snapshot of this same release. This
    // run must wait for it and then find the release compared, never read the old compared date and
    // write the same events a second time.
    let mut other = e.db.pool.begin().await?;
    let dataset: Uuid =
        sqlx::query_scalar("select id from genome_dataset where profile_id = $1 and status = 'current' for update")
            .bind(e.profile("alex"))
            .fetch_one(&mut *other)
            .await?;
    sqlx::query(
        "insert into clinvar_carried (dataset_id, release_date, variation_id, significance, stars) \
         values ($1, '2026-10-05', 7105, 'Pathogenic', 3)",
    )
    .bind(dataset)
    .execute(&mut *other)
    .await?;
    sqlx::query("update genome_dataset set clinvar_compared = '2026-10-05' where id = $1")
        .bind(dataset)
        .execute(&mut *other)
        .await?;
    let (pool, blobs, path) = (e.db.pool.clone(), e.blobs.clone(), one.clone());
    let run = tokio::spawn(async move { clinvar::diff_release(&pool, &blobs, &path).await });
    wait_for_a_lock_wait(&e.db.pool).await?;
    other.commit().await?;
    let r = run.await??;
    assert_eq!(r.datasets[0].skipped, Some("already_compared"), "{r:?}");
    assert!(events(&mut c).await?.is_empty());
    Ok(())
}

#[tokio::test]
async fn a_genome_retracted_while_the_run_waits_is_not_compared() -> R {
    let e = env().await?;
    let mut c = e.conn().await?;
    seed_alex(&e, &mut c).await?;
    let one = write_release(e.dir.path(), "one.vcf", "20261005", &release_one(), false)?;
    // The run has listed Alex's dataset and waits for it; meanwhile the genome file is retracted.
    let mut other = e.db.pool.begin().await?;
    let doc: Uuid = sqlx::query_scalar(
        "select document_id from genome_dataset where profile_id = $1 and status = 'current' for update",
    )
    .bind(e.profile("alex"))
    .fetch_one(&mut *other)
    .await?;
    let (pool, blobs, path) = (e.db.pool.clone(), e.blobs.clone(), one.clone());
    let run = tokio::spawn(async move { clinvar::diff_release(&pool, &blobs, &path).await });
    wait_for_a_lock_wait(&e.db.pool).await?;
    sqlx::query("update document set status = 'retracted' where id = $1")
        .bind(doc)
        .execute(&mut *other)
        .await?;
    other.commit().await?;
    let r = run.await??;
    assert_eq!(r.datasets[0].skipped, Some("no_longer_current"), "{r:?}");
    assert!(events(&mut c).await?.is_empty());
    assert!(alerts_of(&mut c, e.profile("alex")).await?.is_empty());
    Ok(())
}

fn calls_with(dir: &tempfile::TempDir, rows: &[VcfRow]) -> anyhow::Result<VcfAlleles> {
    let f = write_vcf("x.vcf.gz", rows, "X")?;
    let p = dir.path().join("x.vcf.gz");
    std::fs::write(&p, &f.data)?;
    Ok(VcfAlleles::open(&p, "x")?)
}

/// The policy fixture: (old class, old stars, new class, new stars, first run, SF gene, shown P/LP)
/// -> (event, alert).
type Case = (
    Option<(&'static str, Option<i16>)>,
    &'static str,
    i16,
    bool,
    bool,
    bool,
    Option<&'static str>,
    bool,
);

const POLICY: [Case; 14] = [
    (None, "plp", 1, true, false, false, Some("upgraded_to_plp"), true),
    (None, "plp", 0, true, false, false, Some("upgraded_to_plp"), false),
    (
        Some(("vus", Some(1))),
        "plp",
        1,
        false,
        false,
        false,
        Some("upgraded_to_plp"),
        true,
    ),
    (
        Some(("vus", Some(1))),
        "plp",
        0,
        false,
        false,
        false,
        Some("upgraded_to_plp"),
        false,
    ),
    (
        Some(("plp", Some(3))),
        "vus",
        1,
        false,
        false,
        true,
        Some("downgraded_from_plp"),
        true,
    ),
    (
        Some(("plp", Some(3))),
        "vus",
        1,
        false,
        false,
        false,
        Some("downgraded_from_plp"),
        false,
    ),
    (
        Some(("plp", Some(2))),
        "conflicting",
        1,
        false,
        false,
        true,
        Some("became_conflicting"),
        true,
    ),
    (
        Some(("plp", Some(3))),
        "plp",
        2,
        false,
        false,
        true,
        Some("stars_changed"),
        false,
    ),
    (Some(("plp", None)), "plp", 2, true, false, true, None, false),
    (
        Some(("vus", Some(1))),
        "blb",
        1,
        false,
        false,
        false,
        Some("vus_resolved"),
        false,
    ),
    (
        Some(("vus", Some(1))),
        "blb",
        1,
        false,
        true,
        false,
        Some("vus_resolved"),
        true,
    ),
    (
        Some(("blb", Some(2))),
        "vus",
        1,
        false,
        false,
        false,
        Some("significance_changed"),
        false,
    ),
    (None, "blb", 2, true, false, false, None, false),
    (
        None,
        "vus",
        1,
        false,
        false,
        false,
        Some("new_assertion_on_carried_variant"),
        false,
    ),
];

fn run_policy(cases: &[Case]) -> Vec<String> {
    let mut wrong = Vec::new();
    for (i, (old, new, stars, first, sf, shown, want_kind, want_alert)) in cases.iter().enumerate() {
        let old = old.map(|(class, stars)| Before { class, stars });
        let kind = event_kind(old.as_ref(), new, *stars, *first);
        let alert = kind.is_some_and(|k| alerts(k, old.as_ref(), *stars, *sf, *shown));
        if kind != *want_kind || alert != *want_alert {
            wrong.push(format!("case {i}: got {kind:?}/{alert}"));
        }
    }
    wrong
}

#[test]
fn the_policy_fixture_holds_and_one_flipped_star_breaks_it() {
    assert_eq!(run_policy(&POLICY), Vec::<String>::new());
    // The control: the 1-star upgrade read as 0 stars must fail the fixture.
    let mut flipped = POLICY;
    flipped[0].2 = 0;
    assert_eq!(
        run_policy(&flipped),
        vec!["case 0: got Some(\"upgraded_to_plp\")/false".to_string()]
    );
}

#[test]
fn every_alert_class_sf_gene_is_in_the_sensitive_set() {
    let sf = acmg_sf_genes();
    let sensitive = sensitive_genes();
    assert!(sf.iter().all(|g| sensitive.contains(g)));
    // ACMG SF v3.3 cancer (28) and cardiovascular (41, with PLN) genes.
    assert_eq!(sf.len(), 69, "{sf:?}");
    assert!(is_sf_gene(Some("PLN")) && is_sf_gene(Some("brca2")) && !is_sf_gene(Some("APOE")));
}

#[test]
fn a_withheld_alert_names_nothing_in_either_language() {
    let old = Before {
        class: "plp",
        stars: Some(3),
    };
    let t = clinvar::EventText {
        gene: Some("BRCA1"),
        old: Some(&old),
        new_class: "vus",
        new_stars: 1,
        withheld: true,
        unconfirmed: true,
    };
    for lang in ["en", "es"] {
        let s = summary(&t, lang);
        assert!(
            !s.contains("BRCA1") && !s.contains("pathogen") && !s.contains("patogén"),
            "{s}"
        );
    }
}

/// The exit criterion on this box: a household of 4 genomes against a 3M-record synthetic release in
/// under 5 minutes with a peak RSS under 500 MB. Run with
/// `cargo test -p ownhealth-genome --release --test clinvar -- --ignored household_of_four`.
#[test]
#[ignore = "perf: writes about 300 MB of synthetic files"]
fn household_of_four_genomes_against_a_3m_record_release() -> R {
    use std::time::{Duration, Instant};
    let dir = tempfile::tempdir()?;
    let contigs: Vec<String> = (1..=22)
        .map(|i| i.to_string())
        .chain(["X".into(), "Y".into(), "MT".into()])
        .collect();
    let per_contig = 3_000_000 / contigs.len() as i64 + 1;
    let rel = dir.path().join("clinvar.vcf.gz");
    {
        let mut w = bgzf::io::Writer::new(std::io::BufWriter::new(std::fs::File::create(&rel)?));
        writeln!(
            w,
            "##fileformat=VCFv4.1\n##fileDate=20261005\n#CHROM\tPOS\tID\tREF\tALT\tQUAL\tFILTER\tINFO"
        )?;
        let sigs = [
            ("Benign", MULTI),
            ("Uncertain_significance", SINGLE),
            ("Likely_pathogenic", SINGLE),
        ];
        let mut vid = 1;
        for c in &contigs {
            for i in 0..per_contig {
                let (sig, rev) = sigs[(vid % 3) as usize];
                writeln!(
                    w,
                    "{c}\t{}\t{vid}\tA\tG\t.\t.\tALLELEID={vid};CLNDISDB=MedGen:CN169374;CLNDN=not_specified;\
                     CLNREVSTAT={rev};CLNSIG={sig};CLNVC=single_nucleotide_variant;GENEINFO=SYNTH{}:1;\
                     MC=SO:0001583|missense_variant;ORIGIN=1",
                    100 + i * 50,
                    vid % 500
                )?;
                vid += 1;
            }
        }
        w.finish()?;
    }
    // Four genomes of 1.2M calls each, one in three on a ClinVar position.
    let mut genomes = Vec::new();
    for g in 0..4 {
        let p = dir.path().join(format!("g{g}.vcf.gz"));
        let mut w = bgzf::io::Writer::new(std::io::BufWriter::new(std::fs::File::create(&p)?));
        writeln!(
            w,
            "##fileformat=VCFv4.2\n#CHROM\tPOS\tID\tREF\tALT\tQUAL\tFILTER\tINFO\tFORMAT\tS{g}"
        )?;
        for c in &contigs {
            for i in 0..(1_200_000 / contigs.len() as i64) {
                let pos = if i % 3 == 0 { 100 + i * 50 } else { 125 + i * 50 };
                writeln!(
                    w,
                    "chr{}\t{pos}\t.\tA\tG\t50\tPASS\t.\tGT:DP:GQ\t0/1:30:60",
                    if c == "MT" { "M" } else { c }
                )?;
            }
        }
        w.finish()?;
        genomes.push(p);
    }
    let peak = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let sampler = {
        let (peak, stop) = (peak.clone(), stop.clone());
        let pid = std::process::id().to_string();
        std::thread::spawn(move || {
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                if let Ok(o) = std::process::Command::new("ps")
                    .args(["-o", "rss=", "-p", &pid])
                    .output()
                {
                    let kb: u64 = String::from_utf8_lossy(&o.stdout).trim().parse().unwrap_or(0);
                    peak.fetch_max(kb, std::sync::atomic::Ordering::Relaxed);
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        })
    };
    let t = Instant::now();
    let mut total = 0;
    for (g, p) in genomes.iter().enumerate() {
        let mut person = VcfAlleles::open(p, &format!("g{g}"))?;
        let (_, carried) = carried_records(&rel, &mut person)?;
        assert!(!carried.is_empty());
        total += carried.len();
    }
    let took = t.elapsed();
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    let _ = sampler.join();
    let rss_mb = peak.load(std::sync::atomic::Ordering::Relaxed) / 1024;
    eprintln!("4 genomes x 3M records: {took:?}, {total} carried, peak RSS {rss_mb} MB");
    assert!(took < Duration::from_secs(300), "{took:?}");
    assert!(rss_mb < 500, "{rss_mb} MB");
    Ok(())
}
