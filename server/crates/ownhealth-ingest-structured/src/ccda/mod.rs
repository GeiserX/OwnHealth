//! C-CDA import, part 1 (slice 20; ingestion.md 3.3, connectors.md 2.2). Job kind `import.ccda`.
//!
//! A US portal's "download my record" is usually a C-CDA document (a CCD, a referral or discharge
//! note): one XML `ClinicalDocument` with coded sections. The upload sniff (`ownhealth_core::documents`)
//! sends a file whose root element is `ClinicalDocument` here. The converter turns the six core
//! sections into FHIR R4 resources, and [`crate::fhir::import_resources`] imports them under the same
//! rules as a FHIR file: valid LOINC and UCUM with a matching patient auto-accept, the rest waits in
//! review, and a resource seen before is a duplicate.
//!
//! - Sections read: Allergies, Problems, Medications, Immunizations, Results (a DiagnosticReport plus
//!   its Observations) and Vital Signs. [`convert`] says per section what it made.
//! - Any other section (encounters, procedures, notes and social history are part 2), and any entry
//!   or part of an entry of a known section that the converter does not read, becomes one
//!   `fhir_unmapped_summary` review item per section. Sections that hold only narrative text (a core
//!   section too, unless it says it has no information) are listed in one more item. The stored file
//!   keeps all of it; nothing is dropped without a count.
//! - What those items count was read by nothing, so nothing checked it for sensitive content: the
//!   document is flagged `unprojected` with that count, as a connector pull is, and
//!   `ownhealth_core::documents::document_is_sensitive` keeps the file and the items behind
//!   read:sensitive. A file that carries a DS4P label that makes a record sensitive is flagged
//!   `sensitive` the same way, even when the dedupe imports nothing from it.
//! - A file that is not well-formed XML, declares a DOCTYPE, is larger than 32 MB (checked on the
//!   stored size, before the file is read), holds more than 2 million nodes or nests deeper than 256
//!   levels is refused (`Error::Invalid`), never parsed further: no entity is ever expanded and
//!   nothing recurses ([`xml`]). So is a file saved as UTF-16 (the reason says to save it as UTF-8),
//!   and a document whose body is a `nonXMLBody` (a scan or an attached file, with no coded section;
//!   its reference is never followed).
//! - The import locks its document row first, so a move of the document waits for the run to commit
//!   (and then retracts what it wrote) instead of slipping in while it converts. A document that is
//!   no longer active (retracted after a move queued its run) is not read: the run writes nothing
//!   and its summary says `not_assigned_or_retracted`, as the PDF import's does.
//!
//! The mapping is a Rust port of the C-CDA to FHIR converter of Medplum
//! (`packages/ccda/src/ccda-to-fhir.ts`, <https://github.com/medplum/medplum>), Copyright Orangebot,
//! Inc. and Medplum contributors, licensed under the Apache License 2.0; its NOTICE is in the
//! repository NOTICE. [`convert`] lists where this port differs.

pub mod convert;
pub mod xml;

use std::collections::BTreeMap;

use ownhealth_core::access::get_profile;
use ownhealth_core::blobs::{BlobOwner, BlobStore};
use ownhealth_core::ingest::ImportSummary;
use ownhealth_core::jobs::{Job, JobContext, JobError};
use ownhealth_core::review::{ReviewItemIn, create_item};
use ownhealth_core::{Error, Result};
use serde_json::{Value, json};
use sqlx::PgConnection;
use uuid::Uuid;

pub use convert::{Converted, Section, SectionReport, convert};

use crate::py::truthy;

/// The job a C-CDA upload runs.
pub const JOB: &str = "import.ccda";
/// The MIME type a C-CDA document is stored with.
pub const MIME: &str = "application/hl7-cda+xml";
/// The `resource_type` of the review item that lists the narrative-only sections (the UI's
/// `CCDA_NARRATIVE` names it in the reader's language).
pub const NARRATIVE: &str = "C-CDA narrative section";
/// A review item lists this many entry pointers; the stored file holds every entry.
const ITEM_REFS: usize = 20;

/// Import a C-CDA document already stored as a blob. Its `flags.ccda` lists what each section gave.
pub async fn import_ccda(conn: &mut PgConnection, blobs: &BlobStore, document_id: Uuid) -> Result<ImportSummary> {
    let (profile_id, status, sha, filename, flags): (Option<Uuid>, String, String, Option<String>, Value) =
        sqlx::query_as(
            "select profile_id, status, encode(blob_sha256, 'hex'), filename, flags from document \
             where id = $1 for update",
        )
        .bind(document_id)
        .fetch_one(&mut *conn)
        .await?;
    if status != "active" {
        // retracted after this run was queued (a move queues one): nothing is read or written, as
        // the PDF import does
        let mut summary = ImportSummary::new(document_id.to_string());
        summary.reasons.insert("not_assigned_or_retracted".into(), 1);
        return Ok(summary);
    }
    if let Some(done) = flags.get("import").filter(|d| truthy(Some(d))) {
        return Ok(crate::fhir::summary_from(done)); // the job ran before
    }
    let profile_id =
        profile_id.ok_or_else(|| Error::Invalid("assign the document to a profile before importing it".into()))?;
    let profile = get_profile(&mut *conn, profile_id)
        .await?
        .ok_or_else(|| Error::NotFound(format!("unknown profile {profile_id}")))?;
    let owner = BlobOwner::Profile(profile_id);
    let size = blobs.size(owner, &sha)?;
    if size > xml::MAX_BYTES as u64 {
        // refused before a byte is read: the converter would refuse it after reading it all
        return Err(xml::too_large(size));
    }
    let bytes = blobs.get_bytes(owner, &sha)?;
    let converted = tokio::task::spawn_blocking(move || convert(&bytes))
        .await
        .map_err(std::io::Error::other)??;
    let key = format!("ccda-{}", sha.get(..12).unwrap_or(&sha));
    let (mut summary, identity) =
        crate::fhir::import_resources(conn, &converted.entries, &profile, document_id, &key, None, "C-CDA").await?;
    let label = filename
        .filter(|f| !f.is_empty())
        .or_else(|| converted.title.clone())
        .unwrap_or_else(|| "C-CDA".into());
    let held = held_items(&converted, &key, &label);
    let unread: i64 = held.iter().map(|h| h["count"].as_i64().unwrap_or(0)).sum();
    let sensitive = converted
        .entries
        .iter()
        .filter(|e| !crate::fhir::mapping::sensitivity(&e.resource).is_empty())
        .count();
    for payload in held {
        let count = payload["count"].as_i64().unwrap_or(0);
        create_item(
            &mut *conn,
            &ReviewItemIn {
                kind: "fhir_unmapped_summary".into(),
                household_id: Some(profile.household_id),
                profile_id: Some(profile.id),
                document_id: Some(document_id),
                reasons: vec!["unmapped".into()],
                payload,
                ..ReviewItemIn::default()
            },
        )
        .await?;
        summary.queued += 1;
        *summary.reasons.entry("unmapped".into()).or_default() += count;
    }
    sqlx::query(
        "update document set flags = flags || jsonb_build_object('ccda', cast($1 as jsonb)) \
         || jsonb_strip_nulls(jsonb_build_object('unprojected', $2::bigint, 'sensitive', $3::bigint)) where id = $4",
    )
    .bind(sections_flag(&converted))
    .bind((unread > 0).then_some(unread))
    .bind((sensitive > 0).then_some(sensitive as i64))
    .bind(document_id)
    .execute(&mut *conn)
    .await?;
    crate::fhir::mark_imported(conn, document_id, &summary, Some(&identity)).await?;
    Ok(summary)
}

/// The review payloads for what the converter did not read: one per section with entries or parts
/// left (`resource_type` is the section's title), and one for every section that holds narrative
/// only ([`NARRATIVE`]): a section this version does not read without entries, or a core section with
/// narrative text and no entries that does not say it has no information. The shape is a connector
/// pull's `fhir_unmapped_summary`.
pub fn held_items(c: &Converted, key: &str, label: &str) -> Vec<Value> {
    let source = json!({"kind": "ccda", "label": label});
    let mut out = Vec::new();
    let mut narrative = BTreeMap::new();
    let mut narrative_refs = Vec::new();
    for s in &c.sections {
        let title = if s.title.is_empty() {
            s.templates.first().cloned().unwrap_or_else(|| "untitled".into())
        } else {
            s.title.clone()
        };
        let template = s.templates.first().map_or("no templateId", String::as_str);
        let narrative_only = s.entries == 0 && (s.kind.is_none() || s.narrative);
        let (count, notes) = match s.kind {
            _ if narrative_only => {
                *narrative.entry(format!("{title} ({template})")).or_insert(0) += 1;
                narrative_refs.push(format!("{key}{}", s.pointer));
                continue;
            }
            None => {
                let note = format!("section {template} is not read yet");
                (s.entries, BTreeMap::from([(note, s.entries)]))
            }
            Some(_) if s.not_read.is_empty() => continue,
            Some(_) => {
                let mut notes = BTreeMap::new();
                for n in &s.not_read {
                    *notes.entry(format!("not read: {n}")).or_insert(0usize) += 1;
                }
                (s.not_read.len(), notes)
            }
        };
        out.push(json!({
            "projection": null,
            "resource_type": format!("{title} (C-CDA)"),
            "count": count,
            "notes": notes,
            "source": source,
            "section": {"pointer": s.pointer, "templates": s.templates, "code": s.code},
            "import_refs": [format!("{key}{}", s.pointer)],
        }));
    }
    if !narrative.is_empty() {
        narrative_refs.truncate(ITEM_REFS);
        out.push(json!({
            "projection": null,
            "resource_type": NARRATIVE,
            "count": narrative.values().sum::<usize>(),
            "notes": narrative,
            "source": source,
            "import_refs": narrative_refs,
        }));
    }
    out
}

/// `document.flags.ccda`: per section, its title, templateId, kind and what it gave.
fn sections_flag(c: &Converted) -> Value {
    let sections: Vec<Value> = c
        .sections
        .iter()
        .map(|s| {
            json!({
                "title": s.title,
                "template": s.templates.first(),
                "kind": s.kind.map(Section::name),
                "entries": s.entries,
                "resources": s.resources,
                "not_read": s.not_read.len(),
            })
        })
        .collect();
    json!({"title": c.title, "sections": sections})
}

/// Job `import.ccda`: store the upload as a document, then import it, in one transaction. A file the
/// converter refuses fails for good: a retry reads the same bytes.
pub async fn import_ccda_job(ctx: JobContext, job: Job) -> std::result::Result<Option<Value>, JobError> {
    let mut tx = ctx.pool.begin().await?;
    let doc = crate::jobs::document_for(&mut tx, &ctx.blobs, &job.payload, MIME, "ccda").await?;
    let summary = import_ccda(&mut tx, &ctx.blobs, doc).await.map_err(|e| match e {
        Error::Invalid(m) => JobError::permanent(format!("not a readable C-CDA document: {m}")),
        other => JobError::from(other),
    })?;
    tx.commit().await?;
    Ok(Some(summary.to_value()))
}
