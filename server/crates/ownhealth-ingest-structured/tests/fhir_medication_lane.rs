//! The medication lane on FHIR import: every coding, the prescriber and the timing text are kept as
//! received. Synthetic Carmen Ortega IPS; the national code system and its code are made up.

mod common;

use chrono::NaiveDate;
use common::{Env, JSON, R, family, store_doc};
use ownhealth_core::records::list_records;
use ownhealth_ingest_structured::fhir::import_fhir;
use ownhealth_ingest_structured::fhir::synthetic::ips_bundle;
use serde_json::{Value, json};

const RXNORM: &str = "http://www.nlm.nih.gov/research/umls/rxnorm";
const ATC: &str = "http://www.whocc.no/atc";
const NATIONAL: &str = "https://example.org/fhir/sid/national-drug-code";

#[tokio::test]
async fn carmen_ips_keeps_every_coding_the_prescriber_and_the_schedule() -> R {
    let env = Env::new().await?;
    let mut tx = env.tx().await?;
    let fam = family(&mut tx).await?;
    let p = fam.profile("carmen");
    let metformin = json!({
        "resourceType": "MedicationStatement",
        "id": "carmen-ortega-metformin",
        "status": "active",
        "medicationCodeableConcept": {
            // the national code first: RxNorm still becomes the primary code
            "coding": [
                {"system": NATIONAL, "code": "000000-0", "display": "Metformina 850 mg comprimidos"},
                {"system": ATC, "code": "A10BA02", "display": "metformin"},
                {"system": RXNORM, "code": "860975", "display": "metformin hydrochloride 850 MG Oral Tablet"},
            ],
            "text": "Metformin 850 mg",
        },
        "subject": {"reference": "Patient/carmen-ortega"},
        "effectivePeriod": {"start": "2024-02-01"},
        "informationSource": {"display": "Dra. Synthetic, LabNorte clinic"},
        "reasonCode": [{"text": "Type 2 diabetes mellitus"}],
        "dosage": [{"text": "1 tablet", "timing": {"code": {"text": "twice daily, with meals"}}}],
    });
    let bundle = ips_bundle(
        "carmen-meds",
        "Carmen",
        "Ortega",
        NaiveDate::from_ymd_opt(1955, 1, 30).unwrap_or_default(),
        false,
        &[metformin],
    );
    let doc = store_doc(
        &mut tx,
        &env.blobs,
        &fam,
        "carmen",
        &serde_json::to_vec(&bundle)?,
        "ips-carmen.json",
        JSON,
    )
    .await?;
    import_fhir(&mut tx, &env.blobs, doc).await?;
    let meds = list_records(&mut tx, "medication", p, None, true).await?;
    let m = meds
        .iter()
        .find(|m| m["name"] == "Metformin 850 mg")
        .ok_or_else(|| anyhow::anyhow!("metformin not imported: {meds:?}"))?;
    assert_eq!((&m["code_system"], &m["code"]), (&json!("rxnorm"), &json!("860975")));
    let systems: Vec<&str> = m["codes"]
        .as_array()
        .map(|a| a.iter().filter_map(|c| c["system"].as_str()).collect())
        .unwrap_or_default();
    assert_eq!(
        systems,
        vec![NATIONAL, ATC, RXNORM],
        "every coding, in the order received"
    );
    assert_eq!(
        m["codes"][0],
        json!({"system": NATIONAL, "code": "000000-0", "display": "Metformina 850 mg comprimidos"})
    );
    assert_eq!(m["prescriber"], "Dra. Synthetic, LabNorte clinic");
    assert_eq!(m["schedule_text"], "twice daily, with meals");
    assert_eq!(m["dose_text"], "1 tablet");
    assert_eq!(m["reason_text"], "Type 2 diabetes mellitus");
    // a medication through medicationReference keeps its one coding too
    let enalapril = meds
        .iter()
        .find(|m| m["name"] == "Enalapril 10 mg")
        .ok_or_else(|| anyhow::anyhow!("enalapril not imported"))?;
    assert_eq!(
        enalapril["codes"],
        json!([{"system": ATC, "code": "C09AA02", "display": "enalapril"}])
    );
    assert_eq!(
        (&enalapril["prescriber"], &enalapril["schedule_text"]),
        (&Value::Null, &Value::Null)
    );
    Ok(())
}

#[tokio::test]
async fn a_medication_request_keeps_its_requester() -> R {
    let env = Env::new().await?;
    let mut tx = env.tx().await?;
    let fam = family(&mut tx).await?;
    let request = json!({
        "resourceType": "MedicationRequest",
        "id": "carmen-ortega-vitd",
        "status": "active",
        "intent": "order",
        "medicationCodeableConcept": {"coding": [{"system": ATC, "code": "A11CC05", "display": "colecalciferol"}], "text": "Colecalciferol 25000 UI"},
        "subject": {"reference": "Patient/carmen-ortega"},
        "authoredOn": "2026-01-10",
        "requester": {"display": "Dr. Synthetic"},
        "dosageInstruction": [{"text": "1 ampoule", "timing": {"code": {"text": "once a month"}}}],
    });
    let bundle = ips_bundle(
        "carmen-req",
        "Carmen",
        "Ortega",
        NaiveDate::from_ymd_opt(1955, 1, 30).unwrap_or_default(),
        false,
        &[request],
    );
    let doc = store_doc(
        &mut tx,
        &env.blobs,
        &fam,
        "carmen",
        &serde_json::to_vec(&bundle)?,
        "ips-carmen-req.json",
        JSON,
    )
    .await?;
    import_fhir(&mut tx, &env.blobs, doc).await?;
    let meds = list_records(&mut tx, "medication", fam.profile("carmen"), None, true).await?;
    let m = meds
        .iter()
        .find(|m| m["name"] == "Colecalciferol 25000 UI")
        .ok_or_else(|| anyhow::anyhow!("the request was not imported: {meds:?}"))?;
    assert_eq!(
        (&m["prescriber"], &m["schedule_text"]),
        (&json!("Dr. Synthetic"), &json!("once a month"))
    );
    Ok(())
}

#[tokio::test]
async fn a_patient_report_has_no_prescriber_and_a_blank_coding_is_dropped() -> R {
    let env = Env::new().await?;
    let mut tx = env.tx().await?;
    let fam = family(&mut tx).await?;
    let statement = json!({
        "resourceType": "MedicationStatement",
        "id": "carmen-ortega-atorvastatin",
        "status": "active",
        "medicationCodeableConcept": {
            "coding": [
                {"system": ATC, "code": " "},
                {"system": RXNORM, "code": "617310", "display": "atorvastatin 20 MG Oral Tablet"},
            ],
            "text": "Atorvastatin 20 mg",
        },
        "subject": {"reference": "Patient/carmen-ortega"},
        "effectivePeriod": {"start": "2025-03-01"},
        "informationSource": {"reference": "Patient/carmen-ortega", "display": "Carmen Ortega"},
    });
    let by_clinician = json!({
        "resourceType": "MedicationStatement",
        "id": "carmen-ortega-ramipril",
        "status": "active",
        "medicationCodeableConcept": {"coding": [{"system": ATC, "code": "C09AA05"}], "text": "Ramipril 5 mg"},
        "subject": {"reference": "Patient/carmen-ortega"},
        "effectivePeriod": {"start": "2025-03-01"},
        "informationSource": {"reference": "Practitioner/synthetic-1", "display": "Dra. Synthetic"},
    });
    let bundle = ips_bundle(
        "carmen-self",
        "Carmen",
        "Ortega",
        NaiveDate::from_ymd_opt(1955, 1, 30).unwrap_or_default(),
        false,
        &[statement, by_clinician],
    );
    let doc = store_doc(
        &mut tx,
        &env.blobs,
        &fam,
        "carmen",
        &serde_json::to_vec(&bundle)?,
        "ips-carmen-self.json",
        JSON,
    )
    .await?;
    import_fhir(&mut tx, &env.blobs, doc).await?;
    let meds = list_records(&mut tx, "medication", fam.profile("carmen"), None, true).await?;
    let find = |name: &str| {
        meds.iter()
            .find(|m| m["name"] == name)
            .ok_or_else(|| anyhow::anyhow!("{name} not imported: {meds:?}"))
    };
    let m = find("Atorvastatin 20 mg")?;
    assert_eq!(
        m["prescriber"],
        Value::Null,
        "the patient reported it; nobody is named as prescriber"
    );
    assert_eq!(
        m["codes"],
        json!([{"system": RXNORM, "code": "617310", "display": "atorvastatin 20 MG Oral Tablet"}])
    );
    assert_eq!(find("Ramipril 5 mg")?["prescriber"], "Dra. Synthetic");
    Ok(())
}
