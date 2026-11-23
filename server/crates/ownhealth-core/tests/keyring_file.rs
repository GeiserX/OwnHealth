//! Slice 47: `keyring.json`, the profile keys kept out of the database dump (`keyfile.rs`). A rewrite
//! copies every live key of `profile_key` into the file and drops every shredded one for good. A
//! profile the table does not know keeps its key in the file. A damaged file, a file of another
//! secrets key, a symlink or a folder others may write is refused, never reset.

use std::fs;

use ownhealth_core::config::Settings;
use ownhealth_core::keyfile::{self, KeyringFile};
use ownhealth_core::keys::{Kek, Keyring, shred};
use ownhealth_core::testing::{TestDb, random_secrets_key};
use uuid::Uuid;

type R = anyhow::Result<()>;

/// A migrated database, its settings in a fresh data folder, and the KEK of its secrets key.
async fn setup() -> anyhow::Result<(TestDb, tempfile::TempDir, Settings, Kek)> {
    let db = TestDb::new().await?;
    let dir = tempfile::tempdir()?;
    let settings = db.settings(dir.path())?;
    let kek = Kek::from_settings(&settings)?;
    Ok((db, dir, settings, kek))
}

async fn rewrite(db: &TestDb, settings: &Settings) -> anyhow::Result<KeyringFile> {
    let mut c = db.pool.acquire().await?;
    Ok(keyfile::rewrite(&mut c, settings).await?)
}

/// The refusal of a rewrite, or "" when it wrote.
async fn refusal(db: &TestDb, settings: &Settings) -> anyhow::Result<String> {
    let mut c = db.pool.acquire().await?;
    Ok(keyfile::rewrite(&mut c, settings)
        .await
        .err()
        .map(|e| e.to_string())
        .unwrap_or_default())
}

fn b64(bytes: &[u8]) -> String {
    base64::Engine::encode(&base64::engine::general_purpose::STANDARD, bytes)
}

#[tokio::test]
async fn a_rewrite_keeps_every_live_key_and_drops_a_shredded_one_for_good() -> R {
    let (db, _dir, settings, kek) = setup().await?;
    let (alex, sam) = (Uuid::new_v4(), Uuid::new_v4());
    for p in [alex, sam] {
        assert!(Keyring::new(kek.clone(), db.url.clone()).dek(p, true)?.is_some());
    }
    let ring = rewrite(&db, &settings).await?;
    let stored: Vec<(Uuid, Vec<u8>)> =
        sqlx::query_as("select profile_id, wrapped_dek from profile_key order by profile_id")
            .fetch_all(&db.pool)
            .await?;
    assert_eq!(ring.profiles.clone().into_iter().collect::<Vec<_>>(), stored);
    assert_eq!(ring.kek_fingerprint.as_deref(), Some(&kek.fingerprint()[..]));
    assert!(ring.shredded.is_empty());
    // what it wrote is what a read gives back, only the owner reads it, and no plain key is in it
    let file = keyfile::path(&settings);
    assert_eq!(keyfile::read(&file)?, Some(ring));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = |p: &std::path::Path| fs::metadata(p).map(|m| m.permissions().mode() & 0o777);
        assert_eq!(mode(&file)?, 0o600);
        assert_eq!(mode(&settings.keyring_path())?, 0o700);
    }
    let text = fs::read_to_string(&file)?;
    assert!(!text.contains(settings.secrets_key.as_deref().unwrap_or("-")));

    // a profile the table does not know (made after the backup a restore loaded) keeps its key
    let newer = Uuid::new_v4();
    let mut ring = keyfile::read(&file)?.unwrap_or_default();
    ring.profiles.insert(newer, ring.profiles[&alex].clone());
    keyfile::write(&settings.keyring_path(), &ring)?;

    // Sam is deleted: the rewrite drops his key and remembers why it is missing
    let mut tx = db.pool.begin().await?;
    shred(&mut tx, sam).await?;
    tx.commit().await?;
    let ring = rewrite(&db, &settings).await?;
    assert!(!ring.profiles.contains_key(&sam) && ring.shredded.contains(&sam));
    assert_eq!(ring.shredded_keys[&sam].len(), 1, "the tombstone names the key it closes");
    assert!(ring.profiles.contains_key(&alex) && ring.profiles.contains_key(&newer));
    let sams = stored
        .iter()
        .find(|(p, _)| *p == sam)
        .map(|(_, w)| w.clone())
        .unwrap_or_default();
    assert!(
        !fs::read_to_string(&file)?.contains(&b64(&sams)),
        "Sam's key is still in the file"
    );

    // a file that still holds Sam's key (put back from an older copy) loses it again at the next
    // rewrite, and no leftover temporary file sits beside it
    let mut old = ring.clone();
    old.shredded.remove(&sam);
    old.shredded_keys.remove(&sam);
    old.profiles.insert(sam, sams);
    keyfile::write(&settings.keyring_path(), &old)?;
    let ring = rewrite(&db, &settings).await?;
    assert!(!ring.profiles.contains_key(&sam) && ring.shredded.contains(&sam));
    let names: Vec<String> = fs::read_dir(settings.keyring_path())?
        .filter_map(|e| e.ok().map(|e| e.file_name().to_string_lossy().into_owned()))
        .collect();
    assert_eq!(
        names.into_iter().collect::<std::collections::BTreeSet<_>>(),
        [".keyring.lock".into(), keyfile::FILE.into()].into()
    );
    Ok(())
}

#[tokio::test]
async fn a_damaged_or_foreign_keyring_is_refused_and_left_as_it_is() -> R {
    let (db, dir, settings, kek) = setup().await?;
    assert!(Keyring::new(kek, db.url.clone()).dek(Uuid::new_v4(), true)?.is_some());
    let file = keyfile::path(&settings);
    fs::create_dir_all(settings.keyring_path())?;
    for bad in [
        "{",
        r#"{"schema": "ownhealth.keyring/v0", "profiles": {}, "shredded": []}"#,
    ] {
        fs::write(&file, bad)?;
        let err = refusal(&db, &settings).await?;
        assert!(err.contains("cannot read the keyring"), "{err}");
        assert_eq!(fs::read_to_string(&file)?, bad, "a damaged keyring was replaced");
    }
    // a keyring another secrets key made
    let other = Kek::from_base64(&random_secrets_key())?;
    let foreign = KeyringFile {
        kek_fingerprint: Some(other.fingerprint().to_vec()),
        ..KeyringFile::default()
    };
    keyfile::write(&settings.keyring_path(), &foreign)?;
    let err = refusal(&db, &settings).await?;
    assert!(err.contains("another OWNHEALTH_SECRETS_KEY"), "{err}");
    assert_eq!(keyfile::read(&file)?, Some(foreign));

    // a symlinked folder, and a folder others may write
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let real = dir.path().join("real-keyring");
        fs::create_dir_all(&real)?;
        let link = dir.path().join("keyring-link");
        std::os::unix::fs::symlink(&real, &link)?;
        let mut s = settings.clone();
        for (folder, mode, why) in [
            (link, 0o700, "the keyring folder is a symlink"),
            (real.clone(), 0o770, "the keyring folder is group-writable"),
            (real.clone(), 0o777, "the keyring folder is world-writable"),
        ] {
            fs::set_permissions(&real, fs::Permissions::from_mode(mode))?;
            s.keyring_dir = Some(folder);
            let err = refusal(&db, &s).await?;
            assert!(err.contains(why), "{why}: {err}");
            assert_owner_advice(&err);
            assert!(!real.join(keyfile::FILE).exists(), "{why}: a keyring was written");
        }

        // the file itself: a symlink, and a file others may write, in a folder only the owner writes
        fs::set_permissions(&real, fs::Permissions::from_mode(0o700))?;
        let mut s = settings.clone();
        s.keyring_dir = Some(real.clone());
        let mine = real.join(keyfile::FILE);
        // a keyring a rewrite would take and replace, were it not for where it sits or its mode
        let elsewhere = dir.path().join("elsewhere");
        keyfile::write(&elsewhere, &KeyringFile::default())?;
        let target = elsewhere.join(keyfile::FILE);
        std::os::unix::fs::symlink(&target, &mine)?;
        let before = fs::read(&target)?;
        let err = refusal(&db, &s).await?;
        assert!(err.contains("the keyring file is a symlink"), "{err}");
        assert_owner_advice(&err);
        assert!(
            fs::symlink_metadata(&mine)?.file_type().is_symlink(),
            "the symlink was replaced"
        );
        assert_eq!(fs::read(&target)?, before, "the file behind the symlink changed");
        fs::remove_file(&mine)?;
        fs::copy(&target, &mine)?;
        for (mode, why) in [
            (0o660, "the keyring file is group-writable"),
            (0o666, "the keyring file is world-writable"),
        ] {
            fs::set_permissions(&mine, fs::Permissions::from_mode(mode))?;
            let err = refusal(&db, &s).await?;
            assert!(err.contains(why), "{why}: {err}");
            assert_owner_advice(&err);
            assert_eq!(fs::read(&mine)?, before, "{why}: the keyring was replaced");
            assert_eq!(fs::metadata(&mine)?.permissions().mode() & 0o777, mode, "{why}");
        }
    }
    Ok(())
}

/// A keyring refused for its owner or mode is fixed by its mode or path, not by an older copy: the
/// advice says so, and never asks for a copy to be put back.
fn assert_owner_advice(err: &str) {
    assert!(err.contains("only its owner may write"), "{err}");
    assert!(err.contains("OWNHEALTH_KEYRING_DIR"), "{err}");
    assert!(!err.contains("Put back"), "{err}");
}

/// A crash between writing the temporary copy and renaming it leaves `.keyring.json.<hex>.tmp`
/// beside the keyring, still holding every key it was written with. The next rewrite removes it, so
/// no older copy with a shredded key stays in the folder.
#[tokio::test]
async fn a_rewrite_removes_a_temporary_copy_a_crash_left() -> R {
    let (db, _dir, settings, kek) = setup().await?;
    let sam = Uuid::new_v4();
    assert!(Keyring::new(kek, db.url.clone()).dek(sam, true)?.is_some());
    let ring = rewrite(&db, &settings).await?;
    let sams = b64(&ring.profiles[&sam]);
    // the copy a rewrite was writing when the box lost power
    let left = settings
        .keyring_path()
        .join(format!(".{}.0123456789ab.tmp", keyfile::FILE));
    fs::copy(keyfile::path(&settings), &left)?;
    assert!(fs::read_to_string(&left)?.contains(&sams));
    let mut tx = db.pool.begin().await?;
    shred(&mut tx, sam).await?;
    tx.commit().await?;
    let ring = rewrite(&db, &settings).await?;
    assert!(ring.shredded.contains(&sam));
    assert!(
        !left.exists(),
        "a temporary copy with Sam's key is still in the keyring folder"
    );
    let names: Vec<String> = fs::read_dir(settings.keyring_path())?
        .filter_map(|e| e.ok().map(|e| e.file_name().to_string_lossy().into_owned()))
        .collect();
    assert_eq!(
        names.into_iter().collect::<std::collections::BTreeSet<_>>(),
        [".keyring.lock".into(), keyfile::FILE.into()].into()
    );
    Ok(())
}

#[tokio::test]
async fn a_keyring_inside_the_blob_folder_is_refused() -> R {
    let (db, _dir, mut settings, _kek) = setup().await?;
    settings.keyring_dir = Some(settings.blobs_path().join("keyring"));
    let err = refusal(&db, &settings).await?;
    assert!(
        err.contains("inside the blob folder") && err.contains("OWNHEALTH_KEYRING_DIR"),
        "{err}"
    );
    assert!(!settings.keyring_path().exists());
    Ok(())
}

#[cfg(unix)]
#[test]
fn a_fifo_is_refused_as_a_keyring_file() -> R {
    let dir = tempfile::tempdir()?;
    let file = dir.path().join(keyfile::FILE);
    assert!(
        std::process::Command::new("mkfifo")
            .args(["-m", "600"])
            .arg(&file)
            .status()?
            .success()
    );
    assert!(keyfile::owner_writes_only(&file, false, "keyring").is_err());
    Ok(())
}

#[tokio::test]
async fn a_keyring_path_with_parent_traversal_cannot_land_inside_blobs() -> R {
    let (_db, dir, mut settings, _) = setup().await?;
    fs::create_dir(dir.path().join("other"))?;
    fs::create_dir(settings.blobs_path())?;
    settings.keyring_dir = Some(dir.path().join("other/../blobs/keyring"));
    assert!(keyfile::check_folder(&settings).is_err());
    // a '..' after a folder that does not exist yet cannot be resolved: refused, never guessed
    settings.keyring_dir = Some(dir.path().join("nope/../blobs/keyring"));
    let err = keyfile::check_folder(&settings).err().map(|e| e.to_string()).unwrap_or_default();
    assert!(err.contains("'..'"), "{err}");
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(settings.blobs_path(), dir.path().join("blob-alias"))?;
        settings.keyring_dir = Some(dir.path().join("blob-alias/new/keyring"));
        assert!(keyfile::check_folder(&settings).is_err());
    }
    settings.keyring_dir = Some(dir.path().join("sibling/keyring"));
    keyfile::check_folder(&settings)?;
    Ok(())
}

#[tokio::test]
async fn a_rewrite_respects_a_lock_on_the_shared_keyring() -> R {
    use std::time::Duration;

    let (db, _dir, settings, _) = setup().await?;
    let other_db = TestDb::new().await?;
    rewrite(&db, &settings).await?;
    let lock = fs::File::options()
        .create(true)
        .truncate(false)
        .write(true)
        .open(settings.keyring_path().join(".keyring.lock"))?;
    lock.try_lock()?;
    let mut rewrite = Box::pin(rewrite(&other_db, &settings));
    let while_locked = tokio::time::timeout(Duration::from_secs(2), &mut rewrite).await;
    drop(lock);
    assert!(while_locked.is_err(), "rewrite bypassed the shared keyring lock");
    tokio::time::timeout(Duration::from_secs(5), rewrite).await??;
    Ok(())
}

#[tokio::test]
async fn a_keyring_tombstone_outlives_a_live_row_from_another_database() -> R {
    let (db, _dir, settings, kek) = setup().await?;
    let p = Uuid::new_v4();
    let survivor = Uuid::new_v4();
    let keyring = Keyring::new(kek, db.url.clone());
    for id in [p, survivor] {
        assert!(keyring.dek(id, true)?.is_some());
    }
    let mut old = rewrite(&db, &settings).await?;
    let survivor_key = old.profiles[&survivor].clone();
    old.profiles.remove(&p);
    old.shredded.insert(p);
    keyfile::write(&settings.keyring_path(), &old)?;

    let ring = rewrite(&db, &settings).await?;
    assert!(ring.shredded.contains(&p));
    assert!(!ring.profiles.contains_key(&p));
    assert_eq!(ring.profiles[&survivor], survivor_key);
    Ok(())
}

/// OWNHEALTH_KEYRING_DIR may name its folder through '..' (`<data>/../keyring`): the server starts with
/// it, so a delete must be able to rewrite the keyring there too, not leave the key in the file.
#[tokio::test]
async fn a_keyring_folder_named_through_a_parent_directory_is_rewritten() -> R {
    let (db, dir, mut settings, kek) = setup().await?;
    fs::create_dir_all(dir.path().join("data"))?;
    settings.keyring_dir = Some(dir.path().join("data/../keyring"));
    let sam = Uuid::new_v4();
    assert!(Keyring::new(kek, db.url.clone()).dek(sam, true)?.is_some());
    let ring = rewrite(&db, &settings).await?;
    assert!(ring.profiles.contains_key(&sam));
    let mut tx = db.pool.begin().await?;
    shred(&mut tx, sam).await?;
    tx.commit().await?;
    let ring = rewrite(&db, &settings).await?;
    assert!(!ring.profiles.contains_key(&sam) && ring.shredded.contains(&sam));
    assert_eq!(keyfile::read(&dir.path().join("keyring").join(keyfile::FILE))?, Some(ring));
    Ok(())
}
