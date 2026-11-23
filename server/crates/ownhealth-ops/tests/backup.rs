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
    let keys: i64 = sqlx::query_scalar("select count(*) from profile_key where wrapped_dek is not null")
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
    let keys: i64 = sqlx::query_scalar("select count(*) from profile_key where wrapped_dek is not null")
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
async fn a_tree_with_an_items_own_key_backs_up_verifies_and_restores() -> R {
    // a scan that came in with no profile yet: sealed under its own item key, in pending/<item>/
    const SCAN: &[u8] = b"%PDF-1.4 LabNorte scan for the Rivera household, no profile yet";
    let l = live().await?;
    let household: Uuid = sqlx::query_scalar("select household_id from profile where id = $1")
        .bind(l.alex)
        .fetch_one(&l.db.pool)
        .await?;
    let sha = BlobStore::new(l.dir.path().join("scratch"))?.put_bytes(BlobOwner::Reference, SCAN)?;
    let mut c = l.db.pool.acquire().await?;
    let doc = create_document(
        &mut c,
        &NewDocument::upload(household, None, &sha, "application/pdf", SCAN.len() as i64),
    )
    .await?
    .id;
    drop(c);
    let live_store = BlobStore::for_settings(&l.settings)?;
    assert_eq!(live_store.put_bytes(BlobOwner::Pending(doc), SCAN)?, sha);
    let key = live_store.key_of(BlobOwner::Pending(doc), &sha)?;
    assert!(key.starts_with(&format!("pending/{}/", doc.simple())), "{key}");
    let archive = ownhealth_ops::backup(&l.settings, &l.dir.path().join("backup2").join("staging")).await?;
    let m = manifest(&archive)?;
    assert!(sealed_keys(&m).contains(&key), "{:#}", m["blobs"]);
    assert!(strs(&m["blobs"]["referenced"]).contains(&key));
    assert!(!fs::read_to_string(archive.join("manifest.json"))?.contains(&sha));
    let report = ownhealth_ops::verify_restore(&l.settings, &archive).await?;
    assert_eq!(report["ok"], true, "{report:#}");
    let (db, fresh) = fresh(&l).await?;
    let report = ownhealth_ops::restore(&fresh, &archive).await?;
    assert_eq!(report["ok"], true, "{report:#}");
    // the item's wrapped key came back in the dump and opens under the same secrets key
    let profiles: i64 = sqlx::query_scalar("select count(*) from profile_key where wrapped_dek is not null")
        .fetch_one(&db.pool)
        .await?;
    let items: i64 = sqlx::query_scalar("select count(*) from pending_key where wrapped_dek is not null")
        .fetch_one(&db.pool)
        .await?;
    assert_eq!(items, 1);
    assert_eq!(report["keys"]["checked"], profiles + items, "{report:#}");
    assert!(tree_files(&fresh.blobs_path())?.contains_key(&key));
    let store = BlobStore::for_settings(&fresh)?;
    assert_eq!(store.get_bytes(BlobOwner::Pending(doc), &sha)?, SCAN);
    Ok(())
}

/// Files that are not where their name puts them (a blob name in another shard, a sealed name in
/// another shard, a name that is no key under `enc/` or `pending/`) stay out of the backup and its
/// manifest, so no restore places them; the backup still verifies and restores, and the live store
/// keeps its own files (the backup removes only its own links).
#[tokio::test]
async fn a_backup_leaves_misplaced_files_out_and_still_verifies_and_restores() -> R {
    let l = live().await?;
    let root = l.settings.blobs_path();
    let keys: Vec<String> = tree_files(&root)?.into_keys().collect();
    let plain = keys.iter().find(|k| !k.contains('/')).cloned().unwrap_or_default();
    let sealed = keys
        .iter()
        .find(|k| k.contains('/') && !k.starts_with("pending/") && !k.starts_with("?/"))
        .cloned()
        .unwrap_or_default();
    let (pid, address) = sealed.split_once('/').unwrap_or_default();
    assert!(!plain.is_empty() && !address.is_empty(), "{keys:?}");
    let strays = [
        root.join("sha256/zz/zz").join(&plain),
        root.join("enc").join(pid).join("zz/zz").join(address),
        root.join("enc").join("notes.txt"),
        root.join("pending").join("stray"),
    ];
    for f in &strays {
        fs::create_dir_all(f.parent().unwrap_or(&root))?;
        fs::write(f, b"planted")?;
    }
    let misplaced = tree_files(&root)?.into_keys().filter(|k| k.starts_with("?/")).count();
    assert_eq!(misplaced, strays.len());
    let archive = ownhealth_ops::backup(&l.settings, &l.dir.path().join("backup3").join("staging")).await?;
    let backed: Vec<String> = tree(&archive)?.into_keys().collect();
    assert!(!backed.iter().any(|k| k.starts_with("?/")), "{backed:?}");
    let m = manifest(&archive)?;
    assert_eq!(m["blobs"]["count"], backed.len(), "{:#}", m["blobs"]);
    assert!(!fs::read_to_string(archive.join("manifest.json"))?.contains("?/"));
    for f in &strays {
        assert_eq!(
            fs::read(f)?,
            b"planted",
            "the backup removed a live file: {}",
            f.display()
        );
    }
    let report = ownhealth_ops::verify_restore(&l.settings, &archive).await?;
    assert_eq!(report["ok"], true, "{report:#}");
    let (_db, fresh) = fresh(&l).await?;
    let report = ownhealth_ops::restore(&fresh, &archive).await?;
    assert_eq!(report["ok"], true, "{report:#}");
    let restored: Vec<String> = tree_files(&fresh.blobs_path())?.into_keys().collect();
    assert!(!restored.iter().any(|k| k.starts_with("?/")), "{restored:?}");
    assert_eq!(restored, backed);
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
    // not where its name puts it, so it is keyed by its path and never stands in for another file
    let planted = format!("?/{}", added.strip_prefix(bad.join("blobs"))?.display());
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
        err.contains("OWNHEALTH_SECRETS_KEY is not set")
            && err.contains("the key the backup was made with")
            && !err.contains("openssl rand")
            && !err.contains('\n'),
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

#[tokio::test]
async fn restore_refuses_a_listed_file_that_is_missing_before_it_loads_anything() -> R {
    let l = live().await?;
    let bad = copy(&l)?;
    let gone = sealed_keys(&manifest(&bad)?).remove(0);
    fs::remove_file(&tree(&bad)?[&gone])?;
    let (db, fresh) = fresh(&l).await?;
    let err = err_text(ownhealth_ops::restore(&fresh, &bad).await);
    assert!(
        err.contains(&format!("blob {gone}")) && err.contains("missing"),
        "{err}"
    );
    // still empty, so the backup can be restored once the file is back
    assert_eq!(public_tables(&db).await?, 0);
    assert!(tree_files(&fresh.blobs_path())?.is_empty());
    Ok(())
}

/// The manifest's key fingerprint matches, but one stored profile key does not open under it (made
/// under another key, or changed): its files are lost, so the check fails.
#[tokio::test]
async fn verify_restore_fails_when_a_profile_key_does_not_open() -> R {
    use ownhealth_core::keys::{Dek, Kek};
    let l = live().await?;
    let kek = Kek::from_settings(&l.settings)?;
    let stranger = Kek::from_base64(&ownhealth_core::testing::random_secrets_key())?;
    let p = Uuid::new_v4();
    sqlx::query("insert into profile_key (profile_id, wrapped_dek, kek_fingerprint) values ($1, $2, $3)")
        .bind(p)
        .bind(stranger.wrap(p, &Dek::generate())?)
        .bind(&kek.fingerprint()[..])
        .execute(&l.db.pool)
        .await?;
    let archive = ownhealth_ops::backup(&l.settings, &l.dir.path().join("backup").join("second")).await?;
    let report = ownhealth_ops::verify_restore(&l.settings, &archive).await?;
    assert_eq!(report["keys"]["failed"], serde_json::json!([p]), "{report:#}");
    assert_eq!(report["keys"]["ok"], false);
    assert_eq!(report["ok"], false, "{report:#}");
    Ok(())
}

/// verify-restore counts a held mail's raw message among the files the database needs, under the
/// mail's own key: a held mail whose file is gone fails the check, also when the dump still holds
/// the mail's key and when its inbox was removed. The message of a mail deleted from the hold is no
/// longer needed.
#[tokio::test]
async fn verify_restore_fails_when_a_held_mails_file_is_gone() -> R {
    use ownhealth_core::quarantine::{self, QuarantineIn};
    let l = live().await?;
    let household: Uuid = sqlx::query_scalar("select household_id from profile where id = $1")
        .bind(l.alex)
        .fetch_one(&l.db.pool)
        .await?;
    let mailbox: Uuid = sqlx::query_scalar(
        "insert into source (household_id, kind, label, config, created_by) \
         values ($1, 'email', 'Correo de casa', '{}', (select id from app_user limit 1)) returning id",
    )
    .bind(household)
    .fetch_one(&l.db.pool)
    .await?;
    let store = BlobStore::for_settings(&l.settings)?;
    let mut held = Vec::new();
    for n in 1..=2 {
        let id = Uuid::new_v4();
        let raw = format!(
            "From: promo@ofertas-salud.example\r\nSubject: held {n}\r\n\r\nheld mail {n} of the Rivera household"
        );
        let sha = store.put_bytes(BlobOwner::Pending(id), raw.as_bytes())?;
        let mut c = l.db.pool.acquire().await?;
        quarantine::hold(
            &mut c,
            &QuarantineIn {
                id,
                household_id: household,
                source_id: Some(mailbox),
                raw_sha256: &sha,
                message_id: Some(&format!("held-{n}@ofertas-salud.example")),
                sender: Some("promo@ofertas-salud.example"),
                subject: Some("held"),
                attachments: serde_json::json!([]),
                reason: "sender_not_allowed",
            },
        )
        .await?;
        sqlx::query("insert into source_item (source_id, external_id, blob_sha256, quarantine_id) values ($1, $2, decode($3, 'hex'), $4)")
            .bind(mailbox)
            .bind(format!("<held-{n}@ofertas-salud.example>"))
            .bind(&sha)
            .bind(id)
            .execute(&mut *c)
            .await?;
        held.push((id, sha));
    }
    let ((kept, kept_sha), (deleted, _)) = (held[0].clone(), held[1].clone());
    let a1 = ownhealth_ops::backup(&l.settings, &l.dir.path().join("b1").join("staging")).await?;
    let key = store.key_of(BlobOwner::Pending(kept), &kept_sha)?;
    assert!(key.starts_with("pending/"), "{key}");
    assert!(
        strs(&manifest(&a1)?["blobs"]["referenced"]).contains(&key),
        "the held mail's file is not among the files the database needs"
    );
    // the second mail is deleted from the hold (its key shredded) and its folder swept
    let user: Uuid = sqlx::query_scalar("select id from app_user limit 1")
        .fetch_one(&l.db.pool)
        .await?;
    let mut tx = l.db.pool.begin().await?;
    quarantine::mark_deleted(&mut tx, deleted, user).await?;
    tx.commit().await?;
    assert!(store.forget_pending(deleted)?);
    let a2 = ownhealth_ops::backup(&l.settings, &l.dir.path().join("b2").join("staging")).await?;
    let report = ownhealth_ops::verify_restore(&l.settings, &a2).await?;
    assert_eq!(
        report["ok"], true,
        "a deleted mail's message counted as needed: {report:#}"
    );
    // the inbox is removed: its items go with it (on delete cascade) and the held mail stays with no
    // inbox, so only the hold itself names the file
    sqlx::query("delete from source where id = $1")
        .bind(mailbox)
        .execute(&l.db.pool)
        .await?;
    let items: i64 = sqlx::query_scalar("select count(*) from source_item where quarantine_id = $1")
        .bind(kept)
        .fetch_one(&l.db.pool)
        .await?;
    assert_eq!(items, 0);
    // the held mail's file is lost from the store
    assert!(store.forget_pending(kept)?);
    let a3 = ownhealth_ops::backup(&l.settings, &l.dir.path().join("b3").join("staging")).await?;
    let report = ownhealth_ops::verify_restore(&l.settings, &a3).await?;
    assert_eq!(report["ok"], false, "{report:#}");
    assert_eq!(
        report["blobs"]["missing"].as_array().map(Vec::len),
        Some(1),
        "{report:#}"
    );
    Ok(())
}
