//! C-CDA import (slice 20): the HL7 C-CDA-Examples samples (CC0, `fixtures/ccda/`) give exact FHIR
//! resources per section; a section or entry this version does not read lands in review, never in
//! an error; malformed, hostile or oversized XML is refused without a panic.

mod common;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use common::{Env, R, family, store_doc};
use ownhealth_core::Error;
use ownhealth_core::documents::{StoredFile, dispatch_upload, sniff};
use ownhealth_core::jobs::{Job, JobContext};
use ownhealth_core::review::{self, ItemQuery};
use ownhealth_ingest_structured::ccda::{self, Converted, Section, convert, held_items, import_ccda};
use serde_json::{Value, json};
use sqlx::PgConnection;
use uuid::Uuid;

fn fixture(name: &str) -> Vec<u8> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../../fixtures/ccda")
        .join(name);
    std::fs::read(&path).unwrap_or_else(|e| panic!("fixture {}: {e}", path.display()))
}

/// A minimal ClinicalDocument for Isabella Jones (born 2005-05-01) around `body`, the `component`
/// elements of its structured body.
fn document(body: &str) -> Vec<u8> {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<ClinicalDocument xmlns="urn:hl7-org:v3" xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance" xmlns:sdtc="urn:hl7-org:sdtc">
  <templateId root="2.16.840.1.113883.10.20.22.1.2"/>
  <id root="2.16.840.1.113883.19.5.99999.1" extension="ownhealth-test"/>
  <code code="34133-9" codeSystem="2.16.840.1.113883.6.1" displayName="Summary of episode note"/>
  <title>Test document</title>
  <effectiveTime value="20151115"/>
  <recordTarget><patientRole>
    <id root="2.16.840.1.113883.19.5.99999.2" extension="998991"/>
    <patient><name><given>Isabella</given><family>Jones</family></name>
      <administrativeGenderCode code="F" codeSystem="2.16.840.1.113883.5.1"/><birthTime value="20050501"/></patient>
  </patientRole></recordTarget>
  <component><structuredBody>{body}</structuredBody></component>
</ClinicalDocument>"#
    )
    .into_bytes()
}

/// A `fixtures/ccda/sections/` file (one section, no document around it) inside [`document`].
fn section_file(name: &str) -> Vec<u8> {
    let raw = String::from_utf8(fixture(&format!("sections/{name}"))).unwrap_or_default();
    let body = match raw.find("<?xml") {
        Some(at) => raw[at..]
            .split_once("?>")
            .map_or(raw.as_str(), |(_, rest)| rest)
            .to_string(),
        None => raw.clone(),
    };
    let first = body
        .match_indices('<')
        .map(|(i, _)| &body[i..])
        .find(|t| !t.starts_with("<!--"))
        .unwrap_or_default();
    if first.starts_with("<component") {
        document(&body)
    } else {
        document(&format!("<component>{body}</component>"))
    }
}

/// Resources by type made from the sections of `kind`.
fn made(c: &Converted, kind: Section) -> BTreeMap<String, usize> {
    let mut out = BTreeMap::new();
    for s in c.sections.iter().filter(|s| s.kind == Some(kind)) {
        for (t, n) in &s.resources {
            *out.entry(t.clone()).or_default() += n;
        }
    }
    out
}

fn counts(pairs: &[(&str, usize)]) -> BTreeMap<String, usize> {
    pairs.iter().map(|(k, v)| (k.to_string(), *v)).collect()
}

fn resources<'a>(c: &'a Converted, rtype: &str) -> Vec<&'a Value> {
    c.entries
        .iter()
        .map(|e| &e.resource)
        .filter(|r| r["resourceType"] == json!(rtype))
        .collect()
}

#[test]
fn the_history_and_physical_sample_gives_exact_resources_per_section() -> R {
    let c = convert(&fixture("history-and-physical.xml"))?;
    assert_eq!(made(&c, Section::Allergies), counts(&[("AllergyIntolerance", 3)]));
    assert_eq!(made(&c, Section::Problems), counts(&[("Condition", 2)]));
    assert_eq!(made(&c, Section::Medications), counts(&[("MedicationStatement", 1)]));
    assert_eq!(made(&c, Section::Immunizations), counts(&[("Immunization", 4)]));
    assert_eq!(
        made(&c, Section::Results),
        counts(&[("DiagnosticReport", 1), ("Observation", 3)])
    );
    assert_eq!(made(&c, Section::VitalSigns), counts(&[("Observation", 6)]));
    // the patient and the composition, then 3 + 2 + 1 + 4 + 4 + 6 clinical resources
    assert_eq!(c.entries.len(), 22);
    assert_eq!(resources(&c, "Patient").len(), 1);
    assert_eq!(resources(&c, "Composition").len(), 1);
    // every entry of the six sections was read
    let left: Vec<&String> = c
        .sections
        .iter()
        .filter(|s| s.kind.is_some())
        .flat_map(|s| &s.not_read)
        .collect();
    assert!(left.is_empty(), "{left:?}");
    // 17 sections: the six, 4 more with entries, 7 with narrative only
    assert_eq!(c.sections.len(), 17);
    let other: Vec<(&str, usize)> = c
        .sections
        .iter()
        .filter(|s| s.kind.is_none() && s.entries > 0)
        .map(|s| (s.title.as_str(), s.entries))
        .collect();
    assert_eq!(
        other,
        [
            ("FAMILY HISTORY", 1),
            ("PLAN OF CARE", 4),
            ("PROCEDURES", 3),
            ("SOCIAL HISTORY", 3)
        ]
    );
    assert_eq!(
        c.sections.iter().filter(|s| s.kind.is_none() && s.entries == 0).count(),
        7
    );

    let patient = resources(&c, "Patient")[0];
    assert_eq!(patient["birthDate"], "2005-05-01");
    assert_eq!(patient["name"][0]["given"], json!(["Isabella", "Isa"]));
    assert_eq!(patient["name"][0]["family"], "Jones");
    // the social security number stays in the file only
    assert!(!patient.to_string().contains("111-00-2330"), "{patient}");

    let pneumonia = resources(&c, "Condition")
        .into_iter()
        .find(|r| r["code"]["coding"][0]["code"] == "233604007")
        .ok_or_else(|| anyhow::anyhow!("no pneumonia"))?;
    assert_eq!(pneumonia["code"]["coding"][0]["system"], "http://snomed.info/sct");
    assert_eq!(pneumonia["code"]["text"], "Pneumonia");
    let hgb = resources(&c, "Observation")
        .into_iter()
        .find(|r| r["code"]["coding"][0]["code"] == "30313-1")
        .ok_or_else(|| anyhow::anyhow!("no HGB"))?;
    assert_eq!(hgb["code"]["coding"][0]["system"], "http://loinc.org");
    assert_eq!(hgb["valueQuantity"]["value"], json!(13.2));
    assert_eq!(hgb["valueQuantity"]["code"], "g/dl");
    assert_eq!(hgb["category"][0]["coding"][0]["code"], "laboratory");
    // the report names its three results
    let report = resources(&c, "DiagnosticReport")[0];
    assert_eq!(report["result"].as_array().map(Vec::len), Some(3));
    assert!(hgb["effectiveDateTime"].is_string(), "{hgb}");
    // one immunization was not given (negationInd)
    let not_done: Vec<_> = resources(&c, "Immunization")
        .into_iter()
        .filter(|r| r["status"] == "not-done")
        .collect();
    assert_eq!(not_done.len(), 1);
    // ids are unique in the file even where the sample repeats one C-CDA id
    let mut ids: Vec<String> = c
        .entries
        .iter()
        .map(|e| format!("{}/{}", e.resource["resourceType"], e.resource["id"]))
        .collect();
    let n = ids.len();
    ids.sort();
    ids.dedup();
    assert_eq!(ids.len(), n);
    Ok(())
}

#[test]
fn the_ccd_with_no_information_sections_reads_what_it_has() -> R {
    let c = convert(&fixture("ccd-2.xml"))?;
    assert_eq!(made(&c, Section::Allergies), counts(&[("AllergyIntolerance", 1)]));
    assert_eq!(made(&c, Section::Problems), counts(&[]));
    assert_eq!(made(&c, Section::Medications), counts(&[]));
    assert_eq!(
        made(&c, Section::Results),
        counts(&[("DiagnosticReport", 1), ("Observation", 1)])
    );
    assert_eq!(made(&c, Section::VitalSigns), counts(&[("Observation", 9)]));
    // "no known allergies": the IPS absent-unknown code, which the importer does not keep as a record
    let allergy = resources(&c, "AllergyIntolerance")[0];
    assert_eq!(
        allergy["code"]["coding"][0],
        json!({"system": ownhealth_ingest_structured::fhir::mapping::ABSENT_UNKNOWN,
            "code": "no-known-allergies", "display": "no known allergies"})
    );
    // the pending leukocyte count has no value yet
    let pending = resources(&c, "Observation")
        .into_iter()
        .find(|r| r["code"]["coding"][0]["code"] == "804-5")
        .ok_or_else(|| anyhow::anyhow!("no leukocytes"))?;
    assert_eq!(pending["status"], "preliminary");
    assert!(pending.get("valueQuantity").is_none(), "{pending}");
    assert_eq!(resources(&c, "DiagnosticReport")[0]["status"], "partial");
    let bmi = resources(&c, "Observation")
        .into_iter()
        .find(|r| r["code"]["coding"][0]["code"] == "39156-5")
        .ok_or_else(|| anyhow::anyhow!("no BMI"))?;
    assert_eq!(bmi["valueQuantity"]["value"], json!(37.58));
    assert_eq!(bmi["category"][0]["coding"][0]["code"], "vital-signs");
    // procedures and social history wait for part 2, each as one review item
    let held = held_items(&c, "ccda-x", "ccd-2.xml");
    let types: Vec<(&str, i64)> = held
        .iter()
        .map(|h| {
            (
                h["resource_type"].as_str().unwrap_or_default(),
                h["count"].as_i64().unwrap_or(0),
            )
        })
        .collect();
    assert_eq!(types, [("PROCEDURES (C-CDA)", 3), ("SOCIAL HISTORY (C-CDA)", 1)]);
    assert_eq!(held[0]["source"], json!({"kind": "ccda", "label": "ccd-2.xml"}));
    Ok(())
}

#[test]
fn negated_entries_never_become_plain_records() -> R {
    let absent = |c: &Converted, rtype: &str, key: &str| -> String {
        let r = resources(c, rtype);
        assert_eq!(r.len(), 1, "{rtype}");
        let concept = &r[0][key]["coding"][0];
        assert_eq!(
            concept["system"],
            ownhealth_ingest_structured::fhir::mapping::ABSENT_UNKNOWN
        );
        concept["code"].as_str().unwrap_or_default().to_string()
    };
    let c = convert(&section_file("allergies-none-known.xml"))?;
    assert_eq!(absent(&c, "AllergyIntolerance", "code"), "no-known-allergies");
    let c = convert(&section_file("problems-none-known.xml"))?;
    assert_eq!(absent(&c, "Condition", "code"), "no-known-problems");
    let c = convert(&section_file("medications-none.xml"))?;
    assert_eq!(
        absent(&c, "MedicationStatement", "medicationCodeableConcept"),
        "no-known-medications"
    );

    // not allergic to peanuts: no substance code, the peanut sits in the exposure-risk extension
    let c = convert(&section_file("allergies-not-peanuts.xml"))?;
    let peanut = resources(&c, "AllergyIntolerance")[0];
    assert!(peanut.get("code").is_none(), "{peanut}");
    assert_eq!(
        peanut["extension"][0]["extension"][0]["valueCodeableConcept"]["coding"][0]["code"],
        "762952008"
    );
    // does not have diabetes: refuted
    let c = convert(&section_file("problems-not-diabetes.xml"))?;
    let diabetes = resources(&c, "Condition")[0];
    assert_eq!(diabetes["code"]["coding"][0]["code"], "44054006");
    assert_eq!(diabetes["verificationStatus"]["coding"][0]["code"], "refuted");
    // a vaccine the patient refused: not-done, with the reason
    let c = convert(&section_file("immunization-refused.xml"))?;
    let refused = resources(&c, "Immunization")[0];
    assert_eq!(refused["status"], "not-done");
    assert_eq!(refused["statusReason"]["coding"][0]["code"], "PATOBJ");
    assert_eq!(refused["vaccineCode"]["coding"][0]["code"], "166");
    Ok(())
}

#[test]
fn a_result_below_a_limit_keeps_its_comparator() -> R {
    let c = convert(&section_file("results-less-than.xml"))?;
    let bnp = resources(&c, "Observation")[0];
    assert_eq!(
        bnp["valueQuantity"],
        json!({"value": 5, "unit": "pg/mL", "system": "http://unitsofmeasure.org", "code": "pg/mL", "comparator": "<="})
    );
    assert_eq!(bnp["referenceRange"][0]["high"]["value"], json!(100));
    Ok(())
}

#[test]
fn an_unknown_template_is_held_for_review_never_an_error() -> R {
    let c = convert(&document(
        r#"<component><section>
             <templateId root="1.2.3.4.5.6"/><title>Mystery</title><text>a b</text>
             <entry><observation classCode="OBS" moodCode="EVN"><templateId root="1.2.3.4.5.7"/>
               <code code="X1" codeSystem="1.2.3"/></observation></entry>
             <entry><act classCode="ACT" moodCode="EVN"><code code="X2" codeSystem="1.2.3"/></act></entry>
           </section></component>
           <component><section>
             <templateId root="2.16.840.1.113883.10.20.22.2.5.1"/><title>PROBLEMS</title>
             <entry><encounter classCode="ENC" moodCode="EVN"><templateId root="9.9.9"/></encounter></entry>
             <entry><act classCode="ACT" moodCode="EVN"><templateId root="2.16.840.1.113883.10.20.22.4.3"/></act></entry>
           </section></component>"#,
    ))?;
    assert_eq!(c.sections.len(), 2);
    assert_eq!(c.sections[0].kind, None);
    assert_eq!(c.sections[1].kind, Some(Section::Problems));
    assert_eq!(
        c.sections[1].not_read,
        [
            "encounter 9.9.9".to_string(),
            "act 2.16.840.1.113883.10.20.22.4.3".to_string()
        ]
    );
    let held = held_items(&c, "ccda-x", "mystery.xml");
    assert_eq!(held.len(), 2);
    assert_eq!(held[0]["resource_type"], "Mystery (C-CDA)");
    assert_eq!(held[0]["count"], 2);
    assert_eq!(held[0]["notes"], json!({"section 1.2.3.4.5.6 is not read yet": 2}));
    assert_eq!(held[1]["resource_type"], "PROBLEMS (C-CDA)");
    assert_eq!(held[1]["count"], 2);
    Ok(())
}

/// Every hostile or broken file is refused as `Error::Invalid`, with a reason, and nothing panics.
#[test]
fn malformed_hostile_or_huge_xml_is_refused_without_a_panic() {
    let laughs = r#"<?xml version="1.0"?>
<!DOCTYPE lolz [<!ENTITY lol "lol"><!ENTITY lol2 "&lol;&lol;&lol;&lol;&lol;&lol;&lol;&lol;&lol;&lol;">
<!ENTITY lol3 "&lol2;&lol2;&lol2;&lol2;&lol2;&lol2;&lol2;&lol2;&lol2;&lol2;">]>
<ClinicalDocument xmlns="urn:hl7-org:v3"><title>&lol3;</title></ClinicalDocument>"#;
    let external = r#"<?xml version="1.0"?>
<!DOCTYPE ClinicalDocument [<!ENTITY x SYSTEM "file:///etc/passwd">]>
<ClinicalDocument xmlns="urn:hl7-org:v3"><title>&x;</title></ClinicalDocument>"#;
    let deep = format!(
        r#"<ClinicalDocument xmlns="urn:hl7-org:v3">{}{}</ClinicalDocument>"#,
        "<a>".repeat(200_000),
        "</a>".repeat(200_000)
    );
    let many = format!(
        r#"<ClinicalDocument xmlns="urn:hl7-org:v3">{}</ClinicalDocument>"#,
        "<a/>".repeat(ccda::xml::MAX_NODES as usize + 10)
    );
    let mut huge = b"<ClinicalDocument xmlns=\"urn:hl7-org:v3\">".to_vec();
    huge.resize(ccda::xml::MAX_BYTES + 1, b' ');
    let cases: Vec<(&str, Vec<u8>, &str)> = vec![
        ("billion laughs", laughs.into(), "DOCTYPE"),
        ("external entity", external.into(), "DOCTYPE"),
        (
            "undeclared entity",
            br#"<ClinicalDocument xmlns="urn:hl7-org:v3">&x;</ClinicalDocument>"#.to_vec(),
            "not well formed",
        ),
        ("deep nesting", deep.into_bytes(), "nests deeper"),
        ("too many nodes", many.into_bytes(), "nodes"),
        ("too large", huge, "MB"),
        (
            "unclosed",
            br#"<ClinicalDocument xmlns="urn:hl7-org:v3"><title>x</ClinicalDocument>"#.to_vec(),
            "not well formed",
        ),
        ("truncated", fixture("ccd-2.xml")[..20_000].to_vec(), "not well formed"),
        ("empty", Vec::new(), "not well formed"),
        ("binary", vec![0u8, 159, 146, 150, 0, 1], "not well formed"),
        (
            "another root",
            br#"<Bundle xmlns="urn:hl7-org:v3"/>"#.to_vec(),
            "ClinicalDocument",
        ),
        (
            "no namespace",
            b"<ClinicalDocument><title>x</title></ClinicalDocument>".to_vec(),
            "ClinicalDocument",
        ),
    ];
    for (name, bytes, why) in cases {
        match std::panic::catch_unwind(|| convert(&bytes)) {
            Ok(Err(Error::Invalid(m))) => assert!(m.contains(why), "{name}: {m}"),
            Ok(other) => panic!("{name}: {other:?}"),
            Err(_) => panic!("{name}: the converter panicked"),
        }
    }
}

// ------------------------------------------------------------------ import ----

async fn open_items(conn: &mut PgConnection, doc: Uuid, kind: &str) -> anyhow::Result<Vec<review::ReviewItem>> {
    Ok(review::list_items(
        conn,
        &ItemQuery {
            document_id: Some(doc),
            status: Some("open".into()),
            kind: Some(kind.into()),
            ..ItemQuery::default()
        },
    )
    .await?)
}

async fn count(conn: &mut PgConnection, table: &str, profile: Uuid) -> anyhow::Result<i64> {
    let sql = format!("select count(*) from {table} where profile_id = $1");
    Ok(sqlx::query_scalar(sqlx::AssertSqlSafe(sql))
        .bind(profile)
        .fetch_one(conn)
        .await?)
}

#[tokio::test]
async fn the_history_and_physical_imports_into_the_matching_profile_and_holds_the_rest() -> R {
    let env = Env::new().await?;
    let mut tx = env.tx().await?;
    let fam = family(&mut tx).await?;
    let p = fam.profile("lucia");
    sqlx::query("update profile set display_name = 'Isabella Jones', birth_date = '2005-05-01' where id = $1")
        .bind(p)
        .execute(&mut *tx)
        .await?;
    let bytes = fixture("history-and-physical.xml");
    let doc = store_doc(&mut tx, &env.blobs, &fam, "lucia", &bytes, "hp.xml", ccda::MIME).await?;
    let s = import_ccda(&mut tx, &env.blobs, doc).await?;
    // 2 problems, 3 allergies, 1 medicine and the 3 vaccines given are records; the refused vaccine
    // waits in review; every lab and vital sign is a value or a review row; 5 held section items
    assert_eq!(count(&mut tx, "condition", p).await?, 2);
    assert_eq!(count(&mut tx, "allergy", p).await?, 3);
    assert_eq!(count(&mut tx, "medication_statement", p).await?, 1);
    assert_eq!(count(&mut tx, "immunization", p).await?, 3);
    let held = open_items(&mut tx, doc, "fhir_unmapped_summary").await?;
    let mut types: Vec<String> = held
        .iter()
        .map(|h| h.payload["resource_type"].as_str().unwrap_or_default().to_string())
        .collect();
    types.sort();
    assert_eq!(
        types,
        [
            "C-CDA narrative section",
            "FAMILY HISTORY (C-CDA)",
            "PLAN OF CARE (C-CDA)",
            "PROCEDURES (C-CDA)",
            "SOCIAL HISTORY (C-CDA)"
        ]
    );
    let values = count(&mut tx, "observation", p).await?;
    let rows = open_items(&mut tx, doc, "candidate_row").await?.len() as i64;
    assert_eq!(values + rows, 9, "3 labs and 6 vital signs");
    // 10 records (one of them the refused vaccine), 9 values, 5 held sections: each counted once
    assert_eq!(s.accepted + s.queued + s.skipped, 10 + 9 + 5, "{s:?}");
    let flags: Value = sqlx::query_scalar("select flags from document where id = $1")
        .bind(doc)
        .fetch_one(&mut *tx)
        .await?;
    assert_eq!(flags["ccda"]["sections"].as_array().map(Vec::len), Some(17));
    assert_eq!(flags["import"]["accepted"], json!(s.accepted));
    // a second run reads the stored summary and adds nothing
    let again = import_ccda(&mut tx, &env.blobs, doc).await?;
    assert_eq!(again, s);
    assert_eq!(open_items(&mut tx, doc, "fhir_unmapped_summary").await?.len(), 5);
    Ok(())
}

#[tokio::test]
async fn a_document_for_someone_else_is_held_whole() -> R {
    let env = Env::new().await?;
    let mut tx = env.tx().await?;
    let fam = family(&mut tx).await?;
    let p = fam.profile("carmen");
    let doc = store_doc(
        &mut tx,
        &env.blobs,
        &fam,
        "carmen",
        &fixture("history-and-physical.xml"),
        "hp.xml",
        ccda::MIME,
    )
    .await?;
    let s = import_ccda(&mut tx, &env.blobs, doc).await?;
    assert_eq!(s.accepted, 0, "{s:?}");
    assert_eq!(count(&mut tx, "condition", p).await?, 0);
    Ok(())
}

fn job(payload: Value) -> Job {
    Job {
        id: 0,
        queue: "core".into(),
        kind: ccda::JOB.into(),
        payload,
        attempts: 1,
        max_attempts: 5,
        dedupe_key: None,
    }
}

#[tokio::test]
async fn an_uploaded_ccd_is_sniffed_and_imported_by_its_job_and_a_broken_one_fails_for_good() -> R {
    let env = Env::new().await?;
    let mut tx = env.tx().await?;
    let fam = family(&mut tx).await?;
    let p = fam.profile("lucia");
    let bytes = fixture("ccd-2.xml");
    assert_eq!(sniff("record.xml", &bytes[..4096], None), "ccda");
    let sha = env
        .blobs
        .put_bytes(ownhealth_core::blobs::BlobOwner::Profile(p), &bytes)?;
    let out = dispatch_upload(
        &mut tx,
        &env.blobs,
        fam.household_id,
        p,
        &[StoredFile {
            filename: "record.xml".into(),
            sha256: sha.clone(),
            size: bytes.len() as i64,
        }],
        None,
    )
    .await?;
    assert_eq!(out["items"][0]["kind"], "ccda");
    let queued: Vec<(String, Value)> =
        sqlx::query_as("select kind, payload from job where payload->>'blob_sha256' = $1")
            .bind(&sha)
            .fetch_all(&mut *tx)
            .await?;
    assert_eq!(queued.len(), 1);
    assert_eq!(queued[0].0, ccda::JOB);
    tx.commit().await?;

    let settings = Arc::new(env.db.settings(env.dir.path())?);
    let ctx = JobContext::new(env.db.pool.clone(), settings, env.blobs.clone());
    let result = ccda::import_ccda_job(ctx.clone(), job(queued[0].1.clone())).await?;
    let result = result.ok_or_else(|| anyhow::anyhow!("no result"))?;
    let doc = Uuid::parse_str(result["document_id"].as_str().unwrap_or_default())?;
    let (mime, channel): (String, String) = sqlx::query_as(
        "select d.mime, s.channel from document d join document_source s on s.document_id = d.id where d.id = $1",
    )
    .bind(doc)
    .fetch_one(&env.db.pool)
    .await?;
    assert_eq!((mime.as_str(), channel.as_str()), (ccda::MIME, "ccda"));

    // the same root, then broken XML: a retry reads the same bytes, so it fails for good
    let broken = b"<?xml version=\"1.0\"?><ClinicalDocument xmlns=\"urn:hl7-org:v3\"><title>cut".to_vec();
    assert_eq!(sniff("cut.xml", &broken, None), "ccda");
    let sha = env
        .blobs
        .put_bytes(ownhealth_core::blobs::BlobOwner::Profile(p), &broken)?;
    let payload = json!({"profile_id": p.to_string(), "household_id": fam.household_id.to_string(),
        "blob_sha256": sha, "filename": "cut.xml"});
    let Err(e) = ccda::import_ccda_job(ctx, job(payload)).await else {
        panic!("a broken C-CDA file imported");
    };
    assert!(e.permanent, "{e}");
    assert!(e.to_string().contains("not a readable C-CDA document"), "{e}");
    Ok(())
}
