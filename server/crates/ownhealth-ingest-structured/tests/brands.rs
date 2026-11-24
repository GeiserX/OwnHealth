//! The US endpoint directory (brands.rs) against a fake vendor server that serves synthetic files in
//! the HL7 Brands shape (Epic and MEDITECH: Organization and Endpoint by `urn:uuid`, facilities linked
//! by `partOf`) and in the Endpoint-plus-Organization shape (Oracle Health, healow). Never the
//! internet, never a vendor's own file. All names are synthetic.

mod common;

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::get;
use common::{Env, R, family};
use ownhealth_core::demo::Family;
use ownhealth_core::jobs::JobContext;
use ownhealth_core::sources::{self, RunOutcome, Source};
use ownhealth_ingest_structured::brands::{self, BUNDLES, FileError};
use serde_json::{Value, json};

const HOST: &str = "https://fhir.labnorte.example";

fn endpoint(full_url: &str, id: &str, address: &str) -> Value {
    json!({"fullUrl": full_url, "resource": {"resourceType": "Endpoint", "id": id, "status": "active",
        "connectionType": {"system": "http://terminology.hl7.org/CodeSystem/endpoint-connection-type",
                           "code": "hl7-fhir-rest"},
        "payloadType": [{"text": "FHIR R4"}], "address": address}})
}

fn org(full_url: &str, id: &str, name: &str, state: Option<&str>, endpoints: &[&str], part_of: Option<&str>) -> Value {
    let mut r = json!({"resourceType": "Organization", "id": id, "active": true, "name": name,
        "identifier": [{"system": "urn:ietf:rfc:3986", "value": format!("{HOST}/org/{id}")}]});
    if let Some(s) = state {
        r["address"] = json!([{"line": ["1 Calle Mayor"], "city": "Springfield", "state": s, "country": "US"}]);
    }
    if !endpoints.is_empty() {
        r["endpoint"] = endpoints.iter().map(|e| json!({"reference": e})).collect();
    }
    if let Some(p) = part_of {
        r["partOf"] = json!({"reference": p});
    }
    json!({"fullUrl": full_url, "resource": r})
}

fn bundle(entry: Vec<Value>) -> Value {
    json!({"resourceType": "Bundle", "id": "synthetic-brands", "type": "collection",
           "timestamp": "2026-11-23T03:00:00Z", "entry": entry})
}

/// The Brands shape (Epic, MEDITECH): `n` primary brands, each with its endpoint, and with
/// `facilities` one facility per brand linked by partOf. `tag` marks the copy in every address.
fn brands_file(vendor: &str, n: usize, facilities: bool, tag: &str) -> Vec<u8> {
    let mut entry = Vec::new();
    for i in 0..n {
        let ep = format!("urn:uuid:{vendor}-ep-{i:04}");
        let brand = format!("urn:uuid:{vendor}-brand-{i:04}");
        entry.push(endpoint(
            &ep,
            &format!("{vendor}-ep-{i:04}"),
            &format!("{HOST}/{vendor}/{tag}/{i:04}/api/FHIR/R4/"),
        ));
        entry.push(org(
            &brand,
            &format!("{vendor}-brand-{i:04}"),
            &format!("Clínica Rivera {vendor} {i:04}"),
            Some("CA"),
            &[&ep],
            None,
        ));
        if facilities {
            entry.push(org(
                &format!("urn:uuid:{vendor}-site-{i:04}"),
                &format!("{vendor}-site-{i:04}"),
                &format!("Clínica Rivera {vendor} {i:04} Norte"),
                Some("NV"),
                &[],
                Some(&brand),
            ));
        }
    }
    serde_json::to_vec_pretty(&bundle(entry)).unwrap_or_default()
}

/// The Endpoint-plus-Organization shape (Oracle Health, healow): full URLs, relative references.
fn pairs_file(vendor: &str, n: usize, tag: &str) -> Vec<u8> {
    let mut entry = Vec::new();
    for i in 0..n {
        let id = format!("{vendor}{i:04}");
        entry.push(endpoint(
            &format!("{HOST}/{vendor}/Endpoint/{id}"),
            &id,
            &format!("{HOST}/{vendor}/{tag}/r4/{id}"),
        ));
        entry.push(org(
            &format!("{HOST}/{vendor}/Organization/O{id}"),
            &format!("O{id}"),
            &format!("Centro de Salud Ortega {vendor} {i:04}"),
            Some("TX"),
            &[&format!("Endpoint/{id}")],
            None,
        ));
    }
    serde_json::to_vec(&bundle(entry)).unwrap_or_default()
}

/// A complete synthetic copy of every vendor file, each above its vendor's minimum.
fn good_files(tag: &str) -> HashMap<&'static str, Vec<u8>> {
    HashMap::from([
        ("epic", brands_file("epic", 300, true, tag)),
        ("oracle", pairs_file("oracle", 250, tag)),
        ("healow", pairs_file("healow", 600, tag)),
        ("meditech", brands_file("meditech", 120, false, tag)),
    ])
}

type Answers = Arc<Mutex<HashMap<String, (u16, Vec<u8>)>>>;
type Hits = Arc<Mutex<BTreeMap<String, usize>>>;

/// The fake vendor host: `GET /<vendor>` answers what `answers` holds for that vendor, and every
/// request is counted.
struct Vendors {
    base: String,
    answers: Answers,
    hits: Hits,
}

impl Vendors {
    async fn start(files: HashMap<&'static str, Vec<u8>>) -> anyhow::Result<Vendors> {
        let answers: Answers = Arc::new(Mutex::new(
            files.into_iter().map(|(k, v)| (k.to_string(), (200, v))).collect(),
        ));
        let hits: Hits = Arc::default();
        let state = (answers.clone(), hits.clone());
        let app = Router::new()
            .route(
                "/{vendor}",
                get(
                    |State((answers, hits)): State<(Answers, Hits)>, Path(vendor): Path<String>| async move {
                        if let Ok(mut h) = hits.lock() {
                            *h.entry(vendor.clone()).or_default() += 1;
                        }
                        let (status, body) = answers
                            .lock()
                            .ok()
                            .and_then(|a| a.get(&vendor).cloned())
                            .unwrap_or((404, Vec::new()));
                        (StatusCode::from_u16(status).unwrap_or(StatusCode::NOT_FOUND), body)
                    },
                ),
            )
            .with_state(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let base = format!("http://{}", listener.local_addr()?);
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        Ok(Vendors { base, answers, hits })
    }

    fn set(&self, vendor: &str, status: u16, body: Vec<u8>) {
        if let Ok(mut a) = self.answers.lock() {
            a.insert(vendor.to_string(), (status, body));
        }
    }

    fn total_hits(&self) -> usize {
        self.hits.lock().map(|h| h.values().sum()).unwrap_or_default()
    }

    /// A source config that sends every vendor file to this host.
    fn config(&self) -> Value {
        let urls: serde_json::Map<String, Value> = BUNDLES
            .iter()
            .map(|b| (b.vendor.to_string(), json!(format!("{}/{}", self.base, b.vendor))))
            .collect();
        json!({"urls": urls, "allow_private": true})
    }
}

struct Fx {
    e: Env,
    fam: Family,
    ctx: JobContext,
}

async fn fx() -> anyhow::Result<Fx> {
    sources::register();
    ownhealth_ingest_structured::register();
    let e = Env::new().await?;
    let mut c = e.db.pool.acquire().await?;
    let fam = family(&mut c).await?;
    let ctx = JobContext::new(
        e.db.pool.clone(),
        Arc::new(e.db.settings(e.dir.path())?),
        e.blobs.clone(),
    );
    Ok(Fx { e, fam, ctx })
}

impl Fx {
    /// The household's directory source, made as the wizard makes it, pointed at the fake host.
    async fn source(&self, v: &Vendors) -> anyhow::Result<Source> {
        let mut tx = self.e.db.pool.begin().await?;
        let (id, created) = brands::ensure_source(&mut tx, self.fam.household_id, None).await?;
        assert!(created);
        sqlx::query("update source set config = $2 where id = $1")
            .bind(id)
            .bind(v.config())
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        let mut c = self.e.db.pool.acquire().await?;
        sources::get_source(&mut c, id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("no source"))
    }

    async fn poll(&self, s: &Source) -> anyhow::Result<RunOutcome> {
        Ok(brands::poll_brands(self.ctx.clone(), s.clone()).await?)
    }

    /// One vendor's entries, as (name, endpoint, state), sorted.
    async fn rows(&self, vendor: &str) -> anyhow::Result<Vec<(String, String, Option<String>)>> {
        Ok(
            sqlx::query_as("select name, endpoint, state from fhir_brand where vendor = $1 order by name, endpoint")
                .bind(vendor)
                .fetch_all(&self.e.db.pool)
                .await?,
        )
    }

    async fn fetched_at(&self, vendor: &str) -> anyhow::Result<chrono::DateTime<chrono::Utc>> {
        Ok(
            sqlx::query_scalar("select fetched_at from fhir_brand_feed where vendor = $1")
                .bind(vendor)
                .fetch_one(&self.e.db.pool)
                .await?,
        )
    }

    async fn hits(&self, q: &str) -> anyhow::Result<Vec<brands::Hit>> {
        let mut c = self.e.db.pool.acquire().await?;
        Ok(brands::search(&mut c, self.fam.household_id, q, 50).await?)
    }
}

fn codes(o: &RunOutcome) -> Vec<String> {
    o.problems.iter().map(|p| p.code.clone()).collect()
}

#[tokio::test]
async fn a_synthetic_brands_bundle_fills_the_directory_and_a_search_finds_a_facility() -> R {
    let f = fx().await?;
    let v = Vendors::start(good_files("v1")).await?;
    let s = f.source(&v).await?;
    let o = f.poll(&s).await?;
    assert_eq!((o.seen, o.new), (4, 4), "{:?}", o.problems);
    assert!(o.problems.is_empty(), "{:?}", o.problems);
    // every vendor's copy and the two fixed entries
    let feeds: Vec<(String, i32)> =
        sqlx::query_as("select vendor, entries from fhir_brand_feed where source_id = $1 order by vendor")
            .bind(s.id)
            .fetch_all(&f.e.db.pool)
            .await?;
    let want: Vec<(String, i32)> = [
        ("epic", 600),
        ("healow", 600),
        ("meditech", 120),
        ("nextgen", 1),
        ("oracle", 250),
        ("va", 1),
    ]
    .iter()
    .map(|(k, n)| (k.to_string(), *n))
    .collect();
    assert_eq!(feeds, want);
    // a facility found by name carries its brand's endpoint and its own state
    let hits = f.hits("rivera EPIC 0007 norte").await?;
    assert_eq!(hits.len(), 1, "{hits:?}");
    assert_eq!(hits[0].name, "Clínica Rivera epic 0007 Norte");
    assert_eq!(hits[0].endpoint, format!("{HOST}/epic/v1/0007/api/FHIR/R4/"));
    assert_eq!(hits[0].state.as_deref(), Some("NV"));
    assert_eq!(hits[0].vendor, "epic");
    // names that start with the query come first, the shorter first
    let hits = f.hits("clínica rivera epic 0007").await?;
    let names: Vec<&str> = hits.iter().map(|h| h.name.as_str()).collect();
    assert_eq!(
        names,
        vec!["Clínica Rivera epic 0007", "Clínica Rivera epic 0007 Norte"]
    );
    let hits = f.hits("0007 norte").await?;
    assert_eq!(hits.len(), 1, "{hits:?}");
    // the wildcards of a query are literal
    assert!(f.hits("rivera%0007").await?.is_empty());
    assert!(f.hits("salud ortega oracle 0001").await?.len() == 1);
    let va = f.hits("veterans").await?;
    assert_eq!(va.len(), 1);
    assert_eq!(va[0].endpoint, "https://api.va.gov/services/fhir/v0/r4");
    // the vendor files are gone from staging: we keep no copy
    let left: Vec<String> = std::fs::read_dir(f.e.blobs.staging_path("x")?.parent().unwrap_or(f.e.dir.path()))
        .map(|d| {
            d.filter_map(|e| e.ok())
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    assert!(left.iter().all(|n| !n.starts_with("brands-")), "{left:?}");
    assert_eq!(v.total_hits(), 4);
    Ok(())
}

#[tokio::test]
async fn facilities_linked_by_part_of_resolve_to_their_endpoint() -> R {
    let e1 = "urn:uuid:ep-labnorte";
    let hospital = "urn:uuid:org-labnorte";
    let file = bundle(vec![
        endpoint(e1, "ep-labnorte", "https://fhir.labnorte.example/r4/"),
        org(hospital, "org-labnorte", "Hospital LabNorte", Some("TX"), &[e1], None),
        // one step, and two steps, from the endpoint; the state comes from the facility, else the
        // nearest organisation up the chain that gives one
        org(
            "urn:uuid:centro",
            "centro",
            "LabNorte  Centro\r\n",
            None,
            &[],
            Some(hospital),
        ),
        org(
            "urn:uuid:barrio",
            "barrio",
            "LabNorte Barrio",
            Some("NM"),
            &[],
            Some("urn:uuid:centro"),
        ),
        // three steps: the state is the nearest one along the chain (the Barrio's NM), not the TX of
        // the organisation that has the endpoint
        org(
            "urn:uuid:pediatria",
            "pediatria",
            "LabNorte Pediatría",
            None,
            &[],
            Some("urn:uuid:barrio"),
        ),
        // a loop with no endpoint ends, and gives nothing
        org("urn:uuid:a", "a", "Bucle A", None, &[], Some("urn:uuid:b")),
        org("urn:uuid:b", "b", "Bucle B", None, &[], Some("urn:uuid:a")),
        // no endpoint and no parent; a parent that is not in the file
        org("urn:uuid:solo", "solo", "Consulta Ortega", None, &[], None),
        org(
            "urn:uuid:lost",
            "lost",
            "Consulta Perdida",
            None,
            &[],
            Some("urn:uuid:nowhere"),
        ),
        // Oracle and healow: full URLs, a relative reference, and an absolute one to the same endpoint
        endpoint(
            "https://fhir.ortega.example/Endpoint/x1",
            "x1",
            "https://fhir.ortega.example/r4/x1",
        ),
        org(
            "https://fhir.ortega.example/Organization/O1",
            "O1",
            "Ortega Salud",
            None,
            &["Endpoint/x1"],
            None,
        ),
        org(
            "urn:uuid:abs",
            "abs",
            "Ortega Salud Este",
            None,
            &["https://other.example/fhir/Endpoint/x1"],
            None,
        ),
        // an endpoint switched off, one on plain http, an inactive organisation, a resource type we skip
        json!({"resource": {"resourceType": "Endpoint", "id": "off", "status": "off", "address": "https://off.example/r4"}}),
        org(
            "urn:uuid:o-off",
            "o-off",
            "Clínica Apagada",
            None,
            &["Endpoint/off"],
            None,
        ),
        endpoint("urn:uuid:plain", "plain", "http://plain.example/r4"),
        org(
            "urn:uuid:o-plain",
            "o-plain",
            "Clínica Sin Cifrar",
            None,
            &["urn:uuid:plain"],
            None,
        ),
        json!({"resource": {"resourceType": "Organization", "id": "gone", "active": false,
               "name": "Clínica Cerrada", "endpoint": [{"reference": e1}]}}),
        json!({"resource": {"resourceType": "Location", "id": "loc", "name": "Sala 1"}}),
    ]);
    let got = brands::parse_bytes(&serde_json::to_vec(&file)?)?;
    let got: Vec<(&str, &str, Option<&str>)> = got
        .iter()
        .map(|b| (b.name.as_str(), b.endpoint.as_str(), b.state.as_deref()))
        .collect();
    assert_eq!(
        got,
        vec![
            ("Hospital LabNorte", "https://fhir.labnorte.example/r4/", Some("TX")),
            ("LabNorte Centro", "https://fhir.labnorte.example/r4/", Some("TX")),
            ("LabNorte Barrio", "https://fhir.labnorte.example/r4/", Some("NM")),
            ("LabNorte Pediatría", "https://fhir.labnorte.example/r4/", Some("NM")),
            ("Ortega Salud", "https://fhir.ortega.example/r4/x1", None),
            ("Ortega Salud Este", "https://fhir.ortega.example/r4/x1", None),
        ]
    );
    // a field of the wrong shape fails the whole file, and so does a body that is not a Bundle
    let mut broken = file.clone();
    broken["entry"][1]["resource"]["name"] = json!(5);
    assert_eq!(
        brands::parse_bytes(&serde_json::to_vec(&broken)?),
        Err(FileError::Unreadable)
    );
    let outcome = json!({"resourceType": "OperationOutcome", "issue": []});
    assert_eq!(
        brands::parse_bytes(&serde_json::to_vec(&outcome)?),
        Err(FileError::Unreadable)
    );
    Ok(())
}

#[tokio::test]
async fn a_truncated_or_spliced_body_keeps_the_old_index() -> R {
    let f = fx().await?;
    let v = Vendors::start(good_files("v1")).await?;
    let s = f.source(&v).await?;
    assert!(f.poll(&s).await?.problems.is_empty());
    let old = f.rows("epic").await?;
    let old_at = f.fetched_at("epic").await?;
    assert_eq!(old.len(), 600);
    let good = brands_file("epic", 300, true, "v2");
    let small = brands_file("epic", 20, true, "v2");
    let cases: Vec<(&str, u16, Vec<u8>, &str)> = vec![
        (
            "truncated with HTTP 200",
            200,
            good[..good.len() / 2].to_vec(),
            "unreadable",
        ),
        (
            "two copies spliced together",
            200,
            [good.clone(), good.clone()].concat(),
            "unreadable",
        ),
        // both cuts land inside a string value, so this body is well-formed JSON: it parses, and the
        // vendor's minimum is what keeps the old copy
        (
            "the head of one copy and the tail of another",
            200,
            [&good[..good.len() / 2], &small[small.len() / 2..]].concat(),
            "too_few",
        ),
        ("a complete file cut to 40 entries", 200, small.clone(), "too_few"),
        ("an empty Bundle", 200, serde_json::to_vec(&bundle(vec![]))?, "too_few"),
        ("an error page", 503, b"<html>busy</html>".to_vec(), "http_status"),
    ];
    for (i, (what, status, body, code)) in cases.into_iter().enumerate() {
        v.set("epic", status, body);
        // the other vendors still refresh in the same run
        v.set("oracle", 200, pairs_file("oracle", 250, &format!("r{i}")));
        let o = f.poll(&s).await?;
        assert_eq!(codes(&o), vec![code.to_string()], "{what}");
        assert!(o.problems[0].message.starts_with("epic: "), "{what}: {:?}", o.problems);
        assert_eq!(o.new, 3, "{what}");
        assert_eq!(f.rows("epic").await?, old, "{what}: the old index changed");
        assert_eq!(f.fetched_at("epic").await?, old_at, "{what}");
        let oracle = f.rows("oracle").await?;
        assert!(oracle.iter().all(|(_, e, _)| e.contains(&format!("/r{i}/"))), "{what}");
    }
    // the reverse: a complete new copy replaces the old one
    v.set("epic", 200, good);
    assert!(f.poll(&s).await?.problems.is_empty());
    let new = f.rows("epic").await?;
    assert_eq!(new.len(), 600);
    assert!(new.iter().all(|(_, e, _)| e.contains("/v2/")));
    // every file failing is a failed run, so the scheduler retries within hours
    for b in BUNDLES {
        v.set(b.vendor, 500, Vec::new());
    }
    let o = f.poll(&s).await?;
    assert_eq!((o.seen, o.new, o.problems.len()), (4, 0, 4));
    let mut c = f.e.db.pool.acquire().await?;
    let run = sources::begin_run(&mut c, s.id, "poll", None).await?;
    sources::finish_run(&mut c, run, Ok(&o)).await?;
    let failures: i32 = sqlx::query_scalar("select consecutive_failures from source where id = $1")
        .bind(s.id)
        .fetch_one(&mut *c)
        .await?;
    assert_eq!(failures, 1);
    assert_eq!(f.rows("epic").await?, new);
    Ok(())
}

#[tokio::test]
async fn a_fresh_install_makes_no_request_to_any_vendor_host() -> R {
    let f = fx().await?;
    // the brands kind is polled, but nothing adds its source: not the scheduler, not a household with
    // a genome, not a second tick
    assert!(sources::poller_for(brands::KIND).is_some());
    for _ in 0..2 {
        sources::scheduler_tick(&f.ctx).await?;
    }
    let n: i64 = sqlx::query_scalar("select count(*) from source where kind = $1")
        .bind(brands::KIND)
        .fetch_one(&f.e.db.pool)
        .await?;
    assert_eq!(n, 0, "a brands source appeared with nobody opening the wizard");
    let jobs: i64 = sqlx::query_scalar("select count(*) from job where kind = $1")
        .bind(sources::POLL_JOB)
        .fetch_one(&f.e.db.pool)
        .await?;
    assert_eq!(jobs, 0);
    let rows: i64 = sqlx::query_scalar("select count(*) from fhir_brand")
        .fetch_one(&f.e.db.pool)
        .await?;
    assert_eq!(rows, 0);
    let mut c = f.e.db.pool.acquire().await?;
    assert_eq!(brands::directory(&mut c, f.fam.household_id).await?, None);
    assert!(f.hits("rivera").await?.is_empty());
    Ok(())
}

#[tokio::test]
async fn the_wizard_entry_point_makes_one_source_queues_one_pull_and_reads_stale() -> R {
    let f = fx().await?;
    let household = f.fam.household_id;
    let mut tx = f.e.db.pool.begin().await?;
    let (id, created) = brands::ensure_source(&mut tx, household, Some(f.fam.user("alex"))).await?;
    tx.commit().await?;
    assert!(created);
    let mut tx = f.e.db.pool.begin().await?;
    let (again, created) = brands::ensure_source(&mut tx, household, Some(f.fam.user("sam"))).await?;
    tx.commit().await?;
    assert_eq!((again, created), (id, false));
    let mut c = f.e.db.pool.acquire().await?;
    let s = sources::get_source(&mut c, id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("no source"))?;
    assert_eq!(
        (s.kind.as_str(), s.enabled, s.poll_seconds, s.profile_id),
        (brands::KIND, true, Some(brands::POLL_SECONDS), None)
    );
    let queued: i64 = sqlx::query_scalar("select count(*) from job where kind = $1 and payload ->> 'source_id' = $2")
        .bind(sources::POLL_JOB)
        .bind(id.to_string())
        .fetch_one(&mut *c)
        .await?;
    assert_eq!(queued, 1);
    // before the first pull: the directory exists, nothing to search yet, not stale
    let d = brands::directory(&mut c, household)
        .await?
        .ok_or_else(|| anyhow::anyhow!("no directory"))?;
    assert_eq!((d.source_id, d.ready, d.stale, d.feeds.len()), (id, false, false, 0));
    // a pulled copy is fresh; three missed weeks later it reads stale
    let v = Vendors::start(good_files("v1")).await?;
    sqlx::query("update source set config = $2 where id = $1")
        .bind(id)
        .bind(v.config())
        .execute(&mut *c)
        .await?;
    let s = sources::get_source(&mut c, id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("no source"))?;
    f.poll(&s).await?;
    let d = brands::directory(&mut c, household)
        .await?
        .ok_or_else(|| anyhow::anyhow!("no directory"))?;
    assert!(d.ready && !d.stale, "{d:?}");
    sqlx::query("update fhir_brand_feed set fetched_at = now() - interval '22 days' where vendor = 'epic'")
        .execute(&mut *c)
        .await?;
    let d = brands::directory(&mut c, household)
        .await?
        .ok_or_else(|| anyhow::anyhow!("no directory"))?;
    assert!(d.stale, "{d:?}");
    let stale: Vec<&str> = d.feeds.iter().filter(|x| x.stale).map(|x| x.vendor.as_str()).collect();
    assert_eq!(stale, vec!["epic"]);
    // switched off, it stays off: opening the wizard again does not turn it back on
    sqlx::query("update source set enabled = false where id = $1")
        .bind(id)
        .execute(&mut *c)
        .await?;
    let mut tx = f.e.db.pool.begin().await?;
    assert_eq!(brands::ensure_source(&mut tx, household, None).await?, (id, false));
    tx.commit().await?;
    let enabled: bool = sqlx::query_scalar("select enabled from source where id = $1")
        .bind(id)
        .fetch_one(&mut *c)
        .await?;
    assert!(!enabled);
    // another household searches its own directory only
    let other = family(&mut c).await?;
    assert!(
        brands::search(&mut c, other.household_id, "rivera", 50)
            .await?
            .is_empty()
    );
    assert!(!brands::search(&mut c, household, "rivera", 50).await?.is_empty());
    // removing the source removes its directory
    sources::delete_source(&mut c, id).await?;
    let left: i64 = sqlx::query_scalar("select count(*) from fhir_brand")
        .fetch_one(&mut *c)
        .await?;
    assert_eq!(left, 0);
    Ok(())
}

#[tokio::test]
async fn an_odd_entry_is_cleaned_or_left_out_and_never_blocks_its_vendor() -> R {
    let f = fx().await?;
    let mut files = good_files("v1");
    let mut epic: Value = serde_json::from_slice(&brands_file("epic", 300, true, "v1"))?;
    let odd = [
        // a scheme in capitals, and one without its slashes: both are https URLs once read
        endpoint("urn:uuid:odd-upper", "odd-upper", "HTTPS://FHIR.Upper.example/r4/"),
        org(
            "urn:uuid:o-upper",
            "o-upper",
            "Clínica Mayúsculas",
            Some("TX"),
            &["urn:uuid:odd-upper"],
            None,
        ),
        endpoint("urn:uuid:odd-slash", "odd-slash", "https:fhir.slash.example/r4/"),
        org(
            "urn:uuid:o-slash",
            "o-slash",
            "Clínica Sin Barras",
            None,
            &["urn:uuid:odd-slash"],
            None,
        ),
        // control characters (a NUL, a bell) in a name and a state
        org(
            "urn:uuid:o-nul",
            "o-nul",
            "Clínica\u{0}Nul\u{7}",
            Some("T\u{0}X"),
            &["urn:uuid:odd-slash"],
            None,
        ),
        // a name of control characters only gives no entry
        org(
            "urn:uuid:o-blank",
            "o-blank",
            "\u{0}\u{1}",
            None,
            &["urn:uuid:odd-slash"],
            None,
        ),
    ];
    epic["entry"]
        .as_array_mut()
        .ok_or_else(|| anyhow::anyhow!("no entry"))?
        .extend(odd);
    files.insert("epic", serde_json::to_vec(&epic)?);
    let v = Vendors::start(files).await?;
    let s = f.source(&v).await?;
    let o = f.poll(&s).await?;
    assert!(o.problems.is_empty(), "{:?}", o.problems);
    let rows = f.rows("epic").await?;
    assert_eq!(rows.len(), 603);
    for (name, endpoint, state) in [
        ("Clínica Mayúsculas", "https://fhir.upper.example/r4/", Some("TX")),
        ("Clínica Sin Barras", "https://fhir.slash.example/r4/", None),
        ("Clínica Nul", "https://fhir.slash.example/r4/", Some("T X")),
    ] {
        let want = (name.to_string(), endpoint.to_string(), state.map(String::from));
        assert!(rows.contains(&want), "{want:?} not in the epic rows");
    }
    Ok(())
}
