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
    old.profiles.insert(sam, sams);
    keyfile::write(&settings.keyring_path(), &old)?;
    let ring = rewrite(&db, &settings).await?;
    assert!(!ring.profiles.contains_key(&sam) && ring.shredded.contains(&sam));
    let names: Vec<String> = fs::read_dir(settings.keyring_path())?
        .filter_map(|e| e.ok().map(|e| e.file_name().to_string_lossy().into_owned()))
        .collect();
    assert_eq!(names, vec![keyfile::FILE.to_string()]);
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
            assert!(!real.join(keyfile::FILE).exists(), "{why}: a keyring was written");
        }
    }
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
