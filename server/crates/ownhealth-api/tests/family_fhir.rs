//! The Rivera pedigree as FHIR: the export writes FamilyMemberHistory.ndjson (every version, the
//! current one with the latest conditions), and importing it into a fresh profile gives back the same
//! relatives with nothing left unmapped.
//!
//! With `OWNHEALTH_FHIR_OUT=<dir>` the test also writes each exported resource as its own JSON file
//! there, for the HL7 validator job (.github/workflows/fhir-validate.yml).

use std::io::{Cursor, Read};

use ownhealth_core::access::{ProfileIn, create_household, create_profile, get_profile};
use ownhealth_core::blobs::BlobStore;
use ownhealth_core::demo::{DEMO_HOUSEHOLD, seed_demo};
use ownhealth_core::documents::{NewDocument, create_document};
use ownhealth_core::family::{self, FamilyCondition, LinkChange, Member, MemberIn};
use ownhealth_core::ingest::ImportSummary;
use ownhealth_core::provenance::{SrcIn, SrcRef};
use ownhealth_core::testing::TestDb;
use ownhealth_ingest_structured::fhir::{import_resources, read_entries};
use ownhealth_ops::export::export_profile;
use serde_json::Value;
use uuid::Uuid;

fn lines(bytes: &[u8]) -> anyhow::Result<Vec<Value>> {
    Ok(bytes
        .split(|b| *b == b'\n')
        .filter(|l| !l.is_empty())
        .map(serde_json::from_slice)
        .collect::<Result<_, _>>()?)
}

fn lifecycle(r: &Value) -> &str {
    r["meta"]["tag"]
        .as_array()
        .and_then(|t| t.iter().find(|t| t["system"] == "urn:ownhealth:lifecycle"))
        .and_then(|t| t["code"].as_str())
        .unwrap_or_default()
}

/// What a relative is, without ids, links or provenance.
fn essence(rows: &[Member]) -> Vec<String> {
    let mut out: Vec<String> = rows
        .iter()
        .map(|m| {
            format!(
                "{} {:?} {:?} {:?} {} {:?} {:?} {:?}",
                m.relationship,
                m.display_name,
                m.sex_at_birth,
                m.born_year,
                m.deceased,
                m.death_age,
                m.conditions.0,
                m.sensitivity
            )
        })
        .collect();
    out.sort();
    out
}

/// The profile's Patient and FamilyMemberHistory files from a real export.
async fn export_family(db: &TestDb, blobs: &BlobStore, profile: Uuid) -> anyhow::Result<(Vec<u8>, Vec<u8>)> {
    let mut tx = db.pool.begin().await?;
    sqlx::query("set transaction isolation level repeatable read")
        .execute(&mut *tx)
        .await?;
    let mut zip_bytes = Cursor::new(Vec::new());
    export_profile(&mut tx, blobs, profile, &mut zip_bytes, "zip").await?;
    tx.commit().await?;
    let mut z = zip::ZipArchive::new(Cursor::new(zip_bytes.into_inner()))?;
    let mut read = |name: &str| -> anyhow::Result<Vec<u8>> {
        let mut out = Vec::new();
        z.by_name(name)?.read_to_end(&mut out)?;
        Ok(out)
    };
    Ok((read("fhir/Patient.ndjson")?, read("fhir/FamilyMemberHistory.ndjson")?))
}

/// Imports the two files into a fresh profile of a new household, for the same person as `from`.
async fn import_fresh(
    c: &mut sqlx::PgConnection,
    blobs: &BlobStore,
    from: Uuid,
    household: &str,
    ndjson: &[u8],
) -> anyhow::Result<(Uuid, ImportSummary)> {
    let row = get_profile(&mut *c, from).await?.expect("profile");
    let h = create_household(&mut *c, household).await?;
    let fresh = create_profile(
        &mut *c,
        &ProfileIn {
            household_id: h,
            slug: row.slug.clone(),
            display_name: row.display_name.clone(),
            relationship: Some("self".into()),
            birth_date: row.birth_date,
            sex_at_birth: row.sex_at_birth.clone(),
            country: Some("ES".into()),
            user_id: None,
            unit_pref: None,
        },
    )
    .await?;
    let sha = blobs.put_bytes(ndjson)?;
    let doc = create_document(
        &mut *c,
        &NewDocument {
            filename: Some("FamilyMemberHistory.ndjson"),
            ..NewDocument::upload(h, Some(fresh), &sha, "application/fhir+ndjson", ndjson.len() as i64)
        },
    )
    .await?;
    let (_, entries) = read_entries(ndjson, true)?;
    let profile = get_profile(&mut *c, fresh).await?.expect("fresh");
    let (summary, identity) = import_resources(&mut *c, &entries, &profile, doc.id, "restore", None, "FHIR").await?;
    assert_eq!(identity, "matched");
    Ok((fresh, summary))
}

#[tokio::test]
async fn the_rivera_pedigree_exports_and_imports_back_with_nothing_dropped() -> anyhow::Result<()> {
    ownhealth_ops::register();
    let db = TestDb::new().await?;
    let dir = tempfile::tempdir()?;
    let settings = db.settings(dir.path())?;
    let blobs = BlobStore::new(settings.blobs_path())?;
    assert!(seed_demo(&db.pool, &settings, &blobs, false).await?);
    let mut c = db.pool.acquire().await?;
    let alex: Uuid = sqlx::query_scalar(
        "select p.id from profile p join household h on h.id = p.household_id where h.name = $1 and p.slug = 'alex'",
    )
    .bind(DEMO_HOUSEHOLD)
    .fetch_one(&mut *c)
    .await?;
    let seeded = family::list(&mut c, alex, true).await?;
    assert_eq!(seeded.len(), 7);
    assert_eq!(seeded.iter().filter(|m| m.linked_profile_id.is_some()).count(), 3);

    // a later version of the father adds a condition: the export's current version must carry both
    let father = seeded.iter().find(|m| m.relationship == "FTH").expect("father");
    let mut m = MemberIn::new(alex, "FTH");
    m.born_year = father.born_year;
    m.death_age = father.death_age;
    m.conditions = father.conditions.0.clone();
    m.conditions.push(FamilyCondition {
        kind: "hypertension".into(),
        text: None,
        code_system: None,
        code: None,
        onset_age: Some(45),
        onset_approx: true,
    });
    let src = SrcRef::New(SrcIn::manual(None));
    let v2 = family::supersede(
        &mut c,
        father.id,
        &m,
        &src,
        "person",
        "added a condition",
        LinkChange::Keep,
    )
    .await?;
    // a relative with sensitivity tags: they must come back as the same tags
    let mut aunt = MemberIn::new(alex, "MAUNT");
    aunt.sensitivity = vec!["mental_health".into(), "reproductive".into()];
    family::add(&mut c, &aunt, &SrcRef::New(SrcIn::manual(None)), "person", None, None).await?;
    drop(c);

    let (patient_bytes, fmh_bytes) = export_family(&db, &blobs, alex).await?;
    let fmh = lines(&fmh_bytes)?;
    assert_eq!(fmh.len(), 9, "eight relatives and one superseded version");
    let current = fmh
        .iter()
        .find(|r| r["id"] == v2.id.to_string())
        .expect("the new version");
    assert_eq!(lifecycle(current), "current");
    assert_eq!(current["patient"]["reference"], format!("Patient/{alex}"));
    assert!(current.get("subject").is_none());
    let codes: Vec<&Value> = current["condition"]
        .as_array()
        .map(|c| c.iter().map(|c| &c["code"]["coding"][0]["code"]).collect())
        .unwrap_or_default();
    assert_eq!(codes, ["22298006", "38341003"]);
    assert_eq!(current["condition"][1]["onsetString"], "~45");
    let old = fmh
        .iter()
        .find(|r| r["id"] == father.id.to_string())
        .expect("the old version");
    assert_eq!(
        (lifecycle(old), old["condition"].as_array().map(Vec::len)),
        ("superseded", Some(1))
    );
    // HL7 has no label for reproductive alone: it goes out as SEX, and the exact tag rides in meta.tag
    let aunt_out = fmh
        .iter()
        .find(|r| r["relationship"]["coding"][0]["code"] == "MAUNT")
        .expect("the aunt");
    let security: Vec<&Value> = aunt_out["meta"]["security"]
        .as_array()
        .map(|s| s.iter().map(|s| &s["code"]).collect())
        .unwrap_or_default();
    assert_eq!(security, ["PSY", "SEX"]);
    let tags: Vec<&Value> = aunt_out["meta"]["tag"]
        .as_array()
        .map(|t| {
            t.iter()
                .filter(|t| t["system"] == "urn:ownhealth:sensitivity")
                .map(|t| &t["code"])
                .collect()
        })
        .unwrap_or_default();
    assert_eq!(tags, ["mental_health", "reproductive"]);

    if let Ok(out) = std::env::var("OWNHEALTH_FHIR_OUT") {
        std::fs::create_dir_all(&out)?;
        for (i, r) in lines(&patient_bytes)?.iter().chain(fmh.iter()).enumerate() {
            let name = format!("{out}/{i:02}-{}.json", r["resourceType"].as_str().unwrap_or("Resource"));
            std::fs::write(name, serde_json::to_vec_pretty(r)?)?;
        }
    }

    // import the two files into a fresh profile for the same person
    let mut c = db.pool.acquire().await?;
    let mut ndjson = patient_bytes.clone();
    ndjson.extend_from_slice(&fmh_bytes);
    let (fresh, summary) = import_fresh(&mut c, &blobs, alex, "Restore test", &ndjson).await?;
    assert_eq!(
        (summary.accepted, summary.queued, summary.skipped),
        (8, 0, 1),
        "{summary:?}"
    );
    let unmapped: i64 =
        sqlx::query_scalar("select count(*) from review_item where profile_id = $1 and kind = 'fhir_unmapped'")
            .bind(fresh)
            .fetch_one(&mut *c)
            .await?;
    assert_eq!(unmapped, 0);
    let back = family::list(&mut c, fresh, true).await?;
    let now = family::list(&mut c, alex, true).await?;
    assert_eq!(essence(&back), essence(&now));
    // importing the same file again adds nothing
    let profile = get_profile(&mut c, fresh).await?.expect("fresh");
    let sha = blobs.put_bytes(&ndjson)?;
    let doc2 = create_document(
        &mut c,
        &NewDocument {
            filename: Some("again.ndjson"),
            ..NewDocument::upload(
                profile.household_id,
                Some(fresh),
                &sha,
                "application/fhir+ndjson",
                ndjson.len() as i64,
            )
        },
    )
    .await?;
    let (_, entries) = read_entries(&ndjson, true)?;
    let (again, _) = import_resources(&mut c, &entries, &profile, doc2.id, "restore", None, "FHIR").await?;
    assert_eq!((again.accepted, again.queued), (0, 0), "{again:?}");

    // a second hop: a relative removed in the restored profile stays removed in the next one
    let sister = back.iter().find(|m| m.relationship == "SIS").expect("sister");
    family::retract(&mut c, sister.id, "not my sister").await?;
    drop(c);
    let (patient2, fmh2) = export_family(&db, &blobs, fresh).await?;
    let mut ndjson2 = patient2;
    ndjson2.extend_from_slice(&fmh2);
    let mut c = db.pool.acquire().await?;
    let (third, _) = import_fresh(&mut c, &blobs, fresh, "Second restore", &ndjson2).await?;
    let third_rows = family::list(&mut c, third, true).await?;
    assert!(
        third_rows.iter().all(|m| m.relationship != "SIS"),
        "the removed sister came back"
    );
    assert_eq!(essence(&third_rows), essence(&family::list(&mut c, fresh, true).await?));
    Ok(())
}

#[tokio::test]
async fn an_edit_made_in_review_is_what_the_export_carries() -> anyhow::Result<()> {
    ownhealth_ops::register();
    ownhealth_ingest_structured::register();
    let db = TestDb::new().await?;
    let dir = tempfile::tempdir()?;
    let settings = db.settings(dir.path())?;
    let blobs = BlobStore::new(settings.blobs_path())?;
    assert!(seed_demo(&db.pool, &settings, &blobs, false).await?);
    let mut c = db.pool.acquire().await?;
    let (alex, user): (Uuid, Uuid) = sqlx::query_as(
        "select p.id, p.user_id from profile p join household h on h.id = p.household_id \
         where h.name = $1 and p.slug = 'alex'",
    )
    .bind(DEMO_HOUSEHOLD)
    .fetch_one(&mut *c)
    .await?;
    // no Patient in the file: the identity is not matched, so the relative waits in review
    let fmh = br#"{"resourceType":"FamilyMemberHistory","id":"uncle-1","status":"completed","patient":{"reference":"Patient/x"},"relationship":{"coding":[{"system":"http://terminology.hl7.org/CodeSystem/v3-RoleCode","code":"PUNCLE"}]},"bornDate":"1948"}"#;
    let profile = get_profile(&mut c, alex).await?.expect("alex");
    let sha = blobs.put_bytes(fmh)?;
    let doc = create_document(
        &mut c,
        &NewDocument {
            filename: Some("uncle.ndjson"),
            ..NewDocument::upload(
                profile.household_id,
                Some(alex),
                &sha,
                "application/fhir+ndjson",
                fmh.len() as i64,
            )
        },
    )
    .await?;
    let (_, entries) = read_entries(fmh, true)?;
    let (summary, _) = import_resources(&mut c, &entries, &profile, doc.id, "upload", None, "FHIR").await?;
    assert_eq!(summary.queued, 1, "{summary:?}");
    let item: Uuid = sqlx::query_scalar(
        "select id from review_item where profile_id = $1 and kind = 'fhir_unmapped' and status = 'open' \
         and payload->>'projection' = 'family_member'",
    )
    .bind(alex)
    .fetch_one(&mut *c)
    .await?;
    let ids = ownhealth_core::review::accept(&mut c, item, user, Some(&serde_json::json!({"born_year": 1950}))).await?;
    let stored: Value = sqlx::query_scalar("select fhir from family_member where id = $1")
        .bind(ids[0])
        .fetch_one(&mut *c)
        .await?;
    assert_eq!(stored["bornDate"], "1950", "{stored}");
    Ok(())
}
