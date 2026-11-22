//! Backup, restore and verify-restore (`ops/__init__.py`, architecture.md 14).
//!
//! A backup is a plain directory. restic snapshots it (deploy/restic/), so restic does the dedup,
//! encryption and retention, and this module only has to make the directory consistent:
//!
//! ```text
//! <dest>/manifest.json    app version, migrations, row counts, blob list, state_hash per profile
//! <dest>/db.dump          pg_dump -Fc, taken from the same database snapshot as the counts and hashes
//! <dest>/blobs/sha256/..  the blob store, hard-linked when on the same disk (blobs never change)
//! ```
//!
//! pg_dump and pg_restore come from PATH, or from the directory in OWNHEALTH_PG_BINDIR. They read and
//! write only stdin and stdout, so a wrapper that runs them in another container works too.
//!
//! The audit chain head (migrations/1250_audit_chain.sql) leaves the database with every backup: the
//! manifest records the newest link (`audit_chain`: seq, hash, and what `verify_chain` found), and
//! the backup writes the same seq and hash to `<OWNHEALTH_ANCHOR_DIR>/audit-head.json`. Before it
//! dumps anything, the backup checks that link N of the live chain still has the hash H the previous
//! anchor holds, and refuses when it does not. A database owner can edit a row and hash the chain
//! again from there, and `verify_chain` then passes; that check is what catches it. The first backup
//! finds no anchor, checks nothing and writes one. The audit page runs the same check
//! (`audit_status`).

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Stdio;

use chrono::{NaiveDate, SecondsFormat, Utc};
use ownhealth_core::audit::{ChainBreakKind, ChainCheck, verify_chain};
use ownhealth_core::config::Settings;
use ownhealth_core::{Error, Result};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use sqlx::{AssertSqlSafe, Connection, PgConnection};
use tokio::process::Command;
use uuid::Uuid;

pub const MANIFEST_SCHEMA: &str = "ownhealth.backup/v1";
pub const ANCHOR_SCHEMA: &str = "ownhealth.audit-anchor/v1";
const ANCHOR_FILE: &str = "audit-head.json";

/// A backup or restore step failed; the message says which and why (Python's `OpsError`).
fn ops_error(msg: impl Into<String>) -> Error {
    Error::Other(anyhow::anyhow!(msg.into()))
}

fn is_executable(p: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    }
    #[cfg(not(unix))]
    {
        p.is_file()
    }
}

fn pg_tool(name: &str) -> Result<PathBuf> {
    let found = match std::env::var_os("OWNHEALTH_PG_BINDIR").filter(|d| !d.is_empty()) {
        Some(dir) => Some(PathBuf::from(dir).join(name)),
        None => std::env::var_os("PATH").and_then(|paths| {
            std::env::split_paths(&paths)
                .map(|d| d.join(name))
                .find(|p| is_executable(p))
        }),
    };
    found.filter(|p| is_executable(p)).ok_or_else(|| {
        ops_error(format!(
            "{name} not found: install the PostgreSQL 17 client or set OWNHEALTH_PG_BINDIR"
        ))
    })
}

fn libpq_url(url: &str) -> String {
    ownhealth_core::config::sqlx_url(url)
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && let Some(hex) = b.get(i + 1..i + 3)
            && let Some(v) = std::str::from_utf8(hex)
                .ok()
                .and_then(|h| u8::from_str_radix(h, 16).ok())
        {
            out.push(v);
            i += 3;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// (the `--dbname` value without the password, the password for PGPASSWORD): argv is world-readable.
pub(crate) fn pg_conn(url: &str) -> (String, Option<String>) {
    let url = libpq_url(url);
    let Some((scheme, rest)) = url.split_once("://") else {
        return (url, None);
    };
    let auth_end = rest.find(['/', '?']).unwrap_or(rest.len());
    let (authority, tail) = rest.split_at(auth_end);
    let Some((userinfo, host)) = authority.rsplit_once('@') else {
        return (url, None);
    };
    let Some((user, password)) = userinfo.split_once(':') else {
        return (url, None);
    };
    (
        format!("{scheme}://{user}@{host}{tail}"),
        Some(percent_decode(password)),
    )
}

/// The same URL naming another database (query kept).
fn with_db(url: &str, db: &str) -> String {
    let url = libpq_url(url);
    // The query may hold a `/` (sslrootcert=/etc/ca.pem), so cut it off before finding the path.
    let (path, query) = url.split_once('?').unwrap_or((&url, ""));
    let base = path.rsplit_once('/').map_or(path, |(b, _)| b);
    if query.is_empty() {
        format!("{base}/{db}")
    } else {
        format!("{base}/{db}?{query}")
    }
}

fn db_name(url: &str) -> String {
    let url = libpq_url(url);
    let path = url.split_once('?').map_or(url.as_str(), |(p, _)| p);
    path.rsplit_once('/').map_or("", |(_, r)| r).to_string()
}

/// The argv after the tool name, and the password that goes in PGPASSWORD instead.
fn pg_args(url: &str, args: &[&str]) -> (Vec<String>, Option<String>) {
    let (dsn, password) = pg_conn(url);
    let mut argv: Vec<String> = args.iter().map(|a| a.to_string()).collect();
    argv.extend(["--dbname".to_string(), dsn]);
    (argv, password)
}

fn dump_args(url: &str, snapshot: &str) -> (Vec<String>, Option<String>) {
    pg_args(url, &["-Fc", &format!("--snapshot={snapshot}")])
}

fn restore_args(url: &str) -> (Vec<String>, Option<String>) {
    pg_args(
        url,
        &[
            "--no-owner",
            "--no-privileges",
            "--exit-on-error",
            "--single-transaction",
        ],
    )
}

fn pg_command(tool: &str, (argv, password): (Vec<String>, Option<String>)) -> Result<Command> {
    let mut cmd = Command::new(pg_tool(tool)?);
    cmd.args(argv);
    if let Some(pw) = password {
        cmd.env("PGPASSWORD", pw);
    }
    Ok(cmd)
}

fn sha256_file(path: &Path) -> Result<String> {
    let mut f = fs::File::open(path)?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(hex::encode(h.finalize()))
}

fn walk_files(dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_dir() {
            walk_files(&path, out)?;
        } else if path.is_file() {
            out.push(path);
        }
    }
    Ok(())
}

/// Every file under `<root>/sha256`, by file name, in path order.
fn blob_files(root: &Path) -> Result<BTreeMap<String, PathBuf>> {
    let base = root.join("sha256");
    let mut files = Vec::new();
    if base.is_dir() {
        walk_files(&base, &mut files)?;
    }
    files.sort();
    Ok(files
        .into_iter()
        .filter_map(|p| Some((p.file_name()?.to_string_lossy().into_owned(), p)))
        .collect())
}

async fn table_counts(conn: &mut PgConnection) -> Result<BTreeMap<String, i64>> {
    let names: Vec<String> =
        sqlx::query_scalar("select tablename::text from pg_tables where schemaname = 'public' order by 1")
            .fetch_all(&mut *conn)
            .await?;
    let mut out = BTreeMap::new();
    for n in names {
        let q = format!("select count(*) from \"{}\"", n.replace('"', "\"\""));
        let c: i64 = sqlx::query_scalar(AssertSqlSafe(q)).fetch_one(&mut *conn).await?;
        out.insert(n, c);
    }
    Ok(out)
}

async fn state_hashes(conn: &mut PgConnection, as_of: NaiveDate) -> Result<Vec<(String, String)>> {
    let ids: Vec<Uuid> = sqlx::query_scalar("select id from profile order by id")
        .fetch_all(&mut *conn)
        .await?;
    let mut out = Vec::new();
    for pid in ids {
        let state = ownhealth_rules::build_state(&mut *conn, pid, as_of, None).await?;
        let hash = state["state_hash"]
            .as_str()
            .ok_or_else(|| ops_error(format!("build_state gave no state_hash for {pid}")))?;
        out.push((pid.to_string(), hash.to_string()));
    }
    Ok(out)
}

/// Every blob hash a document or genome dataset row points at, so verify can tell a blob the live
/// store had already lost from one the backup lost.
async fn referenced_blobs(conn: &mut PgConnection) -> Result<Vec<String>> {
    let cols: Vec<String> = sqlx::query_scalar(
        "select column_name::text from information_schema.columns where table_schema = 'public' \
         and table_name = 'genome_dataset' and data_type = 'bytea' and column_name like '%\\_blob'",
    )
    .fetch_all(&mut *conn)
    .await?;
    let mut parts = vec!["select blob_sha256 as h from document".to_string()];
    parts.extend(cols.iter().map(|c| format!("select \"{c}\" from genome_dataset")));
    let q = format!(
        "select distinct encode(h, 'hex') from ({}) x where h is not null order by 1",
        parts.join(" union ")
    );
    Ok(sqlx::query_scalar(AssertSqlSafe(q)).fetch_all(&mut *conn).await?)
}

fn link_or_copy(src: &Path, dst: &Path) -> Result<()> {
    if fs::hard_link(src, dst).is_err() {
        fs::copy(src, dst)?;
    }
    Ok(())
}

fn copy_tree(src: &Path, dst: &Path) -> Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let (from, to) = (entry.path(), dst.join(entry.file_name()));
        if from.is_dir() {
            copy_tree(&from, &to)?;
        } else {
            link_or_copy(&from, &to)?;
        }
    }
    Ok(())
}

/// Recursively sorted keys: Python's `json.dumps(..., sort_keys=True)`.
fn sort_keys(v: Value) -> Value {
    match v {
        Value::Object(m) => {
            let sorted: BTreeMap<String, Value> = m.into_iter().map(|(k, v)| (k, sort_keys(v))).collect();
            Value::Object(sorted.into_iter().collect())
        }
        Value::Array(a) => Value::Array(a.into_iter().map(sort_keys).collect()),
        other => other,
    }
}

/// The backup holds the whole health record: only its owner may read it (directories 0700, files
/// 0600), whatever the umask. `dest` itself is closed first, so nothing inside is ever open to others.
#[cfg(unix)]
fn owner_only(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let meta = fs::symlink_metadata(path)?;
    if meta.is_dir() {
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
        for entry in fs::read_dir(path)? {
            owner_only(&entry?.path())?;
        }
    } else if meta.is_file() {
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

#[cfg(not(unix))]
fn owner_only(_path: &Path) -> Result<()> {
    Ok(())
}

// ------------------------------------------------------------------ audit anchor ----

/// The head a backup anchored: link `seq` of the audit chain had hash `hash` at `at`. Seq 0 is the
/// empty chain, which every chain extends.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Anchor {
    seq: i64,
    hash: Vec<u8>,
    at: String,
}

fn anchor_file(settings: &Settings) -> PathBuf {
    settings.anchor_path().join(ANCHOR_FILE)
}

fn anchor_unreadable(path: &Path, why: &str) -> Error {
    ops_error(format!(
        "cannot read the audit anchor {}: {why}. Put back the copy you keep, or move this file aside \
         (keep it) so the next backup anchors the log as it is now",
        path.display()
    ))
}

/// None only when no backup has written one yet. Anything else that is not a valid anchor is an
/// error, never "no anchor yet": a damaged file must not silently restart the anchoring.
fn read_anchor(path: &Path) -> Result<Option<Anchor>> {
    let bytes = match fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(anchor_unreadable(path, &e.to_string())),
    };
    let v: Value = serde_json::from_slice(&bytes).map_err(|e| anchor_unreadable(path, &e.to_string()))?;
    let seq = v["seq"].as_i64().filter(|s| *s >= 0);
    let hash = v["hash"]
        .as_str()
        .and_then(|h| hex::decode(h).ok())
        .filter(|h| h.len() == 32);
    match (v["schema"].as_str(), seq, hash) {
        (Some(ANCHOR_SCHEMA), Some(seq), Some(hash)) => Ok(Some(Anchor {
            seq,
            hash,
            at: v["at"].as_str().unwrap_or_default().to_string(),
        })),
        _ => Err(anchor_unreadable(path, &format!("it is not an {ANCHOR_SCHEMA} file"))),
    }
}

/// Replace the anchor in `dir` atomically: a new file only the owner may read (0600, in a 0700
/// folder), written and synced under a temporary name, then renamed over the old one and the folder
/// synced. A crash leaves the old anchor or the new one, never half of either.
fn write_anchor(dir: &Path, a: &Anchor) -> Result<()> {
    let body = serde_json::to_string_pretty(&json!({
        "schema": ANCHOR_SCHEMA,
        "seq": a.seq,
        "hash": hex::encode(&a.hash),
        "at": a.at,
    }))? + "\n";
    let tmp = dir.join(format!(".{ANCHOR_FILE}.{}.tmp", ownhealth_core::ids::token_hex(6)));
    let written = (|| -> std::io::Result<()> {
        fs::create_dir_all(dir)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
        }
        let mut opts = fs::OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut f = opts.open(&tmp)?;
        f.write_all(body.as_bytes())?;
        f.sync_all()?;
        drop(f);
        fs::rename(&tmp, dir.join(ANCHOR_FILE))?;
        #[cfg(unix)]
        fs::File::open(dir)?.sync_all()?; // the rename is on disk only once the folder is
        Ok(())
    })();
    written.map_err(|e| {
        let _ = fs::remove_file(&tmp);
        ops_error(format!("cannot write the audit anchor in {}: {e}", dir.display()))
    })
}

/// The hash link `seq` holds in this snapshot, if the link is there.
async fn link_hash(conn: &mut PgConnection, seq: i64) -> Result<Option<Vec<u8>>> {
    Ok(
        sqlx::query_scalar("select hash from only public.audit_chain where seq = $1")
            .bind(seq)
            .fetch_optional(conn)
            .await?,
    )
}

/// Link `a.seq` still holds the hash the anchor kept. A rewrite at or before it changes that hash
/// (each link hashes the one before it), and so does removing it.
async fn anchor_holds(conn: &mut PgConnection, a: &Anchor) -> Result<bool> {
    if a.seq == 0 {
        return Ok(true);
    }
    Ok(link_hash(conn, a.seq).await?.as_deref() == Some(a.hash.as_slice()))
}

fn anchor_mismatch(path: &Path, a: &Anchor) -> Error {
    ops_error(format!(
        "backup refused: the audit log no longer matches the head the last backup anchored ({}): \
         entry {} is gone or holds another hash. Someone with database owner rights rewrote the log, \
         or the database was restored from an older backup. Find out which first (the audit page \
         shows it). Then move {} aside and keep it: the next backup anchors the log as it is now",
        a.at,
        a.seq,
        path.display()
    ))
}

fn break_kind(k: ChainBreakKind) -> &'static str {
    match k {
        ChainBreakKind::MissingLink => "missing_link",
        ChainBreakKind::PrevHash => "prev_hash",
        ChainBreakKind::MissingEvent => "missing_event",
        ChainBreakKind::Hash => "hash",
        ChainBreakKind::Unlinked => "unlinked",
    }
}

/// What `verify_chain` found and the newest link, in one snapshot: the manifest's `audit_chain`.
async fn chain_facts(conn: &mut PgConnection) -> Result<(ChainCheck, Option<(i64, Vec<u8>)>)> {
    let check = verify_chain(&mut *conn).await?;
    let last = sqlx::query_as("select seq, hash from only public.audit_chain order by seq desc limit 1")
        .fetch_optional(&mut *conn)
        .await?;
    Ok((check, last))
}

fn chain_json(check: &ChainCheck, last: Option<&(i64, Vec<u8>)>) -> Value {
    json!({
        "seq": last.map_or(0, |l| l.0),
        "hash": last.map(|l| hex::encode(&l.1)),
        "verified": check.verified,
        "broken": check.broken.as_ref().map(|b| json!({"seq": b.seq, "event_id": b.event_id, "kind": break_kind(b.kind)})),
    })
}

/// The audit page's state of the log (`GET /api/audit/chain`): how far `verify_chain` holds, the
/// first link that does not, and whether the head the last backup anchored is still in the chain.
/// The anchor's hash and path stay out: the page only needs the verdict.
pub async fn audit_status(settings: &Settings, conn: &mut PgConnection) -> Result<Value> {
    let (verified, broken) = match verify_chain(&mut *conn).await {
        Ok(c) => (
            c.verified,
            c.broken.map(|b| json!({"seq": b.seq, "kind": break_kind(b.kind)})),
        ),
        Err(Error::AuditUncheckable(why)) => {
            tracing::warn!("audit chain: {why}");
            (0, Some(json!({"seq": null, "kind": "uncheckable"})))
        }
        Err(e) => return Err(e),
    };
    let anchor = match read_anchor(&anchor_file(settings)) {
        Ok(None) => json!({"state": "none", "seq": null, "at": null}),
        Ok(Some(a)) => {
            let state = if anchor_holds(&mut *conn, &a).await? {
                "ok"
            } else {
                "mismatch"
            };
            json!({"state": state, "seq": a.seq, "at": a.at})
        }
        Err(e) => {
            tracing::warn!("{e}");
            json!({"state": "unreadable", "seq": null, "at": null})
        }
    };
    let ok = broken.is_none() && matches!(anchor["state"].as_str(), Some("none" | "ok"));
    Ok(json!({"ok": ok, "verified": verified, "broken": broken, "anchor": anchor}))
}

/// Write a consistent backup directory at `dest` (must be missing or empty). Returns `dest`.
///
/// It refuses, before it writes anything, when the previous anchor no longer holds (see the module
/// doc), or cannot be read. A chain that `verify_chain` finds broken does not stop it: a broken link
/// stays broken, so refusing would stop every later backup of the health data. The manifest records
/// the break, the audit page shows it, and the anchor pins the chain as it is from then on.
pub async fn backup(settings: &Settings, dest: &Path) -> Result<PathBuf> {
    if dest.exists() && fs::read_dir(dest)?.next().is_some() {
        return Err(ops_error(format!("{} is not empty", dest.display())));
    }
    fs::create_dir_all(dest)?;
    owner_only(dest)?;
    let url = settings.sqlx_url();
    let anchor_path = anchor_file(settings);
    let anchored = read_anchor(&anchor_path)?;
    let mut conn = PgConnection::connect(&url).await?;
    let mut tx = conn.begin_with("begin isolation level repeatable read").await?;
    // pg_dump reads the snapshot this transaction exported, so the dump, the counts, the state
    // hashes and the audit head all describe the same instant even while the app keeps writing.
    let snapshot: String = sqlx::query_scalar("select pg_export_snapshot()")
        .fetch_one(&mut *tx)
        .await?;
    // Neither audit table can be renamed, swapped or altered until the backup ends.
    sqlx::raw_sql("lock table public.audit_event, public.audit_chain in access share mode")
        .execute(&mut *tx)
        .await?;
    if let Some(a) = &anchored
        && !anchor_holds(&mut tx, a).await?
    {
        return Err(anchor_mismatch(&anchor_path, a));
    }
    let (chain, last) = chain_facts(&mut tx).await?;
    if let Some(b) = &chain.broken {
        let at = b
            .seq
            .map_or_else(|| "an entry with no link".to_string(), |s| format!("entry {s}"));
        tracing::warn!(
            "the audit log is broken at {at} ({}): the backup goes on and its manifest records it",
            break_kind(b.kind)
        );
    }
    let out = fs::File::create(dest.join("db.dump"))?;
    let mut cmd = pg_command("pg_dump", dump_args(&url, &snapshot))?;
    let dump = cmd.stdout(out).stderr(Stdio::piped()).spawn()?;
    let facts = async {
        let referenced = referenced_blobs(&mut tx).await?;
        let migrations: Vec<i32> = sqlx::query_scalar("select version from schema_migrations order by 1")
            .fetch_all(&mut *tx)
            .await?;
        let tables = table_counts(&mut tx).await?;
        let as_of = Utc::now().date_naive();
        let states = state_hashes(&mut tx, as_of).await?;
        Ok::<_, Error>((referenced, migrations, tables, as_of, states))
    }
    .await;
    // The dump must finish before the exporting transaction ends, whatever the facts did.
    let done = dump.wait_with_output().await?;
    // build_state may store snapshots; the backup leaves the database as it was.
    tx.rollback().await?;
    conn.close().await?;
    let (referenced, migrations, tables, as_of, states) = facts?;
    if !done.status.success() {
        return Err(ops_error(format!(
            "pg_dump failed: {}",
            String::from_utf8_lossy(&done.stderr).trim()
        )));
    }

    // Blobs are written before the rows that reference them commit, so every blob the snapshot can
    // see exists by now. Later blobs are harmless extras.
    let src = settings.blobs_path().join("sha256");
    if src.is_dir() {
        copy_tree(&src, &dest.join("blobs").join("sha256"))?;
    }
    let files = blob_files(&dest.join("blobs"))?;
    let mut bytes = 0u64;
    for p in files.values() {
        bytes += fs::metadata(p)?.len();
    }
    let created_at = Utc::now().to_rfc3339_opts(SecondsFormat::Secs, false);
    let manifest = json!({
        "schema": MANIFEST_SCHEMA,
        "created_at": created_at,
        "audit_chain": chain_json(&chain, last.as_ref()),
        "app_version": env!("CARGO_PKG_VERSION"),
        "migrations": migrations,
        "rules_version": ownhealth_rules::rules_version()?,
        "as_of": as_of.to_string(),
        "tables": tables,
        "blobs": {
            "count": files.len(),
            "bytes": bytes,
            "sha256": files.keys().collect::<Vec<_>>(),
            "referenced": referenced,
        },
        "state_hash": states.into_iter().map(|(p, h)| (p, Value::String(h))).collect::<Map<String, Value>>(),
    });
    let text = serde_json::to_string_pretty(&sort_keys(manifest))? + "\n";
    fs::write(dest.join("manifest.json"), text)?;
    owner_only(dest)?;
    // Last, once the backup is whole: the next backup checks the live chain against this head.
    let (seq, hash) = last.unwrap_or((0, vec![0; 32]));
    write_anchor(
        &settings.anchor_path(),
        &Anchor {
            seq,
            hash,
            at: created_at,
        },
    )?;
    Ok(dest.to_path_buf())
}

fn py_repr(v: Option<&Value>) -> String {
    match v {
        None | Some(Value::Null) => "None".into(),
        Some(Value::String(s)) => format!("'{s}'"),
        Some(other) => other.to_string(),
    }
}

fn read_manifest(archive: &Path) -> Result<Value> {
    let path = archive.join("manifest.json");
    if !path.is_file() {
        return Err(ops_error(format!(
            "{} not found: point at the directory `ownhealth backup` wrote",
            path.display()
        )));
    }
    let manifest: Value = serde_json::from_slice(&fs::read(&path)?)?;
    if manifest.get("schema").and_then(Value::as_str) != Some(MANIFEST_SCHEMA) {
        return Err(ops_error(format!(
            "unknown backup schema {}, expected {MANIFEST_SCHEMA}",
            py_repr(manifest.get("schema"))
        )));
    }
    Ok(manifest)
}

async fn pg_restore(url: &str, archive: &Path) -> Result<()> {
    let dump = fs::File::open(archive.join("db.dump"))?;
    let mut cmd = pg_command("pg_restore", restore_args(url))?;
    let done = cmd
        .stdin(dump)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await?;
    if !done.status.success() {
        return Err(ops_error(format!(
            "pg_restore failed: {}",
            String::from_utf8_lossy(&done.stderr).trim()
        )));
    }
    Ok(())
}

fn str_list(v: &Value) -> Vec<String> {
    v.as_array()
        .map(|a| a.iter().filter_map(|s| s.as_str().map(str::to_string)).collect())
        .unwrap_or_default()
}

fn check_blobs(manifest: &Value, root: &Path) -> Result<Value> {
    let files = blob_files(root)?;
    let listed: BTreeSet<String> = str_list(&manifest["blobs"]["sha256"]).into_iter().collect();
    let mut expected = listed.clone();
    expected.extend(str_list(&manifest["blobs"]["referenced"])); // older manifests have no "referenced"
    let missing: Vec<&String> = expected.iter().filter(|s| !files.contains_key(*s)).collect();
    let mut corrupt = Vec::new();
    for (sha, p) in &files {
        if listed.contains(sha) && &sha256_file(p)? != sha {
            corrupt.push(sha.clone());
        }
    }
    Ok(json!({"checked": files.len(), "missing": missing, "corrupt": corrupt}))
}

/// Row counts, blob hashes and state_hash per profile against the manifest.
async fn compare(url: &str, manifest: &Value, blob_root: &Path) -> Result<Value> {
    let as_of_text = manifest["as_of"].as_str().unwrap_or_default();
    let as_of = NaiveDate::parse_from_str(as_of_text, "%Y-%m-%d")
        .map_err(|e| ops_error(format!("bad as_of {as_of_text:?} in the manifest: {e}")))?;
    // A backup from before the chain has no head to compare (older manifests, the upgrade fixtures).
    let expected_chain = manifest.get("audit_chain").filter(|v| v.is_object());
    let mut conn = PgConnection::connect(url).await?;
    let mut tx = conn.begin().await?;
    let counts = table_counts(&mut tx).await?; // before build_state, which may add state_snapshot rows
    let actual_chain = match expected_chain {
        Some(_) => {
            let (check, last) = chain_facts(&mut tx).await?;
            Some(chain_json(&check, last.as_ref()))
        }
        None => None,
    };
    let states = state_hashes(&mut tx, as_of).await?;
    tx.commit().await?;
    conn.close().await?;
    // The restored chain must end at the head the backup recorded, and hold as far as it did then.
    let audit = match (expected_chain, actual_chain) {
        (Some(e), Some(a)) => {
            let ok = ["seq", "hash", "verified"].iter().all(|k| e[k] == a[k]);
            json!({"expected": e, "actual": a, "ok": ok})
        }
        _ => Value::Null,
    };

    let mut tables = Map::new();
    if let Some(expected) = manifest["tables"].as_object() {
        for (name, n) in expected {
            let actual = counts.get(name).copied();
            if actual.and_then(|a| n.as_i64().map(|n| n == a)) != Some(true) {
                tables.insert(name.clone(), json!({"expected": n, "actual": actual}));
            }
        }
    }
    let actual: BTreeMap<&str, &str> = states.iter().map(|(p, h)| (p.as_str(), h.as_str())).collect();
    let mut profiles = Map::new();
    let empty = Map::new();
    let expected = manifest["state_hash"].as_object().unwrap_or(&empty);
    for (pid, h) in expected {
        let got = actual.get(pid.as_str()).copied();
        let ok = got.is_some() && h.as_str() == got;
        profiles.insert(pid.clone(), json!({"expected": h, "actual": got, "ok": ok}));
    }
    for (pid, h) in &states {
        if !expected.contains_key(pid) {
            profiles.insert(pid.clone(), json!({"expected": null, "actual": h, "ok": false}));
        }
    }
    let blobs = check_blobs(manifest, blob_root)?;
    let ok = tables.is_empty()
        && profiles.values().all(|p| p["ok"] == json!(true))
        && blobs["missing"].as_array().is_some_and(Vec::is_empty)
        && blobs["corrupt"].as_array().is_some_and(Vec::is_empty)
        && (audit.is_null() || audit["ok"] == json!(true));
    Ok(json!({
        "ok": ok,
        "as_of": as_of_text,
        "tables": tables,
        "profiles": profiles,
        "blobs": blobs,
        "audit_chain": audit,
    }))
}

/// Load a backup into the configured, empty database and blob directory, then check it.
/// Returns the same report as `verify_restore`, computed on the restored instance.
pub async fn restore(settings: &Settings, archive: &Path) -> Result<Value> {
    let manifest = read_manifest(archive)?;
    let url = settings.sqlx_url();
    let mut conn = PgConnection::connect(&url).await?;
    let tables: i64 = sqlx::query_scalar("select count(*) from pg_tables where schemaname = 'public'")
        .fetch_one(&mut conn)
        .await?;
    conn.close().await?;
    if tables > 0 {
        return Err(ops_error(
            "the target database is not empty: restore needs a fresh instance (new volume, app not started)",
        ));
    }
    // Check every blob before the database is loaded: a corrupt one found after pg_restore would
    // leave a populated database that a second attempt refuses as "not empty".
    let backup_blobs = blob_files(&archive.join("blobs"))?;
    for (sha, path) in &backup_blobs {
        if &sha256_file(path)? != sha {
            return Err(ops_error(format!("blob {sha} in the backup is corrupt")));
        }
    }
    pg_restore(&url, archive).await?;
    let root = settings.blobs_path();
    for (sha, path) in &backup_blobs {
        if sha.len() < 4 {
            continue;
        }
        let target = root.join("sha256").join(&sha[..2]).join(&sha[2..4]).join(sha);
        if !target.exists() {
            let parent = target.parent().unwrap_or(&root);
            fs::create_dir_all(parent)?;
            let part = parent.join(format!(".{sha}.part")); // copied whole, then renamed: never a truncated blob
            fs::copy(path, &part)?;
            fs::rename(&part, &target)?;
        }
    }
    compare(&url, &manifest, &root).await
}

/// The scratch database of verify-restore. A new database lets PUBLIC connect, and the server may
/// hold other roles (the wearables sidecar's own one), so only the owner may open this one.
async fn create_scratch(admin: &mut PgConnection, scratch: &str) -> Result<()> {
    sqlx::query(AssertSqlSafe(format!("create database \"{scratch}\"")))
        .execute(&mut *admin)
        .await?;
    sqlx::query(AssertSqlSafe(format!(
        "revoke connect, temporary on database \"{scratch}\" from public"
    )))
    .execute(&mut *admin)
    .await?;
    Ok(())
}

/// Restore into a scratch database `<db>_restore_check`, compare with the manifest, drop it.
/// `ok` is true only when every row count, every blob hash and every profile's state_hash match.
pub async fn verify_restore(settings: &Settings, archive: &Path) -> Result<Value> {
    let manifest = read_manifest(archive)?;
    let live = settings.sqlx_url();
    let scratch = format!("{}_restore_check", db_name(&live));
    let drop = format!("drop database if exists \"{scratch}\" with (force)");
    let mut admin = PgConnection::connect(&live).await?;
    sqlx::query(AssertSqlSafe(drop.clone())).execute(&mut admin).await?;
    create_scratch(&mut admin, &scratch).await?;
    let target = with_db(&live, &scratch);
    let result = async {
        pg_restore(&target, archive).await?;
        compare(&target, &manifest, &archive.join("blobs")).await
    }
    .await;
    let dropped = sqlx::query(AssertSqlSafe(drop)).execute(&mut admin).await;
    admin.close().await?;
    let report = result?;
    dropped?;
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// While verify-restore holds its scratch database, a plain role of the same server cannot open
    /// it, nor make temporary tables there.
    #[tokio::test]
    async fn the_scratch_database_is_closed_to_other_roles() -> anyhow::Result<()> {
        use ownhealth_core::ids::token_hex;
        let db = ownhealth_core::testing::TestDb::empty().await?;
        let scratch = format!("{}_restore_check", db.name);
        let (role, password) = (format!("oh_plain_{}", token_hex(4)), token_hex(16));
        let mut admin = PgConnection::connect(&db.url).await?;
        sqlx::query(AssertSqlSafe(format!(
            "create role \"{role}\" login password '{password}'"
        )))
        .execute(&mut admin)
        .await?;
        let created = create_scratch(&mut admin, &scratch).await;
        let (scheme, rest) = db.url.split_once("://").unwrap_or(("postgres", &db.url));
        let host = rest.rsplit_once('@').map_or(rest, |(_, h)| h);
        let plain = with_db(&format!("{scheme}://{role}:{password}@{host}"), &scratch);
        let opened = match PgConnection::connect(&plain).await {
            Ok(conn) => {
                conn.close().await?;
                "connected".to_string()
            }
            Err(e) => e.to_string(),
        };
        let temp: Option<bool> = sqlx::query_scalar("select has_database_privilege($1, $2, 'TEMPORARY')")
            .bind(&role)
            .bind(&scratch)
            .fetch_one(&mut admin)
            .await
            .ok();
        for sql in [
            format!("drop database if exists \"{scratch}\" with (force)"),
            format!("drop role if exists \"{role}\""),
        ] {
            sqlx::query(AssertSqlSafe(sql)).execute(&mut admin).await?;
        }
        admin.close().await?;
        created?;
        assert!(
            opened.contains("permission denied for database"),
            "a plain role opened the scratch database: {opened}"
        );
        assert_eq!(
            temp,
            Some(false),
            "a plain role may make temporary tables in the scratch database"
        );
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn backups_are_readable_by_their_owner_only() -> anyhow::Result<()> {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir()?;
        let dest = tmp.path().join("b1");
        fs::create_dir_all(dest.join("blobs/sha256/ab"))?;
        fs::write(dest.join("db.dump"), b"x")?;
        fs::write(dest.join("blobs/sha256/ab/cd"), b"y")?;
        owner_only(&dest)?;
        let mode = |p: &Path| fs::metadata(p).map(|m| m.permissions().mode() & 0o777);
        assert_eq!(mode(&dest)?, 0o700);
        assert_eq!(mode(&dest.join("blobs/sha256/ab"))?, 0o700);
        assert_eq!(mode(&dest.join("db.dump"))?, 0o600);
        assert_eq!(mode(&dest.join("blobs/sha256/ab/cd"))?, 0o600);
        Ok(())
    }

    #[test]
    fn the_password_stays_out_of_the_pg_tools_argv() {
        let (dsn, pw) = pg_conn("postgresql+psycopg://oh:s3cr%2Ft@db:5432/ownhealth?sslmode=require");
        assert!(!dsn.contains("s3cr") && dsn.starts_with("postgresql://oh@db:5432/ownhealth"));
        assert!(dsn.contains("sslmode=require"));
        assert_eq!(pw.as_deref(), Some("s3cr/t"));
        assert_eq!(
            pg_conn("postgresql://oh@db/ownhealth"),
            ("postgresql://oh@db/ownhealth".into(), None)
        );
    }

    #[test]
    fn backup_and_restore_never_put_the_password_on_a_command_line() {
        let url = "postgresql://oh:hunter2@db:5432/ownhealth";
        for (argv, pw) in [dump_args(url, "00000003-1"), restore_args(url)] {
            assert!(argv.iter().all(|a| !a.contains("hunter2")), "{argv:?}");
            assert!(argv.contains(&"postgresql://oh@db:5432/ownhealth".to_string()));
            assert_eq!(pw.as_deref(), Some("hunter2"));
        }
    }

    #[test]
    fn with_db_keeps_the_query() {
        assert_eq!(
            with_db("postgresql://u:p@h:1/live?sslmode=require", "x"),
            "postgresql://u:p@h:1/x?sslmode=require"
        );
        assert_eq!(with_db("postgresql+psycopg://u@h/live", "x"), "postgresql://u@h/x");
        assert_eq!(db_name("postgresql://u@h/live?a=b"), "live");
    }

    #[test]
    fn with_db_ignores_a_slash_in_the_query() {
        let url = "postgresql://u@h/live?sslrootcert=/etc/ca.pem";
        assert_eq!(db_name(url), "live");
        assert_eq!(with_db(url, "x"), "postgresql://u@h/x?sslrootcert=/etc/ca.pem");
        assert_eq!(
            with_db("postgresql://u@h/live?host=/var/run/postgresql", "x"),
            "postgresql://u@h/x?host=/var/run/postgresql"
        );
    }

    #[test]
    fn sort_keys_sorts_every_level() {
        let v = sort_keys(json!({"b": {"y": 1, "x": 2}, "a": [{"d": 1, "c": 2}]}));
        assert_eq!(
            serde_json::to_string(&v).unwrap_or_default(),
            r#"{"a":[{"c":2,"d":1}],"b":{"x":2,"y":1}}"#
        );
    }
}
