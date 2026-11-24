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
    // every entry of the six sections was read; the medicine's supply order and dispense were not
    let left: Vec<&String> = c
        .sections
        .iter()
        .filter(|s| s.kind.is_some())
        .flat_map(|s| &s.not_read)
        .collect();
    assert_eq!(
        left,
        [
            "supply 2.16.840.1.113883.10.20.22.4.17",
            "supply 2.16.840.1.113883.10.20.22.4.18"
        ],
        "{left:?}"
    );
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
    // waits in review; every lab and vital sign is a value or a review row; 6 held section items (the
    // medicine's supply order and dispense among them)
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
            "MEDICATIONS (C-CDA)",
            "PLAN OF CARE (C-CDA)",
            "PROCEDURES (C-CDA)",
            "SOCIAL HISTORY (C-CDA)"
        ]
    );
    let values = count(&mut tx, "observation", p).await?;
    let rows = open_items(&mut tx, doc, "candidate_row").await?.len() as i64;
    assert_eq!(values + rows, 9, "3 labs and 6 vital signs");
    // 10 records (one of them the refused vaccine), 9 values, 6 held items: each counted once
    assert_eq!(s.accepted + s.queued + s.skipped, 10 + 9 + 6, "{s:?}");
    let flags: Value = sqlx::query_scalar("select flags from document where id = $1")
        .bind(doc)
        .fetch_one(&mut *tx)
        .await?;
    assert_eq!(flags["ccda"]["sections"].as_array().map(Vec::len), Some(17));
    assert_eq!(flags["import"]["accepted"], json!(s.accepted));
    // a second run reads the stored summary and adds nothing
    let again = import_ccda(&mut tx, &env.blobs, doc).await?;
    assert_eq!(again, s);
    assert_eq!(open_items(&mut tx, doc, "fhir_unmapped_summary").await?.len(), 6);
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

/// R1-H1: a DS4P label in the document's Privacy Markings Section (templateId
/// 2.16.840.1.113883.3.3445.4) labels the whole document, so every record made from it is sensitive,
/// even one that carries no label of its own.
#[tokio::test]
async fn a_privacy_markings_section_labels_every_record_of_the_document() -> R {
    let body = format!(
        r#"<component><section><templateId root="2.16.840.1.113883.3.3445.4"/><title>Privacy Markings</title>
             <text>This document contains HIV and mental health information</text><entry>{}</entry><entry>{}</entry>
           </section></component>{}"#,
        security_label("HIV"),
        security_label("PSY"),
        problems_section(&problem_entry(
            "hiv",
            r#"code="86406008" displayName="Human immunodeficiency virus infection""#,
            r#"<low value="20200110"/>"#,
            "",
        ))
    );
    let bytes = document(&body);
    let c = convert(&bytes)?;
    let labels: Vec<Vec<String>> = resources(&c, "Condition")
        .iter()
        .map(|r| {
            r["meta"]["security"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|s| s["code"].as_str().map(String::from))
                .collect()
        })
        .collect();
    // every entry of the section labels the document, not only the first
    assert_eq!(labels, vec![vec!["HIV".to_string(), "PSY".to_string()]]);

    let env = Env::new().await?;
    let mut tx = env.tx().await?;
    let fam = family(&mut tx).await?;
    let p = as_isabella(&mut tx, &fam, "lucia").await?;
    let doc = store_doc(&mut tx, &env.blobs, &fam, "lucia", &bytes, "ds4p-doc.xml", ccda::MIME).await?;
    import_ccda(&mut tx, &env.blobs, doc).await?;
    let rows: Vec<(String, Vec<String>)> =
        sqlx::query_as("select fhir->'code'->'coding'->0->>'code', sensitivity from condition where profile_id = $1")
            .bind(p)
            .fetch_all(&mut *tx)
            .await?;
    assert_eq!(
        rows,
        vec![(
            "86406008".to_string(),
            vec!["mental_health".to_string(), "sexual_health".to_string()]
        )]
    );
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

// ------------------------------------------------------------- round 3 ----

/// CR1: a CDA document whose body is a `nonXMLBody` (a scan or an attached file, never coded) is
/// refused with its own reason. It is never imported as a document that holds nothing, which would
/// leave its text unread and unflagged. A structured body still reads.
#[test]
fn an_unstructured_body_is_refused_never_read_as_empty() -> R {
    let structured = String::from_utf8(document(""))?;
    let unstructured = structured.replace(
        "<component><structuredBody></structuredBody></component>",
        r#"<component><nonXMLBody><text mediaType="text/plain">HIV test positive, see the attached letter.</text></nonXMLBody></component>"#,
    );
    assert_ne!(structured, unstructured);
    match convert(unstructured.as_bytes()) {
        Err(Error::Invalid(m)) => assert!(m.contains("nonXMLBody"), "{m}"),
        other => panic!("a nonXMLBody was read: {other:?}"),
    }
    convert(structured.as_bytes())?;
    Ok(())
}

/// CR5: a C-CDA document saved as UTF-16 (either byte order) reaches the importer, which refuses it
/// with its own reason (save it as UTF-8) instead of an unknown file.
#[test]
fn a_utf16_document_is_refused_with_its_own_reason() -> R {
    let text = String::from_utf8(fixture("ccd-2.xml"))?.replacen("encoding=\"UTF-8\"", "encoding=\"UTF-16\"", 1);
    for le in [true, false] {
        let bom: &[u8] = if le { b"\xff\xfe" } else { b"\xfe\xff" };
        let bytes: Vec<u8> = bom
            .iter()
            .copied()
            .chain(
                text.encode_utf16()
                    .flat_map(|u| if le { u.to_le_bytes() } else { u.to_be_bytes() }),
            )
            .collect();
        assert_eq!(sniff("record.xml", &bytes[..4096], None), "ccda", "le {le}");
        match convert(&bytes) {
            Err(Error::Invalid(m)) => assert!(m.contains("UTF-16") && m.contains("UTF-8"), "{m}"),
            other => panic!("le {le}: {other:?}"),
        }
    }
    Ok(())
}

/// CR6: CCD 1's first medicine has a supply order and a dispense; neither is read, so both are listed
/// for review. The MedicationStatements stay.
#[test]
fn medication_supply_and_dispense_are_listed_as_not_read() -> R {
    let c = convert(&fixture("ccd-1.xml"))?;
    assert_eq!(made(&c, Section::Medications), counts(&[("MedicationStatement", 2)]));
    let meds = c
        .sections
        .iter()
        .find(|s| s.kind == Some(Section::Medications))
        .ok_or_else(|| anyhow::anyhow!("no medications section"))?;
    assert_eq!(
        meds.not_read,
        [
            "supply 2.16.840.1.113883.10.20.22.4.17",
            "supply 2.16.840.1.113883.10.20.22.4.18"
        ]
    );
    Ok(())
}

/// A Problem Status Observation saying `code` (SNOMED: 55561003 active, 413322009 resolved).
fn problem_status(code: &str) -> String {
    format!(
        r#"<entryRelationship typeCode="REFR"><observation classCode="OBS" moodCode="EVN">
          <templateId root="2.16.840.1.113883.10.20.22.4.6"/><code code="33999-4" codeSystem="2.16.840.1.113883.6.1"/>
          <statusCode code="completed"/><value xsi:type="CD" code="{code}" codeSystem="2.16.840.1.113883.6.96"/>
        </observation></entryRelationship>"#
    )
}

/// An allergy concern act (status `act_status`) around one penicillin allergy.
fn allergy_entry(id: &str, act_status: &str) -> String {
    format!(
        r#"<entry><act classCode="ACT" moodCode="EVN"><templateId root="2.16.840.1.113883.10.20.22.4.30"/>
          <id root="1.2.3.4" extension="alg-act-{id}"/><code code="CONC" codeSystem="2.16.840.1.113883.5.6"/>
          <statusCode code="{act_status}"/>
          <entryRelationship typeCode="SUBJ"><observation classCode="OBS" moodCode="EVN">
            <templateId root="2.16.840.1.113883.10.20.22.4.7"/><id root="1.2.3.4" extension="alg-{id}"/>
            <code code="ASSERTION" codeSystem="2.16.840.1.113883.5.4"/><statusCode code="completed"/>
            <effectiveTime><low value="20200101"/></effectiveTime>
            <value xsi:type="CD" code="419511003" codeSystem="2.16.840.1.113883.6.96"/>
            <participant typeCode="CSM"><participantRole classCode="MANU"><playingEntity classCode="MMAT">
              <code code="7980" codeSystem="2.16.840.1.113883.6.88" displayName="Penicillin G"/>
            </playingEntity></participantRole></participant>
          </observation></entryRelationship></act></entry>"#
    )
}

/// CR7: FHIR R4 con-5 and ait-2 (no clinicalStatus on an entered-in-error record) and con-4 (an
/// abated condition is never active). A problem whose status observation says active while its
/// effectiveTime has an end is held for review with that reason; one that says resolved keeps its end.
#[test]
fn entered_in_error_has_no_clinical_status_and_an_active_problem_with_an_end_is_held() -> R {
    let c = convert(&document(&format!(
        r#"{}<component><section><templateId root="2.16.840.1.113883.10.20.22.2.6.1"/><title>ALLERGIES</title>{}{}</section></component>"#,
        problems_section(&format!(
            "{}{}{}",
            problem_entry(
                "void",
                r#"code="233604007" displayName="Pneumonia""#,
                r#"<low value="20200110"/><high value="20200301"/>"#,
                r#"<statusCode code="nullified"/>"#
            ),
            problem_entry(
                "conflict",
                r#"code="38341003" displayName="Hypertension""#,
                r#"<low value="20190101"/><high value="20200301"/>"#,
                &problem_status("55561003")
            ),
            problem_entry(
                "ended",
                r#"code="195967001" displayName="Asthma""#,
                r#"<low value="20100101"/><high value="20200301"/>"#,
                &problem_status("413322009")
            )
        )),
        allergy_entry("void", "nullified"),
        allergy_entry("kept", "active")
    )))?;
    let conditions: Vec<(&str, &Value, &Value, &Value)> = resources(&c, "Condition")
        .into_iter()
        .map(|r| {
            (
                r["code"]["coding"][0]["code"].as_str().unwrap_or_default(),
                &r["verificationStatus"]["coding"][0]["code"],
                &r["clinicalStatus"],
                &r["abatementDateTime"],
            )
        })
        .collect();
    assert_eq!(
        conditions,
        [
            ("233604007", &json!("entered-in-error"), &Value::Null, &Value::Null),
            (
                "195967001",
                &json!("confirmed"),
                &json!({"coding": [{"system": "http://terminology.hl7.org/CodeSystem/condition-clinical", "code": "resolved"}]}),
                &json!("2020-03-01")
            ),
        ]
    );
    let problems = c
        .sections
        .iter()
        .find(|s| s.kind == Some(Section::Problems))
        .ok_or_else(|| anyhow::anyhow!("no problems section"))?;
    assert_eq!(
        problems.not_read,
        ["observation 2.16.840.1.113883.10.20.22.4.4 (status active with an end date)"]
    );
    let allergies: Vec<(&Value, bool)> = resources(&c, "AllergyIntolerance")
        .into_iter()
        .map(|r| {
            (
                &r["verificationStatus"]["coding"][0]["code"],
                r.get("clinicalStatus").is_some(),
            )
        })
        .collect();
    assert_eq!(
        allergies,
        [(&json!("entered-in-error"), false), (&json!("confirmed"), true)]
    );
    Ok(())
}

/// XML text with `&`, `<`, `>` and `"` escaped.
fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn collapse(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Every string inside a JSON value.
fn strings(v: &Value, out: &mut Vec<String>) {
    match v {
        Value::String(s) => out.push(s.clone()),
        Value::Array(a) => a.iter().for_each(|x| strings(x, out)),
        Value::Object(o) => o.values().for_each(|x| strings(x, out)),
        _ => {}
    }
}

/// The planted instructions of one injection fixture: each literal string of a PDF (its page and its
/// metadata), each paragraph of a text file (HTML tags taken off) and each string of a JSON file that
/// holds the marker, and the `.truth` file's `injection_text`.
fn planted(dir: &std::path::Path, fixture: &Value, truth: &Value, marker: &str) -> Vec<String> {
    let mut out = Vec::new();
    for f in fixture["files"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
    {
        let text = String::from_utf8_lossy(&std::fs::read(dir.join(f)).unwrap_or_default()).into_owned();
        let mut pieces = Vec::new();
        if f.ends_with(".pdf") {
            pieces.extend(
                text.split('(')
                    .skip(1)
                    .filter_map(|s| s.split_once(')').map(|(a, _)| a.to_string())),
            );
        } else if f.ends_with(".json") {
            strings(&serde_json::from_str(&text).unwrap_or_default(), &mut pieces);
        } else {
            let mut para = String::new();
            for line in text.lines().chain([""]) {
                if line.trim().is_empty() {
                    pieces.push(std::mem::take(&mut para));
                } else {
                    para.push_str(line);
                    para.push('\n');
                }
            }
        }
        for p in pieces {
            // tags off, so a hidden HTML div gives its words
            let mut words = String::new();
            let mut tag = false;
            for ch in p.chars() {
                match ch {
                    '<' => tag = true,
                    '>' if tag => {
                        tag = false;
                        words.push(' ');
                    }
                    _ if !tag => words.push(ch),
                    _ => {}
                }
            }
            let words = collapse(&words);
            if words.contains(marker) {
                out.push(words);
            }
        }
    }
    if let Some(t) = truth["expect"]["injection_text"].as_str() {
        out.push(collapse(t));
    }
    out.sort();
    out.dedup();
    out
}

/// A document that carries `text` as its title, in a narrative cell a result's name and value point
/// at, as the result's value, and as a medicine's sig (Instruction).
fn carrying(text: &str) -> Vec<u8> {
    let e = escape(text);
    let body = format!(
        r##"<component><section><templateId root="2.16.840.1.113883.10.20.22.2.1.1"/><title>MEDICATIONS</title>
          <entry><substanceAdministration classCode="SBADM" moodCode="EVN">
            <templateId root="2.16.840.1.113883.10.20.22.4.16"/><id root="1.2.3.4" extension="med-inj"/>
            <statusCode code="active"/>
            <consumable><manufacturedProduct><manufacturedMaterial>
              <code code="860975" codeSystem="2.16.840.1.113883.6.88" displayName="Metformin 500 MG Oral Tablet"/>
            </manufacturedMaterial></manufacturedProduct></consumable>
            <entryRelationship typeCode="COMP"><substanceAdministration classCode="SBADM" moodCode="INT">
              <templateId root="2.16.840.1.113883.10.20.22.4.147"/><code code="76662-6" codeSystem="2.16.840.1.113883.6.1"/>
              <text>{e}</text>
              <consumable><manufacturedProduct><manufacturedMaterial nullFlavor="NA"/></manufacturedProduct></consumable>
            </substanceAdministration></entryRelationship>
          </substanceAdministration></entry></section></component>
        <component><section><templateId root="2.16.840.1.113883.10.20.22.2.3.1"/><title>RESULTS</title>
          <text><table><tbody><tr><td ID="res-inj">{e}</td></tr></tbody></table></text>
          <entry><organizer classCode="BATTERY" moodCode="EVN"><templateId root="2.16.840.1.113883.10.20.22.4.1"/>
            <id root="1.2.3.4" extension="org-inj"/>
            <code code="24323-8" codeSystem="2.16.840.1.113883.6.1" displayName="Metabolic panel"/>
            <statusCode code="completed"/><effectiveTime value="20261020"/>
            <component><observation classCode="OBS" moodCode="EVN"><templateId root="2.16.840.1.113883.10.20.22.4.2"/>
              <id root="1.2.3.4" extension="obs-inj"/>
              <code code="2345-7" codeSystem="2.16.840.1.113883.6.1"><originalText><reference value="#res-inj"/></originalText></code>
              <statusCode code="completed"/><effectiveTime value="20261020"/>
              <value xsi:type="ST">{e}</value>
            </observation></component>
          </organizer></entry></section></component>"##
    );
    String::from_utf8(document(&body))
        .unwrap_or_default()
        .replacen("<title>Test document</title>", &format!("<title>{e}</title>"), 1)
        .into_bytes()
}

/// What breaks the injection rule in converted output: the marker anywhere but a free-text field
/// (`title`, `text`, `valueString`), or a number that the `.truth` says must never be a value.
fn injection_breaches(v: &Value, key: &str, marker: &str, never: &[String], out: &mut Vec<String>) {
    match v {
        Value::String(s) if s.contains(marker) && !matches!(key, "title" | "text" | "valueString") => {
            out.push(format!("{key}: {s}"));
        }
        Value::Number(n) if never.contains(&n.to_string()) => out.push(format!("{key}: {n}")),
        Value::Array(a) => a.iter().for_each(|x| injection_breaches(x, key, marker, never, out)),
        Value::Object(o) => o.iter().for_each(|(k, x)| injection_breaches(x, k, marker, never, out)),
        _ => {}
    }
}

/// CR9: the injection fixtures of `fixtures/labslips/injection/` on the C-CDA free-text paths. Each
/// planted instruction rides in a document's title, narrative (a result's name and value) and sig.
/// It is stored as written, as data: the output equals the same document with plain text in those
/// places, the marker sits only in free-text fields, and no number the `.truth` names becomes a value.
#[test]
fn injection_fixtures_on_the_ccda_free_text_paths_stay_data() -> R {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../fixtures/labslips/injection");
    let registry: Value = serde_json::from_slice(&std::fs::read(dir.join("registry.json"))?)?;
    let marker = registry["marker"].as_str().unwrap_or("CANARY");
    // the checker sees a breach: a positive control
    let mut seen = Vec::new();
    injection_breaches(
        &json!({"code": {"coding": [{"code": marker}]}, "valueQuantity": {"value": 45}}),
        "",
        marker,
        &["45".to_string()],
        &mut seen,
    );
    assert_eq!(seen.len(), 2, "{seen:?}");
    let plain = "Take one tablet by mouth daily";
    let control = convert(&carrying(plain))?;
    let fixtures = registry["fixtures"].as_array().cloned().unwrap_or_default();
    assert!(fixtures.len() >= 7, "{registry}");
    for f in &fixtures {
        let name = f["name"].as_str().unwrap_or_default();
        let truth: Value = serde_json::from_slice(&std::fs::read(dir.join(format!("{name}.truth")))?)?;
        let never: Vec<String> = truth["expect"]["never_a_value"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|v| v.as_str().map(String::from))
            .collect();
        let payloads = planted(&dir, f, &truth, marker);
        assert!(!payloads.is_empty(), "{name}: no planted text found");
        for p in payloads {
            let c = convert(&carrying(&p))?;
            let composition = resources(&c, "Composition")[0];
            let statement = resources(&c, "MedicationStatement")[0];
            let result = resources(&c, "Observation")[0];
            assert_eq!(composition["title"], json!(p), "{name}");
            assert_eq!(statement["dosage"][0]["text"], json!(p), "{name}");
            assert_eq!(result["code"]["text"], json!(p), "{name}");
            assert_eq!(result["valueString"], json!(p), "{name}");
            let mut breaches = Vec::new();
            for e in &c.entries {
                injection_breaches(&e.resource, "", marker, &never, &mut breaches);
            }
            assert!(breaches.is_empty(), "{name}: {breaches:?}");
            // the text changes nothing but the fields that hold it
            let quoted = |s: &str| serde_json::to_string(s).unwrap_or_default();
            let same = serde_json::to_string(&c.entries.iter().map(|e| &e.resource).collect::<Vec<_>>())?
                .replace(&quoted(&p), &quoted(plain));
            let want = serde_json::to_string(&control.entries.iter().map(|e| &e.resource).collect::<Vec<_>>())?;
            assert_eq!(same, want, "{name}");
            assert_eq!(c.sections, control.sections, "{name}");
        }
    }
    Ok(())
}

// ------------------------------------------------------------- round 4 ----

/// The codes of `meta.security` of each resource of `rtype`, in document order.
fn label_codes(c: &Converted, rtype: &str) -> Vec<Vec<String>> {
    resources(c, rtype)
        .iter()
        .map(|r| {
            r["meta"]["security"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|s| s["code"].as_str().map(String::from))
                .collect()
        })
        .collect()
}

/// A heart rate entry of a Vital Signs section.
fn vital_entry(id: &str) -> String {
    format!(
        r#"<entry><observation classCode="OBS" moodCode="EVN"><templateId root="2.16.840.1.113883.10.20.22.4.27"/>
          <id root="1.2.3" extension="{id}"/><code code="8867-4" codeSystem="2.16.840.1.113883.6.1" displayName="Heart rate"/>
          <statusCode code="completed"/><effectiveTime value="20240101"/><value xsi:type="PQ" value="72" unit="/min"/></observation></entry>"#
    )
}

const VITALS_HEAD: &str = r#"<templateId root="2.16.840.1.113883.10.20.22.2.4.1"/><code code="8716-3" codeSystem="2.16.840.1.113883.6.1"/><title>VITAL SIGNS</title>"#;
const RESTRICTED: &str = r#"<confidentialityCode code="R" codeSystem="2.16.840.1.113883.5.25"/>"#;

/// T6: a restricted confidentialityCode on a section reaches the records of every section nested in
/// it, at any depth, and one on the structuredBody reaches every record of the document.
#[test]
fn a_restricted_parent_section_or_body_labels_every_record_inside_it() -> R {
    let nested = format!(
        r#"<component><section><templateId root="2.16.840.1.113883.10.20.22.2.65"/><title>Behavioral health</title>
             {RESTRICTED}<text>restricted</text>
             <component><section><title>Notes</title>
               <component><section>{VITALS_HEAD}{}</section></component></section></component>
           </section></component>
           <component><section>{VITALS_HEAD}{}</section></component>"#,
        vital_entry("inside"),
        vital_entry("outside")
    );
    let c = convert(&document(&nested))?;
    assert_eq!(label_codes(&c, "Observation"), vec![vec!["R".to_string()], vec![]]);

    let body = format!(
        r#"{RESTRICTED}<component><section>{VITALS_HEAD}{}</section></component>"#,
        vital_entry("v")
    );
    let c = convert(&document(&body))?;
    assert_eq!(label_codes(&c, "Observation"), vec![vec!["R".to_string()]]);
    assert_eq!(label_codes(&c, "Composition"), vec![vec!["R".to_string()]]);
    Ok(())
}

/// An Immunization Activity in `mood` with `status`.
fn immunization_section(mood: &str, status: &str) -> String {
    format!(
        r#"<component><section><templateId root="2.16.840.1.113883.10.20.22.2.2.1"/><title>IMMUNIZATIONS</title>
          <entry><substanceAdministration classCode="SBADM" moodCode="{mood}" negationInd="false">
            <templateId root="2.16.840.1.113883.10.20.22.4.52"/><id root="1.2.3" extension="imm-{mood}-{status}"/>
            <statusCode code="{status}"/><effectiveTime value="20261201"/>
            <consumable><manufacturedProduct><templateId root="2.16.840.1.113883.10.20.22.4.54"/><manufacturedMaterial>
              <code code="88" codeSystem="2.16.840.1.113883.12.292" displayName="Influenza virus vaccine"/>
            </manufacturedMaterial></manufacturedProduct></consumable>
          </substanceAdministration></entry></section></component>"#
    )
}

/// T8: only a given vaccine (`EVN`, not active) is a completed Immunization. A planned one (`INT`)
/// and one still active are listed as not read, never recorded as given.
#[test]
fn a_planned_or_active_immunization_is_never_recorded_as_given() -> R {
    for (mood, status) in [
        ("INT", "active"),
        ("INT", "completed"),
        ("EVN", "active"),
        ("PRP", "completed"),
    ] {
        let c = convert(&document(&immunization_section(mood, status)))?;
        assert!(resources(&c, "Immunization").is_empty(), "{mood} {status}");
        let s = c
            .sections
            .iter()
            .find(|s| s.kind == Some(Section::Immunizations))
            .ok_or_else(|| anyhow::anyhow!("no immunizations section"))?;
        assert_eq!(
            s.not_read,
            ["substanceAdministration 2.16.840.1.113883.10.20.22.4.52"],
            "{mood} {status}"
        );
    }
    let c = convert(&document(&immunization_section("EVN", "completed")))?;
    let given = resources(&c, "Immunization");
    assert_eq!(given.len(), 1);
    assert_eq!(given[0]["status"], "completed");
    assert!(c.sections.iter().all(|s| s.not_read.is_empty()), "{:?}", c.sections);
    Ok(())
}

/// A medicine of `ccd-1.xml` by a word of its name.
fn ccd1_medicine(c: &Converted, word: &str) -> anyhow::Result<Value> {
    resources(c, "MedicationStatement")
        .into_iter()
        .find(|r| {
            r["medicationCodeableConcept"]["text"]
                .as_str()
                .is_some_and(|t| t.contains(word))
        })
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("no {word}"))
}

/// A Medications section with one medicine that has `extra` as more children.
fn medicine_with(extra: &str) -> Vec<u8> {
    document(&format!(
        r##"<component><section><templateId root="2.16.840.1.113883.10.20.22.2.1.1"/><title>MEDICATIONS</title>
          <text><content ID="med-n">{}</content></text>
          <entry><substanceAdministration classCode="SBADM" moodCode="EVN">
            <templateId root="2.16.840.1.113883.10.20.22.4.16"/><id root="1.2.3.4" extension="med-x"/>
            <text><reference value="#med-n"/>{}</text>
            <statusCode code="active"/>
            <consumable><manufacturedProduct><manufacturedMaterial>
              <code code="573621" codeSystem="2.16.840.1.113883.6.88" displayName="albuterol 0.09 MG/ACTUAT [Proventil]"/>
            </manufacturedMaterial></manufacturedProduct></consumable>
            {extra}
          </substanceAdministration></entry></section></component>"##,
        "narrative ".repeat(300),
        "inline ".repeat(400),
    ))
}

/// T7: an as-needed precondition ("when wheezing", CCD 1's albuterol) becomes the dosage's
/// asNeededCodeableConcept; a precondition that names nothing is listed as not read.
#[test]
fn an_as_needed_precondition_is_kept_or_listed() -> R {
    let c = convert(&fixture("ccd-1.xml"))?;
    let albuterol = ccd1_medicine(&c, "albuterol")?;
    let as_needed = &albuterol["dosage"][0]["asNeededCodeableConcept"];
    assert_eq!(as_needed["coding"][0]["code"], "56018004", "{albuterol}");
    assert_eq!(as_needed["text"], "Wheezing");
    let atenolol = ccd1_medicine(&c, "atenolol")?;
    assert!(
        atenolol["dosage"][0].get("asNeededCodeableConcept").is_none(),
        "{atenolol}"
    );

    let empty = r#"<precondition typeCode="PRCN"><criterion><templateId root="2.16.840.1.113883.10.20.22.4.25"/>
        <code code="ASSERTION" codeSystem="2.16.840.1.113883.5.4"/></criterion></precondition>"#;
    let c = convert(&medicine_with(empty))?;
    let med = resources(&c, "MedicationStatement")[0];
    assert!(med["dosage"][0].get("asNeededCodeableConcept").is_none(), "{med}");
    let s = c
        .sections
        .iter()
        .find(|s| s.kind == Some(Section::Medications))
        .ok_or_else(|| anyhow::anyhow!("no medications section"))?;
    assert_eq!(s.not_read, ["criterion 2.16.840.1.113883.10.20.22.4.25"]);
    Ok(())
}

/// T4: a sig that points at the narrative and goes on with its own words keeps both (the H&P's
/// albuterol: "2 puffs QID PRN wheezing" is written inline), within the 2,000 characters a text has.
#[test]
fn a_sig_keeps_its_own_words_beside_the_narrative_it_points_at() -> R {
    let c = convert(&fixture("history-and-physical.xml"))?;
    let albuterol = ccd1_medicine(&c, "albuterol")?;
    let text = albuterol["dosage"][0]["text"].as_str().unwrap_or_default();
    assert!(text.contains("Proventil 0.09 MG/ACTUAT inhalant solution"), "{text}");
    assert!(text.contains("2 puffs QID PRN wheezing"), "{text}");

    let c = convert(&medicine_with(""))?;
    let text = resources(&c, "MedicationStatement")[0]["dosage"][0]["text"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    assert!(text.starts_with("narrative "), "{text}");
    assert!(text.contains("inline"), "{text}");
    assert!(text.chars().count() <= 2_000, "{}", text.chars().count());
    Ok(())
}
