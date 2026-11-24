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
    document_with(
        r#"<id root="2.16.840.1.113883.19.5.99999.2" extension="998991"/>"#,
        body,
    )
}

/// [`document`] with `ids` as the `id` elements of its patientRole.
fn document_with(ids: &str, body: &str) -> Vec<u8> {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<ClinicalDocument xmlns="urn:hl7-org:v3" xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance" xmlns:sdtc="urn:hl7-org:sdtc">
  <templateId root="2.16.840.1.113883.10.20.22.1.2"/>
  <id root="2.16.840.1.113883.19.5.99999.1" extension="ownhealth-test"/>
  <code code="34133-9" codeSystem="2.16.840.1.113883.6.1" displayName="Summary of episode note"/>
  <title>Test document</title>
  <effectiveTime value="20151115"/>
  <recordTarget><patientRole>
    {ids}
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

// ------------------------------------------------------------- round 1 ----

/// A problem act around one Problem Observation with `value`, `times` (the observation's
/// effectiveTime children) and `extra` (more children of the observation).
fn problem_entry(id: &str, value: &str, times: &str, extra: &str) -> String {
    format!(
        r#"<entry><act classCode="ACT" moodCode="EVN"><templateId root="2.16.840.1.113883.10.20.22.4.3"/>
          <id root="1.2.3.4" extension="act-{id}"/>
          <entryRelationship typeCode="SUBJ"><observation classCode="OBS" moodCode="EVN">
            <templateId root="2.16.840.1.113883.10.20.22.4.4"/><id root="1.2.3.4" extension="obs-{id}"/>
            <code code="55607006" codeSystem="2.16.840.1.113883.6.96"/>
            <effectiveTime>{times}</effectiveTime>
            <value xsi:type="CD" codeSystem="2.16.840.1.113883.6.96" {value}/>{extra}
          </observation></entryRelationship></act></entry>"#
    )
}

fn problems_section(entries: &str) -> String {
    format!(
        r#"<component><section><templateId root="2.16.840.1.113883.10.20.22.2.5.1"/><title>PROBLEMS</title>{entries}</section></component>"#
    )
}

/// A DS4P Security Observation whose value is `code` (v3 ActCode).
fn security_label(code: &str) -> String {
    format!(
        r#"<observation classCode="OBS" moodCode="EVN"><templateId root="2.16.840.1.113883.3.3445.21"/>
          <code code="SECCLASSOBS" codeSystem="2.16.840.1.113883.5.4"/>
          <value xsi:type="CE" code="{code}" codeSystem="2.16.840.1.113883.5.4"/></observation>"#
    )
}

async fn as_isabella(conn: &mut PgConnection, fam: &ownhealth_core::demo::Family, slug: &str) -> anyhow::Result<Uuid> {
    let p = fam.profile(slug);
    sqlx::query("update profile set display_name = 'Isabella Jones', birth_date = '2005-05-01' where id = $1")
        .bind(p)
        .execute(&mut *conn)
        .await?;
    Ok(p)
}

/// C-F1: a narrative that many entries point at is read once, and only up to its first characters.
#[test]
fn a_large_narrative_referenced_many_times_converts_in_bounded_time() -> R {
    let narrative = "word ".repeat(4 * 1024 * 1024 / 5);
    let entry = r##"<entry><observation classCode="OBS" moodCode="EVN"><code><originalText><reference value="#n"/></originalText></code></observation></entry>"##;
    let body = format!(
        r#"<component><section><templateId root="2.16.840.1.113883.10.20.22.2.3.1"/><title>RESULTS</title>
          <text><paragraph ID="n">{narrative}</paragraph></text>{}</section></component>"#,
        entry.repeat(500)
    );
    let bytes = document(&body);
    let started = std::time::Instant::now();
    let c = convert(&bytes)?;
    let took = started.elapsed();
    let observations = resources(&c, "Observation");
    assert_eq!(observations.len(), 500);
    assert!(
        observations
            .iter()
            .all(|o| o["code"]["text"].as_str().is_some_and(|t| t.chars().count() <= 2_000))
    );
    // each reference used to walk the whole 4 MB text node: about 90 ms each in a debug build
    assert!(took < std::time::Duration::from_secs(5), "{took:?}");
    Ok(())
}

/// C-F2: the social security number gives no id, wherever the patientRole lists it.
#[test]
fn no_id_derives_from_the_social_security_number() -> R {
    use sha2::{Digest, Sha256};
    let ssn = r#"<id root="2.16.840.1.113883.4.1" extension="111-00-2330"/>"#;
    let mrn = r#"<id root="2.16.840.1.113883.19.5.99999.2" extension="998991"/>"#;
    let hash = hex::encode(Sha256::digest(b"2.16.840.1.113883.4.1|111-00-2330"));
    let body = problems_section(&problem_entry(
        "p",
        r#"code="233604007" displayName="Pneumonia""#,
        "",
        "",
    ));
    let patient_id = |c: &Converted| resources(c, "Patient")[0]["id"].clone();
    let plain = convert(&document_with(mrn, &body))?;
    for ids in [format!("{ssn}{mrn}"), ssn.to_string()] {
        let c = convert(&document_with(&ids, &body))?;
        for e in &c.entries {
            let text = e.resource.to_string();
            assert!(
                !text.contains(&hash[..16]) && !text.contains("111-00-2330"),
                "{ids}: {text}"
            );
        }
        if ids.contains("998991") {
            // the other id names the patient, as in a file without the number
            assert_eq!(patient_id(&c), patient_id(&plain));
        }
    }
    Ok(())
}

/// C-F4: a core section that holds only narrative (Medications as a table, no entries) is listed
/// with the other narrative-only sections; one that says "no information" (nullFlavor) is not.
#[test]
fn a_core_section_with_only_narrative_is_held_with_the_narrative() -> R {
    let c = convert(&document(
        r#"<component><section><templateId root="2.16.840.1.113883.10.20.22.2.1"/>
             <code code="10160-0" codeSystem="2.16.840.1.113883.6.1"/><title>MEDICATIONS</title>
             <text><table><tr><td>Metformin 500 mg twice a day</td></tr><tr><td>Lisinopril 10 mg</td></tr></table></text>
           </section></component>
           <component><section nullFlavor="NI"><templateId root="2.16.840.1.113883.10.20.22.2.5.1"/>
             <title>PROBLEMS</title><text>No information</text></section></component>
           <component><section><templateId root="1.2.3.9"/><title>NOTES</title><text>free note</text></section></component>"#,
    ))?;
    let held = held_items(&c, "ccda-x", "meds.xml");
    assert_eq!(held.len(), 1, "{}", json!(held));
    assert_eq!(held[0]["resource_type"], ccda::NARRATIVE);
    assert_eq!(
        held[0]["notes"],
        json!({"MEDICATIONS (2.16.840.1.113883.10.20.22.2.1)": 1, "NOTES (1.2.3.9)": 1})
    );
    assert_eq!(held[0]["count"], 2);
    Ok(())
}

/// C-F5: "did not take aspirin" is never a medicine taken: no row, one item in review.
#[tokio::test]
async fn a_medicine_not_taken_is_never_recorded_as_taken() -> R {
    let body = r#"<component><section><templateId root="2.16.840.1.113883.10.20.22.2.1.1"/><title>MEDICATIONS</title>
      <entry><substanceAdministration classCode="SBADM" moodCode="EVN" negationInd="true">
        <templateId root="2.16.840.1.113883.10.20.22.4.16"/><id root="1.2.3.4" extension="med-neg-1"/>
        <statusCode code="completed"/><effectiveTime xsi:type="IVL_TS"><low value="20230101"/></effectiveTime>
        <consumable><manufacturedProduct><templateId root="2.16.840.1.113883.10.20.22.4.23"/>
          <manufacturedMaterial><code code="1191" codeSystem="2.16.840.1.113883.6.88" displayName="Aspirin"/></manufacturedMaterial>
        </manufacturedProduct></consumable></substanceAdministration></entry>
    </section></component>"#;
    let bytes = document(body);
    let c = convert(&bytes)?;
    let aspirin = resources(&c, "MedicationStatement")[0];
    assert_eq!(aspirin["status"], "not-taken");
    assert_eq!(aspirin["medicationCodeableConcept"]["coding"][0]["code"], "1191");
    let env = Env::new().await?;
    let mut tx = env.tx().await?;
    let fam = family(&mut tx).await?;
    let p = as_isabella(&mut tx, &fam, "lucia").await?;
    let doc = store_doc(&mut tx, &env.blobs, &fam, "lucia", &bytes, "neg.xml", ccda::MIME).await?;
    let s = import_ccda(&mut tx, &env.blobs, doc).await?;
    assert_eq!((s.accepted, s.queued), (0, 1), "{s:?}");
    assert_eq!(count(&mut tx, "medication_statement", p).await?, 0);
    Ok(())
}

/// C-F7: a problem whose observation has an end date and no status observation is resolved.
#[test]
fn a_problem_with_an_end_date_is_resolved() -> R {
    let c = convert(&document(&problems_section(&format!(
        "{}{}",
        problem_entry(
            "ended",
            r#"code="233604007" displayName="Pneumonia""#,
            r#"<low value="20200110"/><high value="20200301"/>"#,
            ""
        ),
        problem_entry(
            "open",
            r#"code="38341003" displayName="Hypertension""#,
            r#"<low value="20190101"/>"#,
            ""
        )
    ))))?;
    let conditions = resources(&c, "Condition");
    let status: Vec<(&str, &str)> = conditions
        .iter()
        .map(|r| {
            (
                r["code"]["coding"][0]["code"].as_str().unwrap_or_default(),
                r["clinicalStatus"]["coding"][0]["code"].as_str().unwrap_or_default(),
            )
        })
        .collect();
    assert_eq!(status, [("233604007", "resolved"), ("38341003", "active")]);
    assert_eq!(conditions[0]["abatementDateTime"], "2020-03-01");
    Ok(())
}

/// C-F7: a Medication Activity in INT mood is a MedicationRequest (an order), not a medicine taken.
#[test]
fn an_intended_medicine_is_a_request() -> R {
    let c = convert(&document(
        r#"<component><section><templateId root="2.16.840.1.113883.10.20.22.2.1.1"/><title>MEDICATIONS</title>
          <entry><substanceAdministration classCode="SBADM" moodCode="INT">
            <templateId root="2.16.840.1.113883.10.20.22.4.16"/><id root="1.2.3.4" extension="med-int-1"/>
            <statusCode code="active"/>
            <consumable><manufacturedProduct><manufacturedMaterial>
              <code code="860975" codeSystem="2.16.840.1.113883.6.88" displayName="Metformin 500 MG Oral Tablet"/>
            </manufacturedMaterial></manufacturedProduct></consumable></substanceAdministration></entry>
        </section></component>"#,
    ))?;
    assert_eq!(made(&c, Section::Medications), counts(&[("MedicationRequest", 1)]));
    let order = resources(&c, "MedicationRequest")[0];
    assert_eq!(
        (&order["intent"], &order["status"]),
        (&json!("order"), &json!("active"))
    );
    Ok(())
}

/// C-F7: 256 levels of elements are read, 257 are refused.
#[test]
fn nesting_is_refused_one_level_past_the_limit() -> R {
    // `levels` elements deep, the ClinicalDocument root being the first
    let nested = |levels: usize| {
        format!(
            r#"<ClinicalDocument xmlns="urn:hl7-org:v3">{}{}</ClinicalDocument>"#,
            "<a>".repeat(levels - 1),
            "</a>".repeat(levels - 1)
        )
        .into_bytes()
    };
    assert_eq!(ccda::xml::MAX_DEPTH, 256);
    convert(&nested(256))?;
    match convert(&nested(257)) {
        Err(Error::Invalid(m)) => assert!(m.contains("nests deeper"), "{m}"),
        other => panic!("257 levels: {other:?}"),
    }
    Ok(())
}

/// C-F3: a stored file over the limit is refused by the import, as the converter refuses it.
#[tokio::test]
async fn an_oversized_stored_file_is_refused_by_the_import() -> R {
    let env = Env::new().await?;
    let mut tx = env.tx().await?;
    let fam = family(&mut tx).await?;
    let mut huge = b"<ClinicalDocument xmlns=\"urn:hl7-org:v3\">".to_vec();
    huge.resize(ccda::xml::MAX_BYTES + 1, b' ');
    let doc = store_doc(&mut tx, &env.blobs, &fam, "lucia", &huge, "huge.xml", ccda::MIME).await?;
    match import_ccda(&mut tx, &env.blobs, doc).await {
        Err(Error::Invalid(m)) => assert!(m.contains("MB"), "{m}"),
        other => panic!("{other:?}"),
    }
    Ok(())
}

/// C-F10: the import holds its document row from its first read, so a move waits for the import to
/// commit instead of slipping in while it converts.
#[tokio::test]
async fn the_import_locks_its_document_before_it_reads_the_profile() -> R {
    let env = Env::new().await?;
    let mut tx = env.tx().await?;
    let fam = family(&mut tx).await?;
    as_isabella(&mut tx, &fam, "lucia").await?;
    let bytes = document(&problems_section(&problem_entry(
        "p",
        r#"code="233604007" displayName="Pneumonia""#,
        "",
        "",
    )));
    let doc = store_doc(&mut tx, &env.blobs, &fam, "lucia", &bytes, "p.xml", ccda::MIME).await?;
    tx.commit().await?;
    // the profile table is held, so the import stops at its profile read
    let mut gate = env.tx().await?;
    sqlx::query("lock table profile in access exclusive mode")
        .execute(&mut *gate)
        .await?;
    let (pool, blobs) = (env.db.pool.clone(), env.blobs.clone());
    let run = tokio::spawn(async move {
        let mut t = pool.begin().await?;
        let s = import_ccda(&mut t, &blobs, doc).await?;
        t.commit().await?;
        anyhow::Ok(s)
    });
    let mut waiting = 0i64;
    for _ in 0..200 {
        waiting = sqlx::query_scalar(
            "select count(*) from pg_locks l join pg_class c on c.oid = l.relation \
             where c.relname = 'profile' and not l.granted",
        )
        .fetch_one(&env.db.pool)
        .await?;
        if waiting > 0 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert_eq!(waiting, 1, "the import never reached its profile read");
    let mut mover = env.tx().await?;
    sqlx::query("set local lock_timeout = '500ms'")
        .execute(&mut *mover)
        .await?;
    let moved = sqlx::query("update document set flags = flags where id = $1")
        .bind(doc)
        .execute(&mut *mover)
        .await;
    mover.rollback().await?;
    gate.rollback().await?;
    let s = run.await??;
    assert_eq!(s.accepted, 1, "{s:?}");
    let e = moved
        .err()
        .ok_or_else(|| anyhow::anyhow!("the document row was free while the import ran"))?;
    assert!(e.to_string().contains("lock timeout"), "{e}");
    Ok(())
}

/// U-F1: a DS4P security label (an entry's Security Observation, a section's) or a confidentiality
/// code reaches meta.security, so the record is as sensitive as the same label in a FHIR file.
#[tokio::test]
async fn ds4p_security_labels_make_the_records_sensitive() -> R {
    let hiv = problem_entry(
        "hiv",
        r#"code="86406008" displayName="Human immunodeficiency virus infection""#,
        r#"<low value="20200110"/>"#,
        &format!(
            r#"<entryRelationship typeCode="COMP">{}</entryRelationship>"#,
            security_label("HIV")
        ),
    );
    let flu = problem_entry(
        "flu",
        r#"code="6142004" displayName="Influenza""#,
        r#"<low value="20200110"/>"#,
        "",
    );
    let depression = problem_entry(
        "dep",
        r#"code="35489007" displayName="Depressive disorder""#,
        r#"<low value="20200110"/>"#,
        "",
    );
    let body = format!(
        r#"{}<component><section><templateId root="2.16.840.1.113883.10.20.22.2.5.1"/>
             <templateId root="2.16.840.1.113883.3.3445.3"/><title>PROBLEMS (restricted)</title>
             <confidentialityCode code="R" codeSystem="2.16.840.1.113883.5.25"/>
             <entry>{}</entry>{depression}</section></component>"#,
        problems_section(&format!("{hiv}{flu}")),
        security_label("PSY")
    );
    let bytes = document(&body);
    let c = convert(&bytes)?;
    let labels: BTreeMap<String, Vec<String>> = resources(&c, "Condition")
        .iter()
        .map(|r| {
            let codes = r["meta"]["security"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|s| s["code"].as_str().map(String::from))
                .collect();
            (
                r["code"]["coding"][0]["code"].as_str().unwrap_or_default().to_string(),
                codes,
            )
        })
        .collect();
    // the section's own label is no problem of its own
    assert_eq!(
        labels,
        BTreeMap::from([
            ("86406008".to_string(), vec!["HIV".to_string()]),
            ("6142004".to_string(), vec![]),
            ("35489007".to_string(), vec!["R".to_string(), "PSY".to_string()]),
        ])
    );
    assert!(c.sections.iter().all(|s| s.not_read.is_empty()), "{:?}", c.sections);

    let env = Env::new().await?;
    let mut tx = env.tx().await?;
    let fam = family(&mut tx).await?;
    let p = as_isabella(&mut tx, &fam, "lucia").await?;
    let doc = store_doc(&mut tx, &env.blobs, &fam, "lucia", &bytes, "ds4p.xml", ccda::MIME).await?;
    import_ccda(&mut tx, &env.blobs, doc).await?;
    let rows: Vec<(String, Vec<String>)> =
        sqlx::query_as("select fhir->'code'->'coding'->0->>'code', sensitivity from condition where profile_id = $1")
            .bind(p)
            .fetch_all(&mut *tx)
            .await?;
    let rows: BTreeMap<String, Vec<String>> = rows.into_iter().collect();
    assert_eq!(
        rows,
        BTreeMap::from([
            ("86406008".to_string(), vec!["sexual_health".to_string()]),
            ("6142004".to_string(), vec![]),
            ("35489007".to_string(), vec!["mental_health".to_string()]),
        ])
    );
    assert!(ownhealth_core::documents::document_is_sensitive(&mut tx, doc).await?);
    Ok(())
}

/// U-F2: what no reader read (sections and parts left for review, narrative-only sections) keeps the
/// stored file behind read:sensitive, as a connector pull's unprojected resources do. A file read in
/// full stays open.
#[tokio::test]
async fn a_ccd_with_unread_sections_keeps_its_file_closed() -> R {
    let env = Env::new().await?;
    let mut tx = env.tx().await?;
    let fam = family(&mut tx).await?;
    as_isabella(&mut tx, &fam, "lucia").await?;
    let hp = fixture("history-and-physical.xml");
    let doc = store_doc(&mut tx, &env.blobs, &fam, "lucia", &hp, "hp.xml", ccda::MIME).await?;
    import_ccda(&mut tx, &env.blobs, doc).await?;
    let held: i64 = open_items(&mut tx, doc, "fhir_unmapped_summary")
        .await?
        .iter()
        .map(|i| i.payload["count"].as_i64().unwrap_or(0))
        .sum();
    let flags: Value = sqlx::query_scalar("select flags from document where id = $1")
        .bind(doc)
        .fetch_one(&mut *tx)
        .await?;
    assert!(held > 0);
    assert_eq!(flags["unprojected"], json!(held));
    assert!(ownhealth_core::documents::document_is_sensitive(&mut tx, doc).await?);

    let read = document(&problems_section(&problem_entry(
        "p",
        r#"code="233604007" displayName="Pneumonia""#,
        "",
        "",
    )));
    let doc = store_doc(&mut tx, &env.blobs, &fam, "lucia", &read, "p.xml", ccda::MIME).await?;
    import_ccda(&mut tx, &env.blobs, doc).await?;
    let flags: Value = sqlx::query_scalar("select flags from document where id = $1")
        .bind(doc)
        .fetch_one(&mut *tx)
        .await?;
    assert!(flags.get("unprojected").is_none(), "{flags}");
    assert!(!ownhealth_core::documents::document_is_sensitive(&mut tx, doc).await?);
    Ok(())
}

/// U-F6: HL7 CCD 1 puts a Severity Observation on an allergy concern act. It is no allergy of its
/// own; a severity the converter cannot map is listed for review.
#[test]
fn the_ccd_1_sample_reads_each_allergy_once() -> R {
    let c = convert(&fixture("ccd-1.xml"))?;
    assert_eq!(made(&c, Section::Allergies), counts(&[("AllergyIntolerance", 2)]));
    let substances: Vec<&str> = resources(&c, "AllergyIntolerance")
        .iter()
        .map(|r| r["code"]["coding"][0]["code"].as_str().unwrap_or_default())
        .collect();
    assert_eq!(substances, ["70618", "2670"]);
    let allergies = c
        .sections
        .iter()
        .find(|s| s.kind == Some(Section::Allergies))
        .ok_or_else(|| anyhow::anyhow!("no allergies section"))?;
    // "Moderate to severe" has no FHIR severity
    assert_eq!(allergies.not_read, ["observation 2.16.840.1.113883.10.20.22.4.8"]);
    Ok(())
}
