//! Slice 47, the Keys exit test (docs/ROADMAP.md, "Keys"): back up Alex and Sam Rivera, delete Sam,
//! then restore the old backup with the current keyring and secrets key. Every file of Sam's fails to
//! decrypt, the one that came through the inbox before it was assigned to her included, and every
//! file of Alex's opens. A restore with no keyring fails loudly. The documented limit: a keyring copy
//! taken before the delete opens Sam's files again, which is why the keyring is never kept in a
//! versioned copy.
//!
//! pg_dump and pg_restore 17 come from PATH or OWNHEALTH_PG_BINDIR (ownhealth-ops/tests/backup.rs).

#[path = "auth_support.rs"]
mod support;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::http::StatusCode;
use ownhealth_auth::household::UNLINK_JOB;
use ownhealth_core::blobs::{BlobOwner, BlobStore, sha256_hex_to_bytes, tree_files};
use ownhealth_core::config::Settings;
use ownhealth_core::documents::{NewDocument, create_document};
use ownhealth_core::jobs::{JobContext, drain};
use ownhealth_core::keyfile;
use ownhealth_core::keys::{Dek, Kek, Keyring, SealedReader, Slot};
use ownhealth_core::testing::TestDb;
use support::Env;
use uuid::Uuid;

type R = anyhow::Result<()>;

/// One stored document: its id, its owner when stored, and its bytes.
struct Doc {
    id: Uuid,
    sha: String,
    bytes: Vec<u8>,
}

/// A document of `profile`, its file sealed under the profile's key.
async fn stored(env: &Env, profile: Uuid, text: &str) -> anyhow::Result<Doc> {
    let bytes = text.as_bytes().to_vec();
    let sha = env.rt.blobs.put_bytes(BlobOwner::Profile(profile), &bytes)?;
    let mut c = env.pool().acquire().await?;
    let id = create_document(
        &mut c,
        &NewDocument::upload(
            env.fam().household_id,
            Some(profile),
            &sha,
            "application/pdf",
            i64::try_from(bytes.len())?,
        ),
    )
    .await?
    .id;
    Ok(Doc { id, sha, bytes })
}

/// A scan that arrives with no profile (sealed under its own item key in `pending/`), then is
/// assigned to `profile`: the move seals it again under the profile's key and shreds the item key.
async fn through_the_inbox(env: &Env, profile: Uuid, text: &str) -> anyhow::Result<Doc> {
    let bytes = text.as_bytes().to_vec();
    let scratch = BlobStore::new(env.dir.path().join("scratch"))?;
    let sha = scratch.put_bytes(BlobOwner::Reference, &bytes)?;
    let mut c = env.pool().acquire().await?;
    let id = create_document(
        &mut c,
        &NewDocument::upload(
            env.fam().household_id,
            None,
            &sha,
            "application/pdf",
            i64::try_from(bytes.len())?,
        ),
    )
    .await?
    .id;
    drop(c);
    assert_eq!(env.rt.blobs.put_bytes(BlobOwner::Pending(id), &bytes)?, sha);
    let mut tx = env.pool().begin().await?;
    ownhealth_ingest_pdf::assign_document(&mut tx, &env.rt.blobs, id, profile, env.user("sam"), None).await?;
    tx.commit().await?;
    Ok(Doc { id, sha, bytes })
}

/// An empty database and data folder: a new machine, reading the keyring folder `keyring`.
async fn fresh(env: &Env, name: &str, keyring: &Path) -> anyhow::Result<(TestDb, Settings)> {
    let db = TestDb::empty().await?;
    let mut settings = db.settings(&env.dir.path().join(name))?;
    settings.secrets_key = env.settings.secrets_key.clone(); // the key the backup was made under
    settings.keyring_dir = Some(keyring.to_path_buf());
    Ok((db, settings))
}

/// Every key the restored instance can open: each profile's and each pending item's.
async fn opened_keys(db: &TestDb, settings: &Settings) -> anyhow::Result<Vec<std::sync::Arc<Dek>>> {
    let ring = Keyring::new(Kek::from_settings(settings)?, db.url.clone());
    let mut slots: Vec<Slot> = sqlx::query_scalar::<_, Uuid>("select profile_id from profile_key")
        .fetch_all(&db.pool)
        .await?
        .into_iter()
        .map(Slot::Profile)
        .collect();
    let items: Vec<Uuid> = sqlx::query_scalar("select item_id from pending_key")
        .fetch_all(&db.pool)
        .await?;
    slots.extend(items.into_iter().map(Slot::Pending));
    let mut out = Vec::new();
    for slot in slots {
        if let Some(dek) = ring.dek_for(slot, false)? {
            out.push(dek);
        }
    }
    Ok(out)
}

fn sha32(sha: &str) -> anyhow::Result<[u8; 32]> {
    let v = sha256_hex_to_bytes(sha)?;
    Ok(<[u8; 32]>::try_from(v.as_slice())?)
}

/// The restored blob files of `owners`, by their key in the store.
fn files_of(settings: &Settings, owners: &[String]) -> anyhow::Result<BTreeMap<String, PathBuf>> {
    Ok(tree_files(&settings.blobs_path())?
        .into_iter()
        .filter(|(k, _)| owners.iter().any(|o| k.starts_with(o.as_str())))
        .collect())
}

#[tokio::test]
async fn a_deleted_profile_stays_unreadable_in_older_backups_restored_with_the_current_keyring() -> R {
    ownhealth_ops::register();
    let env = Env::new(true).await?;
    let (alex, sam) = (env.profile("alex"), env.profile("sam"));
    let alex_docs = [
        stored(&env, alex, "%PDF-1.4 LabNorte lipid panel of Alex Rivera").await?,
        stored(&env, alex, "%PDF-1.4 LabNorte thyroid panel of Alex Rivera").await?,
    ];
    let sam_docs = [
        stored(&env, sam, "%PDF-1.4 LabNorte iron panel of Sam Rivera").await?,
        through_the_inbox(&env, sam, "%PDF-1.4 LabNorte scan for Sam Rivera, mailed in").await?,
    ];
    let scan = &sam_docs[1];
    let backup = ownhealth_ops::backup(&env.settings, &env.dir.path().join("b1").join("staging")).await?;
    // the copy an operator should never keep: the keyring as it was before the delete
    let before = env.dir.path().join("keyring-before-delete");
    std::fs::create_dir_all(&before)?;
    std::fs::copy(keyfile::path(&env.settings), before.join(keyfile::FILE))?;

    let mut me = env.as_user(env.user("sam")).await?;
    let r = me.delete("/api/profiles/sam?confirm=sam").await?;
    assert_eq!(r.status, StatusCode::OK, "{}", r.json);
    // at once: the keyring no longer holds Sam's key and says she was deleted
    let ring = keyfile::read(&keyfile::path(&env.settings))?.unwrap_or_default();
    assert!(
        !ring.profiles.contains_key(&sam),
        "the delete left Sam's key in the keyring"
    );
    assert!(ring.shredded.contains(&sam) && ring.profiles.contains_key(&alex));

    // no keyring: the restore stops before it loads anything
    let (empty_db, empty) = fresh(&env, "no-keyring", &env.dir.path().join("no-keyring-here")).await?;
    let err = ownhealth_ops::restore(&empty, &backup)
        .await
        .err()
        .map(|e| e.to_string())
        .unwrap_or_default();
    assert!(err.contains("no keyring at"), "{err}");
    let tables: i64 = sqlx::query_scalar("select count(*) from pg_tables where schemaname = 'public'")
        .fetch_one(&empty_db.pool)
        .await?;
    assert_eq!(tables, 0);

    // the old backup with the current keyring and key: Sam's rows come back, her files do not open
    let (db, restored) = fresh(&env, "restored", &env.settings.keyring_path()).await?;
    let report = ownhealth_ops::restore(&restored, &backup).await?;
    assert_eq!(report["ok"], true, "{report:#}");
    assert_eq!(report["keys"]["shredded"], serde_json::json!([sam]), "{report:#}");
    let store = BlobStore::for_settings(&restored)?;
    for d in &alex_docs {
        assert_eq!(
            store.get_bytes(BlobOwner::Profile(alex), &d.sha)?,
            d.bytes,
            "Alex's file"
        );
    }
    for d in &sam_docs {
        assert!(
            store.get_bytes(BlobOwner::Profile(sam), &d.sha).is_err(),
            "Sam's file {} opened after her delete",
            d.id
        );
    }
    assert!(store.get_bytes(BlobOwner::Pending(scan.id), &scan.sha).is_err());
    // and no key the restored instance holds opens any file of hers, sealed for her or for the scan's
    // time in the inbox: each one fails its check
    let keys = opened_keys(&db, &restored).await?;
    assert!(!keys.is_empty());
    let hers = files_of(
        &restored,
        &[format!("{}/", sam.simple()), format!("pending/{}/", scan.id.simple())],
    )?;
    assert!(!hers.is_empty(), "the backup holds none of Sam's sealed files");
    for (key, path) in &hers {
        for dek in &keys {
            for owner in [sam, scan.id] {
                for d in &sam_docs {
                    let opened = SealedReader::open(std::fs::File::open(path)?, dek, owner, &sha32(&d.sha)?);
                    assert!(opened.is_err(), "{key} opened with a key of the restored instance");
                }
            }
        }
    }

    // the documented limit: a keyring copy taken before the delete opens them again
    let (_db2, reopened) = fresh(&env, "reopened", &before).await?;
    let report = ownhealth_ops::restore(&reopened, &backup).await?;
    assert_eq!(report["ok"], true, "{report:#}");
    assert_eq!(report["keys"]["shredded"], serde_json::json!([]), "{report:#}");
    let store = BlobStore::for_settings(&reopened)?;
    for d in &sam_docs {
        assert_eq!(store.get_bytes(BlobOwner::Profile(sam), &d.sha)?, d.bytes, "{}", d.id);
    }
    Ok(())
}

/// The delete cannot rewrite the keyring (here a damaged file the operator must look at): the delete
/// still succeeds and the files go, and its retry job stays queued whole. Once the file is readable
/// again, the retry drops the key from the keyring.
#[tokio::test]
async fn a_keyring_the_delete_cannot_write_is_rewritten_by_its_retry() -> R {
    ownhealth_auth::register();
    let env = Env::new(true).await?;
    let sam = env.profile("sam");
    stored(&env, sam, "%PDF-1.4 LabNorte iron panel of Sam Rivera").await?;
    let mut c = env.pool().acquire().await?;
    keyfile::rewrite(&mut c, &env.settings).await?;
    drop(c);
    let file = keyfile::path(&env.settings);
    let good = std::fs::read(&file)?;
    std::fs::write(&file, b"{")?;

    let mut me = env.as_user(env.user("sam")).await?;
    let r = me.delete("/api/profiles/sam?confirm=sam").await?;
    assert_eq!(r.status, StatusCode::OK, "{}", r.json);
    assert_eq!(
        std::fs::read(&file)?,
        b"{",
        "the delete replaced a keyring it could not read"
    );
    assert!(
        !env.settings
            .blobs_path()
            .join("enc")
            .join(sam.simple().to_string())
            .exists()
    );
    let queued: i64 = sqlx::query_scalar("select count(*) from job where kind = $1 and state = 'queued'")
        .bind(UNLINK_JOB)
        .fetch_one(env.pool())
        .await?;
    assert_eq!(queued, 1, "the delete dropped its retry with the keyring unwritten");

    let run = || async {
        sqlx::query("update job set run_after = now() where kind = $1 and state = 'queued'")
            .bind(UNLINK_JOB)
            .execute(env.pool())
            .await?;
        let ctx = JobContext::new(env.pool().clone(), Arc::new(env.settings.clone()), env.rt.blobs.clone());
        anyhow::Ok(drain(&ctx, "core", 5, Some(&[UNLINK_JOB.to_string()])).await?)
    };
    // still damaged: the retry fails and waits for the next attempt
    assert_eq!(run().await?, 1);
    let (state, error): (String, Option<String>) = sqlx::query_as("select state, last_error from job where kind = $1")
        .bind(UNLINK_JOB)
        .fetch_one(env.pool())
        .await?;
    assert_eq!(state, "queued", "{error:?}");
    assert!(error.is_some_and(|e| e.contains("keyring")));
    // the operator puts the keyring back: the next attempt drops Sam's key
    std::fs::write(&file, &good)?;
    assert!(keyfile::read(&file)?.is_some_and(|k| k.profiles.contains_key(&sam)));
    assert_eq!(run().await?, 1);
    let ring = keyfile::read(&file)?.unwrap_or_default();
    assert!(!ring.profiles.contains_key(&sam) && ring.shredded.contains(&sam));
    let state: String = sqlx::query_scalar("select state from job where kind = $1")
        .bind(UNLINK_JOB)
        .fetch_one(env.pool())
        .await?;
    assert_eq!(state, "done");
    Ok(())
}
