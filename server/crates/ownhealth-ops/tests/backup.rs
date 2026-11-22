//! Backup, restore and verify-restore (`tests/ops/test_backup_restore.py`).
//!
//! pg_dump and pg_restore 17 must be on PATH or in OWNHEALTH_PG_BINDIR. Without a local client, point
//! it at the wrapper that runs them in the test container:
//! `OWNHEALTH_PG_BINDIR=$PWD/../scripts/pg-bin cargo test -p ownhealth-ops`.
//!
//! The live store is mixed, as an upgraded install is: the demo's files are sealed under each
//! profile's key, and one report of Alex's sits in the plain layout of a 0.1.1 install. Each TestDb
//! makes its own OWNHEALTH_SECRETS_KEY at run time; a fresh stack takes the live one unless a test
//! wants another.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use ownhealth_core::blobs::{BlobOwner, BlobStore, tree_files};
use ownhealth_core::config::Settings;
use ownhealth_core::demo::seed_demo;
use ownhealth_core::documents::{NewDocument, create_document};
use ownhealth_core::migrate;
use ownhealth_core::testing::TestDb;
use serde_json::Value;
use sha2::Digest;
use tempfile::TempDir;
use uuid::Uuid;

type R = anyhow::Result<()>;

/// A migrated database with the demo Rivera family and its LabNorte PDFs, and a backup of it.
struct Live {
    db: TestDb,
    settings: Settings,
    archive: PathBuf,
    dir: TempDir,
    /// Alex's profile and the bytes of his plain (pre-key) report
    alex: Uuid,
    legacy: Vec<u8>,
}

const LEGACY: &[u8] = b"%PDF-1.4 LabNorte report of Alex Rivera, stored before the keys";

async fn live() -> anyhow::Result<Live> {
    ownhealth_ops::register();
    let db = TestDb::new().await?;
    let dir = tempfile::tempdir()?;
    let settings = db.settings(&dir.path().join("live"))?;
    let blobs = BlobStore::for_settings(&settings)?;
    assert!(seed_demo(&db.pool, &settings, &blobs, false).await?);
    // a report stored by 0.1.1, before profiles had keys: the plain layout
    let (alex, household): (Uuid, Uuid) = sqlx::query_as("select id, household_id from profile where slug = 'alex'")
        .fetch_one(&db.pool)
        .await?;
    let sha = BlobStore::new(settings.blobs_path())?.put_bytes(BlobOwner::Reference, LEGACY)?;
    let mut c = db.pool.acquire().await?;
    create_document(
        &mut c,
        &NewDocument::upload(household, Some(alex), &sha, "application/pdf", LEGACY.len() as i64),
    )
    .await?;
    drop(c);
    let archive = ownhealth_ops::backup(&settings, &dir.path().join("backup").join("staging")).await?;
    Ok(Live {
        db,
        settings,
        archive,
        dir,
        alex,
        legacy: LEGACY.to_vec(),
    })
}

/// An empty database (no migrations) and an empty blob directory: a fresh stack.
async fn fresh(l: &Live) -> anyhow::Result<(TestDb, Settings)> {
    let db = TestDb::empty().await?;
    let mut settings = db.settings(&l.dir.path().join("fresh"))?;
    settings.secrets_key = l.settings.secrets_key.clone();
    Ok((db, settings))
}

/// The backup's blob files by manifest key: a sha256, or `<profile>/<address>` for a sealed one.
fn tree(archive: &Path) -> anyhow::Result<BTreeMap<String, PathBuf>> {
    Ok(tree_files(&archive.join("blobs"))?)
}

fn sealed_keys(m: &Value) -> Vec<String> {
    m["blobs"]["sealed"]
        .as_object()
        .map(|o| o.keys().cloned().collect())
        .unwrap_or_default()
}

fn flip_last_byte(p: &Path) -> R {
    make_writable(p)?; // blobs are stored read-only
    let mut bytes = fs::read(p)?;
    if let Some(b) = bytes.last_mut() {
        *b ^= 1;
    }
    Ok(fs::write(p, bytes)?)
}

fn err_text<T>(r: ownhealth_core::Result<T>) -> String {
    r.err().map(|e| e.to_string()).unwrap_or_default()
}

fn manifest(dir: &Path) -> anyhow::Result<Value> {
    Ok(serde_json::from_slice(&fs::read(dir.join("manifest.json"))?)?)
}

fn write_manifest(dir: &Path, m: &Value) -> R {
    Ok(fs::write(dir.join("manifest.json"), serde_json::to_vec(m)?)?)
}

fn copy_dir(src: &Path, dst: &Path) -> R {
    fs::create_dir_all(dst)?;
    for e in fs::read_dir(src)? {
        let e = e?;
        let to = dst.join(e.file_name());
        if e.path().is_dir() {
            copy_dir(&e.path(), &to)?;
        } else {
            fs::copy(e.path(), to)?;
        }
    }
    Ok(())
}

fn copy(l: &Live) -> anyhow::Result<PathBuf> {
    let dst = l.dir.path().join("copy");
    copy_dir(&l.archive, &dst)?;
    Ok(dst)
}

fn files(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Ok(rd) = fs::read_dir(dir) {
        for e in rd.flatten() {
            if e.path().is_dir() {
                out.extend(files(&e.path()));
            } else {
                out.push(e.path());
            }
        }
    }
    out.sort();
    out
}

fn name(p: &Path) -> String {
    p.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

fn make_writable(p: &Path) -> R {
    use std::os::unix::fs::PermissionsExt;
    Ok(fs::set_permissions(p, fs::Permissions::from_mode(0o644))?)
}

fn strs(v: &Value) -> Vec<String> {
    v.as_array()
        .into_iter()
        .flatten()
        .filter_map(|s| s.as_str().map(str::to_string))
        .collect()
}

async fn public_tables(db: &TestDb) -> anyhow::Result<i64> {
    Ok(
        sqlx::query_scalar("select count(*) from pg_tables where schemaname = 'public'")
            .fetch_one(&db.pool)
            .await?,
    )
}

#[tokio::test]
async fn backup_writes_dump_blobs_and_a_manifest_with_every_profile() -> R {
    let l = live().await?;
    let m = manifest(&l.archive)?;
    assert_eq!(m["schema"], "ownhealth.backup/v1");
    assert!(fs::metadata(l.archive.join("db.dump"))?.len() > 0);
    let profiles: Vec<String> = sqlx::query_scalar("select id::text from profile order by 1")
        .fetch_all(&l.db.pool)
        .await?;
    let observations: i64 = sqlx::query_scalar("select count(*) from observation")
        .fetch_one(&l.db.pool)
        .await?;
    let snapshots: i64 = sqlx::query_scalar("select count(*) from state_snapshot")
        .fetch_one(&l.db.pool)
        .await?;
    let hashed: Vec<String> = m["state_hash"]
        .as_object()
        .map(|o| o.keys().cloned().collect())
        .unwrap_or_default();
    assert_eq!(hashed, profiles);
    assert_eq!(profiles.len(), 4);
    assert!(
        m["state_hash"]
            .as_object()
            .into_iter()
            .flatten()
            .all(|(_, h)| h.as_str().is_some_and(|h| h.starts_with("sha256:")))
    );
    assert!(observations > 0 && m["tables"]["observation"] == observations);
    assert!(m["blobs"]["count"].as_u64().unwrap_or(0) > 0);
    let (listed, sealed) = (strs(&m["blobs"]["sha256"]), sealed_keys(&m));
    assert!(
        !listed.is_empty() && !sealed.is_empty(),
        "not a mixed tree: {:#}",
        m["blobs"]
    );
    assert_eq!(m["blobs"]["count"].as_u64(), Some((listed.len() + sealed.len()) as u64));
    // build_state ran inside the backup's rolled-back transaction, so the live database is unchanged.
    assert_eq!(m["tables"]["state_snapshot"], snapshots);
    // Python's json.dumps(indent=2, sort_keys=True) + "\n"
    let text = fs::read_to_string(l.archive.join("manifest.json"))?;
    assert!(text.ends_with("}\n") && text.starts_with("{\n  \"app_version\": "));
    // the blobs rows reference are all in it, Alex's plain report under its sha256
    let referenced = strs(&m["blobs"]["referenced"]);
    assert!(!referenced.is_empty() && referenced.iter().all(|r| listed.contains(r) || sealed.contains(r)));
    let legacy = hex::encode(sha2::Sha256::digest(&l.legacy));
    assert!(referenced.contains(&legacy) && listed.contains(&legacy));
    // a sealed file is listed with the sha256 of its bytes, and named by an address, not its hash
    let files = tree(&l.archive)?;
    let shas: Vec<String> = sqlx::query_scalar("select encode(blob_sha256, 'hex') from document")
        .fetch_all(&l.db.pool)
        .await?;
    for key in &sealed {
        assert_eq!(
            m["blobs"]["sealed"][key],
            hex::encode(sha2::Sha256::digest(fs::read(&files[key])?))
        );
        assert!(
            shas.iter().all(|h| !key.contains(h.as_str())),
            "{key} names its plaintext hash"
        );
    }
    // the key's fingerprint, never the key
    let keys: i64 = sqlx::query_scalar("select count(*) from profile_key")
        .fetch_one(&l.db.pool)
        .await?;
    assert_eq!(m["keys"]["profiles"], keys);
    assert_eq!(m["keys"]["kek_fingerprint"].as_str().map(str::len), Some(16));
    let secret = l.settings.secrets_key.clone().unwrap_or_default();
    assert!(!secret.is_empty() && !text.contains(&secret));
    Ok(())
}

#[tokio::test]
async fn backup_refuses_a_directory_that_is_not_empty() -> R {
    let dir = tempfile::tempdir()?;
    fs::write(dir.path().join("x"), "old")?;
    let settings = Settings::default();
    let err = ownhealth_ops::backup(&settings, dir.path())
        .await
        .err()
        .map(|e| e.to_string())
        .unwrap_or_default();
    assert!(err.contains("not empty"), "{err}");
    Ok(())
}

#[tokio::test]
async fn restore_into_a_fresh_instance_gives_the_same_state_hash_for_every_profile() -> R {
    let l = live().await?;
    let (db, fresh) = fresh(&l).await?;
    let report = ownhealth_ops::restore(&fresh, &l.archive).await?;
    assert_eq!(report["ok"], true, "{report:#}");
    let keys: i64 = sqlx::query_scalar("select count(*) from profile_key")
        .fetch_one(&db.pool)
        .await?;
    assert!(
        keys > 0 && report["keys"]["checked"] == keys && report["keys"]["ok"] == true,
        "{report:#}"
    );
    let profiles = report["profiles"].as_object().cloned().unwrap_or_default();
    assert_eq!(profiles.len(), 4);
    assert!(profiles.values().all(|p| p["actual"] == p["expected"]));
    let m = manifest(&l.archive)?;
    let restored: Vec<String> = tree_files(&fresh.blobs_path())?.into_keys().collect();
    let mut listed = strs(&m["blobs"]["sha256"]);
    listed.extend(sealed_keys(&m));
    listed.sort();
    assert_eq!(restored, listed);
    // every document's file opens on the restored instance, sealed or plain, with the same bytes
    let store = BlobStore::for_settings(&fresh)?;
    let live_store = BlobStore::for_settings(&l.settings)?;
    let docs: Vec<(Uuid, String)> =
        sqlx::query_as("select profile_id, encode(blob_sha256, 'hex') from document where profile_id is not null")
            .fetch_all(&db.pool)
            .await?;
    assert!(docs.len() > 1);
    for (pid, sha) in &docs {
        let owner = BlobOwner::Profile(*pid);
        assert_eq!(store.get_bytes(owner, sha)?, live_store.get_bytes(owner, sha)?, "{sha}");
    }
    let legacy = hex::encode(sha2::Sha256::digest(&l.legacy));
    assert_eq!(store.get_bytes(BlobOwner::Profile(l.alex), &legacy)?, l.legacy);
    // the restored schema is the app's schema; nothing to migrate
    let mut c = db.pool.acquire().await?;
    assert!(migrate::pending(&mut c, &migrate::embedded()?).await?.is_empty());
    Ok(())
}

#[tokio::test]
async fn restore_refuses_a_database_that_is_not_empty() -> R {
    let l = live().await?;
    let err = ownhealth_ops::restore(&l.settings, &l.archive)
        .await
        .err()
        .map(|e| e.to_string())
        .unwrap_or_default();
    assert!(err.contains("not empty"), "{err}");
    Ok(())
}

#[tokio::test]
async fn verify_restore_passes_and_drops_its_scratch_database() -> R {
    let l = live().await?;
    let report = ownhealth_ops::verify_restore(&l.settings, &l.archive).await?;
    assert_eq!(report["ok"], true, "{report:#}");
    assert!(strs(&report["blobs"]["missing"]).is_empty() && strs(&report["blobs"]["corrupt"]).is_empty());
    let left: Option<i32> = sqlx::query_scalar("select 1 from pg_database where datname = $1")
        .bind(format!("{}_restore_check", l.db.name))
        .fetch_optional(&l.db.pool)
        .await?;
    assert!(left.is_none());
    Ok(())
}

#[tokio::test]
async fn verify_restore_fails_on_a_corrupt_blob() -> R {
    let l = live().await?;
    let bad = copy(&l)?;
    let victim = files(&bad.join("blobs").join("sha256")).remove(0);
    make_writable(&victim)?; // blobs are stored read-only
    let mut bytes = fs::read(&victim)?;
    bytes.push(b'x');
    fs::write(&victim, bytes)?;
    let report = ownhealth_ops::verify_restore(&l.settings, &bad).await?;
    assert_eq!(report["ok"], false);
    assert_eq!(strs(&report["blobs"]["corrupt"]), [name(&victim)]);
    Ok(())
}

#[tokio::test]
async fn verify_restore_fails_on_a_changed_missing_or_added_sealed_file() -> R {
    let l = live().await?;
    let m = manifest(&l.archive)?;
    let sealed = sealed_keys(&m);
    let (changed, gone) = (&sealed[0], &sealed[1]);
    let bad = copy(&l)?;
    let files = tree(&bad)?;
    flip_last_byte(&files[changed])?;
    fs::remove_file(&files[gone])?;
    // a file no manifest lists, dropped into a profile's folder
    let added = files[changed].with_file_name("f".repeat(64));
    fs::write(&added, b"OHB\x01 planted")?;
    let planted = format!("{}/{}", changed.split('/').next().unwrap_or_default(), "f".repeat(64));
    let report = ownhealth_ops::verify_restore(&l.settings, &bad).await?;
    assert_eq!(report["ok"], false);
    let mut corrupt = vec![changed.clone(), planted];
    corrupt.sort();
    assert_eq!(strs(&report["blobs"]["corrupt"]), corrupt, "{report:#}");
    assert_eq!(strs(&report["blobs"]["missing"]), std::slice::from_ref(gone));
    Ok(())
}

#[tokio::test]
async fn restore_refuses_another_key_or_none_before_it_loads_anything() -> R {
    let l = live().await?;
    let (db, mut other) = fresh(&l).await?;
    other.secrets_key = Some(ownhealth_core::testing::random_secrets_key());
    assert_ne!(other.secrets_key, l.settings.secrets_key);
    let err = err_text(ownhealth_ops::restore(&other, &l.archive).await);
    assert!(err.contains("another OWNHEALTH_SECRETS_KEY"), "{err}");
    let err = err_text(ownhealth_ops::verify_restore(&other, &l.archive).await);
    assert!(err.contains("another OWNHEALTH_SECRETS_KEY"), "{err}");
    let secret = l.settings.secrets_key.clone().unwrap_or_default();
    assert!(!err.contains(&secret));
    other.secrets_key = None;
    let err = err_text(ownhealth_ops::restore(&other, &l.archive).await);
    assert!(
        err.contains("OWNHEALTH_SECRETS_KEY is not set") && !err.contains('\n'),
        "{err}"
    );
    assert_eq!(public_tables(&db).await?, 0);
    assert!(tree_files(&other.blobs_path())?.is_empty());
    Ok(())
}

#[tokio::test]
async fn restore_refuses_a_sealed_file_its_manifest_does_not_list() -> R {
    let l = live().await?;
    let bad = copy(&l)?;
    let mut m = manifest(&bad)?;
    let victim = sealed_keys(&m).remove(0);
    if let Some(o) = m["blobs"]["sealed"].as_object_mut() {
        o.shift_remove(&victim);
    }
    write_manifest(&bad, &m)?;
    let (db, fresh) = fresh(&l).await?;
    let err = err_text(ownhealth_ops::restore(&fresh, &bad).await);
    assert!(
        err.contains(&format!("blob {victim} in the backup is corrupt")),
        "{err}"
    );
    assert_eq!(public_tables(&db).await?, 0);
    Ok(())
}

#[tokio::test]
async fn verify_restore_fails_on_a_missing_blob() -> R {
    let l = live().await?;
    let bad = copy(&l)?;
    let victim = files(&bad.join("blobs").join("sha256")).remove(0);
    fs::remove_file(&victim)?;
    let report = ownhealth_ops::verify_restore(&l.settings, &bad).await?;
    assert_eq!(report["ok"], false);
    assert_eq!(strs(&report["blobs"]["missing"]), [name(&victim)]);
    Ok(())
}

#[tokio::test]
async fn verify_restore_fails_when_a_state_hash_or_a_row_count_differs() -> R {
    let l = live().await?;
    let bad = copy(&l)?;
    let mut m = manifest(&bad)?;
    let pid = m["state_hash"]
        .as_object()
        .and_then(|o| o.keys().next().cloned())
        .unwrap_or_default();
    m["state_hash"][&pid] = Value::String(format!("sha256:{}", "0".repeat(64)));
    write_manifest(&bad, &m)?;
    let report = ownhealth_ops::verify_restore(&l.settings, &bad).await?;
    assert_eq!(report["ok"], false);
    assert_eq!(report["profiles"][&pid]["ok"], false);
    let failed = report["profiles"]
        .as_object()
        .into_iter()
        .flatten()
        .filter(|(_, p)| p["ok"] != true)
        .count();
    assert_eq!(failed, 1);

    let mut m = manifest(&l.archive)?;
    m["tables"]["observation"] = Value::from(m["tables"]["observation"].as_i64().unwrap_or(0) + 1);
    write_manifest(&bad, &m)?;
    let report = ownhealth_ops::verify_restore(&l.settings, &bad).await?;
    assert_eq!(report["ok"], false);
    assert!(report["tables"].get("observation").is_some());
    Ok(())
}

#[tokio::test]
async fn unknown_manifest_schema_is_refused() -> R {
    let l = live().await?;
    let bad = copy(&l)?;
    write_manifest(&bad, &serde_json::json!({"schema": "other/v9"}))?;
    let err = ownhealth_ops::verify_restore(&l.settings, &bad)
        .await
        .err()
        .map(|e| e.to_string())
        .unwrap_or_default();
    assert!(err.contains("unknown backup schema 'other/v9'"), "{err}");
    Ok(())
}

#[tokio::test]
async fn verify_restore_fails_on_a_referenced_blob_the_backup_never_had() -> R {
    let l = live().await?;
    let bad = copy(&l)?;
    let mut m = manifest(&bad)?;
    // one plain and one sealed, as if the live store had already lost them at backup time
    let referenced = strs(&m["blobs"]["referenced"]);
    let plain = referenced
        .iter()
        .find(|k| !k.contains('/'))
        .cloned()
        .unwrap_or_default();
    let sealed = referenced.iter().find(|k| k.contains('/')).cloned().unwrap_or_default();
    let listed: Vec<String> = strs(&m["blobs"]["sha256"])
        .into_iter()
        .filter(|s| *s != plain)
        .collect();
    m["blobs"]["sha256"] = serde_json::json!(listed);
    if let Some(o) = m["blobs"]["sealed"].as_object_mut() {
        o.shift_remove(&sealed);
    }
    write_manifest(&bad, &m)?;
    let files = tree(&bad)?;
    fs::remove_file(&files[&plain])?;
    fs::remove_file(&files[&sealed])?;
    let mut victim = vec![plain, sealed];
    victim.sort();
    let report = ownhealth_ops::verify_restore(&l.settings, &bad).await?;
    assert_eq!(report["ok"], false);
    assert_eq!(strs(&report["blobs"]["missing"]), victim);
    Ok(())
}

#[tokio::test]
async fn restore_checks_blobs_before_it_loads_the_database() -> R {
    let l = live().await?;
    let bad = copy(&l)?;
    let victim = files(&bad.join("blobs").join("sha256")).remove(0);
    make_writable(&victim)?;
    let mut bytes = fs::read(&victim)?;
    bytes.push(b'x');
    fs::write(&victim, bytes)?;
    let (db, fresh) = fresh(&l).await?;
    let err = ownhealth_ops::restore(&fresh, &bad)
        .await
        .err()
        .map(|e| e.to_string())
        .unwrap_or_default();
    assert!(err.contains("corrupt"), "{err}");
    // still empty, so a fixed backup can be restored into it
    assert_eq!(public_tables(&db).await?, 0);
    assert!(files(&fresh.blobs_path()).iter().all(|p| !name(p).ends_with(".part")));
    Ok(())
}
