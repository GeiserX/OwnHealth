//! FHIR R4 Bundle / IPS / NDJSON import (`ingest/fhir/__init__.py`; ingestion.md 3.3, architecture.md
//! 9). Job kind `import.fhir_bundle`.
//!
//! Accepts a Bundle (`collection`, `document` with a Composition as in IPS, `searchset`,
//! `transaction`), NDJSON (one resource or Bundle per line) or a single resource. Each resource keeps
//! its received JSON in `fhir` and gets `src(kind='fhir_import', import_ref='<Bundle.id>/<type>/<id>')`.
//!
//! - Observations go through `review::route_fact`: a valid LOINC code (format and check digit) and a
//!   UCUM unit, with the Patient matching the profile, auto-accept; anything else waits in review.
//! - Condition, AllergyIntolerance, Immunization, Procedure, Encounter (a visit), MedicationStatement
//!   and MedicationRequest become records under the same auto-accept rule; held ones wait in review as `fhir_unmapped` with
//!   the projected record in the payload, and accepting creates it.
//! - Other resource types are stored as received (the bundle is the blob) and listed in review as
//!   `fhir_unmapped`, "stored, not projected". Nothing is dropped.
//! - A resource already imported with the same JSON is skipped; one that changed upstream goes to
//!   review with reason `conflict`, and accepting it supersedes the old version. "The same resource"
//!   means the same type/id from the same issuer (`issuer_key`).
//! - A connector pull ([`import_pull`]) first asks the source's `source_item` ledger, keyed
//!   `<type>/<id>@<versionId or sha>` (and the Medication's sha for a medicine named only by
//!   reference), and skips what the source sent before; its unmapped resources become one
//!   `fhir_unmapped_summary` item per type (connectors.md 6.2).

pub mod mapping;
pub(crate) mod smart_medication;
pub mod synthetic;

use std::collections::{BTreeMap, BTreeSet, HashMap};

use chrono::{DateTime, FixedOffset, Utc};
use futures::future::BoxFuture;
use ownhealth_core::acceptance::{Check, DEFAULT_THRESHOLD, auto_accept};
use ownhealth_core::access::{Profile, get_profile};
use ownhealth_core::blobs::{BlobOwner, BlobStore};
use ownhealth_core::family::{self, MemberIn};
use ownhealth_core::ingest::ImportSummary;
use ownhealth_core::observations::ObservationIn;
use ownhealth_core::provenance::{SrcIn, SrcRef};
use ownhealth_core::records::{RecordIn, RecordOptions, add_record, supersede_record};
use ownhealth_core::review::{ReviewItem, ReviewItemIn, RouteFact, create_item, route_fact};
use ownhealth_core::{Error, Result};
use ownhealth_normalize as normalize;
use serde::Serialize;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use sqlx::{AssertSqlSafe, PgConnection};
use uuid::Uuid;

use crate::py::{self, truthy};
use mapping::Mapped;

/// Normalizer reasons about resolving the printed name; a valid received LOINC code makes them moot.
const NAME_RESOLUTION: [&str; 2] = ["unmapped", "loinc_unknown"];

#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    /// Where the resource sits in the file: `/entry/3/resource`, `L12`, `/`.
    pub pointer: String,
    pub resource: Value,
    pub full_url: Option<String>,
}

impl Entry {
    fn rtype(&self) -> &str {
        self.resource
            .get("resourceType")
            .and_then(Value::as_str)
            .unwrap_or_default()
    }
}

fn not_readable(e: impl std::fmt::Display) -> Error {
    Error::Invalid(e.to_string())
}

/// (Bundle.id, entries) of a JSON file or an NDJSON file.
pub fn read_entries(data: &[u8], ndjson: bool) -> Result<(Option<String>, Vec<Entry>)> {
    if ndjson {
        let mut out = Vec::new();
        for (n, line) in py::splitlines(data).into_iter().enumerate() {
            if line.trim_ascii().is_empty() {
                continue;
            }
            let v: Value = serde_json::from_slice(line).map_err(not_readable)?;
            out.extend(expand(v, &format!("L{}", n + 1))?);
        }
        return Ok((None, out));
    }
    let doc: Value = serde_json::from_slice(data).map_err(not_readable)?;
    let bundle_id = (doc.get("resourceType").and_then(Value::as_str) == Some("Bundle"))
        .then(|| py::get_truthy(Some(&doc), "id").map(|v| py::py_str(Some(v))))
        .flatten();
    Ok((bundle_id, expand(doc, "")?))
}

fn expand(res: Value, prefix: &str) -> Result<Vec<Entry>> {
    let at = if prefix.is_empty() { "/" } else { prefix };
    let Some(rtype) = res.get("resourceType") else {
        return Err(Error::Invalid(format!("not a FHIR resource at {at}")));
    };
    if !res.is_object() {
        return Err(Error::Invalid(format!("not a FHIR resource at {at}")));
    }
    if rtype.as_str() != Some("Bundle") {
        return Ok(vec![Entry {
            pointer: at.to_string(),
            resource: res,
            full_url: None,
        }]);
    }
    let mut out = Vec::new();
    if let Some(Value::Array(entries)) = res.get("entry") {
        for (i, e) in entries.iter().enumerate() {
            if let Some(r) = e.get("resource").filter(|r| r.is_object()) {
                out.push(Entry {
                    pointer: format!("{prefix}/entry/{i}/resource"),
                    resource: r.clone(),
                    full_url: e.get("fullUrl").and_then(Value::as_str).map(String::from),
                });
            }
        }
    }
    Ok(out)
}

/// A summary stored in `document.flags.import` (or `job.result`).
pub(crate) fn summary_from(v: &Value) -> ImportSummary {
    let n = |k: &str| v.get(k).and_then(Value::as_i64).unwrap_or(0);
    ImportSummary {
        document_id: v.get("document_id").map(|d| py::py_str(Some(d))).unwrap_or_default(),
        accepted: n("accepted"),
        queued: n("queued"),
        skipped: n("skipped"),
        reasons: v
            .get("reasons")
            .and_then(Value::as_object)
            .map(|m| m.iter().map(|(k, c)| (k.clone(), c.as_i64().unwrap_or(0))).collect())
            .unwrap_or_default(),
    }
}

/// A FHIR/IPS bundle (json or ndjson) already stored as a blob.
pub async fn import_fhir(conn: &mut PgConnection, blobs: &BlobStore, document_id: Uuid) -> Result<ImportSummary> {
    let (profile_id, sha, mime, filename, flags): (Option<Uuid>, String, String, Option<String>, Value) =
        sqlx::query_as(
            "select profile_id, encode(blob_sha256, 'hex'), mime, filename, flags from document where id = $1",
        )
        .bind(document_id)
        .fetch_one(&mut *conn)
        .await?;
    if let Some(done) = flags.get("import").filter(|d| truthy(Some(d))) {
        return Ok(summary_from(done)); // the job ran before (a retry, or the same bytes uploaded again)
    }
    let profile_id =
        profile_id.ok_or_else(|| Error::Invalid("assign the document to a profile before importing it".into()))?;
    let profile = get_profile(&mut *conn, profile_id)
        .await?
        .ok_or_else(|| Error::NotFound(format!("unknown profile {profile_id}")))?;
    let ndjson = mime == "application/fhir+ndjson" || filename.unwrap_or_default().to_lowercase().ends_with(".ndjson");
    let (bundle_id, entries) = read_entries(&blobs.get_bytes(BlobOwner::Profile(profile_id), &sha)?, ndjson)?;
    let key = bundle_id.unwrap_or_else(|| format!("doc-{}", &sha[..12]));
    let (summary, identity) = import_resources(conn, &entries, &profile, document_id, &key, None, "FHIR").await?;
    mark_imported(conn, document_id, &summary, Some(&identity)).await?;
    Ok(summary)
}

pub async fn mark_imported(
    conn: &mut PgConnection,
    document_id: Uuid,
    summary: &ImportSummary,
    identity: Option<&str>,
) -> Result<()> {
    sqlx::query(
        "update document set flags = flags || jsonb_build_object('import', cast($1 as jsonb)), \
         identity_match = coalesce($2, identity_match) where id = $3",
    )
    .bind(summary.to_value())
    .bind(identity)
    .bind(document_id)
    .execute(conn)
    .await?;
    Ok(())
}

// ---------------------------------------------------------- connector pulls ----

/// What one connector pull brought (connectors.md 6.2, "One bundle per pull"). Every entry of the
/// bundle is counted once: `seen = new + duplicate + context`, so nothing leaves without a number.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct PullSummary {
    pub summary: ImportSummary,
    /// Entries in the bundle.
    pub seen: i64,
    /// Resources imported this time: sent for the first time, or in a new version.
    pub new: i64,
    /// Resources not imported again: this source sent them before, in a pull that still holds them,
    /// or a fact or an open review item of another import already holds them.
    pub duplicate: i64,
    /// Context for the others (Patient, Practitioner, DiagnosticReport, ...): read, kept in the bundle.
    pub context: i64,
    /// Entries per resource type, context included.
    pub counted: BTreeMap<String, i64>,
}

/// A summary item lists this many import refs; the stored bundle holds every resource.
const SUMMARY_REFS: usize = 20;

/// The note of a resource whose type has no table.
const UNMAPPED_TYPE: &str = "stored, not projected";

/// Context for the others: read for references and identity, kept in the stored bundle, not imported.
fn is_context(rtype: &str) -> bool {
    mapping::SUPPORTING.contains(&rtype) || rtype == "DiagnosticReport"
}

/// `sha-<16 hex>` of a resource's JSON, keys sorted.
fn json_sha(r: &Value) -> String {
    format!(
        "sha-{}",
        &hex::encode(Sha256::digest(py::dumps_sorted(r).as_bytes()))[..16]
    )
}

/// The id a resource is known by in this importer: its own, or a hash of its JSON when it has none.
fn resource_id(r: &Value) -> String {
    py::get_truthy(Some(r), "id").map_or_else(|| json_sha(r), |id| py::py_str(Some(id)))
}

/// What a server writes into `meta` about its own copy, not about the resource: when it last stored
/// it, its version counter and the system it came from. Blue Button sends `lastUpdated` and no
/// `versionId`, and restamps it when it reloads its data. `meta.tag` and `meta.security` stay in:
/// they label the resource itself (adjudicated, restricted).
const SERVER_META: [&str; 3] = ["lastUpdated", "versionId", "source"];

/// [`json_sha`] of a resource without [`SERVER_META`], so a server that only restamps a resource
/// sends the same item again.
pub(crate) fn content_sha(r: &Value) -> String {
    let mut r = r.clone();
    let emptied = match r.get_mut("meta") {
        Some(Value::Object(meta)) => {
            for k in SERVER_META {
                meta.shift_remove(k);
            }
            meta.is_empty()
        }
        _ => false,
    };
    if emptied && let Some(o) = r.as_object_mut() {
        o.shift_remove("meta");
    }
    json_sha(&r)
}

/// The `source_item` key of a pulled resource: `<type>/<id>@<versionId>`, or `@sha-<16 hex>` of its
/// content ([`content_sha`]: the JSON without `meta.lastUpdated`, `meta.versionId` and `meta.source`)
/// when the server sends no version. A changed resource is a new item either way; a restamped one is
/// not.
pub fn pull_key(r: &Value) -> String {
    let version = r
        .get("meta")
        .and_then(|m| m.get("versionId"))
        .and_then(Value::as_str)
        .filter(|v| !v.is_empty())
        .map_or_else(|| content_sha(r), String::from);
    let id = py::get_truthy(Some(r), "id").map_or_else(|| content_sha(r), |id| py::py_str(Some(id)));
    format!("{}/{id}@{version}", py::py_str(r.get("resourceType")))
}

/// The connector run a [`Run`] imports for: its source, its bundle and the unmapped resources it
/// groups.
struct Pull<'a> {
    source_id: Uuid,
    kind: &'a str,
    label: &'a str,
    sha: &'a str,
    fhir_base: Option<&'a str>,
    /// resource type -> the resources of that type with no table, in this run
    unmapped: BTreeMap<String, Unmapped>,
}

/// What a run did with each entry, counted once each; how many of its resources OwnHealth does not
/// project (no table, not readable, or skipped by the mapper); and how many are sensitive, whatever
/// the run did with them. Both counts include duplicates.
#[derive(Debug, Clone, Copy, Default)]
struct Tally {
    new: i64,
    duplicate: i64,
    context: i64,
    unprojected: i64,
    sensitive: i64,
}

#[derive(Default)]
struct Unmapped {
    count: i64,
    notes: BTreeMap<String, i64>,
    refs: Vec<String>,
    /// their ledger keys, written once the summary item is
    keys: BTreeSet<String>,
}

/// What [`Run::import_one`] did with a resource, for the ledger.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Held {
    /// This import holds it now: a fact, a review item, or the mapper's skip.
    Here,
    /// Waits for the summary item of its type ([`Run::queue_grouped`]).
    Grouped,
    /// Not imported: a fact or an open review item of another import holds it. The ledger stays
    /// silent, so a later pull imports it once that one is gone (retracted, dismissed).
    Elsewhere,
}

/// A bundle a connector pulled for the profile of its source, already stored as that profile's
/// document (bytes first). The import is the file one, with these differences:
///
/// - Each resource is looked up in `source_item` as [`pull_key`] first. One this source sent before,
///   in a pull that still holds it, is skipped as a duplicate, so the next pull never brings back what
///   a person dismissed, and a new version is imported once. The ledger names this pull only for what
///   it holds: a resource that a fact or an open item of another import holds is left out, and comes
///   in on a later pull once that one is gone. An Observation is keyed row by row, so each component
///   of a panel follows that rule on its own. A medicine named only by `medicationReference` is
///   keyed with the Medication it points at, so a pull that first brings that Medication imports it
///   once.
/// - Resources of a type with no table become one `fhir_unmapped_summary` item per type, with their
///   count, the bundle as its document and a sample of their refs, in place of one item each. Their
///   ledger rows are written after that item. A mapped type that does not project (an Observation
///   without a value) stays one `fhir_unmapped` item with its note, as from a file.
/// - A bundle that holds resources OwnHealth does not project is flagged `unprojected`: nothing read
///   them for sensitivity, so `documents::document_is_sensitive` keeps the file behind read:sensitive.
///   A bundle that holds a sensitive resource is flagged `sensitive` the same way, read from every
///   entry before the ledger or the dedupe can skip it: the mapped value's sensitivity, or a
///   sensitive `meta.security` label on any resource, context included. A daily pull of the same
///   sensitive fact imports nothing new, and its file holds that fact all the same.
///
/// The caller owns the transaction (RUST-CONTRACTS.md), and runs one pull per source at a time: the
/// ledger is read, then written, with no lock in between. The `source.poll` job's dedupe key gives
/// that order. A file import ([`import_fhir`]) keeps one item per resource and writes no ledger.
pub async fn import_pull(
    conn: &mut PgConnection,
    blobs: &BlobStore,
    source_id: Uuid,
    document_id: Uuid,
) -> Result<PullSummary> {
    import_pull_with_identity(conn, blobs, source_id, document_id, None).await
}

/// A SMART caller can supply its verified match after locking and checking the profile.
pub(crate) async fn import_pull_with_identity(
    conn: &mut PgConnection,
    blobs: &BlobStore,
    source_id: Uuid,
    document_id: Uuid,
    identity: Option<&str>,
) -> Result<PullSummary> {
    let (profile_id, sha, mime, filename, flags): (Option<Uuid>, String, String, Option<String>, Value) =
        sqlx::query_as(
            "select profile_id, encode(blob_sha256, 'hex'), mime, filename, flags from document where id = $1",
        )
        .bind(document_id)
        .fetch_one(&mut *conn)
        .await?;
    let profile_id =
        profile_id.ok_or_else(|| Error::Invalid("a pull is stored on the profile of its connection".into()))?;
    let (kind, label, source_profile, issuer): (String, String, Option<Uuid>, Option<String>) =
        sqlx::query_as("select kind, label, profile_id, config#>>'{endpoints,issuer}' from source where id = $1")
            .bind(source_id)
            .fetch_optional(&mut *conn)
            .await?
            .ok_or_else(|| Error::NotFound(format!("unknown source {source_id}")))?;
    if source_profile != Some(profile_id) {
        return Err(Error::Invalid(
            "a pull is stored on the profile of its connection".into(),
        ));
    }
    let profile = get_profile(&mut *conn, profile_id)
        .await?
        .ok_or_else(|| Error::NotFound(format!("unknown profile {profile_id}")))?;
    let ndjson = mime == "application/fhir+ndjson" || filename.unwrap_or_default().to_lowercase().ends_with(".ndjson");
    let (bundle_id, entries) = read_entries(&blobs.get_bytes(BlobOwner::Profile(profile_id), &sha)?, ndjson)?;
    let key = bundle_id.unwrap_or_else(|| format!("doc-{}", &sha[..12]));
    let imported_before = flags.get("import").is_some_and(|d| truthy(Some(d)));
    if !imported_before {
        // These bytes were pulled and then retracted (the retraction cleared the import flag), and
        // the same bytes brought the document back: what the ledger says it brought is gone.
        ownhealth_core::dedup::forget_pulled_by(&mut *conn, source_id, document_id).await?;
    }
    let fhir_base = issuer.as_deref().and_then(crate::smart::fhir_base);
    let pull = Pull {
        source_id,
        kind: &kind,
        label: &label,
        sha: &sha,
        fhir_base: fhir_base.as_deref(),
        unmapped: BTreeMap::new(),
    };
    let (summary, identity, tally) = import_with(
        conn,
        &entries,
        &profile,
        document_id,
        &key,
        identity,
        &label,
        Some(pull),
    )
    .await?;
    if tally.unprojected > 0 || tally.sensitive > 0 {
        let count = |n: i64| (n > 0).then_some(n);
        sqlx::query(
            "update document set flags = flags || jsonb_strip_nulls(jsonb_build_object('unprojected', $1::bigint, \
             'sensitive', $2::bigint)) where id = $3",
        )
        .bind(count(tally.unprojected))
        .bind(count(tally.sensitive))
        .bind(document_id)
        .execute(&mut *conn)
        .await?;
    }
    if !imported_before {
        mark_imported(conn, document_id, &summary, Some(&identity)).await?;
    }
    let mut counted: BTreeMap<String, i64> = BTreeMap::new();
    for e in &entries {
        *counted.entry(e.rtype().to_string()).or_default() += 1;
    }
    Ok(PullSummary {
        summary,
        seen: entries.len() as i64,
        new: tally.new,
        duplicate: tally.duplicate,
        context: tally.context,
        counted,
    })
}

// ----------------------------------------------------------------- importer ----

/// Import resources into one profile. `identity` is computed from the Patient when not given (Apple
/// Health passes the result of its own birth-date check). Returns (summary, identity).
pub async fn import_resources(
    conn: &mut PgConnection,
    entries: &[Entry],
    profile: &Profile,
    document_id: Uuid,
    bundle_key: &str,
    identity: Option<&str>,
    label: &str,
) -> Result<(ImportSummary, String)> {
    let (summary, identity, _) =
        import_with(conn, entries, profile, document_id, bundle_key, identity, label, None).await?;
    Ok((summary, identity))
}

/// [`import_resources`], for a file (`pull` None) or for a connector pull.
#[allow(clippy::too_many_arguments)]
async fn import_with(
    conn: &mut PgConnection,
    entries: &[Entry],
    profile: &Profile,
    document_id: Uuid,
    bundle_key: &str,
    identity: Option<&str>,
    label: &str,
    pull: Option<Pull<'_>>,
) -> Result<(ImportSummary, String, Tally)> {
    if let Some(e) = entries
        .iter()
        .find(|e| !e.resource.is_object() || !e.resource["resourceType"].is_string())
    {
        return Err(Error::Invalid(format!("'resourceType' missing at {}", e.pointer)));
    }
    let mut index: HashMap<String, &Value> = HashMap::new();
    for e in entries {
        let r = &e.resource;
        if let Some(id) = py::get_truthy(Some(r), "id") {
            index.insert(format!("{}/{}", e.rtype(), py::py_str(Some(id))), r);
        }
        if let Some(url) = e.full_url.as_deref().filter(|u| !u.is_empty()) {
            index.insert(url.to_string(), r);
        }
    }
    let run_index = Index(index);
    let smart_medications = pull.as_ref().filter(|p| p.kind == crate::smart::KIND).map(|p| {
        let mut index = smart_medication::Index::default();
        if let Some(base) = p.fhir_base {
            for e in entries.iter().filter(|e| e.rtype() == "Medication") {
                index.insert(base, e.full_url.as_deref(), &e.resource);
            }
        }
        index
    });

    let identity = match identity {
        Some(i) => i.to_string(),
        None => {
            let mut patient = None;
            if let Some(c) = entries.iter().find(|e| e.rtype() == "Composition") {
                patient = run_index.resolve(
                    c.resource
                        .get("subject")
                        .and_then(|s| s.get("reference"))
                        .and_then(Value::as_str),
                );
            }
            let patient = patient.or_else(|| entries.iter().find(|e| e.rtype() == "Patient").map(|e| &e.resource));
            mapping::match_patient(patient, &profile.display_name, profile.birth_date).to_string()
        }
    };

    let mut report_keys: HashMap<String, String> = HashMap::new();
    for e in entries.iter().filter(|e| e.rtype() == "DiagnosticReport") {
        for res in py::list(Some(&e.resource), "result") {
            let target = run_index.resolve(res.get("reference").and_then(Value::as_str));
            if let Some(id) = target.and_then(|t| py::get_truthy(Some(t), "id")) {
                report_keys.insert(
                    format!("Observation/{}", py::py_str(Some(id))),
                    format!("{bundle_key}/DiagnosticReport/{}", py::py_str(e.resource.get("id"))),
                );
            }
        }
    }

    let issuer = issuer_key(entries);
    sqlx::query(
        "update document set flags = flags || jsonb_build_object('fhir_issuer', cast($1 as text)) where id = $2",
    )
    .bind(&issuer)
    .bind(document_id)
    .execute(&mut *conn)
    .await?;
    let mut run = Run {
        profile,
        document_id,
        bundle_key,
        identity: &identity,
        label,
        index: &run_index,
        smart_medications,
        report_keys,
        issuer,
        summary: ImportSummary::new(document_id.to_string()),
        pull,
        tally: Tally::default(),
    };
    for e in entries {
        run.one(conn, e).await?;
    }
    run.queue_grouped(conn).await?;
    let (summary, tally) = (run.summary, run.tally);
    Ok((summary, identity, tally))
}

struct Index<'a>(HashMap<String, &'a Value>);

impl<'a> Index<'a> {
    fn resolve(&self, reference: Option<&str>) -> Option<&'a Value> {
        let r = reference.filter(|r| !r.is_empty())?;
        if let Some(v) = self.0.get(r) {
            return Some(v);
        }
        let parts: Vec<&str> = r.trim_end_matches('/').split('/').collect();
        let tail = parts[parts.len().saturating_sub(2)..].join("/");
        self.0.get(&tail).copied()
    }
}

/// Who issued these resource ids, as a short hash, or None when the file does not say.
pub fn issuer_key(entries: &[Entry]) -> Option<String> {
    let patient = entries
        .iter()
        .find(|e| e.resource.get("resourceType").and_then(Value::as_str) == Some("Patient"))
        .map(|e| &e.resource);
    let mut key = None;
    for ident in py::list(patient, "identifier") {
        if ident.is_object()
            && let Some(value) = py::get_truthy(Some(ident), "value")
        {
            let system = py::get_truthy(Some(ident), "system")
                .map(|s| py::py_str(Some(s)))
                .unwrap_or_default();
            key = Some(format!("identifier:{system}|{}", py::py_str(Some(value))));
            break;
        }
    }
    if key.is_none() {
        for e in entries {
            let url = e.full_url.as_deref().unwrap_or_default();
            let tail = format!(
                "/{}/{}",
                py::py_str(e.resource.get("resourceType")),
                py::py_str(e.resource.get("id"))
            );
            if (url.starts_with("http://") || url.starts_with("https://")) && url.ends_with(&tail) {
                key = Some(format!("base:{}", url[..url.len() - tail.len()].trim_end_matches('/')));
                break;
            }
        }
    }
    if key.is_none()
        && let Some(id) = py::get_truthy(patient, "id")
    {
        key = Some(format!("patient:{}", py::py_str(Some(id))));
    }
    key.map(|k| hex::encode(Sha256::digest(k.as_bytes()))[..16].to_string())
}

fn like(tail: &str) -> String {
    format!(
        "%/{}",
        tail.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_")
    )
}

struct Run<'a> {
    profile: &'a Profile,
    document_id: Uuid,
    bundle_key: &'a str,
    identity: &'a str,
    label: &'a str,
    index: &'a Index<'a>,
    smart_medications: Option<smart_medication::Index<'a>>,
    report_keys: HashMap<String, String>,
    issuer: Option<String>,
    summary: ImportSummary,
    /// Set for a connector pull: the `source_item` ledger and grouped unmapped resources.
    pull: Option<Pull<'a>>,
    tally: Tally,
}

impl Run<'_> {
    fn count(&mut self, reasons: &[String]) {
        for r in reasons {
            *self.summary.reasons.entry(r.clone()).or_default() += 1;
        }
    }

    fn skipped(&mut self, reason: &str) {
        self.summary.skipped += 1;
        self.count(&[reason.to_string()]);
    }

    fn src(&self, reference: &str, when: Option<DateTime<FixedOffset>>) -> SrcIn {
        let day = when.map_or_else(
            || "no date".to_string(),
            |w| w.date_naive().format("%Y-%m-%d").to_string(),
        );
        let after = reference.split_once('/').map_or(reference, |(_, rest)| rest);
        let rtype = after.split('/').next().unwrap_or_default();
        SrcIn {
            profile_id: Some(self.profile.id),
            document_id: Some(self.document_id),
            import_ref: Some(reference.to_string()),
            label: Some(format!("{} · {rtype} · {day}", self.label)),
            ..SrcIn::new("fhir_import")
        }
    }

    /// An open review item of the same issuer already holds this type/id.
    async fn pending(&self, conn: &mut PgConnection, tail: &str) -> Result<bool> {
        let hit: Option<i32> = sqlx::query_scalar(
            "select 1 from review_item ri left join document d on d.id = ri.document_id \
             where ri.profile_id = $1 and ri.status = 'open' \
             and ri.kind in ('candidate_row', 'fhir_unmapped') and ri.payload->'src'->>'import_ref' like $2 \
             and (ri.document_id = $3 or (cast($4 as text) is not null and d.flags->>'fhir_issuer' = $4)) limit 1",
        )
        .bind(self.profile.id)
        .bind(like(tail))
        .bind(self.document_id)
        .bind(&self.issuer)
        .fetch_optional(conn)
        .await?;
        Ok(hit.is_some())
    }

    /// (skip, superseded candidate). Same JSON imported before: skip. Changed at the same issuer: a
    /// conflict check naming the version it would replace. Another issuer's id: a separate fact.
    /// `medicine` is the (name, code) a medication read from the Medication it references: the stored
    /// JSON is the request alone, so a changed Medication behind an unchanged request is a change too.
    async fn dedupe(
        &self,
        conn: &mut PgConnection,
        table: &str,
        tail: &str,
        resource: &Value,
        medicine: Option<(&str, Option<&str>)>,
        checks: &mut Vec<Check>,
    ) -> Result<(bool, Option<Uuid>)> {
        if self.pending(&mut *conn, tail).await? {
            return Ok((true, None));
        }
        let same_medicine = if medicine.is_some() {
            " and f.name = $6 and f.code is not distinct from $7"
        } else {
            ""
        };
        let sql = format!(
            "select f.id, f.fhir = $5{same_medicine} as same_json, (s.document_id = $3 or (cast($4 as text) is not null \
             and d.flags->>'fhir_issuer' = $4)) as same_issuer \
             from {table} f join src s on s.id = f.src_id left join document d on d.id = s.document_id \
             where f.profile_id = $1 and s.kind = 'fhir_import' and f.status in ('current', 'provisional') \
             and s.import_ref like $2"
        );
        let mut query = sqlx::query_as::<_, (Uuid, Option<bool>, Option<bool>)>(AssertSqlSafe(sql.as_str()))
            .bind(self.profile.id)
            .bind(like(tail))
            .bind(self.document_id)
            .bind(&self.issuer)
            .bind(resource);
        if let Some((name, code)) = medicine {
            query = query.bind(name).bind(code);
        }
        let existing = query.fetch_all(conn).await?;
        if existing.iter().any(|(_, same, _)| *same == Some(true)) {
            return Ok((true, None));
        }
        if let Some((id, _, _)) = existing.iter().find(|(_, _, same_issuer)| *same_issuer == Some(true)) {
            checks.push(Check {
                detail: Some("changed since import".into()),
                ..Check::fail("upstream_version", "conflict")
            });
            return Ok((false, Some(*id)));
        }
        Ok((false, None))
    }

    /// A pull's resource of a type with no table: counted into its type's summary item
    /// ([`Self::queue_grouped`]), its ledger key with it.
    fn group_unmapped(&mut self, e: &Entry, reference: &str, key: &str) {
        let Some(pull) = self.pull.as_mut() else {
            return;
        };
        let group = pull.unmapped.entry(e.rtype().to_string()).or_default();
        group.count += 1;
        *group.notes.entry(UNMAPPED_TYPE.to_string()).or_default() += 1;
        if group.refs.len() < SUMMARY_REFS {
            group.refs.push(reference.to_string());
        }
        group.keys.insert(key.to_string());
        self.count(&["unmapped".to_string()]);
    }

    async fn queue_unmapped(&mut self, conn: &mut PgConnection, e: &Entry, note: &str, reference: &str) -> Result<()> {
        let payload = json!({
            "projection": null,
            "note": note,
            "resource_type": e.rtype(),
            "json_pointer": e.pointer,
            "src": {"import_ref": reference},
            "resource": e.resource,
        });
        create_item(
            conn,
            &ReviewItemIn {
                kind: "fhir_unmapped".into(),
                household_id: Some(self.profile.household_id),
                profile_id: Some(self.profile.id),
                document_id: Some(self.document_id),
                reasons: vec!["unmapped".into()],
                payload,
                ..ReviewItemIn::default()
            },
        )
        .await?;
        self.summary.queued += 1;
        self.count(&["unmapped".to_string()]);
        Ok(())
    }

    /// A pull's resources of types with no table: one `fhir_unmapped_summary` item per type, with
    /// the count, the notes, the source and the first refs. The pulled bundle is the item's document
    /// and holds every resource as received. Review accepts none: a person reads it and dismisses it.
    /// The ledger rows of a type are written after its item, so a failed write leaves them unpulled
    /// even on a connection with no transaction.
    async fn queue_grouped(&mut self, conn: &mut PgConnection) -> Result<()> {
        let Some(pull) = self.pull.as_mut() else {
            return Ok(());
        };
        let groups = std::mem::take(&mut pull.unmapped);
        let (source_id, sha) = (pull.source_id, pull.sha.to_string());
        let source = json!({"id": source_id.to_string(), "kind": pull.kind, "label": pull.label});
        for (rtype, group) in groups {
            let payload = json!({
                "projection": null,
                "resource_type": rtype,
                "count": group.count,
                "notes": group.notes,
                "source": source,
                "import_refs": group.refs,
            });
            create_item(
                &mut *conn,
                &ReviewItemIn {
                    kind: "fhir_unmapped_summary".into(),
                    household_id: Some(self.profile.household_id),
                    profile_id: Some(self.profile.id),
                    document_id: Some(self.document_id),
                    reasons: vec!["unmapped".into()],
                    payload,
                    ..ReviewItemIn::default()
                },
            )
            .await?;
            self.summary.queued += 1;
            for key in &group.keys {
                ownhealth_core::dedup::record_pulled(
                    &mut *conn,
                    source_id,
                    key,
                    &sha,
                    self.document_id,
                    self.profile.id,
                )
                .await?;
            }
        }
        Ok(())
    }

    async fn one(&mut self, conn: &mut PgConnection, e: &Entry) -> Result<()> {
        let r = &e.resource;
        let rtype = e.rtype();
        let pulled = self.pull.as_ref().map(|p| (p.source_id, p.sha.to_string()));
        // a pull's bundle holds every entry as received, whatever this run does with it below
        let labelled = pulled.is_some() && !mapping::sensitivity(r).is_empty();
        if is_context(rtype) {
            self.tally.context += 1;
            self.tally.sensitive += i64::from(labelled);
            return Ok(()); // context for the others; kept in the stored bundle
        }
        let m = self.map(e);
        let Some((source_id, sha)) = pulled else {
            self.import_one(&mut *conn, e, m, "").await?;
            return Ok(());
        };
        if labelled || mapped_sensitive(m.as_ref()) {
            self.tally.sensitive += 1;
        }
        if matches!(m, None | Some(Mapped::Unmapped(_) | Mapped::Skip(_))) {
            // the bundle holds it as received, and nothing read it for sensitivity
            self.tally.unprojected += 1;
        }
        // A pull asks the source's ledger first: a resource it sent before, in this version, in a
        // pull that still holds it, is not imported again, whatever a person did with it since. One
        // this run already grouped (twice in the bundle) waits for its summary's ledger row.
        let key = self.ledger_key(e);
        let m = match m {
            Some(Mapped::Observations(rows)) => return self.pull_rows(conn, e, rows, &key, source_id, &sha).await,
            other => other,
        };
        let grouped = self
            .pull
            .as_ref()
            .is_some_and(|p| p.unmapped.get(rtype).is_some_and(|g| g.keys.contains(&key)));
        if grouped || ownhealth_core::dedup::pulled_before(&mut *conn, source_id, &key, self.profile.id).await? {
            self.tally.duplicate += 1;
            self.skipped("duplicate");
            return Ok(());
        }
        match self.import_one(&mut *conn, e, m, &key).await? {
            Held::Here => {
                self.tally.new += 1;
                ownhealth_core::dedup::record_pulled(
                    &mut *conn,
                    source_id,
                    &key,
                    &sha,
                    self.document_id,
                    self.profile.id,
                )
                .await?;
            }
            Held::Grouped => self.tally.new += 1,
            Held::Elsewhere => self.tally.duplicate += 1,
        }
        Ok(())
    }

    /// A pull's Observation, row by row: each row (the value, or a panel's component) has its own
    /// ledger key, `<key><suffix>` (`#component1`). A component a person dismissed stays gone while a
    /// sibling still waits in review, and a component an open item or another import's fact holds
    /// gets no row, so it comes in once that one is gone. A single value keeps the resource's key.
    async fn pull_rows(
        &mut self,
        conn: &mut PgConnection,
        e: &Entry,
        rows: Vec<mapping::ObsRow>,
        key: &str,
        source_id: Uuid,
        sha: &str,
    ) -> Result<()> {
        let tail = format!("{}/{}", e.rtype(), resource_id(&e.resource));
        let reference = format!("{}/{tail}", self.bundle_key);
        let mut here = false;
        for row in rows {
            let row_key = format!("{key}{}", row.suffix);
            if ownhealth_core::dedup::pulled_before(&mut *conn, source_id, &row_key, self.profile.id).await? {
                self.skipped("duplicate");
                continue;
            }
            let (t, rf) = (format!("{tail}{}", row.suffix), format!("{reference}{}", row.suffix));
            if self.observation(&mut *conn, e, &row, &t, &rf).await? {
                here = true;
                ownhealth_core::dedup::record_pulled(
                    &mut *conn,
                    source_id,
                    &row_key,
                    sha,
                    self.document_id,
                    self.profile.id,
                )
                .await?;
            }
        }
        if here {
            self.tally.new += 1;
        } else {
            self.tally.duplicate += 1;
        }
        Ok(())
    }

    /// [`pull_key`], plus what the mapping read besides the resource. A medication named only by
    /// `medicationReference` maps from the Medication it points at, so its key carries that
    /// Medication's [`content_sha`], or `+unresolved` when the bundle does not hold it: a later pull
    /// that brings the Medication imports the request once, and an unchanged one stays a duplicate.
    fn ledger_key(&self, e: &Entry) -> String {
        let r = &e.resource;
        let key = pull_key(r);
        if !by_reference(r) {
            return key;
        }
        let reference = r["medicationReference"].get("reference").and_then(Value::as_str);
        match self.resolve_medication(e, reference) {
            Some(medication) => format!("{key}+{}", content_sha(medication)),
            None => format!("{key}+unresolved"),
        }
    }

    fn resolve_medication<'a>(&'a self, e: &'a Entry, reference: Option<&str>) -> Option<&'a Value> {
        let Some(index) = &self.smart_medications else {
            return self.index.resolve(reference);
        };
        let raw = reference?;
        if raw.starts_with('#') {
            return smart_medication::contained(&e.resource, raw);
        }
        let base = self.pull.as_ref()?.fhir_base?;
        let target = smart_medication::Reference::in_order(raw, base, e.full_url.as_deref(), &e.resource)?;
        match index.lookup(&target) {
            smart_medication::Match::Found(resource) => Some(resource),
            _ => None,
        }
    }

    /// The mapper's reading of a resource; None for a type with no table.
    fn map(&self, e: &Entry) -> Option<Mapped> {
        let r = &e.resource;
        let pid = self.profile.id;
        match e.rtype() {
            "Observation" => Some(mapping::observation(r)),
            "Condition" => Some(mapping::condition(r, pid)),
            "AllergyIntolerance" => Some(mapping::allergy(r, pid)),
            "Immunization" => Some(mapping::immunization(r, pid)),
            "Procedure" => Some(mapping::procedure(r, pid)),
            "Encounter" => Some(mapping::encounter(r, pid)),
            "MedicationStatement" | "MedicationRequest" => {
                let medication = r["medicationReference"]["reference"].as_str();
                Some(mapping::medication(r, pid, |reference| {
                    if reference == medication {
                        self.resolve_medication(e, reference)
                    } else {
                        self.index.resolve(reference)
                    }
                }))
            }
            "FamilyMemberHistory" => Some(mapping::family_member_history(r, pid)),
            _ => None,
        }
    }

    /// Import one resource as `m` maps it. `key` is its ledger key in a pull (empty for a file).
    /// Says whether this import holds it now, for the ledger.
    async fn import_one(&mut self, conn: &mut PgConnection, e: &Entry, m: Option<Mapped>, key: &str) -> Result<Held> {
        let rid = resource_id(&e.resource);
        let tail = format!("{}/{rid}", e.rtype());
        let reference = format!("{}/{tail}", self.bundle_key);
        let held = |here: bool| if here { Held::Here } else { Held::Elsewhere };
        if matches!(m, None | Some(Mapped::Unmapped(_))) && self.pending(&mut *conn, &tail).await? {
            self.skipped("duplicate");
            return Ok(Held::Elsewhere);
        }
        Ok(match m {
            // a pull groups the types with no table: one item per type once the run ends
            None if self.pull.is_some() => {
                self.group_unmapped(e, &reference, key);
                Held::Grouped
            }
            None => {
                self.queue_unmapped(conn, e, UNMAPPED_TYPE, &reference).await?;
                Held::Here
            }
            Some(Mapped::Unmapped(note)) => {
                self.queue_unmapped(conn, e, &note, &reference).await?;
                Held::Here
            }
            Some(Mapped::Skip(reason)) => {
                self.skipped(&format!("skip:{reason}"));
                Held::Here
            }
            // a pull imports Observations row by row instead ([`Self::pull_rows`])
            Some(Mapped::Observations(rows)) => {
                let mut here = true;
                for row in rows {
                    let (t, rf) = (format!("{tail}{}", row.suffix), format!("{reference}{}", row.suffix));
                    here &= self.observation(&mut *conn, e, &row, &t, &rf).await?;
                }
                held(here)
            }
            Some(Mapped::Record { record, checks }) => {
                held(self.record(conn, e, &record, checks, &tail, &reference).await?)
            }
            Some(Mapped::Family { member, checks }) => {
                held(self.family(conn, e, &member, checks, &tail, &reference).await?)
            }
        })
    }

    async fn observation(
        &mut self,
        conn: &mut PgConnection,
        e: &Entry,
        row: &mapping::ObsRow,
        tail: &str,
        reference: &str,
    ) -> Result<bool> {
        let resource = &e.resource;
        let mut checks = row.checks.clone();
        let (skip, previous) = self
            .dedupe(&mut *conn, "observation", tail, resource, None, &mut checks)
            .await?;
        if skip {
            self.skipped("duplicate");
            return Ok(false);
        }
        let base_tail = tail.split('#').next().unwrap_or(tail);
        let (obs, norm_checks) = self.normalize(&mut *conn, row, resource, base_tail).await?;
        let names: BTreeSet<String> = checks.iter().map(|c| c.name.clone()).collect();
        checks.extend(norm_checks.into_iter().filter(|c| !names.contains(&c.name)));
        let allowance = if checks.iter().all(|c| c.ok) {
            "structured_valid"
        } else {
            "structured_partial"
        };
        let src = self.src(reference, row.effective_at);
        let mut extra = Map::new();
        extra.insert("json_pointer".into(), json!(e.pointer));
        extra.insert("resource_type".into(), json!("Observation"));
        extra.insert("supersedes".into(), json!(previous.map(|p| p.to_string())));
        let result = route_fact(
            conn,
            RouteFact {
                provisional: resource.get("status").and_then(Value::as_str) == Some("preliminary"),
                extra_payload: Some(extra),
                ..RouteFact::new(&obs, &src, &checks, 1.0, allowance, self.identity)
            },
        )
        .await?;
        self.summary.count(&result);
        Ok(true)
    }

    /// Received codes win; lane A fills the derived layer (alias group, canonical unit) when it can.
    async fn normalize(
        &self,
        conn: &mut PgConnection,
        row: &mapping::ObsRow,
        resource: &Value,
        tail: &str,
    ) -> Result<(ObservationIn, Vec<Check>)> {
        let when = row.effective_at.unwrap_or_else(|| Utc::now().fixed_offset());
        let nonempty = |v: &Option<String>| v.clone().filter(|s| !s.is_empty());
        let received_loinc = nonempty(&row.loinc);
        let received_ucum = nonempty(&row.unit_ucum);
        let received_interp = nonempty(&row.interpretation);
        let loinc_ok = mapping::loinc_valid(row.loinc.as_deref());
        let lang = if resource
            .get("language")
            .and_then(Value::as_str)
            .is_some_and(|l| l.starts_with("es"))
        {
            "es"
        } else {
            "en"
        };
        let printed_unit = received_ucum.as_deref().or(row.printed_unit.as_deref());
        let normalized = normalize::normalize_row(
            &mut *conn,
            &normalize::RowIn {
                profile_id: self.profile.id,
                printed_name: &row.printed_name,
                printed_value: &row.printed_value,
                printed_unit,
                printed_range: row.printed_ref_range.as_deref(),
                printed_flag: row.interpretation.as_deref(),
                effective_at: when,
                lang,
                specimen: None,
                provider: None,
                // the received LOINC and UCUM code are truth like the printed text: they drive the
                // derived layer (alias group, canonical value), not only the columns they land in
                loinc: if loinc_ok { row.loinc.as_deref() } else { None },
                method: None,
                document_id: Some(self.document_id),
            },
        )
        .await;
        let normalized = normalized?;
        let (mut obs, mut checks) = (normalized.observation, normalized.checks);
        // the fields the importer owns, then the received codes
        obs.profile_id = self.profile.id;
        obs.category = row.category.clone();
        obs.source_kind = "clinical".into();
        obs.printed_name = row.printed_name.clone();
        obs.printed_value = row.printed_value.clone();
        obs.printed_unit = row.printed_unit.clone();
        obs.printed_ref_range = row.printed_ref_range.clone();
        obs.value_num = row.value_num;
        obs.value_cmp = row.value_cmp.clone();
        obs.value_text = row.value_text.clone();
        obs.value_code = row.value_code.clone();
        obs.ref_low = row.ref_low;
        obs.ref_high = row.ref_high;
        obs.effective_at = when;
        obs.document_id = Some(self.document_id);
        obs.report_key = self.report_keys.get(tail).cloned();
        obs.sensitivity = mapping::sensitivity(resource);
        obs.fhir = Some(resource.clone());
        if received_loinc.is_some() {
            obs.loinc = received_loinc;
        }
        if received_ucum.is_some() {
            obs.unit_ucum = received_ucum;
        }
        if received_interp.is_some() {
            obs.interpretation = received_interp;
        }
        if loinc_ok {
            // the received code answers what name resolution could not
            checks.retain(|c| c.ok || !c.reason.as_deref().is_some_and(|r| NAME_RESOLUTION.contains(&r)));
        }
        Ok((obs, checks))
    }

    async fn record(
        &mut self,
        conn: &mut PgConnection,
        e: &Entry,
        record: &RecordIn,
        mut checks: Vec<Check>,
        tail: &str,
        reference: &str,
    ) -> Result<bool> {
        let medicine = match record {
            RecordIn::Medication(m) if by_reference(&e.resource) => Some((m.name.as_str(), m.code.as_deref())),
            _ => None,
        };
        let (skip, previous) = self
            .dedupe(&mut *conn, record.table(), tail, &e.resource, medicine, &mut checks)
            .await?;
        if skip {
            self.skipped("duplicate");
            return Ok(false);
        }
        let allowance = if checks.iter().all(|c| c.ok) {
            "structured_valid"
        } else {
            "structured_partial"
        };
        let decision = auto_accept(1.0, &checks, allowance, self.identity, DEFAULT_THRESHOLD);
        let src = self.src(reference, record.common().effective_at);
        if decision.auto {
            add_record(conn, record, &SrcRef::New(src), RecordOptions::new("import")).await?;
            self.summary.accepted += 1;
            return Ok(true);
        }
        let payload = json!({
            "projection": "record",
            "record_kind": record.kind(),
            "record": record_json(record)?,
            "src": serde_json::to_value(&src)?,
            "supersedes": previous.map(|p| p.to_string()),
            "checks": serde_json::to_value(&checks)?,
            "identity": self.identity,
            "why_not": decision.why_not,
            "resource_type": e.rtype(),
            "json_pointer": e.pointer,
        });
        create_item(
            conn,
            &ReviewItemIn {
                kind: "fhir_unmapped".into(),
                household_id: Some(self.profile.household_id),
                profile_id: Some(self.profile.id),
                document_id: Some(self.document_id),
                reasons: decision.reasons.clone(),
                row_conf: Some(1.0),
                payload,
                ..ReviewItemIn::default()
            },
        )
        .await?;
        self.summary.queued += 1;
        self.count(&decision.reasons);
        Ok(true)
    }
}

impl Run<'_> {
    /// A relative (v0.3): auto-accepted under the same rule as records, held in review otherwise.
    async fn family(
        &mut self,
        conn: &mut PgConnection,
        e: &Entry,
        member: &MemberIn,
        mut checks: Vec<Check>,
        tail: &str,
        reference: &str,
    ) -> Result<bool> {
        let (skip, previous) = self
            .dedupe(&mut *conn, family::TABLE, tail, &e.resource, None, &mut checks)
            .await?;
        if skip {
            self.skipped("duplicate");
            return Ok(false);
        }
        let allowance = if checks.iter().all(|c| c.ok) {
            "structured_valid"
        } else {
            "structured_partial"
        };
        let decision = auto_accept(1.0, &checks, allowance, self.identity, DEFAULT_THRESHOLD);
        let src = self.src(reference, None);
        if decision.auto {
            family::add(conn, member, &SrcRef::New(src), "import", None, None).await?;
            self.summary.accepted += 1;
            return Ok(true);
        }
        let payload = json!({
            "projection": "family_member",
            "member": serde_json::to_value(member)?,
            "src": serde_json::to_value(&src)?,
            "supersedes": previous.map(|p| p.to_string()),
            "checks": serde_json::to_value(&checks)?,
            "identity": self.identity,
            "why_not": decision.why_not,
            "resource_type": e.rtype(),
            "json_pointer": e.pointer,
        });
        create_item(
            conn,
            &ReviewItemIn {
                kind: "fhir_unmapped".into(),
                household_id: Some(self.profile.household_id),
                profile_id: Some(self.profile.id),
                document_id: Some(self.document_id),
                reasons: decision.reasons.clone(),
                row_conf: Some(1.0),
                payload,
                ..ReviewItemIn::default()
            },
        )
        .await?;
        self.summary.queued += 1;
        self.count(&decision.reasons);
        Ok(true)
    }
}

/// A FHIR id ([A-Za-z0-9\-.]{1,64}).
pub(crate) fn fhir_id(s: &str) -> bool {
    !s.is_empty() && s.len() <= 64 && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'.')
}

/// A medication that names its medicine only through `medicationReference`: the mapper reads the
/// Medication it points at (`mapping::medication`).
pub(crate) fn by_reference(r: &Value) -> bool {
    matches!(
        r.get("resourceType").and_then(Value::as_str),
        Some("MedicationStatement" | "MedicationRequest")
    ) && r.get("medicationCodeableConcept").is_none_or(Value::is_null)
        && truthy(r.get("medicationReference"))
}

/// Whether the mapper read `m` as sensitive: a record's or a relative's own tags. An Observation's
/// tags are its resource's `meta.security`, which [`Run::one`] reads for every entry.
fn mapped_sensitive(m: Option<&Mapped>) -> bool {
    match m {
        Some(Mapped::Record { record, .. }) => !record.common().sensitivity.is_empty(),
        Some(Mapped::Family { member, .. }) => !member.sensitivity.is_empty(),
        _ => false,
    }
}

/// `record.model_dump(mode="json")`.
pub fn record_json(record: &RecordIn) -> Result<Value> {
    Ok(match record {
        RecordIn::Condition(r) => serde_json::to_value(r)?,
        RecordIn::Medication(r) => serde_json::to_value(r)?,
        RecordIn::Allergy(r) => serde_json::to_value(r)?,
        RecordIn::Immunization(r) => serde_json::to_value(r)?,
        RecordIn::Procedure(r) => serde_json::to_value(r)?,
        RecordIn::Visit(r) => serde_json::to_value(r)?,
    })
}

// ----------------------------------------------------------- review accept ----

/// A held record becomes a record (a correction when it changed upstream). A resource we do not
/// project cannot be accepted: it stays stored, and the person dismisses the item.
async fn accept_fhir_item(
    conn: &mut PgConnection,
    item: &ReviewItem,
    user_id: Uuid,
    edits: Option<&Value>,
) -> Result<Vec<Uuid>> {
    let payload = &item.payload;
    if payload.get("projection").and_then(Value::as_str) == Some("family_member") {
        return accept_family_item(conn, payload, user_id, edits).await;
    }
    if payload.get("projection").and_then(Value::as_str) != Some("record") {
        return Err(Error::Review(
            "stored, not projected: this resource type has no table yet; dismiss the item".into(),
        ));
    }
    let kind = payload
        .get("record_kind")
        .and_then(Value::as_str)
        .ok_or_else(|| Error::Review("the held record has no kind".into()))?;
    let mut data = payload
        .get("record")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let received = data.clone();
    if let Some(Value::Object(e)) = edits {
        data.extend(e.clone());
    }
    let mut record = RecordIn::from_json(kind, Value::Object(data))?;
    // a visit whose period was edited: the stored resource carries the edited period, so the export
    // never says a date the visit's columns do not
    if let RecordIn::Visit(v) = &mut record
        && let Some(Value::Object(e)) = edits
        && ["period_start", "period_end"]
            .iter()
            .any(|k| e.contains_key(*k) && e.get(*k) != received.get(*k))
    {
        let period = ownhealth_core::fhir::encounter(v)["period"].clone();
        if let Some(Value::Object(resource)) = v.common.fhir.as_mut() {
            resource.insert("period".into(), period);
        }
    }
    let src = held_src(payload, user_id)?;
    let committed = match payload.get("supersedes").and_then(Value::as_str) {
        Some(old) => {
            let old = Uuid::parse_str(old).map_err(|e| Error::Invalid(e.to_string()))?;
            supersede_record(conn, old, &record, &SrcRef::New(src), "person", "updated at the source").await?
        }
        None => add_record(conn, &record, &SrcRef::New(src), RecordOptions::new("person")).await?,
    };
    Ok(vec![committed.id])
}

fn held_src(payload: &Value, user_id: Uuid) -> Result<SrcIn> {
    let mut src = payload
        .get("src")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    src.insert("actor_kind".into(), json!("user"));
    src.insert("actor_id".into(), json!(user_id.to_string()));
    serde_json::from_value::<SrcIn>(Value::Object(src))
        .map_err(|e| Error::Invalid(e.to_string()))?
        .validate()
}

/// A held relative becomes a family_member version (a correction when it changed upstream).
async fn accept_family_item(
    conn: &mut PgConnection,
    payload: &Value,
    user_id: Uuid,
    edits: Option<&Value>,
) -> Result<Vec<Uuid>> {
    let mut data = payload
        .get("member")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    if let Some(Value::Object(e)) = edits {
        data.extend(e.clone());
        // the edited fields, not the received resource, are what the stored FHIR must say
        data.shift_remove("fhir");
    }
    let member: MemberIn = serde_json::from_value(Value::Object(data)).map_err(|e| Error::Invalid(e.to_string()))?;
    let src = SrcRef::New(held_src(payload, user_id)?);
    let committed = match payload.get("supersedes").and_then(Value::as_str) {
        Some(old) => {
            let old = Uuid::parse_str(old).map_err(|e| Error::Invalid(e.to_string()))?;
            family::supersede(
                conn,
                old,
                &member,
                &src,
                "person",
                "updated at the source",
                family::LinkChange::Keep,
            )
            .await?
        }
        None => family::add(conn, &member, &src, "person", None, None).await?,
    };
    Ok(vec![committed.id])
}

pub(crate) fn accept_fhir_boxed<'c>(
    conn: &'c mut PgConnection,
    item: &'c ReviewItem,
    user_id: Uuid,
    edits: Option<&'c Value>,
) -> BoxFuture<'c, Result<Vec<Uuid>>> {
    Box::pin(accept_fhir_item(conn, item, user_id, edits))
}
