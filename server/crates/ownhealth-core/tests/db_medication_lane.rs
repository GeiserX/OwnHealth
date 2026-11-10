//! The medication lane (migration 1100): schedule, prescriber and the codes as received travel from
//! the model to the row and to the FHIR we build, and are null when absent.

mod common;

use common::*;
use ownhealth_core::provenance::SrcRef;
use ownhealth_core::records::{RecordIn, RecordOptions, add_record, get_record, supersede_record};
use ownhealth_core::testing::TestDb;
use serde_json::{Value, json};

fn med(v: Value) -> RecordIn {
    RecordIn::from_json("medication", v).unwrap_or_else(|e| panic!("bad test record: {e}"))
}

#[tokio::test]
async fn schedule_prescriber_and_codes_round_trip() -> R {
    let db = TestDb::new().await?;
    let mut c = db.pool.acquire().await?;
    let f = family(&mut c).await?;
    let p = f.profile("carmen");
    let src: SrcRef = manual_src().into();
    let codes = json!([
        {"system": "http://www.nlm.nih.gov/research/umls/rxnorm", "code": "860975", "display": "metformin 850 MG Oral Tablet"},
        {"system": "http://www.whocc.no/atc", "code": "A10BA02"},
    ]);
    let full = add_record(
        &mut c,
        &med(json!({
            "profile_id": p, "name": "Metformin", "strength": "850 mg", "dose_text": "1 tablet",
            "schedule_text": "twice daily, with meals", "prescriber": "Dra. Synthetic (LabNorte clinic)",
            "code_system": "rxnorm", "code": "860975", "codes": codes, "started": "2024-02-01",
            "reason_text": "Type 2 diabetes mellitus",
        })),
        &src,
        RecordOptions::new("person"),
    )
    .await?;
    let row = get_record(&mut c, "medication", full.id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("missing"))?;
    assert_eq!(row["schedule_text"], "twice daily, with meals");
    assert_eq!(row["prescriber"], "Dra. Synthetic (LabNorte clinic)");
    assert_eq!(row["codes"], codes);
    // the FHIR we build carries them too: timing text, the prescriber, the reason and every coding
    let fhir = &row["fhir"];
    assert_eq!(
        fhir["dosage"],
        json!([{"text": "1 tablet", "timing": {"code": {"text": "twice daily, with meals"}}}])
    );
    assert_eq!(
        fhir["informationSource"],
        json!({"display": "Dra. Synthetic (LabNorte clinic)"})
    );
    assert_eq!(fhir["reasonCode"], json!([{"text": "Type 2 diabetes mellitus"}]));
    let coding = fhir["medicationCodeableConcept"]["coding"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    assert_eq!(
        coding.len(),
        2,
        "the primary RxNorm code once, then the ATC code: {coding:?}"
    );
    assert_eq!(coding[0]["code"], "860975");
    assert_eq!(
        coding[1],
        json!({"system": "http://www.whocc.no/atc", "code": "A10BA02"})
    );

    // absent fields stay null, and the FHIR of a plain row is what it always was
    let plain = add_record(
        &mut c,
        &med(json!({"profile_id": p, "name": "Omeprazole", "strength": "20 mg", "dose_text": "1 capsule", "started": "2026-04-01"})),
        &src,
        RecordOptions::new("person"),
    )
    .await?;
    let row = get_record(&mut c, "medication", plain.id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("missing"))?;
    for k in ["schedule_text", "prescriber", "codes"] {
        assert_eq!(row[k], Value::Null, "{k}");
    }
    assert_eq!(
        row["fhir"],
        json!({"resourceType": "MedicationStatement", "status": "active",
               "medicationCodeableConcept": {"text": "Omeprazole 20 mg"},
               "effectivePeriod": {"start": "2026-04-01"}, "dosage": [{"text": "1 capsule"}]})
    );

    // a new schedule is a new version; the old one keeps its own
    let next = supersede_record(
        &mut c,
        plain.id,
        &med(
            json!({"profile_id": p, "name": "Omeprazole", "strength": "20 mg", "dose_text": "1 capsule",
                     "schedule_text": "before breakfast", "started": "2026-04-01"}),
        ),
        &src,
        "person",
        "schedule added",
    )
    .await?;
    let row = get_record(&mut c, "medication", next.id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("missing"))?;
    assert_eq!(
        (&row["schedule_text"], &row["supersedes"]),
        (&json!("before breakfast"), &json!(plain.id.to_string()))
    );
    let old = get_record(&mut c, "medication", plain.id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("missing"))?;
    assert_eq!(
        (&old["schedule_text"], &old["status"]),
        (&Value::Null, &json!("superseded"))
    );
    Ok(())
}

#[test]
fn codes_must_be_coded() {
    let bad = RecordIn::from_json(
        "medication",
        json!({"profile_id": uuid::Uuid::nil(), "name": "X", "codes": [{"system": "http://www.whocc.no/atc"}]}),
    );
    assert!(bad.is_err(), "a coding without a code is refused");
    let bad = RecordIn::from_json(
        "medication",
        json!({"profile_id": uuid::Uuid::nil(), "name": "X", "codes": {"code": "A10BA02"}}),
    );
    assert!(bad.is_err(), "codes is a list");
}
