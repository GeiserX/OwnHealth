//! The upload dispatcher and the sensitive-document rule (`core/documents.py`).

mod common;

use common::*;
use ownhealth_core::Error;
use ownhealth_core::blobs::BlobStore;
use ownhealth_core::documents::{StoredFile, dispatch_upload, document_is_sensitive};
use ownhealth_core::observations::{CommitOptions, ObservationIn, commit_observation};
use ownhealth_core::testing::TestDb;
use serde_json::json;

fn stored(blobs: &BlobStore, name: &str, bytes: &[u8]) -> anyhow::Result<StoredFile> {
    Ok(StoredFile {
        filename: name.into(),
        sha256: blobs.put_bytes(ownhealth_core::blobs::BlobOwner::Reference, bytes)?,
        size: bytes.len() as i64,
    })
}

#[tokio::test]
async fn uploads_become_documents_and_jobs_once() -> R {
    let db = TestDb::new().await?;
    let dir = tempfile::tempdir()?;
    let blobs = BlobStore::new(dir.path())?;
    let mut c = db.pool.acquire().await?;
    let f = family(&mut c).await?;
    let (hh, p) = (f.household_id, f.profile("alex"));
    let pdf = stored(&blobs, "labnorte.pdf", b"%PDF-1.4 synthetic")?;
    let out = dispatch_upload(&mut c, &blobs, hh, p, std::slice::from_ref(&pdf), None).await?;
    let item = &out["items"][0];
    let keys: Vec<&String> = item.as_object().map(|m| m.keys().collect()).unwrap_or_default();
    assert_eq!(
        keys,
        ["filename", "kind", "sha256", "document_id", "duplicate", "job_id"]
    );
    assert!(item["kind"] == "pdf" && item["duplicate"] == false && item["job_id"].is_i64());
    let again = dispatch_upload(&mut c, &blobs, hh, p, &[pdf], None).await?;
    assert_eq!(again["items"][0]["duplicate"], true);
    assert_eq!(again["items"][0]["document_id"], item["document_id"]);
    assert!(again["items"][0].get("job_id").is_none());
    let sources: i64 = sqlx::query_scalar("select count(*) from document_source")
        .fetch_one(&mut *c)
        .await?;
    assert_eq!(sources, 2);

    let vcf = stored(&blobs, "x.vcf.gz", b"\x1f\x8bsynthetic")?;
    let e = dispatch_upload(&mut c, &blobs, hh, p, std::slice::from_ref(&vcf), None).await;
    assert!(
        matches!(&e, Err(Error::Upload(m)) if m.contains("genome-facts.json")),
        "{e:?}"
    );
    let facts = stored(
        &blobs,
        "genome-facts.json",
        br#"{"schema": "ownhealth.genome-facts/1"}"#,
    )?;
    let out = dispatch_upload(&mut c, &blobs, hh, p, &[facts, vcf.clone()], None).await?;
    assert_eq!(out["items"][1]["attached_to"], "genome_facts");
    let payload: serde_json::Value = sqlx::query_scalar("select payload from job where kind = 'import.genome_facts'")
        .fetch_one(&mut *c)
        .await?;
    assert_eq!(payload["files"], json!({"x.vcf.gz": vcf.sha256}));
    let txt = stored(&blobs, "notes.txt", b"hello")?;
    assert!(matches!(
        dispatch_upload(&mut c, &blobs, hh, p, &[txt], None).await,
        Err(Error::Upload(_))
    ));
    Ok(())
}

/// An upload whose bytes sit in the plain layout of an install from before the keys: the dispatcher
/// reads its head for the profile, which hashes such a file once. That runs off the async runtime, so
/// a large legacy file never holds up the requests the runtime serves.
#[tokio::test]
async fn the_upload_sniff_checks_a_legacy_plain_file_off_the_runtime() -> R {
    let db = TestDb::new().await?;
    let dir = tempfile::tempdir()?;
    let blobs = BlobStore::for_settings(&db.settings(dir.path())?)?;
    assert!(blobs.seals());
    let mut c = db.pool.acquire().await?;
    let f = family(&mut c).await?;
    // bytes of its own, so no other test hashes the same file
    let text = format!("%PDF-1.4 LabNorte report {}", uuid::Uuid::new_v4());
    let pdf = stored(&blobs, "labnorte.pdf", text.as_bytes())?;
    let p = f.profile("alex");
    let out = dispatch_upload(&mut c, &blobs, f.household_id, p, std::slice::from_ref(&pdf), None).await?;
    assert_eq!(out["items"][0]["kind"], "pdf");
    let on = ownhealth_core::blobs::plain_hashed_on(&pdf.sha256);
    assert!(!on.is_empty(), "the legacy file was never checked");
    let here = std::thread::current().name().map(str::to_string);
    assert!(
        on.iter().all(|t| Some(t) != here.as_ref()),
        "hashed on the runtime's own thread: {on:?}"
    );
    Ok(())
}

#[tokio::test]
async fn a_document_with_a_sensitive_value_is_sensitive() -> R {
    let db = TestDb::new().await?;
    let dir = tempfile::tempdir()?;
    let blobs = BlobStore::new(dir.path())?;
    let mut c = db.pool.acquire().await?;
    let f = family(&mut c).await?;
    let (doc, _) = lab_document(&mut c, &blobs, &f, "alex").await?;
    assert!(!document_is_sensitive(&mut c, doc).await?);
    let o = ObservationIn {
        document_id: Some(doc),
        sensitivity: vec!["sexual_health".into()],
        ..obs(f.profile("alex"), "1")
    };
    commit_observation(&mut c, &o, &manual_src().into(), CommitOptions::new("person")).await?;
    assert!(document_is_sensitive(&mut c, doc).await?);
    Ok(())
}

#[tokio::test]
async fn concurrent_uploads_of_the_same_bytes_return_one_document() -> R {
    use ownhealth_core::documents::{NewDocument, create_document};
    use sqlx::Connection;
    let db = TestDb::new().await?;
    let mut c = db.pool.acquire().await?;
    let f = family(&mut c).await?;
    let (hh, p) = (f.household_id, f.profile("alex"));
    let sha = "ab".repeat(32);
    let mut a = db.pool.acquire().await?;
    let mut tx = a.begin().await?;
    let first = create_document(&mut tx, &NewDocument::upload(hh, Some(p), &sha, "application/pdf", 3)).await?;
    // The second upload selects before the first commits, then meets its row on insert.
    let pool = db.pool.clone();
    let second = tokio::spawn(async move {
        let mut b = pool.acquire().await?;
        let sha = "ab".repeat(32);
        anyhow::Ok(create_document(&mut b, &NewDocument::upload(hh, Some(p), &sha, "application/pdf", 3)).await?)
    });
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    tx.commit().await?;
    let second = second.await??;
    assert!(first.created && !second.created && !second.reactivated);
    assert_eq!(second.id, first.id);
    let sources: i64 = sqlx::query_scalar("select count(*) from document_source where document_id = $1")
        .bind(first.id)
        .fetch_one(&mut *c)
        .await?;
    assert_eq!(sources, 2);
    Ok(())
}

/// Removes `profile`'s upload of `sha` in a transaction of its own, as an importer does once it released it.
async fn remove_released_upload(c: &mut sqlx::PgConnection, blobs: &BlobStore, profile: uuid::Uuid, sha: &str) -> R {
    use sqlx::Connection;
    let mut tx = c.begin().await?;
    ownhealth_core::blobs::remove_unheld_upload(&mut tx, blobs, profile, sha).await?;
    tx.commit().await?;
    Ok(())
}

/// A document row of `profile` naming bytes of its own, for a page to hang from.
async fn bare_document(
    c: &mut sqlx::PgConnection,
    household: uuid::Uuid,
    profile: uuid::Uuid,
) -> anyhow::Result<uuid::Uuid> {
    Ok(sqlx::query_scalar(
        "insert into document (household_id, profile_id, blob_sha256, filename, mime, size_bytes, doc_type) \
         values ($1, $2, decode($3, 'hex'), 'informe.pdf', 'application/pdf', 3, 'other') returning id",
    )
    .bind(household)
    .bind(profile)
    .bind(ownhealth_core::ids::token_hex(32))
    .fetch_one(c)
    .await?)
}

/// The bytes of a released upload that another row also names (an imaging zip, a rendered page, any
/// blob column the backup and the seal job count) stay after the upload is removed: a row of the same
/// profile keeps the profile's copy, a row of another profile keeps the plain copy. With no other row
/// left, the upload's file goes.
#[tokio::test]
async fn removing_a_released_upload_keeps_the_bytes_any_blob_column_names() -> R {
    use ownhealth_core::blobs::BlobOwner;
    let db = TestDb::new().await?;
    let dir = tempfile::tempdir()?;
    let blobs = BlobStore::for_settings(&db.settings(dir.path())?)?;
    assert!(blobs.seals());
    let mut c = db.pool.acquire().await?;
    let f = family(&mut c).await?;
    let (hh, alex, lucia) = (f.household_id, f.profile("alex"), f.profile("lucia"));

    // the same profile's imaging zip
    let zip = format!("PK imaging LabNorte {}", uuid::Uuid::new_v4());
    let sha = blobs.put_bytes(BlobOwner::Profile(alex), zip.as_bytes())?;
    sqlx::query(
        "insert into imaging_import (profile_id, zip_blob, size_bytes, files) values ($1, decode($2, 'hex'), $3, 1)",
    )
    .bind(alex)
    .bind(&sha)
    .bind(zip.len() as i64)
    .execute(&mut *c)
    .await?;
    remove_released_upload(&mut c, &blobs, alex, &sha).await?;
    assert!(
        blobs.exists(BlobOwner::Profile(alex), &sha),
        "the imaging zip lost its file"
    );
    // nothing else names it: the file goes
    sqlx::query("delete from imaging_import where zip_blob = decode($1, 'hex')")
        .bind(&sha)
        .execute(&mut *c)
        .await?;
    remove_released_upload(&mut c, &blobs, alex, &sha).await?;
    assert!(
        !blobs.exists(BlobOwner::Profile(alex), &sha),
        "an unheld upload kept its file"
    );

    // a rendered page of the same profile's document
    let png = format!("PNG page LabNorte {}", uuid::Uuid::new_v4());
    let sha = blobs.put_bytes(BlobOwner::Profile(alex), png.as_bytes())?;
    let doc = bare_document(&mut c, hh, alex).await?;
    sqlx::query(
        "insert into document_page (document_id, page_no, width_pt, height_pt, text_layer_ok, image_blob) \
         values ($1, 1, 595, 842, true, decode($2, 'hex'))",
    )
    .bind(doc)
    .bind(&sha)
    .execute(&mut *c)
    .await?;
    remove_released_upload(&mut c, &blobs, alex, &sha).await?;
    assert!(
        blobs.exists(BlobOwner::Profile(alex), &sha),
        "the page image lost its file"
    );

    // a rendered page of another profile's document, over a plain copy from before the keys
    let png = format!("PNG page LabNorte {}", uuid::Uuid::new_v4());
    let sha = blobs.put_bytes(BlobOwner::Profile(alex), png.as_bytes())?;
    assert_eq!(blobs.put_bytes(BlobOwner::Reference, png.as_bytes())?, sha);
    let doc = bare_document(&mut c, hh, lucia).await?;
    sqlx::query(
        "insert into document_page (document_id, page_no, width_pt, height_pt, text_layer_ok, words_blob) \
         values ($1, 1, 595, 842, true, decode($2, 'hex'))",
    )
    .bind(doc)
    .bind(&sha)
    .execute(&mut *c)
    .await?;
    remove_released_upload(&mut c, &blobs, alex, &sha).await?;
    assert!(
        blobs.exists(BlobOwner::Reference, &sha),
        "another profile's page lost the plain copy"
    );
    assert!(
        !blobs.is_sealed(BlobOwner::Profile(alex), &sha)?,
        "the upload kept a copy no row of its profile names"
    );
    Ok(())
}
