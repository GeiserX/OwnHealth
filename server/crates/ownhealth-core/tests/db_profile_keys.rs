//! Slice 46: the `profile_key` table seen from two secrets keys. A start refuses a key that does not
//! open the stored keys, and a key is stored only under the KEK every stored key has, so a key
//! swapped while the app runs cannot add keys the right one never opens.

use ownhealth_core::keys::{Kek, Keyring, check_start, shred};
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
