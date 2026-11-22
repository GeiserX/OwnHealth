//! Slice 46: the `profile_key` table seen from two secrets keys. A start refuses a key that does not
//! open the stored keys, and a key is stored only under the KEK every stored key has, so a key
//! swapped while the app runs cannot add keys the right one never opens.

use std::sync::Arc;

use ownhealth_core::blobs::{BlobError, BlobOwner, BlobStore, tree_files};
use ownhealth_core::keys::{Kek, Keyring, Slot, check_start, shred, shred_pending};
use ownhealth_core::testing::{TestDb, random_secrets_key};
use uuid::Uuid;

#[tokio::test]
async fn a_key_is_stored_and_a_start_passes_only_under_the_kek_of_the_stored_keys() -> anyhow::Result<()> {
    let db = TestDb::new().await?;
    let right = Kek::from_base64(&random_secrets_key())?;
    let wrong = Kek::from_base64(&random_secrets_key())?;
    let mut c = db.pool.acquire().await?;
    // a fresh install has no key yet: any key starts
    check_start(&mut c, &wrong).await?;
    let (a, b) = (Uuid::new_v4(), Uuid::new_v4());
    assert!(Keyring::new(right.clone(), db.url.clone()).dek(a, true)?.is_some());
    check_start(&mut c, &right).await?;
    let err = check_start(&mut c, &wrong)
        .await
        .expect_err("another key started")
        .to_string();
    assert!(
        err.contains(&hex::encode(wrong.fingerprint()))
            && err.contains(&hex::encode(right.fingerprint()))
            && err.contains("OWNHEALTH_SECRETS_KEY")
            && err.contains("deploy/README.md")
            && !err.contains('\n'),
        "{err}"
    );
    // a keyring on the wrong KEK stores no key for a new profile, and says why
    let err = Keyring::new(wrong, db.url.clone())
        .dek(b, true)
        .expect_err("a key stored under another KEK");
    assert!(err.to_string().contains("another OWNHEALTH_SECRETS_KEY"), "{err}");
    let rows: i64 = sqlx::query_scalar("select count(*) from profile_key")
        .fetch_one(&mut *c)
        .await?;
    assert_eq!(rows, 1);
    // the right key stores the next profile's key
    assert!(Keyring::new(right, db.url.clone()).dek(b, true)?.is_some());
    Ok(())
}

/// A profile delete shreds the key in its own transaction. A key write that comes while that
/// transaction is still open waits for it, then refuses: no key is made for a deleted profile, so no
/// file is sealed for it after the delete. A profile with a key already loses it the same way.
#[tokio::test]
async fn a_key_write_racing_a_delete_waits_for_it_and_then_refuses() -> anyhow::Result<()> {
    let db = TestDb::new().await?;
    let kek = Kek::from_base64(&random_secrets_key())?;
    let (never, keyed) = (Uuid::new_v4(), Uuid::new_v4());
    assert!(Keyring::new(kek.clone(), db.url.clone()).dek(keyed, true)?.is_some());
    for p in [never, keyed] {
        let mut delete = db.pool.begin().await?;
        assert_eq!(shred(&mut delete, p).await?, u64::from(p == keyed));
        let (k, url) = (kek.clone(), db.url.clone());
        let write = std::thread::spawn(move || Keyring::new(k, url).dek(p, true));
        std::thread::sleep(std::time::Duration::from_millis(500));
        assert!(!write.is_finished(), "the key write did not wait for the delete");
        delete.commit().await?;
        let made = write.join().map_err(|_| anyhow::anyhow!("the key write panicked"))?;
        assert!(made.is_err(), "a key was made for a deleted profile");
        assert!(Keyring::new(kek.clone(), db.url.clone()).dek(p, false)?.is_none());
        let live: i64 =
            sqlx::query_scalar("select count(*) from profile_key where profile_id = $1 and wrapped_dek is not null")
                .bind(p)
                .fetch_one(&db.pool)
                .await?;
        assert_eq!(live, 0);
    }
    // a start still passes: a shredded row holds no key to open
    check_start(&mut *db.pool.acquire().await?, &kek).await?;
    Ok(())
}

/// A pending item's key lives in `pending_key`, wraps under the same KEK, counts at a start, and once
/// shredded is never made again: a write for the item after its release refuses.
#[tokio::test]
async fn a_pending_items_key_is_stored_checked_at_a_start_and_never_made_again_once_shredded() -> anyhow::Result<()> {
    let db = TestDb::new().await?;
    let right = Kek::from_base64(&random_secrets_key())?;
    let wrong = Kek::from_base64(&random_secrets_key())?;
    let item = Uuid::new_v4();
    let ring = Keyring::new(right.clone(), db.url.clone());
    assert!(ring.dek_for(Slot::Pending(item), true)?.is_some());
    let rows: (i64, i64) = sqlx::query_as(
        "select (select count(*) from pending_key where item_id = $1), (select count(*) from profile_key)",
    )
    .bind(item)
    .fetch_one(&db.pool)
    .await?;
    assert_eq!(rows, (1, 0), "an item key went to the wrong table");
    let mut c = db.pool.acquire().await?;
    // an install whose only key is an item's still refuses another secrets key
    check_start(&mut c, &right).await?;
    assert!(check_start(&mut c, &wrong).await.is_err(), "another key started");
    // a keyring on the wrong KEK stores no item key either
    assert!(
        Keyring::new(wrong, db.url.clone())
            .dek_for(Slot::Pending(Uuid::new_v4()), true)
            .is_err()
    );
    assert_eq!(shred_pending(&mut c, &[item]).await?, 1);
    let fresh = Keyring::new(right, db.url.clone());
    assert!(fresh.dek_for(Slot::Pending(item), false)?.is_none());
    let err = fresh
        .dek_for(Slot::Pending(item), true)
        .expect_err("a key made again for a settled item");
    assert!(err.to_string().contains("item"), "{err}");
    Ok(())
}

/// Two processes, each with its own keyring, on one blob folder and database (the app and the OCR
/// worker). The worker read Alex's key before Alex was deleted; after the delete removed the folder,
/// it lands one more sealed file there. That file never opens again, and the sweep removes it, with
/// the folders of a settled held mail and of a key that is not stored at all, while every live
/// profile's and item's folder stays.
#[tokio::test]
async fn the_sweep_removes_what_lands_after_a_delete_and_keeps_every_live_folder() -> anyhow::Result<()> {
    let db = TestDb::new().await?;
    let dir = tempfile::tempdir()?;
    let kek = Kek::from_base64(&random_secrets_key())?;
    let store = |kek: &Kek| BlobStore::with_keys(dir.path(), Arc::new(Keyring::new(kek.clone(), db.url.clone())));
    let (app, worker) = (store(&kek)?, store(&kek)?);
    let (alex, sam) = (Uuid::new_v4(), Uuid::new_v4());
    let (held, settled) = (Uuid::new_v4(), Uuid::new_v4());
    let page = app.put_bytes(BlobOwner::Profile(alex), b"Alex Rivera, LabNorte page 1")?;
    app.put_bytes(BlobOwner::Profile(sam), b"Sam Rivera, lipid panel")?;
    let held_sha = app.put_bytes(BlobOwner::Pending(held), b"From: LabNorte, held mail")?;
    app.put_bytes(BlobOwner::Pending(settled), b"From: promo, deleted mail")?;
    // the worker opens Alex's file, so it holds Alex's key in memory
    assert_eq!(
        worker.get_bytes(BlobOwner::Profile(alex), &page)?,
        b"Alex Rivera, LabNorte page 1"
    );
    // Alex is deleted (key shredded in the delete's transaction, folder removed after it), and the
    // held mail of the other item is deleted
    let mut tx = db.pool.begin().await?;
    shred(&mut tx, alex).await?;
    shred_pending(&mut tx, &[settled]).await?;
    tx.commit().await?;
    assert!(app.forget_profile(alex)?);
    // the worker, still holding the key, lands an OCR layer for Alex after the delete
    let late = worker.put_bytes(BlobOwner::Profile(alex), b"{\"words\": \"Alex Rivera\"}")?;
    let alex_dir = dir.path().join("enc").join(alex.simple().to_string());
    assert!(alex_dir.is_dir(), "the late write did not land");
    // nothing that starts after the delete opens it
    let after = store(&kek)?;
    assert!(matches!(
        after.get_bytes(BlobOwner::Profile(alex), &late),
        Err(BlobError::NotFound)
    ));
    // a folder whose key was never stored (another install's files dropped in)
    let stranger = dir
        .path()
        .join("enc")
        .join(Uuid::new_v4().simple().to_string())
        .join("ab");
    std::fs::create_dir_all(&stranger)?;
    std::fs::write(stranger.join("junk"), b"x")?;
    let mut c = db.pool.acquire().await?;
    assert_eq!(app.sweep_unkeyed(&mut c).await?, 3);
    assert!(!alex_dir.exists());
    let keys: Vec<String> = tree_files(dir.path())?.into_keys().collect();
    assert_eq!(keys.len(), 2, "{keys:?}");
    assert!(keys.iter().any(|k| k.starts_with(&format!("{}/", sam.simple()))));
    assert!(
        keys.iter()
            .any(|k| k.starts_with(&format!("pending/{}/", held.simple())))
    );
    assert_eq!(
        worker.get_bytes(BlobOwner::Pending(held), &held_sha)?,
        b"From: LabNorte, held mail"
    );
    assert_eq!(app.sweep_unkeyed(&mut c).await?, 0);
    Ok(())
}

/// A pending key whose item never came to be (a hold that rolled back after its key was made) is
/// shredded once a day old, and its folder swept; a young one, or a held mail's, stays.
#[tokio::test]
async fn an_item_key_with_no_item_is_shredded_after_a_day_and_its_folder_swept() -> anyhow::Result<()> {
    let db = TestDb::new().await?;
    let dir = tempfile::tempdir()?;
    let kek = Kek::from_base64(&random_secrets_key())?;
    let app = BlobStore::with_keys(dir.path(), Arc::new(Keyring::new(kek, db.url.clone())))?;
    let (old, young) = (Uuid::new_v4(), Uuid::new_v4());
    app.put_bytes(BlobOwner::Pending(old), b"a mail whose hold rolled back")?;
    app.put_bytes(BlobOwner::Pending(young), b"a mail being held right now")?;
    sqlx::query("update pending_key set created_at = now() - interval '2 days' where item_id = $1")
        .bind(old)
        .execute(&db.pool)
        .await?;
    let mut c = db.pool.acquire().await?;
    assert_eq!(app.sweep_unkeyed(&mut c).await?, 1);
    let keys: Vec<String> = tree_files(dir.path())?.into_keys().collect();
    assert_eq!(keys.len(), 1);
    assert!(keys[0].starts_with(&format!("pending/{}/", young.simple())), "{keys:?}");
    Ok(())
}

/// While a backup copies the blob tree (it holds the copy lock from before its database snapshot
/// until the copy ends), the sweep removes nothing: the snapshot may still name a folder whose key
/// went after it. The next sweep after the copy removes the folder.
#[tokio::test]
async fn the_sweep_waits_for_a_backups_copy_and_the_next_one_removes_the_folder() -> anyhow::Result<()> {
    let db = TestDb::new().await?;
    let dir = tempfile::tempdir()?;
    let kek = Kek::from_base64(&random_secrets_key())?;
    let app = BlobStore::with_keys(dir.path(), Arc::new(Keyring::new(kek, db.url.clone())))?;
    let settled = Uuid::new_v4();
    app.put_bytes(BlobOwner::Pending(settled), b"From: LabNorte, released mail")?;
    let mut tx = db.pool.begin().await?;
    shred_pending(&mut tx, &[settled]).await?;
    tx.commit().await?;
    let folder = dir.path().join("pending").join(settled.simple().to_string());
    let mut backup = db.pool.acquire().await?;
    sqlx::query("select pg_catalog.pg_advisory_lock(pg_catalog.hashtext('ownhealth.blob_copy'))")
        .execute(&mut *backup)
        .await?;
    let mut c = db.pool.acquire().await?;
    assert_eq!(
        app.sweep_unkeyed(&mut c).await?,
        0,
        "the sweep ran while a backup copied the files"
    );
    assert!(folder.is_dir());
    sqlx::query("select pg_catalog.pg_advisory_unlock(pg_catalog.hashtext('ownhealth.blob_copy'))")
        .execute(&mut *backup)
        .await?;
    assert_eq!(app.sweep_unkeyed(&mut c).await?, 1);
    assert!(!folder.exists());
    Ok(())
}

/// A folder the sweep cannot remove does not keep the others: every one gets its try, then the sweep
/// reports the failure, and a later sweep removes the folder once it can go.
#[tokio::test]
async fn a_folder_the_sweep_cannot_remove_leaves_the_next_one_to_go() -> anyhow::Result<()> {
    use std::fs::{Permissions, set_permissions};
    use std::os::unix::fs::PermissionsExt;
    // As root (or with CAP_DAC_OVERRIDE) a read-only folder does not stop a removal, so nothing here
    // can make the sweep fail: say so and stop.
    let probe = tempfile::tempdir()?;
    let ro = probe.path().join("read-only");
    std::fs::create_dir(&ro)?;
    std::fs::write(ro.join("entry"), b"probe")?;
    set_permissions(&ro, Permissions::from_mode(0o555))?;
    let removable = std::fs::remove_file(ro.join("entry")).is_ok();
    set_permissions(&ro, Permissions::from_mode(0o755))?;
    if removable {
        eprintln!(
            "skipped: this runner removes entries from a read-only folder (root or CAP_DAC_OVERRIDE), \
             so the sweep cannot be made to fail"
        );
        return Ok(());
    }
    let db = TestDb::new().await?;
    let dir = tempfile::tempdir()?;
    let kek = Kek::from_base64(&random_secrets_key())?;
    let app = BlobStore::with_keys(dir.path(), Arc::new(Keyring::new(kek, db.url.clone())))?;
    let (deleted, settled) = (Uuid::new_v4(), Uuid::new_v4());
    app.put_bytes(
        BlobOwner::Profile(deleted),
        b"Alex Rivera, lipid panel of a deleted profile",
    )?;
    app.put_bytes(BlobOwner::Pending(settled), b"From: LabNorte, released mail")?;
    let mut tx = db.pool.begin().await?;
    shred(&mut tx, deleted).await?;
    shred_pending(&mut tx, &[settled]).await?;
    tx.commit().await?;
    // the sweep tries profile folders first: this one cannot go while its files cannot be unlinked
    let stuck = dir.path().join("enc").join(deleted.simple().to_string());
    let next = dir.path().join("pending").join(settled.simple().to_string());
    set_permissions(&stuck, Permissions::from_mode(0o555))?;
    let mut c = db.pool.acquire().await?;
    let swept = app.sweep_unkeyed(&mut c).await;
    set_permissions(&stuck, Permissions::from_mode(0o755))?;
    assert!(swept.is_err(), "a folder that did not go was not reported: {swept:?}");
    assert!(stuck.is_dir());
    assert!(!next.exists(), "one folder that could not go kept the next one");
    assert_eq!(app.sweep_unkeyed(&mut c).await?, 1);
    assert!(!stuck.exists());
    Ok(())
}
