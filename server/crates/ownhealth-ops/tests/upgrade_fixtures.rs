//! The upgrade exit: databases that the released images actually wrote, migrated with HEAD.
//!
//! `fixtures/upgrade/<version>/` is that release's own `ownhealth backup` of a fresh demo install
//! (`scripts/upgrade-fixtures.sh`): the synthetic Rivera family, nothing else. 0.1.1 also covers
//! 0.1.0, which ships the same migrations. Each fixture is restored into an empty database with the
//! old schema, migrated with every HEAD migration, and checked against itself only, because each demo
//! seed draws random src ids and two fixtures never share a hash:
//!
//! - every profile's HEAD state_hash, at the manifest's `as_of`, equals the fixture's `golden.json`
//!   (recorded with 1.0's rule pack; a mismatch prints the hashes HEAD gave);
//! - for 0.4.0, the state_hash its own backup recorded equals the HEAD hash too (no pack bump in 1.0);
//! - the truth rows (every lifecycle-guarded clinical table, `src` and `document`) are the same before
//!   and after the migration, on the columns the old release had;
//! - every blob a row references is in the fixture and still hashes to its name, and the migration
//!   adds or drops no reference;
//! - the fixture holds no install secret: the token, session, OAuth and secret tables are empty.
//!
//! pg_restore 17 comes from PATH or OWNHEALTH_PG_BINDIR, as for the backup tests.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use ownhealth_core::migrate::{embedded, migrate};
use ownhealth_core::testing::TestDb;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::{AssertSqlSafe, PgConnection};
use uuid::Uuid;

/// Tables that hold a credential, a session or a token of the install that wrote the fixture.
const SECRET_TABLES: &[&str] = &[
    "api_token",
    "auth_challenge",
    "calendar_feed",
    "oauth_client",
    "oauth_code",
    "oauth_consent",
    "oauth_refresh_token",
    "oauth_request",
    "passkey",
    "recovery_code",
    "secret",
    "user_session",
];

fn fixture(version: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../../fixtures/upgrade")
        .join(version)
}

fn read_json(path: &Path) -> anyhow::Result<Value> {
    let bytes = fs::read(path).map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))?;
    Ok(serde_json::from_slice(&bytes)?)
}

fn pg_restore(url: &str, dump: &Path) -> anyhow::Result<()> {
    let tool = std::env::var_os("OWNHEALTH_PG_BINDIR")
        .filter(|d| !d.is_empty())
        .map_or_else(|| PathBuf::from("pg_restore"), |d| PathBuf::from(d).join("pg_restore"));
    let out = std::process::Command::new(tool)
        .args([
            "--no-owner",
            "--no-privileges",
            "--exit-on-error",
            "--single-transaction",
            "--dbname",
            url,
        ])
        .stdin(fs::File::open(dump)?)
        .output()?;
    anyhow::ensure!(
        out.status.success(),
        "pg_restore failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    Ok(())
}

async fn table_exists(conn: &mut PgConnection, table: &str) -> anyhow::Result<bool> {
    Ok(
        sqlx::query_scalar("select exists (select 1 from pg_tables where schemaname = 'public' and tablename = $1)")
            .bind(table)
            .fetch_one(&mut *conn)
            .await?,
    )
}

fn quoted(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// (columns, row count, md5 of the sorted row texts) per truth table, on the columns given, or on
/// every column the table has now when `columns` is None.
type Truth = BTreeMap<String, (Vec<String>, i64, String)>;

async fn truth(conn: &mut PgConnection, columns: Option<&Truth>) -> anyhow::Result<Truth> {
    let tables: Vec<String> = sqlx::query_scalar(
        "select c.relname::text from pg_trigger t join pg_proc p on p.oid = t.tgfoid \
         join pg_class c on c.oid = t.tgrelid where p.proname = 'oh_clinical_lifecycle_guard' \
         union select 'src' union select 'document' order by 1",
    )
    .fetch_all(&mut *conn)
    .await?;
    let mut out = Truth::new();
    for table in tables {
        let cols: Vec<String> = match columns.and_then(|c| c.get(&table)) {
            Some((cols, _, _)) => cols.clone(),
            None => {
                sqlx::query_scalar(
                    "select column_name::text from information_schema.columns \
                     where table_schema = 'public' and table_name = $1 order by ordinal_position",
                )
                .bind(&table)
                .fetch_all(&mut *conn)
                .await?
            }
        };
        let list: Vec<String> = cols.iter().map(|c| quoted(c)).collect();
        let q = format!(
            "select count(*), coalesce(md5(string_agg(r::text, E'\\n' order by r::text)), '') \
             from (select {} from {}) r",
            list.join(", "),
            quoted(&table)
        );
        let (n, md5): (i64, String) = sqlx::query_as(AssertSqlSafe(q)).fetch_one(&mut *conn).await?;
        out.insert(table, (cols, n, md5));
    }
    Ok(out)
}

/// Every blob hash a row points at: each bytea column named `blob_sha256` or `*_blob`.
async fn referenced_blobs(conn: &mut PgConnection) -> anyhow::Result<BTreeSet<String>> {
    let cols: Vec<(String, String)> = sqlx::query_as(
        "select table_name::text, column_name::text from information_schema.columns \
         where table_schema = 'public' and data_type = 'bytea' \
         and (column_name = 'blob_sha256' or column_name like '%\\_blob') order by 1, 2",
    )
    .fetch_all(&mut *conn)
    .await?;
    let mut out = BTreeSet::new();
    for (table, col) in cols {
        let q = format!(
            "select distinct encode({c}, 'hex') from {t} where {c} is not null",
            c = quoted(&col),
            t = quoted(&table)
        );
        let found: Vec<String> = sqlx::query_scalar(AssertSqlSafe(q)).fetch_all(&mut *conn).await?;
        out.extend(found);
    }
    Ok(out)
}

/// The fixture's blob files by name, each checked to hash to its own name.
fn fixture_blobs(dir: &Path) -> anyhow::Result<BTreeSet<String>> {
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) -> anyhow::Result<()> {
        for e in fs::read_dir(dir)? {
            let p = e?.path();
            if p.is_dir() {
                walk(&p, out)?;
            } else {
                out.push(p);
            }
        }
        Ok(())
    }
    let mut files = Vec::new();
    walk(&dir.join("blobs").join("sha256"), &mut files)?;
    let mut out = BTreeSet::new();
    for p in files {
        let name = p
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let actual = hex::encode(Sha256::digest(fs::read(&p)?));
        anyhow::ensure!(actual == name, "blob {} hashes to {actual}", p.display());
        out.insert(name);
    }
    Ok(out)
}

async fn state_hashes(conn: &mut PgConnection, as_of: chrono::NaiveDate) -> anyhow::Result<BTreeMap<String, Value>> {
    let ids: Vec<Uuid> = sqlx::query_scalar("select id from profile order by id")
        .fetch_all(&mut *conn)
        .await?;
    let mut out = BTreeMap::new();
    for id in ids {
        let state = ownhealth_rules::build_state(&mut *conn, id, as_of, None).await?;
        out.insert(id.to_string(), state["state_hash"].clone());
    }
    Ok(out)
}

async fn upgrade(version: &str) -> anyhow::Result<()> {
    let dir = fixture(version);
    let manifest = read_json(&dir.join("manifest.json"))?;
    assert_eq!(manifest["app_version"], json!(version), "{}", dir.display());
    let as_of = chrono::NaiveDate::parse_from_str(manifest["as_of"].as_str().unwrap_or_default(), "%Y-%m-%d")?;

    let mut db = TestDb::empty().await?;
    pg_restore(&db.url, &dir.join("db.dump"))?;
    let mut c = db.pool.acquire().await?;
    for table in SECRET_TABLES {
        if table_exists(&mut c, table).await? {
            let q = format!("select count(*) from {}", quoted(table));
            let n: i64 = sqlx::query_scalar(AssertSqlSafe(q)).fetch_one(&mut *c).await?;
            assert_eq!(n, 0, "{version}: the fixture holds rows in {table}");
        }
    }
    let truth_before = truth(&mut c, None).await?;
    assert!(
        truth_before.get("observation").is_some_and(|t| t.1 > 0),
        "{version}: the fixture has no observations: {truth_before:?}"
    );
    let refs_before = referenced_blobs(&mut c).await?;
    drop(c);

    let all = embedded()?;
    migrate(&db.pool, &all).await?;
    // A connection from before the migration may hold plans for the old row types, which Postgres
    // refuses to reuse ("cached plan must not change result type"): the server restarts after a
    // migration, so the checks use new connections.
    db.pool.close().await;
    db.pool = sqlx::PgPool::connect(&db.url).await?;
    let mut c = db.pool.acquire().await?;
    let applied: Vec<i32> = sqlx::query_scalar("select version from schema_migrations order by 1")
        .fetch_all(&mut *c)
        .await?;
    let head: Vec<i32> = all.iter().map(|m| m.version).collect();
    assert_eq!(applied, head, "{version}: not every HEAD migration is applied");

    let truth_after = truth(&mut c, Some(&truth_before)).await?;
    for (table, before) in &truth_before {
        assert_eq!(
            truth_after.get(table),
            Some(before),
            "{version}: the migration changed the truth rows of {table}"
        );
    }

    let refs_after = referenced_blobs(&mut c).await?;
    assert_eq!(
        refs_after, refs_before,
        "{version}: the migration changed the blob references"
    );
    let files = fixture_blobs(&dir)?;
    let missing: Vec<&String> = refs_after.difference(&files).collect();
    assert!(
        missing.is_empty(),
        "{version}: referenced blobs missing from the fixture: {missing:?}"
    );

    let head_hashes = state_hashes(&mut c, as_of).await?;
    assert_eq!(head_hashes.len(), 4, "{version}: {head_hashes:?}");
    let actual = json!({ "state_hash": head_hashes });
    let golden = read_json(&dir.join("golden.json")).unwrap_or(Value::Null);
    assert_eq!(
        golden,
        actual,
        "{version}: HEAD state_hash differs from {}; HEAD gave:\n{}",
        dir.join("golden.json").display(),
        serde_json::to_string_pretty(&actual)?
    );
    if version == "0.4.0" {
        assert_eq!(
            manifest["state_hash"], actual["state_hash"],
            "0.4.0: the hash its own backup recorded differs from HEAD"
        );
    }
    Ok(())
}

#[tokio::test]
async fn a_database_written_by_0_1_1_migrates_to_its_golden_state() -> anyhow::Result<()> {
    upgrade("0.1.1").await
}

#[tokio::test]
async fn a_database_written_by_0_2_0_migrates_to_its_golden_state() -> anyhow::Result<()> {
    upgrade("0.2.0").await
}

#[tokio::test]
async fn a_database_written_by_0_3_0_migrates_to_its_golden_state() -> anyhow::Result<()> {
    upgrade("0.3.0").await
}

#[tokio::test]
async fn a_database_written_by_0_4_0_migrates_with_the_hash_its_backup_recorded() -> anyhow::Result<()> {
    upgrade("0.4.0").await
}
