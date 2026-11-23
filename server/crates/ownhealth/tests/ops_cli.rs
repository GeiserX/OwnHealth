//! Lane H: the `backup | restore | verify-restore` commands of the binary, and the restic recipe in
//! deploy/restic/ run end to end against a local restic repository (`tests/ops/test_backup_restore.py`
//! CLI case, `tests/ops/test_restic_scripts.py`).
//!
//! Needs pg_dump/pg_restore 17 on PATH or in OWNHEALTH_PG_BINDIR (see ownhealth-ops/tests/backup.rs).
//! The restic cases need restic on PATH: skipped without it, except in CI.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use ownhealth_core::blobs::BlobStore;
use ownhealth_core::config::Settings;
use ownhealth_core::demo::seed_demo;
use ownhealth_core::testing::TestDb;
use serde_json::Value;
use sqlx::Connection;

type R = anyhow::Result<()>;

const BIN: &str = env!("CARGO_BIN_EXE_ownhealth");

fn deploy() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../deploy/restic")
}

async fn seeded(dir: &Path) -> anyhow::Result<(TestDb, Settings)> {
    ownhealth_ops::register();
    let db = TestDb::new().await?;
    let settings = db.settings(&dir.join("live"))?;
    let blobs = BlobStore::for_settings(&settings)?;
    assert!(seed_demo(&db.pool, &settings, &blobs, false).await?);
    Ok((db, settings))
}

/// The binary on this fixture's database and data folder. The anchor and keyring folders are the
/// fixture's too, whatever the environment that runs the tests sets.
fn ownhealth(s: &Settings, args: &[&str]) -> anyhow::Result<Output> {
    Ok(Command::new(BIN)
        .args(args)
        .env("OWNHEALTH_DATABASE_URL", &s.database_url)
        .env("OWNHEALTH_DATA_DIR", &s.data_dir)
        .env("OWNHEALTH_ANCHOR_DIR", s.anchor_path())
        .env("OWNHEALTH_KEYRING_DIR", s.keyring_path())
        .env("OWNHEALTH_SECRETS_KEY", s.secrets_key.as_deref().unwrap_or_default())
        .env_remove("OWNHEALTH_ANCHOR_DIR_FILE")
        .env_remove("OWNHEALTH_KEYRING_DIR_FILE")
        .env_remove("OWNHEALTH_SECRETS_KEY_FILE")
        .output()?)
}

/// An install that never set OWNHEALTH_KEYRING_DIR (an older compose file) keeps the keyring on the
/// data volume, where a host-level volume backup versions it. The backup still runs, and says so on
/// stderr, which backup.log keeps.
#[tokio::test]
async fn a_backup_with_the_keyring_on_the_data_volume_says_so() -> R {
    let dir = tempfile::tempdir()?;
    let (_db, s) = seeded(dir.path()).await?;
    let archive = dir.path().join("backup");
    let out = Command::new(BIN)
        .args(["backup", &archive.display().to_string()])
        .env("OWNHEALTH_DATABASE_URL", &s.database_url)
        .env("OWNHEALTH_DATA_DIR", &s.data_dir)
        .env("OWNHEALTH_ANCHOR_DIR", s.anchor_path())
        .env("OWNHEALTH_SECRETS_KEY", s.secrets_key.as_deref().unwrap_or_default())
        .env_remove("OWNHEALTH_KEYRING_DIR")
        .env_remove("OWNHEALTH_ANCHOR_DIR_FILE")
        .env_remove("OWNHEALTH_KEYRING_DIR_FILE")
        .env_remove("OWNHEALTH_SECRETS_KEY_FILE")
        .output()?;
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{stderr}");
    assert!(s.data_dir.join("keyring").join("keyring.json").is_file());
    assert!(
        stderr.contains("OWNHEALTH_KEYRING_DIR is not set") && stderr.contains("data volume"),
        "{stderr}"
    );
    // the control: the same backup with the variable set says nothing of it
    let again = ownhealth(&s, &["backup", &dir.path().join("backup2").display().to_string()])?;
    assert!(again.status.success());
    assert!(!String::from_utf8_lossy(&again.stderr).contains("OWNHEALTH_KEYRING_DIR is not set"));
    Ok(())
}

/// The keyring copy at hand is older than the backup: `restore` refuses it and names the flag, and
/// `restore --allow-missing-keys` loads every record, opens the files of the profiles whose key the
/// copy holds, and lists the others under `keys.missing`.
#[tokio::test]
async fn cli_restore_can_go_on_without_the_keys_a_stale_keyring_lacks() -> R {
    let dir = tempfile::tempdir()?;
    let (_db, s) = seeded(dir.path()).await?;
    let archive = dir.path().join("backup");
    let out = ownhealth(&s, &["backup", &archive.display().to_string()])?;
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let file = ownhealth_core::keyfile::path(&s);
    let mut ring = ownhealth_core::keyfile::read(&file)?.unwrap_or_default();
    let lost = ring.profiles.keys().next().copied().unwrap_or_default();
    ring.profiles.remove(&lost);
    ownhealth_core::keyfile::write(&s.keyring_path(), &ring)?;
    let fresh_db = TestDb::empty().await?;
    let mut fresh = fresh_db.settings(&dir.path().join("fresh"))?;
    fresh.secrets_key = s.secrets_key.clone();
    fresh.keyring_dir = Some(s.keyring_path());
    let refused = ownhealth(&fresh, &["restore", &archive.display().to_string()])?;
    assert_eq!(refused.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&refused.stderr).contains("--allow-missing-keys"));
    let done = ownhealth(
        &fresh,
        &["restore", "--allow-missing-keys", &archive.display().to_string()],
    )?;
    assert_eq!(done.status.code(), Some(0), "{}", String::from_utf8_lossy(&done.stderr));
    let report: Value = serde_json::from_slice(&done.stdout)?;
    assert_eq!(report["keys"]["missing"], serde_json::json!([lost]), "{report:#}");
    Ok(())
}

#[tokio::test]
async fn cli_verify_restore_exits_non_zero_on_a_mismatch() -> R {
    let dir = tempfile::tempdir()?;
    let (_db, s) = seeded(dir.path()).await?;
    let archive = dir.path().join("backup");
    let out = ownhealth(&s, &["backup", &archive.display().to_string()])?;
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(String::from_utf8(out.stdout)?.trim(), archive.display().to_string());
    let ok = ownhealth(&s, &["verify-restore", &archive.display().to_string()])?;
    assert_eq!(ok.status.code(), Some(0), "{}", String::from_utf8_lossy(&ok.stdout));
    let path = archive.join("manifest.json");
    let mut m: Value = serde_json::from_slice(&std::fs::read(&path)?)?;
    m["tables"]["profile"] = Value::from(m["tables"]["profile"].as_i64().unwrap_or(0) + 1);
    std::fs::write(&path, serde_json::to_vec(&m)?)?;
    let bad = ownhealth(&s, &["verify-restore", &archive.display().to_string()])?;
    assert_eq!(bad.status.code(), Some(1));
    assert!(String::from_utf8(bad.stdout)?.contains("\"ok\": false"));
    // an OpsError is one line on stderr and exit 1, as in Python
    let err = ownhealth(
        &s,
        &["verify-restore", &dir.path().join("nowhere").display().to_string()],
    )?;
    assert_eq!(err.status.code(), Some(1));
    assert!(String::from_utf8(err.stderr)?.contains("error: "));
    Ok(())
}

/// The audit anchor seen from cron: once the database owner has edited an entry and hashed the log
/// again, `ownhealth backup` exits 1 with one line that says why, and writes no backup.
#[tokio::test]
async fn cli_backup_exits_non_zero_when_the_anchored_head_no_longer_matches() -> R {
    let dir = tempfile::tempdir()?;
    let (db, s) = seeded(dir.path()).await?;
    let mut c = db.pool.acquire().await?;
    let read = ownhealth_core::audit::AuditEvent::new("system", "job", "read");
    ownhealth_core::audit::write_audit(&mut c, &read).await?;
    drop(c);
    let first = dir.path().join("b1");
    let out = ownhealth(&s, &["backup", &first.display().to_string()])?;
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(s.anchor_path().join("audit-head.json").is_file());
    sqlx::raw_sql(
        "alter table audit_event disable trigger user; alter table audit_chain disable trigger user; \
         update audit_event set tool = 'rewritten' where id = (select min(id) from audit_event); \
         do $$ declare c record; prev bytea := decode(repeat('00', 32), 'hex'); h bytea; begin \
           for c in select seq, event_id from audit_chain order by seq loop \
             select sha256(prev || oh_audit_row_v1(e)) into h from audit_event e where e.id = c.event_id; \
             update audit_chain set prev_hash = prev, hash = h where seq = c.seq; \
             prev := h; \
           end loop; end $$; \
         alter table audit_event enable trigger user; alter table audit_chain enable trigger user",
    )
    .execute(&db.pool)
    .await?;
    let second = dir.path().join("b2");
    let out = ownhealth(&s, &["backup", &second.display().to_string()])?;
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "{stderr}");
    assert!(
        stderr.contains("error: backup refused: the audit log no longer matches"),
        "{stderr}"
    );
    assert!(!second.join("manifest.json").exists());
    Ok(())
}

#[tokio::test]
async fn cli_reads_the_database_url_from_its_file() -> R {
    let dir = tempfile::tempdir()?;
    let db = TestDb::new().await?;
    let s = db.settings(&dir.path().join("live"))?;
    let file = dir.path().join("database_url");
    std::fs::write(&file, &s.database_url)?;
    // only the _FILE variable, as a compose secret sets it
    let out = Command::new(BIN)
        .arg("migrate")
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("OWNHEALTH_DATABASE_URL_FILE", &file)
        .env("OWNHEALTH_DATA_DIR", &s.data_dir)
        .output()?;
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    Ok(())
}

/// Without OWNHEALTH_SECRETS_KEY the app, the worker and the backup refuse to start, with one line
/// that says how to make one; migrate (above) needs none.
#[tokio::test]
async fn cli_refuses_to_start_without_the_secrets_key() -> R {
    let dir = tempfile::tempdir()?;
    let db = TestDb::new().await?;
    let s = db.settings(&dir.path().join("live"))?;
    let backup = dir.path().join("backup").display().to_string();
    for args in [vec!["serve"], vec!["worker"], vec!["backup", backup.as_str()]] {
        let out = Command::new(BIN)
            .args(&args)
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("OWNHEALTH_DATABASE_URL", &s.database_url)
            .env("OWNHEALTH_DATA_DIR", &s.data_dir)
            .output()?;
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(!out.status.success(), "{args:?} started: {err}");
        assert_eq!(err.trim_end().lines().count(), 1, "{args:?}: {err}");
        assert!(
            err.contains("OWNHEALTH_SECRETS_KEY is not set") && err.contains("openssl rand -base64 32"),
            "{args:?}: {err}"
        );
    }
    assert!(!dir.path().join("backup").exists());
    // a restore needs the key the backup was made with: a new one never opens it
    for args in [
        vec!["restore", backup.as_str()],
        vec!["verify-restore", backup.as_str()],
    ] {
        let out = Command::new(BIN)
            .args(&args)
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("OWNHEALTH_DATABASE_URL", &s.database_url)
            .env("OWNHEALTH_DATA_DIR", &s.data_dir)
            .output()?;
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(!out.status.success(), "{args:?} started: {err}");
        assert_eq!(err.trim_end().lines().count(), 1, "{args:?}: {err}");
        assert!(
            err.contains("OWNHEALTH_SECRETS_KEY is not set")
                && err.contains("the key the backup was made with")
                && !err.contains("openssl rand"),
            "{args:?}: {err}"
        );
    }
    Ok(())
}

/// `cmd` run until it exits or `secs` pass (then killed): its exit status, if it ended, and stderr.
fn run_for(mut cmd: Command, secs: u64) -> anyhow::Result<(Option<std::process::ExitStatus>, String)> {
    let mut child = cmd
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()?;
    let until = std::time::Instant::now() + std::time::Duration::from_secs(secs);
    let status = loop {
        if let Some(s) = child.try_wait()? {
            break Some(s);
        }
        if std::time::Instant::now() > until {
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    };
    let mut err = String::new();
    if let Some(mut e) = child.stderr.take() {
        std::io::Read::read_to_string(&mut e, &mut err)?;
    }
    Ok((status, err))
}

/// A start with a well-formed OWNHEALTH_SECRETS_KEY that is not the one the stored profile keys were
/// made with is refused before anything listens or writes a key: new profiles would otherwise get
/// keys the right key never opens. A fresh install, with no profile key yet, starts.
#[tokio::test]
async fn cli_refuses_to_start_with_another_secrets_key() -> R {
    let dir = tempfile::tempdir()?;
    let db = TestDb::new().await?;
    let right = db.settings(&dir.path().join("live"))?;
    // a fresh install starts and stores its first profile keys
    let out = ownhealth(&right, &["demo-seed"])?;
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let keys = || async {
        let rows: Vec<(Vec<u8>, i64)> =
            sqlx::query_as("select kek_fingerprint, count(*) from profile_key group by 1 order by 1")
                .fetch_all(&db.pool)
                .await?;
        anyhow::Ok(rows)
    };
    let before = keys().await?;
    assert_eq!(before.len(), 1, "{before:?}");
    let mut wrong = right.clone();
    wrong.secrets_key = Some(ownhealth_core::testing::random_secrets_key());
    let port = std::net::TcpListener::bind("127.0.0.1:0")?
        .local_addr()?
        .port()
        .to_string();
    for args in [vec!["worker"], vec!["serve"], vec!["demo-seed", "--force"]] {
        let mut cmd = Command::new(BIN);
        cmd.args(&args)
            .env("OWNHEALTH_DATABASE_URL", &wrong.database_url)
            .env("OWNHEALTH_DATA_DIR", &wrong.data_dir)
            .env("OWNHEALTH_ANCHOR_DIR", wrong.anchor_path())
            .env(
                "OWNHEALTH_SECRETS_KEY",
                wrong.secrets_key.as_deref().unwrap_or_default(),
            )
            .env("OWNHEALTH_DEV_AUTH", "1")
            .env("OWNHEALTH_HOST", "127.0.0.1")
            .env("OWNHEALTH_PORT", &port)
            .env_remove("OWNHEALTH_ANCHOR_DIR_FILE")
            .env_remove("OWNHEALTH_SECRETS_KEY_FILE");
        let (status, err) = run_for(cmd, 30)?;
        assert!(status.is_some(), "{args:?} kept running with another key: {err}");
        assert!(!status.is_some_and(|s| s.success()), "{args:?} succeeded: {err}");
        let line = err.trim_end().lines().last().unwrap_or_default().to_string();
        assert!(
            line.starts_with("error: ")
                && line.contains("OWNHEALTH_SECRETS_KEY")
                && line.contains("deploy/README.md")
                && !line.contains(wrong.secrets_key.as_deref().unwrap_or("-"))
                && !line.contains(right.secrets_key.as_deref().unwrap_or("-")),
            "{args:?}: {err}"
        );
        assert!(!err.contains("listening"), "{args:?} listened: {err}");
    }
    assert_eq!(keys().await?, before, "a key was stored under another key");
    // the right key still starts
    let out = ownhealth(&right, &["demo-seed"])?;
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    Ok(())
}

/// `migrate` runs only as the owner of the database. Run by another role, a superuser here, the
/// tables and the SECURITY DEFINER chain function would belong to that role, and the database owner,
/// who owns schema public, could drop them and put its own in their place. The control hands the
/// database to the migrating role, and the same command then runs.
#[tokio::test]
async fn cli_migrate_refuses_a_database_it_does_not_own() -> R {
    let dir = tempfile::tempdir()?;
    let db = TestDb::empty().await?;
    let s = db.settings(&dir.path().join("live"))?;
    let owner = format!("oh_test_own_{}", ownhealth_core::ids::token_hex(4));
    let mut admin = sqlx::PgConnection::connect(&ownhealth_core::testing::admin_url()).await?;
    let result: R = async {
        sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
            "create role \"{owner}\" nologin; alter database \"{}\" owner to \"{owner}\"",
            db.name
        )))
        .execute(&mut admin)
        .await?;
        let out = ownhealth(&s, &["migrate"])?;
        let stderr = String::from_utf8_lossy(&out.stderr);
        anyhow::ensure!(out.status.code() == Some(1), "migrate must refuse: {stderr}");
        anyhow::ensure!(stderr.contains(&format!("owner of database {}", db.name)), "{stderr}");
        let tables: i64 = sqlx::query_scalar("select count(*) from pg_tables where schemaname = 'public'")
            .fetch_one(&db.pool)
            .await?;
        anyhow::ensure!(tables == 0, "the refused run made {tables} tables");
        sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
            "alter database \"{}\" owner to current_user",
            db.name
        )))
        .execute(&mut admin)
        .await?;
        let out = ownhealth(&s, &["migrate"])?;
        anyhow::ensure!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        Ok(())
    }
    .await;
    drop(db);
    sqlx::query(sqlx::AssertSqlSafe(format!("drop role if exists \"{owner}\"")))
        .execute(&mut admin)
        .await?;
    result
}

fn restic() -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|d| d.join("restic"))
        .find(|p| p.is_file())
}

/// None when restic is missing outside CI (skip); in CI a missing restic is a broken runner.
fn restic_or_skip() -> Option<PathBuf> {
    let r = restic();
    assert!(
        r.is_some() || std::env::var_os("CI").is_none(),
        "restic is not on PATH: CI must install the version the image ships"
    );
    if r.is_none() {
        eprintln!("skipped: restic not on PATH");
    }
    r
}

struct Env {
    vars: Vec<(String, String)>,
}

fn env_for(s: &Settings, tmp: &Path) -> Env {
    let bin_dir = Path::new(BIN).parent().map(Path::to_path_buf).unwrap_or_default();
    let path = std::env::var("PATH").unwrap_or_default();
    let vars = [
        ("OWNHEALTH_RUN", String::new()),
        ("OWNHEALTH_DATABASE_URL", s.database_url.clone()),
        ("OWNHEALTH_DATA_DIR", s.data_dir.display().to_string()),
        ("OWNHEALTH_ANCHOR_DIR", s.anchor_path().display().to_string()),
        ("OWNHEALTH_KEYRING_DIR", s.keyring_path().display().to_string()),
        ("OWNHEALTH_SECRETS_KEY", s.secrets_key.clone().unwrap_or_default()),
        ("OWNHEALTH_BACKUP_STAGING", tmp.join("staging").display().to_string()),
        ("OWNHEALTH_RESTORE_DIR", tmp.join("restore").display().to_string()),
        ("RESTIC_REPOSITORY", tmp.join("repo").display().to_string()),
        ("RESTIC_PASSWORD", "test-only-password".into()),
        ("RESTIC_CACHE_DIR", tmp.join("cache").display().to_string()),
        ("PATH", format!("{}:{path}", bin_dir.display())),
    ];
    Env {
        vars: vars.into_iter().map(|(k, v)| (k.to_string(), v)).collect(),
    }
}

fn run(cmd: impl AsRef<std::ffi::OsStr>, env: &Env, args: &[&str]) -> anyhow::Result<Output> {
    Ok(Command::new(cmd)
        .args(args)
        .envs(env.vars.iter().map(|(k, v)| (k, v)))
        .env_remove("OWNHEALTH_ANCHOR_DIR_FILE")
        .env_remove("OWNHEALTH_KEYRING_DIR_FILE")
        .env_remove("OWNHEALTH_SECRETS_KEY_FILE")
        .output()?)
}

fn script(name: &str, env: &Env) -> anyhow::Result<Output> {
    run(deploy().join(name), env, &[])
}

fn text(o: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    )
}

#[tokio::test]
async fn backup_then_restore_test_then_restore_into_a_fresh_stack() -> R {
    let Some(restic) = restic_or_skip() else { return Ok(()) };
    let tmp = tempfile::tempdir()?;
    let (_db, s) = seeded(tmp.path()).await?;
    let env = env_for(&s, tmp.path());
    assert!(run(&restic, &env, &["init", "--quiet"])?.status.success());

    let done = script("backup.sh", &env)?;
    assert!(done.status.success(), "{}", text(&done));
    let snaps: Value = serde_json::from_slice(&run(&restic, &env, &["snapshots", "--json"])?.stdout)?;
    assert_eq!(snaps.as_array().map(Vec::len), Some(1));
    assert_eq!(snaps[0]["tags"], serde_json::json!(["ownhealth"]));
    assert!(!tmp.path().join("staging").exists());
    // the profile keys are in the keyring folder, never in the snapshot
    assert!(s.keyring_path().join("keyring.json").is_file());
    let listed = run(&restic, &env, &["ls", "latest"])?;
    assert!(listed.status.success(), "{}", text(&listed));
    let listed = String::from_utf8_lossy(&listed.stdout).to_string();
    assert!(listed.contains("db.dump") && !listed.contains("keyring"), "{listed}");

    let check = script("restore-test.sh", &env)?;
    assert!(check.status.success(), "{}", text(&check));
    let report: Value = serde_json::from_slice(&check.stdout)?;
    assert!(report["ok"] == true && report["profiles"].as_object().map(|p| p.len()) == Some(4));
    assert!(!tmp.path().join("restore").exists());

    let fresh_db = TestDb::empty().await?;
    let mut fresh = fresh_db.settings(&tmp.path().join("fresh"))?;
    fresh.secrets_key = s.secrets_key.clone(); // the key the backup was made under
    fresh.keyring_dir = Some(s.keyring_path()); // the keyring mount, outside the snapshot
    let restored = script("restore.sh", &env_for(&fresh, tmp.path()))?;
    assert!(restored.status.success(), "{}", text(&restored));
    let fresh_report: Value = serde_json::from_slice(&restored.stdout)?;
    assert_eq!(fresh_report["ok"], true);
    let actual: Vec<(String, Value)> = fresh_report["profiles"]
        .as_object()
        .into_iter()
        .flatten()
        .map(|(p, r)| (p.clone(), r["actual"].clone()))
        .collect();
    let expected: Vec<(String, Value)> = report["profiles"]
        .as_object()
        .into_iter()
        .flatten()
        .map(|(p, r)| (p.clone(), r["expected"].clone()))
        .collect();
    assert_eq!(actual, expected);
    Ok(())
}

/// restore.sh with OWNHEALTH_ALLOW_MISSING_KEYS=1 restores with a keyring copy older than the
/// snapshot: every record comes back, and the report names the profile whose key the copy lacks.
#[tokio::test]
async fn restore_sh_can_go_on_without_the_keys_a_stale_keyring_lacks() -> R {
    let Some(restic) = restic_or_skip() else { return Ok(()) };
    let tmp = tempfile::tempdir()?;
    let (_db, s) = seeded(tmp.path()).await?;
    let env = env_for(&s, tmp.path());
    assert!(run(&restic, &env, &["init", "--quiet"])?.status.success());
    assert!(script("backup.sh", &env)?.status.success());
    let file = ownhealth_core::keyfile::path(&s);
    let mut ring = ownhealth_core::keyfile::read(&file)?.unwrap_or_default();
    let lost = ring.profiles.keys().next().copied().unwrap_or_default();
    ring.profiles.remove(&lost);
    let stale = tmp.path().join("stale-keyring");
    ownhealth_core::keyfile::write(&stale, &ring)?;
    let fresh_db = TestDb::empty().await?;
    let mut fresh = fresh_db.settings(&tmp.path().join("fresh"))?;
    fresh.secrets_key = s.secrets_key.clone();
    fresh.keyring_dir = Some(stale);
    let mut env = env_for(&fresh, tmp.path());
    env.vars.push(("OWNHEALTH_ALLOW_MISSING_KEYS".into(), "1".into()));
    let restored = script("restore.sh", &env)?;
    assert!(restored.status.success(), "{}", text(&restored));
    let report: Value = serde_json::from_slice(&restored.stdout)?;
    assert_eq!(report["keys"]["missing"], serde_json::json!([lost]), "{report:#}");
    Ok(())
}

#[tokio::test]
async fn restore_test_fails_when_the_latest_snapshot_does_not_match_its_manifest() -> R {
    let Some(restic) = restic_or_skip() else { return Ok(()) };
    let tmp = tempfile::tempdir()?;
    let (_db, s) = seeded(tmp.path()).await?;
    let env = env_for(&s, tmp.path());
    assert!(run(&restic, &env, &["init", "--quiet"])?.status.success());
    let staging = tmp.path().join("staging");
    ownhealth_ops::backup(&s, &staging).await?;
    let path = staging.join("manifest.json");
    let mut m: Value = serde_json::from_slice(&std::fs::read(&path)?)?;
    let pid = m["state_hash"]
        .as_object()
        .and_then(|o| o.keys().next().cloned())
        .unwrap_or_default();
    m["state_hash"][&pid] = Value::String(format!("sha256:{}", "0".repeat(64)));
    std::fs::write(&path, serde_json::to_vec(&m)?)?;
    let staging_arg = staging.display().to_string();
    let tag = [
        "backup",
        "--host",
        "ownhealth",
        "--tag",
        "ownhealth",
        "--quiet",
        staging_arg.as_str(),
    ];
    assert!(run(&restic, &env, &tag)?.status.success());

    let check = script("restore-test.sh", &env)?;
    assert_eq!(check.status.code(), Some(1), "{}", text(&check));
    let report: Value = serde_json::from_slice(&check.stdout)?;
    assert_eq!(report["ok"], false);
    assert!(String::from_utf8_lossy(&check.stderr).contains("FAILED"));
    Ok(())
}

#[tokio::test]
async fn restore_refuses_a_stack_that_already_has_data() -> R {
    let Some(restic) = restic_or_skip() else { return Ok(()) };
    let tmp = tempfile::tempdir()?;
    let (_db, s) = seeded(tmp.path()).await?;
    let env = env_for(&s, tmp.path());
    assert!(run(&restic, &env, &["init", "--quiet"])?.status.success());
    assert!(script("backup.sh", &env)?.status.success());
    let done = script("restore.sh", &env)?;
    assert!(
        !done.status.success() && String::from_utf8_lossy(&done.stderr).contains("not empty"),
        "{}",
        text(&done)
    );
    Ok(())
}
