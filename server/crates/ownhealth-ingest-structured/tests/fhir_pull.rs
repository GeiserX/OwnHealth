//! Connector pulls (connectors.md 6.2, "One bundle per pull"). Each pulled resource is looked up in
//! `source_item` as `<type>/<id>@<versionId or sha>`, so a pull that comes again imports nothing, even
//! after a person dismissed what the first one queued, and a new version is imported once. Resource
//! types we do not project become one `fhir_unmapped_summary` item per pull and type, with their count.
//! A file import keeps one item per resource.

mod common;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use axum::Router;
use axum::routing::get;
use common::{Env, JSON, R, family, store_doc};
use ownhealth_core::Error;
use ownhealth_core::blobs::BlobStore;
use ownhealth_core::connector_oauth::SourceHttp;
use ownhealth_core::demo::Family;
use ownhealth_core::documents::{NewDocument, create_document};
use ownhealth_core::net::Egress;
use ownhealth_core::records::list_records;
use ownhealth_core::review::{self, ItemQuery, ReviewItem};
use ownhealth_core::sources::{NewSource, create_source};
use ownhealth_ingest_structured::fhir::{PullSummary, import_fhir, import_pull};
use serde_json::{Value, json};
use sqlx::PgConnection;
use uuid::Uuid;

const BASE: &str = "https://fhir.labnorte.example/r4";

fn patient() -> Value {
    json!({"resourceType": "Patient", "id": "alex-rivera", "meta": {"versionId": "1"},
           "name": [{"family": "Rivera", "given": ["Alex"]}], "birthDate": "1979-05-14"})
}

/// Hypertension, coded: auto-accepted.
fn htn(version: &str, onset: &str) -> Value {
    json!({"resourceType": "Condition", "id": "htn", "meta": {"versionId": version},
           "clinicalStatus": {"coding": [{"system": "http://terminology.hl7.org/CodeSystem/condition-clinical", "code": "active"}]},
           "verificationStatus": {"coding": [{"system": "http://terminology.hl7.org/CodeSystem/condition-ver-status", "code": "confirmed"}]},
           "code": {"coding": [{"system": "http://snomed.info/sct", "code": "38341003", "display": "Hypertensive disorder"}]},
           "subject": {"reference": "Patient/alex-rivera"}, "onsetDateTime": onset})
}

/// A condition with text and no code: held in review.
fn back_pain() -> Value {
    json!({"resourceType": "Condition", "id": "lumbar", "meta": {"versionId": "1"},
           "code": {"text": "Dolor lumbar"}, "subject": {"reference": "Patient/alex-rivera"}, "recordedDate": "2026-02-03"})
}

/// A type with no table: stored, not projected. No versionId, so its JSON hash keys it.
fn care_plan() -> Value {
    json!({"resourceType": "CarePlan", "id": "cp-1", "status": "active", "intent": "plan",
           "subject": {"reference": "Patient/alex-rivera"}})
}

fn eob(n: usize) -> Value {
    json!({"resourceType": "ExplanationOfBenefit", "id": format!("eob-{n:03}"), "meta": {"versionId": "1"},
           "status": "active", "use": "claim", "outcome": "complete",
           "type": {"coding": [{"system": "http://terminology.hl7.org/CodeSystem/claim-type", "code": "professional"}]},
           "patient": {"reference": "Patient/alex-rivera"}, "created": format!("2026-01-{:02}", n % 28 + 1),
           "insurer": {"display": "LabNorte Salud"}, "provider": {"display": "Clínica LabNorte"}})
}

fn coverage(n: usize) -> Value {
    json!({"resourceType": "Coverage", "id": format!("cov-{n}"), "meta": {"versionId": "1"}, "status": "active",
           "beneficiary": {"reference": "Patient/alex-rivera"}, "payor": [{"display": "LabNorte Salud"}]})
}

/// A searchset as a server answers it: the Patient first, every entry with its full URL. A new `page`
/// gives the same resources in another envelope (another Bundle.id and time), so other bytes.
fn searchset(page: &str, resources: Vec<Value>) -> Vec<u8> {
    let mut all = vec![patient()];
    all.extend(resources);
    let entries: Vec<Value> = all
        .into_iter()
        .map(|r| {
            let url = format!(
                "{BASE}/{}/{}",
                r["resourceType"].as_str().unwrap_or(""),
                r["id"].as_str().unwrap_or("")
            );
            json!({"fullUrl": url, "resource": r, "search": {"mode": "match"}})
        })
        .collect();
    serde_json::to_vec(&json!({"resourceType": "Bundle", "id": page, "type": "searchset",
        "total": entries.len(), "entry": entries}))
    .unwrap_or_default()
}

async fn smart_source(conn: &mut PgConnection, fam: &Family, slug: &str) -> anyhow::Result<Uuid> {
    Ok(create_source(
        conn,
        &NewSource {
            profile_id: Some(fam.profile(slug)),
            ..NewSource::new(fam.household_id, "smart", "Hospital LabNorte")
        },
    )
    .await?)
}

/// What a poller does with one answer: the bytes become the profile's document first, then the import.
async fn store_pull(
    conn: &mut PgConnection,
    blobs: &BlobStore,
    fam: &Family,
    source: Uuid,
    bytes: &[u8],
) -> anyhow::Result<Uuid> {
    let sha = blobs.put_bytes(bytes)?;
    let doc = create_document(
        conn,
        &NewDocument {
            filename: Some("Hospital LabNorte · pull.json"),
            channel: "smart",
            source_id: Some(source),
            ..NewDocument::upload(
                fam.household_id,
                Some(fam.profile("alex")),
                &sha,
                JSON,
                bytes.len() as i64,
            )
        },
    )
    .await?;
    Ok(doc.id)
}

async fn pull(
    conn: &mut PgConnection,
    blobs: &BlobStore,
    fam: &Family,
    source: Uuid,
    bytes: &[u8],
) -> anyhow::Result<(Uuid, PullSummary)> {
    let doc = store_pull(&mut *conn, blobs, fam, source, bytes).await?;
    Ok((doc, import_pull(conn, blobs, source, doc).await?))
}

async fn open_items(conn: &mut PgConnection, profile: Uuid) -> anyhow::Result<Vec<ReviewItem>> {
    let mut rows = review::list_items(
        conn,
        &ItemQuery {
            profile_ids: Some(vec![profile]),
            status: Some("open".into()),
            ..ItemQuery::default()
        },
    )
    .await?;
    rows.retain(|r| r.profile_id == Some(profile));
    rows.sort_by_key(|r| {
        (
            r.kind.clone(),
            r.payload["resource_type"].as_str().unwrap_or_default().to_string(),
        )
    });
    Ok(rows)
}

async fn conditions(conn: &mut PgConnection, profile: Uuid) -> anyhow::Result<usize> {
    Ok(list_records(conn, "condition", profile, None, true).await?.len())
}

fn kinds(items: &[ReviewItem]) -> Vec<(String, String)> {
    items
        .iter()
        .map(|i| {
            (
                i.kind.clone(),
                i.payload["resource_type"].as_str().unwrap_or_default().to_string(),
            )
        })
        .collect()
}

fn pair(kind: &str, rtype: &str) -> (String, String) {
    (kind.to_string(), rtype.to_string())
}

#[tokio::test]
async fn a_pull_that_comes_again_adds_nothing_after_a_person_dismissed_its_items() -> R {
    let env = Env::new().await?;
    let mut tx = env.tx().await?;
    let fam = family(&mut tx).await?;
    let p = fam.profile("alex");
    let source = smart_source(&mut tx, &fam, "alex").await?;
    let resources = || vec![htn("1", "2015-03"), back_pain(), care_plan(), eob(1), eob(2)];

    let (_, first) = pull(&mut tx, &env.blobs, &fam, source, &searchset("page-1", resources())).await?;
    assert_eq!((first.seen, first.new, first.duplicate, first.context), (6, 5, 0, 1));
    assert_eq!(first.summary.accepted, 1, "{first:?}");
    assert_eq!(conditions(&mut tx, p).await?, 1);
    let queued = open_items(&mut tx, p).await?;
    assert_eq!(
        kinds(&queued),
        [
            pair("fhir_unmapped", "Condition"),
            pair("fhir_unmapped_summary", "CarePlan"),
            pair("fhir_unmapped_summary", "ExplanationOfBenefit"),
        ]
    );
    for item in &queued {
        review::dismiss(&mut tx, item.id, fam.user("alex"), Some("seen")).await?;
    }

    // the same answer again (the same bytes), then the same resources in another envelope
    for page in ["page-1", "page-2"] {
        let (_, again) = pull(&mut tx, &env.blobs, &fam, source, &searchset(page, resources())).await?;
        assert_eq!(
            (again.seen, again.new, again.duplicate, again.context),
            (6, 0, 5, 1),
            "{page}"
        );
        assert_eq!((again.summary.accepted, again.summary.queued), (0, 0), "{page}");
        assert!(
            open_items(&mut tx, p).await?.is_empty(),
            "{page}: the queue stays empty"
        );
        assert_eq!(conditions(&mut tx, p).await?, 1, "{page}");
    }
    let ledger: Vec<String> =
        sqlx::query_scalar("select external_id from source_item where source_id = $1 order by external_id")
            .bind(source)
            .fetch_all(&mut *tx)
            .await?;
    assert_eq!(ledger.len(), 5, "{ledger:?}");
    assert!(ledger.contains(&"Condition/htn@1".to_string()), "{ledger:?}");
    assert!(
        ledger.contains(&"ExplanationOfBenefit/eob-001@1".to_string()),
        "{ledger:?}"
    );
    let plan = ledger
        .iter()
        .find(|k| k.starts_with("CarePlan/cp-1@sha-"))
        .expect("no versionId: keyed by its JSON");
    assert_eq!(plan.len(), "CarePlan/cp-1@sha-".len() + 16, "{plan}");
    Ok(())
}

#[tokio::test]
async fn three_hundred_claims_are_one_summary_item_with_their_count() -> R {
    let env = Env::new().await?;
    let mut tx = env.tx().await?;
    let fam = family(&mut tx).await?;
    let p = fam.profile("alex");
    let source = smart_source(&mut tx, &fam, "alex").await?;
    let mut resources: Vec<Value> = (1..=300).map(eob).collect();
    resources.extend([coverage(1), coverage(2)]);

    let (doc, got) = pull(&mut tx, &env.blobs, &fam, source, &searchset("claims", resources)).await?;
    // every entry is counted: served equals counted, so nothing is dropped in silence
    assert_eq!((got.seen, got.new, got.duplicate, got.context), (303, 302, 0, 1));
    let counted: BTreeMap<String, i64> = [("Coverage", 2), ("ExplanationOfBenefit", 300), ("Patient", 1)]
        .map(|(k, v)| (k.to_string(), v))
        .into();
    assert_eq!(got.counted, counted);
    assert_eq!((got.summary.accepted, got.summary.queued), (0, 2));
    assert_eq!(got.summary.reasons.get("unmapped"), Some(&302));

    let items = open_items(&mut tx, p).await?;
    assert_eq!(
        kinds(&items),
        [
            pair("fhir_unmapped_summary", "Coverage"),
            pair("fhir_unmapped_summary", "ExplanationOfBenefit")
        ]
    );
    let claims = &items[1];
    assert_eq!(claims.payload["count"], 300, "{}", claims.payload);
    assert_eq!(claims.payload["notes"], json!({"stored, not projected": 300}));
    assert_eq!(claims.document_id, Some(doc), "the item links to the stored bundle");
    assert_eq!(claims.reasons, ["unmapped"]);
    assert_eq!(
        claims.payload["source"],
        json!({"id": source.to_string(), "kind": "smart", "label": "Hospital LabNorte"})
    );
    let refs = claims.payload["import_refs"].as_array().cloned().unwrap_or_default();
    assert_eq!(refs.len(), 20, "a sample of the ids; the bundle holds them all");
    assert_eq!(refs[0], "claims/ExplanationOfBenefit/eob-001");
    assert!(
        claims.payload.get("resource").is_none(),
        "the resources stay in the bundle"
    );
    assert_eq!(items[0].payload["count"], 2);
    // review accepts no summary: it is information, dismissed once read
    let e = review::accept(&mut tx, claims.id, fam.user("alex"), None).await.err();
    assert!(matches!(e, Some(Error::Review(_))), "{e:?}");
    Ok(())
}

#[tokio::test]
async fn a_new_version_is_imported_once() -> R {
    let env = Env::new().await?;
    let mut tx = env.tx().await?;
    let fam = family(&mut tx).await?;
    let p = fam.profile("alex");
    let source = smart_source(&mut tx, &fam, "alex").await?;

    let (_, v1) = pull(
        &mut tx,
        &env.blobs,
        &fam,
        source,
        &searchset("a", vec![htn("1", "2015-03")]),
    )
    .await?;
    assert_eq!((v1.new, v1.summary.accepted), (1, 1));
    // version 2 changed the onset: one conflict item that names the version it would replace
    let (_, v2) = pull(
        &mut tx,
        &env.blobs,
        &fam,
        source,
        &searchset("b", vec![htn("2", "2014-11")]),
    )
    .await?;
    assert_eq!((v2.new, v2.duplicate, v2.summary.queued), (1, 0, 1), "{v2:?}");
    let items = open_items(&mut tx, p).await?;
    assert_eq!(kinds(&items), [pair("fhir_unmapped", "Condition")]);
    assert!(
        items[0].reasons.contains(&"conflict".to_string()),
        "{:?}",
        items[0].reasons
    );
    review::dismiss(&mut tx, items[0].id, fam.user("alex"), Some("keep mine")).await?;
    // version 2 again: already pulled, so nothing comes back
    let (_, again) = pull(
        &mut tx,
        &env.blobs,
        &fam,
        source,
        &searchset("c", vec![htn("2", "2014-11")]),
    )
    .await?;
    assert_eq!(
        (again.new, again.duplicate, again.summary.queued),
        (0, 1, 0),
        "{again:?}"
    );
    assert!(open_items(&mut tx, p).await?.is_empty());
    assert_eq!(conditions(&mut tx, p).await?, 1);
    Ok(())
}

#[tokio::test]
async fn a_retracted_pull_imports_again_when_it_comes_back() -> R {
    let env = Env::new().await?;
    let mut tx = env.tx().await?;
    let fam = family(&mut tx).await?;
    let p = fam.profile("alex");
    let source = smart_source(&mut tx, &fam, "alex").await?;
    let bytes = searchset("a", vec![htn("1", "2015-03"), care_plan()]);

    let (doc, _) = pull(&mut tx, &env.blobs, &fam, source, &bytes).await?;
    // what the retract route does
    ownhealth_ingest_structured::retract::retract_document_facts(&mut tx, doc, "document retracted: test").await?;
    for item in open_items(&mut tx, p).await? {
        review::dismiss(&mut tx, item.id, fam.user("alex"), Some("document retracted")).await?;
    }
    sqlx::query("update document set status = 'retracted' where id = $1")
        .bind(doc)
        .execute(&mut *tx)
        .await?;
    assert_eq!(conditions(&mut tx, p).await?, 0);

    // the same bytes bring the document back, and its resources with it
    let (again_doc, again) = pull(&mut tx, &env.blobs, &fam, source, &bytes).await?;
    assert_eq!(again_doc, doc);
    assert_eq!(
        (again.new, again.duplicate, again.summary.accepted, again.summary.queued),
        (2, 0, 1, 1)
    );
    assert_eq!(conditions(&mut tx, p).await?, 1);
    // and a later pull of them is a duplicate again
    let (_, later) = pull(
        &mut tx,
        &env.blobs,
        &fam,
        source,
        &searchset("b", vec![htn("1", "2015-03"), care_plan()]),
    )
    .await?;
    assert_eq!((later.new, later.duplicate), (0, 2));
    Ok(())
}

#[tokio::test]
async fn a_pull_must_be_stored_on_the_profile_of_its_connection() -> R {
    let env = Env::new().await?;
    let mut tx = env.tx().await?;
    let fam = family(&mut tx).await?;
    let sams = smart_source(&mut tx, &fam, "sam").await?;
    let alexs = smart_source(&mut tx, &fam, "alex").await?;
    let doc = store_pull(&mut tx, &env.blobs, &fam, alexs, &searchset("a", vec![care_plan()])).await?;
    let e = import_pull(&mut tx, &env.blobs, sams, doc).await.err();
    assert!(matches!(e, Some(Error::Invalid(_))), "{e:?}");
    Ok(())
}

#[tokio::test]
async fn a_file_import_still_queues_one_item_per_resource() -> R {
    let env = Env::new().await?;
    let mut tx = env.tx().await?;
    let fam = family(&mut tx).await?;
    let p = fam.profile("alex");
    let bytes = searchset("file", vec![eob(1), eob(2), eob(3)]);
    let doc = store_doc(&mut tx, &env.blobs, &fam, "alex", &bytes, "claims.json", JSON).await?;
    let s = import_fhir(&mut tx, &env.blobs, doc).await?;
    assert_eq!(s.queued, 3);
    assert_eq!(
        kinds(&open_items(&mut tx, p).await?),
        vec![pair("fhir_unmapped", "ExplanationOfBenefit"); 3]
    );
    let ledger: i64 = sqlx::query_scalar("select count(*) from source_item")
        .fetch_one(&mut *tx)
        .await?;
    assert_eq!(ledger, 0, "a file has no source ledger");
    Ok(())
}

/// A FHIR server on loopback that answers one search with `bytes` and counts the requests.
async fn fake_server(bytes: Vec<u8>) -> anyhow::Result<(String, Arc<AtomicUsize>)> {
    let hits = Arc::new(AtomicUsize::new(0));
    let seen = hits.clone();
    let app = Router::new().route(
        "/r4/ExplanationOfBenefit",
        get(move || {
            let bytes = bytes.clone();
            let seen = seen.clone();
            async move {
                seen.fetch_add(1, Ordering::SeqCst);
                ([("content-type", "application/fhir+json")], bytes)
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let at = listener.local_addr()?;
    tokio::spawn(async move { axum::serve(listener, app).await.ok() });
    Ok((format!("127.0.0.1:{}", at.port()), hits))
}

#[tokio::test]
async fn a_real_pull_twice_over_http_imports_once() -> R {
    let env = Env::new().await?;
    let mut tx = env.tx().await?;
    let fam = family(&mut tx).await?;
    let p = fam.profile("alex");
    let source = smart_source(&mut tx, &fam, "alex").await?;
    let mut resources: Vec<Value> = (1..=12).map(eob).collect();
    resources.extend([htn("1", "2015-03"), back_pain()]);
    let (host, hits) = fake_server(searchset("live", resources)).await?;
    // the client a poller builds: only the declared host, through the egress guard (loopback allowed here)
    let http = SourceHttp::new(
        vec![host.clone()],
        Egress {
            allow_private: true,
            https_only: false,
            max_bytes: 8 * 1024 * 1024,
            ..Egress::connector()
        },
    );
    let url = format!("http://{host}/r4/ExplanationOfBenefit?patient=alex-rivera");
    let mut runs = Vec::new();
    for _ in 0..2 {
        let answer = http.get(&url, &[("accept", "application/fhir+json")]).await?;
        assert_eq!(answer.status, 200);
        runs.push(pull(&mut tx, &env.blobs, &fam, source, &answer.body).await?.1);
    }
    assert_eq!(hits.load(Ordering::SeqCst), 2);
    assert_eq!((runs[0].seen, runs[0].new, runs[0].duplicate), (15, 14, 0));
    assert_eq!(
        (runs[0].summary.accepted, runs[0].summary.queued),
        (1, 2),
        "{:?}",
        runs[0]
    );
    assert_eq!((runs[1].seen, runs[1].new, runs[1].duplicate), (15, 0, 14));
    assert_eq!(
        kinds(&open_items(&mut tx, p).await?),
        [
            pair("fhir_unmapped", "Condition"),
            pair("fhir_unmapped_summary", "ExplanationOfBenefit")
        ]
    );
    Ok(())
}

/// `r` as a server that sends no versionId stamps it: `meta.lastUpdated` only.
fn restamped(mut r: Value, updated: &str) -> Value {
    r["meta"] = json!({"lastUpdated": updated});
    r
}

/// An LDL the lab could not measure: a mapped type that does not project.
fn ldl_pending() -> Value {
    json!({"resourceType": "Observation", "id": "ldl-pend", "meta": {"versionId": "1"}, "status": "final",
           "code": {"coding": [{"system": "http://loinc.org", "code": "13457-7", "display": "LDL"}]},
           "subject": {"reference": "Patient/alex-rivera"}, "effectiveDateTime": "2026-02-01",
           "dataAbsentReason": {"text": "Muestra hemolizada, repetir"}})
}

/// What the retract route does to a document: its facts withdrawn, its open items dismissed.
async fn retract(conn: &mut PgConnection, fam: &Family, doc: Uuid) -> R {
    ownhealth_ingest_structured::retract::retract_document_facts(&mut *conn, doc, "document retracted: test").await?;
    let items: Vec<Uuid> = sqlx::query_scalar("select id from review_item where document_id = $1 and status = 'open'")
        .bind(doc)
        .fetch_all(&mut *conn)
        .await?;
    for id in items {
        review::dismiss(&mut *conn, id, fam.user("alex"), Some("document retracted")).await?;
    }
    sqlx::query("update document set status = 'retracted' where id = $1")
        .bind(doc)
        .execute(&mut *conn)
        .await?;
    Ok(())
}

#[tokio::test]
async fn a_server_that_only_restamps_last_updated_brings_nothing_back() -> R {
    // Blue Button sends meta.lastUpdated and no versionId; a reload restamps it
    let env = Env::new().await?;
    let mut tx = env.tx().await?;
    let fam = family(&mut tx).await?;
    let p = fam.profile("alex");
    let source = smart_source(&mut tx, &fam, "alex").await?;
    let at = |t: &str| {
        vec![
            restamped(htn("1", "2015-03"), t),
            restamped(back_pain(), t),
            restamped(eob(1), t),
        ]
    };

    let (_, first) = pull(
        &mut tx,
        &env.blobs,
        &fam,
        source,
        &searchset("a", at("2026-11-21T06:00:00Z")),
    )
    .await?;
    assert_eq!(
        (first.new, first.summary.accepted, first.summary.queued),
        (3, 1, 2),
        "{first:?}"
    );
    let queued = open_items(&mut tx, p).await?;
    review::reject(&mut tx, queued[0].id, fam.user("alex"), "not mine").await?;
    review::dismiss(&mut tx, queued[1].id, fam.user("alex"), Some("seen")).await?;

    let (_, again) = pull(
        &mut tx,
        &env.blobs,
        &fam,
        source,
        &searchset("b", at("2026-11-22T06:00:00Z")),
    )
    .await?;
    assert_eq!(
        (again.new, again.duplicate, again.summary.queued),
        (0, 3, 0),
        "{again:?}"
    );
    assert!(open_items(&mut tx, p).await?.is_empty(), "nothing comes back");
    assert_eq!(conditions(&mut tx, p).await?, 1);
    // the same resource with another meta.tag is another item: a tag carries meaning
    let mut tagged = restamped(eob(1), "2026-11-23T06:00:00Z");
    tagged["meta"]["tag"] =
        json!([{"system": "https://bluebutton.cms.gov/resources/codesystem/tag", "code": "Adjudicated"}]);
    let (_, tag) = pull(&mut tx, &env.blobs, &fam, source, &searchset("c", vec![tagged])).await?;
    assert_eq!((tag.new, tag.duplicate), (1, 0), "{tag:?}");
    Ok(())
}

#[tokio::test]
async fn a_new_version_held_back_by_an_open_older_one_arrives_after_the_dismiss() -> R {
    let env = Env::new().await?;
    let mut tx = env.tx().await?;
    let fam = family(&mut tx).await?;
    let p = fam.profile("alex");
    let source = smart_source(&mut tx, &fam, "alex").await?;
    let mut v2 = back_pain();
    v2["meta"]["versionId"] = json!("2");
    v2["recordedDate"] = json!("2026-02-10");

    pull(&mut tx, &env.blobs, &fam, source, &searchset("a", vec![back_pain()])).await?;
    let held = open_items(&mut tx, p).await?;
    assert_eq!(held.len(), 1);
    // version 2 while version 1 still waits: the open item holds the resource, nothing new
    let (_, waiting) = pull(&mut tx, &env.blobs, &fam, source, &searchset("b", vec![v2.clone()])).await?;
    assert_eq!(waiting.summary.queued, 0, "{waiting:?}");
    review::dismiss(&mut tx, held[0].id, fam.user("alex"), Some("old")).await?;
    // the person dismissed version 1: the next pull brings version 2, once
    let (_, after) = pull(&mut tx, &env.blobs, &fam, source, &searchset("c", vec![v2.clone()])).await?;
    assert_eq!((after.new, after.summary.queued), (1, 1), "{after:?}");
    let (_, later) = pull(&mut tx, &env.blobs, &fam, source, &searchset("d", vec![v2])).await?;
    assert_eq!(
        (later.new, later.duplicate, later.summary.queued),
        (0, 1, 0),
        "{later:?}"
    );
    Ok(())
}

#[tokio::test]
async fn a_fact_another_document_holds_comes_back_when_that_document_is_retracted() -> R {
    let env = Env::new().await?;
    let mut tx = env.tx().await?;
    let fam = family(&mut tx).await?;
    let p = fam.profile("alex");
    let source = smart_source(&mut tx, &fam, "alex").await?;
    // the person uploaded the hospital's file first, then connected the hospital
    let file = store_doc(
        &mut tx,
        &env.blobs,
        &fam,
        "alex",
        &searchset("file", vec![htn("1", "2015-03")]),
        "ips.json",
        JSON,
    )
    .await?;
    import_fhir(&mut tx, &env.blobs, file).await?;
    assert_eq!(conditions(&mut tx, p).await?, 1);
    let (_, first) = pull(
        &mut tx,
        &env.blobs,
        &fam,
        source,
        &searchset("a", vec![htn("1", "2015-03")]),
    )
    .await?;
    assert_eq!(
        (first.summary.accepted, first.summary.queued),
        (0, 0),
        "the file holds it: {first:?}"
    );
    assert_eq!(conditions(&mut tx, p).await?, 1);
    // the file goes: the next pull brings the hypertension back from the hospital
    retract(&mut tx, &fam, file).await?;
    assert_eq!(conditions(&mut tx, p).await?, 0);
    let (_, again) = pull(
        &mut tx,
        &env.blobs,
        &fam,
        source,
        &searchset("b", vec![htn("1", "2015-03")]),
    )
    .await?;
    assert_eq!((again.new, again.summary.accepted), (1, 1), "{again:?}");
    assert_eq!(conditions(&mut tx, p).await?, 1);
    Ok(())
}

#[tokio::test]
async fn a_mapped_type_that_does_not_project_stays_one_item_with_its_note() -> R {
    let env = Env::new().await?;
    let mut tx = env.tx().await?;
    let fam = family(&mut tx).await?;
    let p = fam.profile("alex");
    let source = smart_source(&mut tx, &fam, "alex").await?;
    let (_, got) = pull(
        &mut tx,
        &env.blobs,
        &fam,
        source,
        &searchset("a", vec![ldl_pending(), care_plan()]),
    )
    .await?;
    assert_eq!(got.summary.queued, 2, "{got:?}");
    let items = open_items(&mut tx, p).await?;
    // OwnHealth shows Observations: the unmeasured LDL is its own item with its note, as from a file
    assert_eq!(
        kinds(&items),
        [
            pair("fhir_unmapped", "Observation"),
            pair("fhir_unmapped_summary", "CarePlan")
        ]
    );
    assert_eq!(
        items[0].payload["note"], "Observation without a value",
        "{}",
        items[0].payload
    );
    assert_eq!(items[0].payload["resource"]["id"], "ldl-pend");
    // and a later pull keeps it where the person left it
    review::dismiss(&mut tx, items[0].id, fam.user("alex"), Some("seen")).await?;
    let (_, again) = pull(&mut tx, &env.blobs, &fam, source, &searchset("b", vec![ldl_pending()])).await?;
    assert_eq!(
        (again.new, again.duplicate, again.summary.queued),
        (0, 1, 0),
        "{again:?}"
    );
    Ok(())
}

#[tokio::test]
async fn a_retracted_pull_imports_again_from_a_new_envelope() -> R {
    let env = Env::new().await?;
    let mut tx = env.tx().await?;
    let fam = family(&mut tx).await?;
    let p = fam.profile("alex");
    let source = smart_source(&mut tx, &fam, "alex").await?;
    let (doc, _) = pull(
        &mut tx,
        &env.blobs,
        &fam,
        source,
        &searchset("a", vec![htn("1", "2015-03"), care_plan()]),
    )
    .await?;
    retract(&mut tx, &fam, doc).await?;
    // the next daily pull: the same resources in another Bundle
    let (other, again) = pull(
        &mut tx,
        &env.blobs,
        &fam,
        source,
        &searchset("b", vec![htn("1", "2015-03"), care_plan()]),
    )
    .await?;
    assert_ne!(other, doc);
    assert_eq!(
        (again.new, again.duplicate, again.summary.accepted, again.summary.queued),
        (2, 0, 1, 1),
        "{again:?}"
    );
    assert_eq!(conditions(&mut tx, p).await?, 1);
    Ok(())
}

#[tokio::test]
async fn a_pull_moved_to_another_profile_imports_again_for_its_own() -> R {
    let env = Env::new().await?;
    let mut tx = env.tx().await?;
    let fam = family(&mut tx).await?;
    let p = fam.profile("alex");
    let source = smart_source(&mut tx, &fam, "alex").await?;
    let (doc, _) = pull(
        &mut tx,
        &env.blobs,
        &fam,
        source,
        &searchset("a", vec![htn("1", "2015-03"), care_plan()]),
    )
    .await?;
    // what assign_document does when the person moves the pull to Sam
    retract(&mut tx, &fam, doc).await?;
    sqlx::query("update document set status = 'active', profile_id = $1 where id = $2")
        .bind(fam.profile("sam"))
        .bind(doc)
        .execute(&mut *tx)
        .await?;
    let (_, again) = pull(
        &mut tx,
        &env.blobs,
        &fam,
        source,
        &searchset("b", vec![htn("1", "2015-03"), care_plan()]),
    )
    .await?;
    assert_eq!(
        (again.new, again.duplicate, again.summary.accepted, again.summary.queued),
        (2, 0, 1, 1),
        "{again:?}"
    );
    assert_eq!(conditions(&mut tx, p).await?, 1);
    Ok(())
}

#[tokio::test]
async fn a_failed_summary_write_drops_nothing_without_a_transaction() -> R {
    // a caller with no transaction of its own: the grouped resources are written to the ledger only
    // after their summary item is
    let env = Env::new().await?;
    let mut c = env.db.pool.acquire().await?;
    let fam = family(&mut c).await?;
    let p = fam.profile("alex");
    let source = smart_source(&mut c, &fam, "alex").await?;
    sqlx::raw_sql(
        "create function fail_summary() returns trigger language plpgsql as $$ begin \
         if new.kind = 'fhir_unmapped_summary' then raise exception 'summary refused'; end if; return new; end $$; \
         create trigger fail_summary before insert on review_item for each row execute function fail_summary();",
    )
    .execute(&mut *c)
    .await?;
    let failed = pull(&mut c, &env.blobs, &fam, source, &searchset("a", vec![eob(1), eob(2)])).await;
    assert!(failed.is_err());
    sqlx::raw_sql("drop trigger fail_summary on review_item; drop function fail_summary();")
        .execute(&mut *c)
        .await?;
    let ledger: i64 = sqlx::query_scalar("select count(*) from source_item where source_id = $1")
        .bind(source)
        .fetch_one(&mut *c)
        .await?;
    assert_eq!(ledger, 0);
    let (_, again) = pull(&mut c, &env.blobs, &fam, source, &searchset("b", vec![eob(1), eob(2)])).await?;
    assert_eq!((again.new, again.summary.queued), (2, 1), "{again:?}");
    assert_eq!(
        kinds(&open_items(&mut c, p).await?),
        [pair("fhir_unmapped_summary", "ExplanationOfBenefit")]
    );
    Ok(())
}

#[tokio::test]
async fn a_pull_that_holds_resources_we_do_not_project_is_sensitive() -> R {
    // their content was never read for sensitivity, so the file stays closed without read:sensitive
    let env = Env::new().await?;
    let mut tx = env.tx().await?;
    let fam = family(&mut tx).await?;
    let source = smart_source(&mut tx, &fam, "alex").await?;
    let (claims, _) = pull(&mut tx, &env.blobs, &fam, source, &searchset("a", vec![eob(1)])).await?;
    let (plain, _) = pull(
        &mut tx,
        &env.blobs,
        &fam,
        source,
        &searchset("b", vec![htn("1", "2015-03")]),
    )
    .await?;
    // the same claim again in a new envelope: a duplicate, and the new file still holds it
    let (again, _) = pull(&mut tx, &env.blobs, &fam, source, &searchset("c", vec![eob(1)])).await?;
    use ownhealth_core::documents::document_is_sensitive;
    assert!(document_is_sensitive(&mut tx, claims).await?);
    assert!(document_is_sensitive(&mut tx, again).await?);
    assert!(!document_is_sensitive(&mut tx, plain).await?);
    Ok(())
}

#[tokio::test]
async fn a_claim_twice_in_one_bundle_is_counted_once() -> R {
    let env = Env::new().await?;
    let mut tx = env.tx().await?;
    let fam = family(&mut tx).await?;
    let p = fam.profile("alex");
    let source = smart_source(&mut tx, &fam, "alex").await?;
    let (_, got) = pull(
        &mut tx,
        &env.blobs,
        &fam,
        source,
        &searchset("a", vec![eob(1), eob(1), eob(2)]),
    )
    .await?;
    assert_eq!((got.seen, got.new, got.duplicate, got.context), (4, 2, 1, 1), "{got:?}");
    let items = open_items(&mut tx, p).await?;
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].payload["count"], 2, "{}", items[0].payload);
    Ok(())
}
