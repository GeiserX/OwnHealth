//! Backup, restore and verify-restore (`tests/ops/test_backup_restore.py`).
//!
//! pg_dump and pg_restore 17 must be on PATH or in OWNHEALTH_PG_BINDIR. Without a local client, point
//! it at the wrapper that runs them in the test container:
//! `OWNHEALTH_PG_BINDIR=$PWD/../scripts/pg-bin cargo test -p ownhealth-ops`.
//!
//! The live store is mixed, as an upgraded install is: the demo's files are sealed under each
//! profile's key, and one report of Alex's sits in the plain layout of a 0.1.1 install. Each TestDb
//! makes its own OWNHEALTH_SECRETS_KEY at run time; a fresh stack takes the live one unless a test
//! wants another, and reads the live keyring folder, as a restore on the same host does.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use ownhealth_core::blobs::{BlobOwner, BlobStore, read_restore_losses, tree_files};
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

/// An empty database (no migrations) and an empty blob directory: a fresh stack, on the live keyring.
async fn fresh(l: &Live) -> anyhow::Result<(TestDb, Settings)> {
    let db = TestDb::empty().await?;
    let mut settings = db.settings(&l.dir.path().join("fresh"))?;
    settings.secrets_key = l.settings.secrets_key.clone();
    settings.keyring_dir = Some(l.settings.keyring_path());
    Ok((db, settings))
}

/// The table of contents of a dump, as `pg_restore --list` prints it.
fn dump_toc(dump: &Path) -> anyhow::Result<String> {
    let tool = std::env::var_os("OWNHEALTH_PG_BINDIR")
        .filter(|d| !d.is_empty())
        .map_or_else(|| PathBuf::from("pg_restore"), |d| PathBuf::from(d).join("pg_restore"));
    let out = std::process::Command::new(tool)
        .arg("--list")
        .stdin(fs::File::open(dump)?)
        .output()?;
    anyhow::ensure!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    Ok(String::from_utf8(out.stdout)?)
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
async fn verify_restore_passes_with_no_blob_errors() -> R {
    let l = live().await?;
    let report = ownhealth_ops::verify_restore(&l.settings, &l.archive).await?;
    assert_eq!(report["ok"], true, "{report:#}");
    assert!(strs(&report["blobs"]["missing"]).is_empty() && strs(&report["blobs"]["corrupt"]).is_empty());
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

/// A backup must be able to identify each profile key for the manifest.
#[tokio::test]
async fn backup_refuses_a_profile_key_it_cannot_identify() -> R {
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
    let dest = l.dir.path().join("backup").join("second");
    let err = err_text(ownhealth_ops::backup(&l.settings, &dest).await);
    assert!(err.contains(&p.to_string()) && err.contains("does not open"), "{err}");
    assert!(fs::read_dir(dest)?.next().is_none());
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
    // Pending keys travel in the dump, so the held mail can open without a profile keyring.
    let (_db, mut restored) = fresh(&l).await?;
    restored.keyring_dir = Some(l.dir.path().join("missing-profile-keyring"));
    ownhealth_ops::restore_with(&restored, &a1, true).await?;
    assert_eq!(
        BlobStore::for_settings(&restored)?.get_bytes(BlobOwner::Pending(kept), &kept_sha)?,
        store.get_bytes(BlobOwner::Pending(kept), &kept_sha)?
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

/// The keys never enter a backup (slice 47): the dump leaves out the rows of `profile_key` (its
/// pending-item keys stay), the backup writes every key into keyring.json in the keyring folder
/// instead, and the manifest lists the profiles whose key a restore needs from it.
#[tokio::test]
async fn the_dump_holds_no_profile_key_and_the_keyring_holds_every_one() -> R {
    let l = live().await?;
    let toc = dump_toc(&l.archive.join("db.dump"))?;
    let data = |t: &str| {
        toc.lines()
            .any(|line| line.contains(&format!("TABLE DATA public {t} ")))
    };
    assert!(data("profile") && data("pending_key"), "{toc}");
    assert!(!data("profile_key"), "the dump holds the profile keys:\n{toc}");
    assert!(
        toc.contains("TABLE public profile_key "),
        "the dump lost the table itself:\n{toc}"
    );
    let stored: Vec<(Uuid, Vec<u8>)> = sqlx::query_as(
        "select profile_id, wrapped_dek from profile_key where wrapped_dek is not null order by profile_id",
    )
    .fetch_all(&l.db.pool)
    .await?;
    assert_eq!(stored.len(), 4);
    let ring = ownhealth_core::keyfile::read(&ownhealth_core::keyfile::path(&l.settings))?
        .ok_or_else(|| anyhow::anyhow!("the backup wrote no keyring"))?;
    assert_eq!(ring.profiles.into_iter().collect::<Vec<_>>(), stored);
    let m = manifest(&l.archive)?;
    let mut listed = strs(&m["keys"]["keyring"]);
    listed.sort();
    let ids: Vec<String> = stored.iter().map(|(p, _)| p.to_string()).collect();
    assert_eq!(listed, ids);
    assert!(m["tables"].get("profile_key").is_none(), "{:#}", m["tables"]);
    // nothing of the keyring is in the backup folder
    assert!(files(&l.archive).iter().all(|f| name(f) != "keyring.json"));
    Ok(())
}

/// A backup refuses a keyring it would carry (in the backup folder or the blob folder) and a keyring
/// it cannot read, before it writes anything.
#[tokio::test]
async fn a_backup_refuses_a_keyring_it_would_carry_or_cannot_read() -> R {
    let l = live().await?;
    let dest = l.dir.path().join("b2").join("staging");
    let mut s = l.settings.clone();
    s.keyring_dir = Some(dest.join("keys"));
    let err = err_text(ownhealth_ops::backup(&s, &dest).await);
    assert!(err.contains("inside the backup folder"), "{err}");
    s.keyring_dir = Some(s.blobs_path().join("keys"));
    let err = err_text(ownhealth_ops::backup(&s, &dest).await);
    assert!(err.contains("inside the blob folder"), "{err}");
    assert!(!dest.join("manifest.json").exists());
    let file = ownhealth_core::keyfile::path(&l.settings);
    fs::write(&file, b"{")?;
    let err = err_text(ownhealth_ops::backup(&l.settings, &dest).await);
    assert!(err.contains("cannot read the keyring"), "{err}");
    assert!(!dest.exists(), "the backup wrote into its folder before it refused");
    assert_eq!(fs::read(&file)?, b"{");
    Ok(())
}

/// A restore with no keyring fails loudly before it loads anything, and so does verify-restore: the
/// backup alone opens no file.
#[tokio::test]
async fn a_restore_with_no_keyring_fails_before_it_loads_anything() -> R {
    let l = live().await?;
    let (db, mut fresh) = fresh(&l).await?;
    fresh.keyring_dir = Some(l.dir.path().join("empty-keyring"));
    let err = err_text(ownhealth_ops::restore(&fresh, &l.archive).await);
    assert!(
        err.contains("no keyring at") && err.contains("OWNHEALTH_KEYRING_DIR") && err.contains("nothing was loaded"),
        "{err}"
    );
    assert_eq!(public_tables(&db).await?, 0);
    assert!(tree_files(&fresh.blobs_path())?.is_empty());
    let err = err_text(ownhealth_ops::verify_restore(&fresh, &l.archive).await);
    assert!(err.contains("no keyring at"), "{err}");
    Ok(())
}

/// A keyring that lacks a key of the backup without listing it as deleted (an older copy, another
/// install's) is refused by restore before it loads anything, and fails verify-restore by name.
#[tokio::test]
async fn a_keyring_missing_a_key_it_does_not_list_as_deleted_fails_the_restore() -> R {
    let l = live().await?;
    let file = ownhealth_core::keyfile::path(&l.settings);
    let mut ring = ownhealth_core::keyfile::read(&file)?.unwrap_or_default();
    assert!(ring.profiles.remove(&l.alex).is_some());
    ownhealth_core::keyfile::write(&l.settings.keyring_path(), &ring)?;
    let (db, fresh) = fresh(&l).await?;
    let err = err_text(ownhealth_ops::restore(&fresh, &l.archive).await);
    assert!(
        err.contains("holds no key for 1 profile") && err.contains(&l.alex.to_string()),
        "{err}"
    );
    assert_eq!(public_tables(&db).await?, 0);
    let report = ownhealth_ops::verify_restore(&l.settings, &l.archive).await?;
    assert_eq!(report["keys"]["missing"], serde_json::json!([l.alex]), "{report:#}");
    assert_eq!(report["ok"], false, "{report:#}");
    Ok(())
}

/// A keyring with the backup's profiles but made under another secrets key (another install's file
/// put in place) is refused by name before anything is loaded, by restore and verify-restore alike.
#[tokio::test]
async fn a_keyring_of_another_secrets_key_is_refused_before_anything_loads() -> R {
    let l = live().await?;
    let file = ownhealth_core::keyfile::path(&l.settings);
    let mut ring = ownhealth_core::keyfile::read(&file)?.unwrap_or_default();
    assert!(ring.profiles.contains_key(&l.alex));
    let other = ownhealth_core::keys::Kek::from_base64(&ownhealth_core::testing::random_secrets_key())?;
    ring.kek_fingerprint = Some(other.fingerprint().to_vec());
    ownhealth_core::keyfile::write(&l.settings.keyring_path(), &ring)?;
    let (db, fresh) = fresh(&l).await?;
    let err = err_text(ownhealth_ops::restore(&fresh, &l.archive).await);
    assert!(
        err.contains("was made under another OWNHEALTH_SECRETS_KEY") && err.contains("nothing was loaded"),
        "{err}"
    );
    assert_eq!(public_tables(&db).await?, 0);
    assert!(tree_files(&fresh.blobs_path())?.is_empty());
    let err = err_text(ownhealth_ops::verify_restore(&fresh, &l.archive).await);
    assert!(err.contains("was made under another OWNHEALTH_SECRETS_KEY"), "{err}");
    // the same refusal when the restore may go on without the keys it lacks: a foreign file is
    // not a lost one
    let err = err_text(ownhealth_ops::restore_with(&fresh, &l.archive, true).await);
    assert!(err.contains("was made under another OWNHEALTH_SECRETS_KEY"), "{err}");
    assert_eq!(public_tables(&db).await?, 0);
    Ok(())
}

/// A profile deleted before the backup is not in the dump, but the keyring still lists it as
/// shredded. The restore puts its shredded row back, so no new key can ever be made for it.
#[tokio::test]
async fn a_restore_keeps_the_shredded_row_of_a_profile_deleted_before_the_backup() -> R {
    let l = live().await?;
    let gone = Uuid::new_v4();
    let file = ownhealth_core::keyfile::path(&l.settings);
    let mut ring = ownhealth_core::keyfile::read(&file)?.unwrap_or_default();
    ring.shredded.insert(gone);
    ownhealth_core::keyfile::write(&l.settings.keyring_path(), &ring)?;
    let (db, fresh) = fresh(&l).await?;
    let report = ownhealth_ops::restore(&fresh, &l.archive).await?;
    assert_eq!(report["ok"], true, "{report:#}");
    let row: Option<bool> = sqlx::query_scalar("select wrapped_dek is null from profile_key where profile_id = $1")
        .bind(gone)
        .fetch_optional(&db.pool)
        .await?;
    assert_eq!(
        row,
        Some(true),
        "no shredded row for the profile deleted before the backup"
    );
    let keys = ownhealth_core::keys::Keyring::new(ownhealth_core::keys::Kek::from_settings(&fresh)?, db.url.clone());
    assert!(
        keys.dek(gone, true).is_err(),
        "a new key was made for a deleted profile"
    );
    Ok(())
}

/// A backup made before any profile had a key lists no key in `keys.keyring`: it opens no sealed
/// file, so a new machine with no keyring restores it.
#[tokio::test]
async fn a_backup_that_needs_no_key_restores_without_a_keyring() -> R {
    ownhealth_ops::register();
    let db = TestDb::new().await?;
    let dir = tempfile::tempdir()?;
    let settings = db.settings(&dir.path().join("live"))?;
    let archive = ownhealth_ops::backup(&settings, &dir.path().join("backup").join("staging")).await?;
    assert_eq!(manifest(&archive)?["keys"]["keyring"], serde_json::json!([]));
    let target = TestDb::empty().await?;
    let mut fresh = target.settings(&dir.path().join("fresh"))?;
    fresh.secrets_key = settings.secrets_key.clone();
    fresh.keyring_dir = Some(dir.path().join("no-keyring-here"));
    let report = ownhealth_ops::restore(&fresh, &archive).await?;
    assert_eq!(report["ok"], true, "{report:#}");
    assert!(public_tables(&target).await? > 0);

    let blocked = dir.path().join("blocked-keyring");
    fs::create_dir_all(blocked.join(".keyring.lock"))?;
    let target = TestDb::empty().await?;
    let mut fresh = target.settings(&dir.path().join("blocked-restore"))?;
    fresh.secrets_key = settings.secrets_key.clone();
    fresh.keyring_dir = Some(blocked);
    for result in [
        ownhealth_ops::verify_restore(&fresh, &archive).await,
        ownhealth_ops::restore(&fresh, &archive).await,
    ] {
        assert!(err_text(result).contains("keyring lock"));
    }
    assert_eq!(public_tables(&target).await?, 0);
    Ok(())
}

/// The keyring copy is older than the backup (or gone with the disk). Told to go on without the
/// keys it lacks, the restore loads every record and every file, and only the profiles whose key the
/// keyring holds open their files. The report names the others under `keys.missing`. Their records
/// are back and no shredded row stops them from getting a new key.
#[tokio::test]
async fn a_restore_allowed_to_miss_keys_loads_every_record_and_opens_what_it_can() -> R {
    let l = live().await?;
    let file = ownhealth_core::keyfile::path(&l.settings);
    let mut ring = ownhealth_core::keyfile::read(&file)?.unwrap_or_default();
    // the copy lacks the key of a profile made after it was taken: here anyone but Alex
    let lost = ring
        .profiles
        .keys()
        .copied()
        .find(|p| *p != l.alex)
        .ok_or_else(|| anyhow::anyhow!("the demo has one profile"))?;
    ring.profiles.remove(&lost);
    let kept = ring.profiles.len();
    ownhealth_core::keyfile::write(&l.settings.keyring_path(), &ring)?;
    let (db, fresh) = fresh(&l).await?;
    let err = err_text(ownhealth_ops::restore(&fresh, &l.archive).await);
    assert!(err.contains("--allow-missing-keys"), "{err}");
    assert_eq!(public_tables(&db).await?, 0);
    let report = ownhealth_ops::restore_with(&fresh, &l.archive, true).await?;
    assert_eq!(report["ok"], true, "{report:#}");
    assert_eq!(report["keys"]["missing"], serde_json::json!([lost]), "{report:#}");
    let row: Option<Option<Vec<u8>>> = sqlx::query_scalar("select wrapped_dek from profile_key where profile_id = $1")
        .bind(lost)
        .fetch_optional(&db.pool)
        .await?;
    assert_eq!(row, None, "the restore made up a row for a key it does not have");
    let profile: i64 = sqlx::query_scalar("select count(*) from profile where id = $1")
        .bind(lost)
        .fetch_one(&db.pool)
        .await?;
    assert_eq!(profile, 1, "the records of the profile with no key did not come back");
    // Alex's key is in the copy: every file of his opens, sealed or plain
    let store = BlobStore::for_settings(&fresh)?;
    let live_store = BlobStore::for_settings(&l.settings)?;
    let docs: Vec<String> = sqlx::query_scalar("select encode(blob_sha256, 'hex') from document where profile_id = $1")
        .bind(l.alex)
        .fetch_all(&db.pool)
        .await?;
    assert!(docs.len() > 1);
    for sha in &docs {
        let owner = BlobOwner::Profile(l.alex);
        assert_eq!(store.get_bytes(owner, sha)?, live_store.get_bytes(owner, sha)?, "{sha}");
    }
    // and with no keyring at all, the records still come back, with no key of any profile
    let db2 = TestDb::empty().await?;
    let mut bare = db2.settings(&l.dir.path().join("fresh-bare"))?;
    bare.secrets_key = l.settings.secrets_key.clone();
    bare.keyring_dir = Some(l.dir.path().join("lost-with-the-disk"));
    let report = ownhealth_ops::restore_with(&bare, &l.archive, true).await?;
    assert_eq!(report["ok"], true, "{report:#}");
    assert_eq!(strs(&report["keys"]["missing"]).len(), kept + 1, "{report:#}");
    let keyed: i64 = sqlx::query_scalar("select count(*) from profile_key")
        .fetch_one(&db2.pool)
        .await?;
    assert_eq!(keyed, 0);
    assert!(public_tables(&db2).await? > 0);
    Ok(())
}

#[tokio::test]
async fn a_foreign_keyring_leaves_the_backup_destination_ready_for_retry() -> R {
    use ownhealth_core::{keyfile, keys::Kek, testing::random_secrets_key};

    let l = live().await?;
    let original = keyfile::read(&keyfile::path(&l.settings))?.unwrap_or_default();
    let mut ring = original.clone();
    ring.kek_fingerprint = Some(Kek::from_base64(&random_secrets_key())?.fingerprint().to_vec());
    keyfile::write(&l.settings.keyring_path(), &ring)?;
    let dest = l.dir.path().join("retry");
    fs::create_dir(&dest)?;
    let err = err_text(ownhealth_ops::backup(&l.settings, &dest).await);
    assert!(err.contains("another OWNHEALTH_SECRETS_KEY"), "{err}");
    assert!(
        fs::read_dir(&dest)?.next().is_none(),
        "backup left files before refusing"
    );
    keyfile::write(&l.settings.keyring_path(), &original)?;
    ownhealth_ops::backup(&l.settings, &dest).await?;
    Ok(())
}

#[tokio::test]
async fn a_backup_cannot_carry_a_keyring_reached_through_a_parent_directory() -> R {
    let l = live().await?;
    let dest = l.dir.path().join("retry");
    fs::create_dir(&dest)?;
    fs::create_dir(l.dir.path().join("other"))?;
    let mut settings = l.settings.clone();
    settings.keyring_dir = Some(l.dir.path().join("other/../retry/keys"));
    assert!(ownhealth_ops::backup(&settings, &dest).await.is_err());
    assert!(fs::read_dir(dest)?.next().is_none());
    Ok(())
}

#[tokio::test]
async fn an_archives_profile_key_must_match_its_files() -> R {
    use ownhealth_core::{
        keyfile,
        keys::{Dek, Kek},
    };

    let l = live().await?;
    let mut ring = keyfile::read(&keyfile::path(&l.settings))?.unwrap_or_default();
    let kek = Kek::from_settings(&l.settings)?;
    let dek = kek.unwrap(l.alex, &kek.fingerprint(), &ring.profiles[&l.alex])?;
    ring.profiles.insert(l.alex, kek.wrap(l.alex, &dek)?);
    keyfile::write(&l.settings.keyring_path(), &ring)?;
    assert_eq!(
        ownhealth_ops::verify_restore(&l.settings, &l.archive).await?["ok"],
        true
    );
    ring.profiles.insert(l.alex, kek.wrap(l.alex, &Dek::generate())?);
    keyfile::write(&l.settings.keyring_path(), &ring)?;
    let (db, fresh) = fresh(&l).await?;
    let err = err_text(ownhealth_ops::restore(&fresh, &l.archive).await);
    assert!(
        err.contains("--allow-missing-keys") && err.contains(&l.alex.to_string()),
        "{err}"
    );
    assert_eq!(public_tables(&db).await?, 0);
    let report = ownhealth_ops::restore_with(&fresh, &l.archive, true).await?;
    assert_eq!(report["keys"]["missing"], serde_json::json!([l.alex]), "{report:#}");
    // the profile keeps the keyring's key, never the archive's: the archived files stay closed
    let kept: Option<Vec<u8>> = sqlx::query_scalar("select wrapped_dek from profile_key where profile_id = $1")
        .bind(l.alex)
        .fetch_optional(&db.pool)
        .await?
        .flatten();
    assert_eq!(kept.as_ref(), ring.profiles.get(&l.alex));

    Ok(())
}

#[tokio::test]
async fn a_backup_after_a_partial_restore_survives_the_unkeyed_blob_sweep() -> R {
    use ownhealth_core::keyfile;

    let l = live().await?;
    let mut ring = keyfile::read(&keyfile::path(&l.settings))?.unwrap_or_default();
    assert!(ring.profiles.remove(&l.alex).is_some());
    keyfile::write(&l.settings.keyring_path(), &ring)?;
    let (db, fresh) = fresh(&l).await?;
    ownhealth_ops::restore_with(&fresh, &l.archive, true).await?;
    let store = BlobStore::for_settings(&fresh)?;
    let mut conn = db.pool.acquire().await?;
    assert!(store.sweep_unkeyed(&mut conn).await? > 0);
    drop(conn);

    let new_backup = l.dir.path().join("after-partial");
    ownhealth_ops::backup(&fresh, &new_backup).await?;
    let target = TestDb::empty().await?;
    let mut target_settings = target.settings(&l.dir.path().join("target"))?;
    target_settings.secrets_key = fresh.secrets_key.clone();
    target_settings.keyring_dir = fresh.keyring_dir.clone();
    let report = ownhealth_ops::restore(&target_settings, &new_backup).await?;
    assert_eq!(report["ok"], true, "{report:#}");
    let legacy_sha = hex::encode(sha2::Sha256::digest(&l.legacy));
    assert!(strs(&manifest(&new_backup)?["blobs"]["referenced"]).contains(&legacy_sha));
    assert_eq!(
        BlobStore::for_settings(&target_settings)?.get_bytes(BlobOwner::Profile(l.alex), &legacy_sha)?,
        l.legacy
    );
    Ok(())
}

#[tokio::test]
async fn a_backup_requires_a_repaired_file_if_it_goes_missing_again() -> R {
    use ownhealth_core::keyfile;

    let l = live().await?;
    let owner = BlobOwner::Profile(l.alex);
    let legacy_sha = hex::encode(sha2::Sha256::digest(&l.legacy));
    let sha: String = sqlx::query_scalar(
        "select encode(blob_sha256, 'hex') from document \
         where profile_id = $1 and encode(blob_sha256, 'hex') <> $2 limit 1",
    )
    .bind(l.alex)
    .bind(legacy_sha)
    .fetch_one(&l.db.pool)
    .await?;
    let bytes = BlobStore::for_settings(&l.settings)?.get_bytes(owner, &sha)?;
    let mut ring = keyfile::read(&keyfile::path(&l.settings))?.expect("backup wrote the keyring");
    assert!(ring.profiles.remove(&l.alex).is_some());
    keyfile::write(&l.settings.keyring_path(), &ring)?;

    let (_db, settings) = fresh(&l).await?;
    ownhealth_ops::restore_with(&settings, &l.archive, true).await?;
    let store = BlobStore::for_settings(&settings)?;
    assert!(!store.exists(owner, &sha));
    assert!(read_restore_losses(&settings)?[&l.alex].contains(&sha));

    assert_eq!(store.put_bytes(owner, &bytes)?, sha);
    let repaired = l.dir.path().join("repaired");
    ownhealth_ops::backup(&settings, &repaired).await?;
    assert_eq!(ownhealth_ops::verify_restore(&settings, &repaired).await?["ok"], true);

    assert!(store.drop_sealed(owner, &sha)?);
    let lost_again = l.dir.path().join("lost-again");
    ownhealth_ops::backup(&settings, &lost_again).await?;
    let report = ownhealth_ops::verify_restore(&settings, &lost_again).await?;
    assert!(strs(&report["blobs"]["missing"]).contains(&sha), "{report:#}");

    let other_losses = &read_restore_losses(&settings)?[&l.alex];
    assert!(!other_losses.is_empty() && !other_losses.contains(&sha));
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn a_partial_restore_can_retry_after_the_loss_record_write_fails() -> R {
    use ownhealth_core::keyfile;
    use std::os::unix::fs::PermissionsExt;

    let l = live().await?;
    let mut ring = keyfile::read(&keyfile::path(&l.settings))?.expect("backup wrote the keyring");
    assert!(ring.profiles.remove(&l.alex).is_some());
    keyfile::write(&l.settings.keyring_path(), &ring)?;

    let (db, settings) = fresh(&l).await?;
    let folder = settings.blobs_path();
    fs::create_dir_all(&folder)?;
    fs::set_permissions(&folder, fs::Permissions::from_mode(0o500))?;
    let result = ownhealth_ops::restore_with(&settings, &l.archive, true).await;
    fs::set_permissions(&folder, fs::Permissions::from_mode(0o700))?;
    let error = result.expect_err("restore cannot write the loss record");
    let source = std::error::Error::source(&error).and_then(|source| source.downcast_ref::<ownhealth_core::Error>());
    assert!(
        matches!(source, Some(ownhealth_core::Error::Io(_))),
        "expected the original write error, got {source:?}: {error}"
    );
    let error = error.to_string();
    assert!(error.contains("cannot write restore losses"), "{error}");

    let m = manifest(&l.archive)?;
    let mut other = m.clone();
    other["created_at"] = serde_json::json!("2026-11-01T00:00:00Z");
    write_manifest(&l.archive, &other)?;
    let rejected = ownhealth_ops::restore_with(&settings, &l.archive, true).await;
    write_manifest(&l.archive, &m)?;
    assert!(err_text(rejected).contains("target database is not empty"));

    let report = ownhealth_ops::restore_with(&settings, &l.archive, true).await?;
    assert_eq!(report["ok"], true, "{report:#}");
    assert_eq!(report["keys"]["missing"], serde_json::json!([l.alex]));
    assert!(error.contains("rerun the same restore"), "{error}");
    let legacy_sha = hex::encode(sha2::Sha256::digest(&l.legacy));
    let store = BlobStore::for_settings(&settings)?;
    assert_eq!(store.get_bytes(BlobOwner::Profile(l.alex), &legacy_sha)?, l.legacy);
    assert!(
        read_restore_losses(&settings)?
            .get(&l.alex)
            .is_some_and(|lost| !lost.is_empty())
    );
    let pending: bool = sqlx::query_scalar("select to_regnamespace('ownhealth_restore') is not null")
        .fetch_one(&db.pool)
        .await?;
    assert!(!pending);
    Ok(())
}

#[tokio::test]
async fn a_partial_restore_can_retry_after_blob_placement_fails() -> R {
    use ownhealth_core::keyfile;

    let l = live().await?;
    let mut ring = keyfile::read_for_install(&l.settings)?.expect("backup wrote the keyring");
    assert!(ring.profiles.remove(&l.alex).is_some());
    keyfile::write(&l.settings.keyring_path(), &ring)?;

    let (db, settings) = fresh(&l).await?;
    let archived = tree(&l.archive)?;
    let (first, first_path) = archived.first_key_value().expect("first blob");
    let (last, last_path) = archived.last_key_value().expect("last blob");
    assert_ne!(first, last);
    let archive_blobs = l.archive.join("blobs");
    let part_path = |key: &str, file: &Path| -> anyhow::Result<PathBuf> {
        let parent = file.parent().expect("blob shard").strip_prefix(&archive_blobs)?;
        Ok(settings
            .blobs_path()
            .join(parent)
            .join(format!(".{}.part", key.replace('/', "-"))))
    };
    let blocked = part_path(last, last_path)?;
    fs::create_dir_all(&blocked)?;

    let error = ownhealth_ops::restore_with(&settings, &l.archive, true)
        .await
        .expect_err("later blob's temporary path is a directory");
    assert!(matches!(error, ownhealth_core::Error::Io(_)), "{error}");
    let placed = tree_files(&settings.blobs_path())?;
    assert!(placed.contains_key(first));
    assert!(!placed.contains_key(last));
    let pending: bool = sqlx::query_scalar("select to_regclass('ownhealth_restore.pending_loss') is not null")
        .fetch_one(&db.pool)
        .await?;
    assert!(pending);

    fs::remove_dir(blocked)?;
    // A retry that tries to copy the already placed blob would fail at this path.
    let already_placed = part_path(first, first_path)?;
    fs::create_dir(&already_placed)?;
    let result = ownhealth_ops::restore_with(&settings, &l.archive, true).await;
    fs::remove_dir(already_placed)?;
    let report = result?;
    assert_eq!(report["ok"], true, "{report:#}");
    let placed = tree_files(&settings.blobs_path())?;
    assert_eq!(placed.keys().collect::<Vec<_>>(), archived.keys().collect::<Vec<_>>());
    for (key, path) in archived {
        assert_eq!(fs::read(&placed[&key])?, fs::read(path)?);
    }
    let pending: bool = sqlx::query_scalar("select to_regnamespace('ownhealth_restore') is not null")
        .fetch_one(&db.pool)
        .await?;
    assert!(!pending);
    Ok(())
}

#[tokio::test]
async fn a_damaged_present_key_is_rejected_before_restore_even_with_allow_missing() -> R {
    use ownhealth_core::keyfile;

    let l = live().await?;
    let original = keyfile::read(&keyfile::path(&l.settings))?.unwrap_or_default();
    let mut ring = original.clone();
    let wrapped = ring.profiles.get_mut(&l.alex).expect("backup wrote Alex's key");
    *wrapped.last_mut().expect("wrapped key contains an authentication tag") ^= 1;
    keyfile::write(&l.settings.keyring_path(), &ring)?;
    let (db, fresh) = fresh(&l).await?;
    let err = err_text(ownhealth_ops::restore_with(&fresh, &l.archive, true).await);
    assert!(
        err.contains("nothing was loaded") && err.contains(&l.alex.to_string()),
        "{err}"
    );
    // the way forward when no clean copy is left: drop that entry and go on without its key
    assert!(
        err.contains("damaged") && err.contains("remove that entry") && err.contains("--allow-missing-keys"),
        "{err}"
    );
    assert_eq!(public_tables(&db).await?, 0);
    keyfile::write(&l.settings.keyring_path(), &original)?;
    let report = ownhealth_ops::restore(&fresh, &l.archive).await?;
    assert_eq!(report["ok"], true, "{report:#}");
    Ok(())
}

#[tokio::test]
async fn restore_waits_for_the_shared_keyring_before_selecting_keys() -> R {
    use ownhealth_core::keyfile;
    use std::time::Duration;

    let l = live().await?;
    let (db, fresh) = fresh(&l).await?;
    let lock = fs::File::options()
        .write(true)
        .create(true)
        .truncate(false)
        .open(l.settings.keyring_path().join(".keyring.lock"))?;
    lock.try_lock()?;
    let mut restore = Box::pin(ownhealth_ops::restore(&fresh, &l.archive));
    let while_locked = tokio::time::timeout(Duration::from_secs(2), &mut restore).await;

    let mut ring = keyfile::read(&keyfile::path(&l.settings))?.unwrap_or_default();
    assert!(ring.profiles.remove(&l.alex).is_some());
    ring.shredded.insert(l.alex);
    keyfile::write(&l.settings.keyring_path(), &ring)?;
    drop(lock);
    let report = match while_locked {
        Ok(done) => done?,
        Err(_) => tokio::time::timeout(Duration::from_secs(30), restore).await??,
    };
    assert_eq!(report["keys"]["shredded"], serde_json::json!([l.alex]), "{report:#}");
    let row: bool = sqlx::query_scalar("select wrapped_dek is null from profile_key where profile_id = $1")
        .bind(l.alex)
        .fetch_one(&db.pool)
        .await?;
    assert!(row);
    Ok(())
}
#[tokio::test]
async fn a_keyed_dump_from_before_the_keyring_is_refused_before_loading() -> R {
    let l = live().await?;
    // These manifests counted profile_key (even when empty) and had no keys.keyring list.
    let mut m = manifest(&l.archive)?;
    let facts = m["keys"].as_object_mut().expect("backup has key facts");
    facts.shift_remove("keyring");
    facts.shift_remove("key_ids");
    m["tables"]["profile_key"] = serde_json::json!(0);
    write_manifest(&l.archive, &m)?;
    let (db, fresh) = fresh(&l).await?;
    for result in [
        ownhealth_ops::restore(&fresh, &l.archive).await,
        ownhealth_ops::verify_restore(&fresh, &l.archive).await,
    ] {
        let err = err_text(result);
        assert!(err.contains("keyed backup from before the keyring"), "{err}");
        assert!(
            err.contains("build that wrote it") && err.contains("take a new backup"),
            "{err}"
        );
    }
    assert_eq!(public_tables(&db).await?, 0);
    Ok(())
}

#[tokio::test]
async fn an_unscoped_tombstone_refuses_new_sealed_files_after_a_keyless_restore() -> R {
    use ownhealth_core::blobs::BlobError;
    use ownhealth_core::keyfile;
    use ownhealth_core::keys::PROFILE_DELETED;

    ownhealth_ops::register();
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../fixtures/upgrade/0.4.0");
    let dir = tempfile::tempdir()?;
    let db = TestDb::empty().await?;
    let settings = db.settings(dir.path())?;
    ownhealth_ops::restore(&settings, &fixture).await?;
    migrate::migrate_embedded(&db.pool).await?;
    let alex: Uuid = sqlx::query_scalar("select id from profile where slug = 'alex'")
        .fetch_one(&db.pool)
        .await?;
    // The profile was deleted before its key was published, so there is no key ID to record.
    let mut ring = keyfile::KeyringFile::default();
    ring.shredded.insert(alex);
    keyfile::write(&settings.keyring_path(), &ring)?;

    let store = BlobStore::for_settings(&settings)?;
    let written = store.put_bytes(BlobOwner::Profile(alex), b"LabNorte report after restoring 0.4.0");
    assert!(
        matches!(written, Err(BlobError::Key(ref e)) if e.0 == PROFILE_DELETED),
        "{written:?}"
    );
    let exists: bool = sqlx::query_scalar("select exists (select 1 from profile_key where profile_id = $1)")
        .bind(alex)
        .fetch_one(&db.pool)
        .await?;
    assert!(!exists);
    Ok(())
}

/// A 1.0 install deleted Alex after a rewrite published his key, so the keyring closes that key. The
/// 0.4.0 backup (no key table) is then restored on the same keyring: Alex is back, and his next file
/// makes a new key. The tombstone closes only the key that was deleted, so the new key is published
/// and a backup of the new file restores it.
#[tokio::test]
async fn a_tombstone_closes_only_the_deleted_key_after_a_restore_from_before_the_keyring() -> R {
    use ownhealth_core::keyfile;
    use ownhealth_core::keys::{Kek, Keyring, shred};

    ownhealth_ops::register();
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../fixtures/upgrade/0.4.0");
    let dir = tempfile::tempdir()?;
    let first = TestDb::empty().await?;
    let s1 = first.settings(&dir.path().join("first"))?;
    ownhealth_ops::restore(&s1, &fixture).await?;
    migrate::migrate_embedded(&first.pool).await?;
    let alex: Uuid = sqlx::query_scalar("select id from profile where slug = 'alex'")
        .fetch_one(&first.pool)
        .await?;
    assert!(
        Keyring::new(Kek::from_settings(&s1)?, first.url.clone())
            .dek(alex, true)?
            .is_some()
    );
    let mut c = first.pool.acquire().await?;
    keyfile::rewrite(&mut c, &s1).await?;
    let mut tx = first.pool.begin().await?;
    shred(&mut tx, alex).await?;
    tx.commit().await?;
    let ring = keyfile::rewrite(&mut c, &s1).await?;
    drop(c);
    assert!(ring.shredded.contains(&alex) && !ring.profiles.contains_key(&alex));

    let db = TestDb::empty().await?;
    let mut settings = db.settings(&dir.path().join("restored"))?;
    settings.secrets_key = s1.secrets_key.clone();
    settings.keyring_dir = Some(s1.keyring_path());
    ownhealth_ops::restore(&settings, &fixture).await?;
    migrate::migrate_embedded(&db.pool).await?;
    let household: Uuid = sqlx::query_scalar("select household_id from profile where id = $1")
        .bind(alex)
        .fetch_one(&db.pool)
        .await?;
    let bytes = b"%PDF-1.4 LabNorte report of Alex Rivera, stored after the restore".to_vec();
    let sha = BlobStore::for_settings(&settings)?.put_bytes(BlobOwner::Profile(alex), &bytes)?;
    let mut c = db.pool.acquire().await?;
    create_document(
        &mut c,
        &NewDocument::upload(household, Some(alex), &sha, "application/pdf", bytes.len() as i64),
    )
    .await?;
    let ring = keyfile::rewrite(&mut c, &settings).await?;
    drop(c);
    assert!(
        ring.profiles.contains_key(&alex),
        "the new key of Alex was not published"
    );

    let archive = ownhealth_ops::backup(&settings, &dir.path().join("after")).await?;
    let report = ownhealth_ops::verify_restore(&settings, &archive).await?;
    assert_eq!(report["keys"]["shredded"], serde_json::json!([]), "{report:#}");
    let target = TestDb::empty().await?;
    let mut ts = target.settings(&dir.path().join("target"))?;
    ts.secrets_key = settings.secrets_key.clone();
    ts.keyring_dir = Some(settings.keyring_path());
    let restored = ownhealth_ops::restore(&ts, &archive).await?;
    assert_eq!(restored["ok"], true, "{restored:#}");
    assert_eq!(
        BlobStore::for_settings(&ts)?.get_bytes(BlobOwner::Profile(alex), &sha)?,
        bytes
    );
    Ok(())
}

/// The keyring holds another key for Alex than the one that sealed the archive (an earlier partial
/// restore gave him a new one, and newer backups are sealed under it). A restore allowed to miss
/// keys reports Alex under missing but keeps the keyring's key: his next file goes under it, and the
/// keyring still holds it after the rewrite. The files the archive sealed under the old key are an
/// accepted loss, so a backup of the restored install verifies.
#[tokio::test]
async fn a_restore_keeps_the_keyrings_key_when_it_does_not_match_the_archive() -> R {
    use ownhealth_core::keyfile;
    use ownhealth_core::keys::{Dek, Kek, Keyring};

    let l = live().await?;
    let mut ring = keyfile::read(&keyfile::path(&l.settings))?.unwrap_or_default();
    let newer = Kek::from_settings(&l.settings)?.wrap(l.alex, &Dek::generate())?;
    ring.profiles.insert(l.alex, newer.clone());
    keyfile::write(&l.settings.keyring_path(), &ring)?;
    let (db, fresh) = fresh(&l).await?;
    let report = ownhealth_ops::restore_with(&fresh, &l.archive, true).await?;
    assert_eq!(report["keys"]["missing"], serde_json::json!([l.alex]), "{report:#}");
    let row: Option<Vec<u8>> = sqlx::query_scalar("select wrapped_dek from profile_key where profile_id = $1")
        .bind(l.alex)
        .fetch_optional(&db.pool)
        .await?
        .flatten();
    assert_eq!(row.as_ref(), Some(&newer), "the restore did not keep the keyring's key");
    assert!(
        Keyring::new(Kek::from_settings(&fresh)?, db.url.clone())
            .dek(l.alex, true)?
            .is_some()
    );
    let mut c = db.pool.acquire().await?;
    let after = keyfile::rewrite(&mut c, &fresh).await?;
    drop(c);
    assert_eq!(
        after.profiles.get(&l.alex),
        Some(&newer),
        "the keyring lost the newer key"
    );

    let new_backup = l.dir.path().join("after-mismatch");
    ownhealth_ops::backup(&fresh, &new_backup).await?;
    let report = ownhealth_ops::verify_restore(&fresh, &new_backup).await?;
    assert_eq!(report["ok"], true, "{report:#}");
    Ok(())
}

/// A partial restore loses the files sealed under the key the keyring lacks, and later backups do not
/// require them. A plain file of the same profile is not part of that loss: when it goes missing
/// afterwards, the next backup still requires it and verify-restore fails.
#[tokio::test]
async fn a_lost_plain_file_of_a_profile_restored_without_its_key_is_still_required() -> R {
    use ownhealth_core::keyfile;

    let l = live().await?;
    let mut ring = keyfile::read(&keyfile::path(&l.settings))?.unwrap_or_default();
    assert!(ring.profiles.remove(&l.alex).is_some());
    keyfile::write(&l.settings.keyring_path(), &ring)?;
    let (_db, fresh) = fresh(&l).await?;
    ownhealth_ops::restore_with(&fresh, &l.archive, true).await?;
    let legacy_sha = hex::encode(sha2::Sha256::digest(&l.legacy));
    let plain = tree_files(&fresh.blobs_path())?
        .remove(&legacy_sha)
        .expect("the restore placed the plain file");
    fs::remove_file(plain)?;
    let new_backup = l.dir.path().join("after-loss");
    ownhealth_ops::backup(&fresh, &new_backup).await?;
    assert!(strs(&manifest(&new_backup)?["blobs"]["referenced"]).contains(&legacy_sha));
    let report = ownhealth_ops::verify_restore(&fresh, &new_backup).await?;
    assert_eq!(report["ok"], false, "{report:#}");
    assert!(strs(&report["blobs"]["missing"]).contains(&legacy_sha), "{report:#}");
    Ok(())
}

/// An install upgraded from 0.4.0 keeps its files plain, and its profiles have no key until their
/// next file. A plain file lost there is real loss: the next backup still requires it.
#[tokio::test]
async fn a_lost_file_of_an_upgraded_profile_with_no_key_yet_is_still_required() -> R {
    ownhealth_ops::register();
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../fixtures/upgrade/0.4.0");
    let dir = tempfile::tempdir()?;
    let db = TestDb::empty().await?;
    let settings = db.settings(&dir.path().join("upgraded"))?;
    ownhealth_ops::restore(&settings, &fixture).await?;
    migrate::migrate_embedded(&db.pool).await?;
    let (sha, path) = tree_files(&settings.blobs_path())?
        .into_iter()
        .find(|(key, _)| !key.contains('/'))
        .expect("the 0.4.0 install has plain files");
    make_writable(&path)?;
    fs::remove_file(path)?;
    let archive = ownhealth_ops::backup(&settings, &dir.path().join("after")).await?;
    assert!(strs(&manifest(&archive)?["blobs"]["referenced"]).contains(&sha));
    let report = ownhealth_ops::verify_restore(&settings, &archive).await?;
    assert_eq!(report["ok"], false, "{report:#}");
    Ok(())
}

/// The keyring passes the checks at the start, but its rewrite after pg_dump fails (the folder is
/// read-only now): the dump is never published, so the destination stays empty for the next attempt.
#[cfg(unix)]
#[tokio::test]
async fn a_keyring_rewrite_that_fails_after_pg_dump_publishes_no_dump() -> R {
    use std::os::unix::fs::PermissionsExt;

    let l = live().await?;
    let keyring = l.settings.keyring_path();
    let dest = l.dir.path().join("unwritable-keyring");
    fs::set_permissions(&keyring, fs::Permissions::from_mode(0o500))?;
    let err = err_text(ownhealth_ops::backup(&l.settings, &dest).await);
    fs::set_permissions(&keyring, fs::Permissions::from_mode(0o700))?;
    assert!(err.contains("cannot write the keyring"), "{err}");
    assert_eq!(
        files(&dest),
        Vec::<PathBuf>::new(),
        "backup published files before its rewrite"
    );
    ownhealth_ops::backup(&l.settings, &dest).await?;
    Ok(())
}

/// Backup B was taken right after the 0.4.0 upgrade, before Alex stored a file, so it holds no key
/// for him. His next file made key K, and backup C is sealed under it. Restoring B on the same
/// keyring gives Alex K again, so the file he stores next goes under K, the rewrite keeps K, and C
/// still restores with every file.
#[tokio::test]
async fn a_restore_gives_a_profile_the_archive_holds_no_key_for_the_keyrings_key() -> R {
    use ownhealth_core::keyfile;

    ownhealth_ops::register();
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../fixtures/upgrade/0.4.0");
    let dir = tempfile::tempdir()?;
    let first = TestDb::empty().await?;
    let s1 = first.settings(&dir.path().join("first"))?;
    ownhealth_ops::restore(&s1, &fixture).await?;
    migrate::migrate_embedded(&first.pool).await?;
    let (alex, household): (Uuid, Uuid) = sqlx::query_as("select id, household_id from profile where slug = 'alex'")
        .fetch_one(&first.pool)
        .await?;
    let b = ownhealth_ops::backup(&s1, &dir.path().join("b")).await?;
    assert!(!strs(&manifest(&b)?["keys"]["keyring"]).contains(&alex.to_string()));
    let day2 = b"%PDF-1.4 LabNorte report of Alex Rivera, day 2".to_vec();
    let day2_sha = BlobStore::for_settings(&s1)?.put_bytes(BlobOwner::Profile(alex), &day2)?;
    let mut c = first.pool.acquire().await?;
    create_document(
        &mut c,
        &NewDocument::upload(household, Some(alex), &day2_sha, "application/pdf", day2.len() as i64),
    )
    .await?;
    drop(c);
    let c_archive = ownhealth_ops::backup(&s1, &dir.path().join("c")).await?;
    let current = keyfile::read(&keyfile::path(&s1))?.expect("backup wrote the keyring");
    let k = current.profiles[&alex].clone();
    let mut damaged = current.clone();
    let wrapped = damaged.profiles.get_mut(&alex).expect("Alex has a key");
    *wrapped.last_mut().expect("wrapped key includes an authentication tag") ^= 1;
    keyfile::write(&s1.keyring_path(), &damaged)?;

    let db = TestDb::empty().await?;
    let mut s2 = db.settings(&dir.path().join("second"))?;
    s2.secrets_key = s1.secrets_key.clone();
    s2.keyring_dir = Some(s1.keyring_path());
    let result = ownhealth_ops::restore(&s2, &b).await;
    assert_eq!(public_tables(&db).await?, 0, "{result:?}");
    let error = err_text(result);
    assert!(
        error.contains("damaged key") && error.contains(&alex.to_string()) && error.contains("nothing was loaded"),
        "{error}"
    );
    assert!(err_text(ownhealth_ops::verify_restore(&s2, &b).await).contains("damaged key"));
    // A key for a profile outside this archive is irrelevant when restoring it.
    let mut with_unrelated_key = current;
    with_unrelated_key
        .profiles
        .insert(Uuid::new_v4(), damaged.profiles[&alex].clone());
    keyfile::write(&s1.keyring_path(), &with_unrelated_key)?;

    let restored = ownhealth_ops::restore(&s2, &b).await?;
    assert_eq!(restored["ok"], true, "{restored:#}");
    let row: Option<Vec<u8>> = sqlx::query_scalar("select wrapped_dek from profile_key where profile_id = $1")
        .bind(alex)
        .fetch_optional(&db.pool)
        .await?
        .flatten();
    assert_eq!(row.as_ref(), Some(&k), "Alex did not get the keyring's key");
    let later = b"%PDF-1.4 LabNorte report of Alex Rivera, after restoring B".to_vec();
    let later_sha = BlobStore::for_settings(&s2)?.put_bytes(BlobOwner::Profile(alex), &later)?;
    let mut c = db.pool.acquire().await?;
    create_document(
        &mut c,
        &NewDocument::upload(household, Some(alex), &later_sha, "application/pdf", later.len() as i64),
    )
    .await?;
    let after = keyfile::rewrite(&mut c, &s2).await?;
    drop(c);
    assert_eq!(after.profiles.get(&alex), Some(&k), "the keyring lost K");

    let t = TestDb::empty().await?;
    let mut ts = t.settings(&dir.path().join("third"))?;
    ts.secrets_key = s1.secrets_key.clone();
    ts.keyring_dir = Some(s1.keyring_path());
    let report = ownhealth_ops::restore(&ts, &c_archive).await?;
    assert_eq!(report["ok"], true, "{report:#}");
    assert_eq!(
        BlobStore::for_settings(&ts)?.get_bytes(BlobOwner::Profile(alex), &day2_sha)?,
        day2
    );
    Ok(())
}

/// The same through a dump from before 1.0, which has no key table when it is restored, so the
/// restore cannot install a key. Alex stored a file after the 0.4.0 upgrade (key K) and backup C is
/// sealed under K. Restoring the 0.4.0 backup on the same keyring brings Alex back with no key row:
/// the file he stores next takes K from the keyring, the rewrite keeps K, and C still restores with
/// every file.
#[tokio::test]
async fn a_profile_a_dump_from_before_1_0_brings_back_takes_the_keyrings_key_for_its_next_file() -> R {
    use ownhealth_core::keyfile;

    ownhealth_ops::register();
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../fixtures/upgrade/0.4.0");
    let dir = tempfile::tempdir()?;
    let first = TestDb::empty().await?;
    let s1 = first.settings(&dir.path().join("first"))?;
    ownhealth_ops::restore(&s1, &fixture).await?;
    migrate::migrate_embedded(&first.pool).await?;
    let (alex, household): (Uuid, Uuid) = sqlx::query_as("select id, household_id from profile where slug = 'alex'")
        .fetch_one(&first.pool)
        .await?;
    let day2 = b"%PDF-1.4 LabNorte report of Alex Rivera, day 2".to_vec();
    let day2_sha = BlobStore::for_settings(&s1)?.put_bytes(BlobOwner::Profile(alex), &day2)?;
    let mut c = first.pool.acquire().await?;
    create_document(
        &mut c,
        &NewDocument::upload(household, Some(alex), &day2_sha, "application/pdf", day2.len() as i64),
    )
    .await?;
    drop(c);
    let c_archive = ownhealth_ops::backup(&s1, &dir.path().join("c")).await?;
    let k = keyfile::read(&keyfile::path(&s1))?.unwrap_or_default().profiles[&alex].clone();

    let db = TestDb::empty().await?;
    let mut s2 = db.settings(&dir.path().join("second"))?;
    s2.secrets_key = s1.secrets_key.clone();
    s2.keyring_dir = Some(s1.keyring_path());
    let restored = ownhealth_ops::restore(&s2, &fixture).await?;
    assert_eq!(restored["ok"], true, "{restored:#}");
    migrate::migrate_embedded(&db.pool).await?;
    let later = b"%PDF-1.4 LabNorte report of Alex Rivera, after restoring 0.4.0".to_vec();
    let later_sha = BlobStore::for_settings(&s2)?.put_bytes(BlobOwner::Profile(alex), &later)?;
    let row: Option<Vec<u8>> = sqlx::query_scalar("select wrapped_dek from profile_key where profile_id = $1")
        .bind(alex)
        .fetch_optional(&db.pool)
        .await?
        .flatten();
    assert_eq!(
        row.as_ref(),
        Some(&k),
        "Alex's next file did not take the keyring's key"
    );
    let mut c = db.pool.acquire().await?;
    create_document(
        &mut c,
        &NewDocument::upload(household, Some(alex), &later_sha, "application/pdf", later.len() as i64),
    )
    .await?;
    let after = keyfile::rewrite(&mut c, &s2).await?;
    drop(c);
    assert_eq!(after.profiles.get(&alex), Some(&k), "the keyring lost K");

    let t = TestDb::empty().await?;
    let mut ts = t.settings(&dir.path().join("third"))?;
    ts.secrets_key = s1.secrets_key.clone();
    ts.keyring_dir = Some(s1.keyring_path());
    let report = ownhealth_ops::restore(&ts, &c_archive).await?;
    assert_eq!(report["ok"], true, "{report:#}");
    assert_eq!(
        BlobStore::for_settings(&ts)?.get_bytes(BlobOwner::Profile(alex), &day2_sha)?,
        day2
    );
    Ok(())
}

#[tokio::test]
async fn uuid_only_manifests_require_the_build_that_wrote_them() -> R {
    let l = live().await?;
    let mut m = manifest(&l.archive)?;
    m["keys"].as_object_mut().expect("key facts").shift_remove("key_ids");
    write_manifest(&l.archive, &m)?;
    let (target, settings) = fresh(&l).await?;
    for result in [
        ownhealth_ops::verify_restore(&settings, &l.archive).await,
        ownhealth_ops::restore(&settings, &l.archive).await,
    ] {
        let err = err_text(result);
        assert!(
            err.contains("build that wrote it") && err.contains("take a new backup"),
            "{err}"
        );
    }
    assert_eq!(public_tables(&target).await?, 0);
    Ok(())
}

#[tokio::test]
async fn keyless_commands_skip_the_keyring_lock() -> R {
    ownhealth_ops::register();
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../fixtures/upgrade/0.4.0");
    let dir = tempfile::tempdir()?;
    let db = TestDb::empty().await?;
    let settings = db.settings(dir.path())?;
    // An occupied path makes the lock file impossible to create without a second process.
    fs::create_dir_all(settings.keyring_path().join(".keyring.lock"))?;
    let verified = ownhealth_ops::verify_restore(&settings, &fixture).await;
    let restored = ownhealth_ops::restore(&settings, &fixture).await;
    assert_eq!(verified?["ok"], true);
    assert_eq!(restored?["ok"], true);
    Ok(())
}

#[tokio::test]
async fn a_shredded_key_does_not_exempt_a_legacy_plain_report() -> R {
    use ownhealth_core::{keyfile, keys};

    ownhealth_ops::register();
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../fixtures/upgrade/0.4.0");
    let dir = tempfile::tempdir()?;
    let first = TestDb::empty().await?;
    let settings = first.settings(&dir.path().join("first"))?;
    assert_eq!(ownhealth_ops::restore(&settings, &fixture).await?["ok"], true);
    migrate::migrate_embedded(&first.pool).await?;
    let (profile, household, legacy_sha): (Uuid, Uuid, String) = sqlx::query_as(
        "select profile_id, household_id, encode(blob_sha256, 'hex') from document \
         where profile_id is not null order by id limit 1",
    )
    .fetch_one(&first.pool)
    .await?;
    let owner = BlobOwner::Profile(profile);
    let store = BlobStore::for_settings(&settings)?;
    let legacy = store.get_bytes(owner, &legacy_sha)?;
    let sealed = b"%PDF-1.4 LabNorte report stored after the upgrade";
    let sealed_sha = store.put_bytes(owner, sealed)?;
    let mut conn = first.pool.acquire().await?;
    create_document(
        &mut conn,
        &NewDocument::upload(
            household,
            Some(profile),
            &sealed_sha,
            "application/pdf",
            sealed.len() as i64,
        ),
    )
    .await?;
    drop(conn);
    let archive = ownhealth_ops::backup(&settings, &dir.path().join("before-shred")).await?;

    let mut tx = first.pool.begin().await?;
    keys::shred(&mut tx, profile).await?;
    tx.commit().await?;
    let mut conn = first.pool.acquire().await?;
    keyfile::rewrite(&mut conn, &settings).await?;
    drop(conn);

    let db = TestDb::empty().await?;
    let mut restored = db.settings(&dir.path().join("restored"))?;
    restored.secrets_key = settings.secrets_key.clone();
    restored.keyring_dir = Some(settings.keyring_path());
    let report = ownhealth_ops::restore(&restored, &archive).await?;
    assert_eq!(report["keys"]["shredded"], serde_json::json!([profile]), "{report:#}");
    let store = BlobStore::for_settings(&restored)?;
    assert_eq!(store.get_bytes(owner, &legacy_sha)?, legacy);
    assert!(!store.exists(owner, &sealed_sha));
    let mut conn = db.pool.acquire().await?;
    assert!(store.sweep_unkeyed(&mut conn).await? > 0);
    drop(conn);
    fs::remove_file(&tree_files(&restored.blobs_path())?[&legacy_sha])?;

    let after = ownhealth_ops::backup(&restored, &dir.path().join("after-shred")).await?;
    let report = ownhealth_ops::verify_restore(&restored, &after).await?;
    assert_eq!(strs(&report["blobs"]["missing"]), [legacy_sha], "{report:#}");
    assert_eq!(report["ok"], false, "{report:#}");
    Ok(())
}

#[tokio::test]
async fn older_restore_keeps_its_omission_without_remarking_a_newer_store() -> R {
    use ownhealth_core::{
        blobs::sealed_key,
        keyfile,
        keys::{Dek, Kek, Slot},
    };

    let l = live().await?;
    let owner = BlobOwner::Profile(l.alex);
    let slot = Slot::Profile(l.alex);
    let plain_sha = hex::encode(sha2::Sha256::digest(&l.legacy));
    let sha: String = sqlx::query_scalar(
        "select encode(blob_sha256, 'hex') from document \
         where profile_id = $1 and encode(blob_sha256, 'hex') <> $2 order by id limit 1",
    )
    .bind(l.alex)
    .bind(&plain_sha)
    .fetch_one(&l.db.pool)
    .await?;
    let bytes = BlobStore::for_settings(&l.settings)?.get_bytes(owner, &sha)?;
    let mut ring = keyfile::read(&keyfile::path(&l.settings))?.expect("backup wrote the keyring");
    let kek = Kek::from_settings(&l.settings)?;
    let old = kek.unwrap(l.alex, &kek.fingerprint(), &ring.profiles[&l.alex])?;
    let old_path = sealed_key(slot, &old, &sha)?;
    ring.profiles.insert(l.alex, kek.wrap(l.alex, &Dek::generate())?);
    keyfile::write(&l.settings.keyring_path(), &ring)?;

    let (_db, settings) = fresh(&l).await?;
    assert_eq!(
        ownhealth_ops::restore_with(&settings, &l.archive, true).await?["ok"],
        true
    );
    fs::remove_file(&tree_files(&settings.blobs_path())?[&old_path])?;
    let older = ownhealth_ops::backup(&settings, &l.dir.path().join("older-loss")).await?;
    assert!(!tree(&older)?.contains_key(&old_path));
    assert!(strs(&manifest(&older)?["blobs"]["accepted_omissions"][l.alex.to_string()]).contains(&sha));
    assert_eq!(ownhealth_ops::verify_restore(&settings, &older).await?["ok"], true);

    let store = BlobStore::for_settings(&settings)?;
    assert_eq!(store.put_bytes(owner, &bytes)?, sha);
    let repaired = ownhealth_ops::backup(&settings, &l.dir.path().join("newer-repair")).await?;
    assert_eq!(ownhealth_ops::verify_restore(&settings, &repaired).await?["ok"], true);
    let current = read_restore_losses(&settings)?;
    assert!(!current.get(&l.alex).is_some_and(|losses| losses.contains(&sha)));

    let target = TestDb::empty().await?;
    let mut restored = target.settings(&l.dir.path().join("restored-older"))?;
    restored.secrets_key = settings.secrets_key.clone();
    restored.keyring_dir = settings.keyring_dir.clone();
    assert_eq!(ownhealth_ops::restore(&restored, &older).await?["ok"], true);
    let next = ownhealth_ops::backup(&restored, &l.dir.path().join("next")).await?;
    let report = ownhealth_ops::verify_restore(&restored, &next).await?;
    assert_eq!(report["ok"], true, "{report:#}");
    assert!(read_restore_losses(&restored)?[&l.alex].contains(&sha));

    assert!(store.drop_sealed(owner, &sha)?);
    let newer_without_repair = ownhealth_ops::backup(&settings, &l.dir.path().join("newer-missing")).await?;
    let report = ownhealth_ops::verify_restore(&settings, &newer_without_repair).await?;
    assert_eq!(strs(&report["blobs"]["missing"]), [sha], "{report:#}");
    assert_eq!(report["ok"], false, "{report:#}");

    fs::remove_file(&tree_files(&restored.blobs_path())?[&plain_sha])?;
    let unrelated = ownhealth_ops::backup(&restored, &l.dir.path().join("unrelated-loss")).await?;
    let report = ownhealth_ops::verify_restore(&restored, &unrelated).await?;
    assert_eq!(strs(&report["blobs"]["missing"]), [plain_sha], "{report:#}");
    Ok(())
}
