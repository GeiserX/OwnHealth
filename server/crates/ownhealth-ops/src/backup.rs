//! Backup, restore and verify-restore (`ops/__init__.py`, architecture.md 14).
//!
//! A backup is a plain directory. restic snapshots it (deploy/restic/), so restic does the dedup,
//! encryption and retention, and this module only has to make the directory consistent:
//!
//! ```text
//! <dest>/manifest.json    app version, migrations, row counts, blob list, state_hash per profile
//! <dest>/db.dump          pg_dump -Fc, taken from the same database snapshot as the counts and hashes,
//!                         without the rows of `profile_key`
//! <dest>/blobs/..         the blob store, hard-linked when on the same disk (blobs never change)
//! ```
//!
//! A profile's files are sealed under its key (slice 46, `ownhealth_core::keys`), and the files of an
//! item with no profile yet under the item's key (`pending/<item>/<address>` in the manifest). The
//! backup keeps them sealed. The profile keys never enter it: the dump leaves the rows of
//! `profile_key` out, and every backup rewrites `keyring.json` in OWNHEALTH_KEYRING_DIR instead
//! (`ownhealth_core::keyfile`), a folder outside the blob folder and every backup. The manifest lists
//! the profiles whose key the backup needs (`keys.keyring`) and the data key identifiers
//! (`keys.key_ids`). Restore and verify-restore take matching keys from the current keyring. A profile delete rewrites the keyring at once, so a backup taken
//! before the delete no longer opens that profile's files; its rows stay in the dump until retention
//! drops it. The keys of pending items still travel in the dump: an item's key is shredded when it
//! gets its profile, so the keyring of a later day would no longer open the item in an older backup.
//! The manifest lists each sealed file under `<profile>/<address>` with the sha256 of its bytes on
//! disk, so a check finds a changed or added file without the key, and records the key's
//! fingerprint (never the key), so a restore under another key refuses before it loads anything.
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
//! finds no anchor, checks nothing and writes one. A backup moves the anchor only onto a log that
//! holds, and only forward; a restore that passed its checks sets it to the restored head. The
//! audit page runs the same check (`audit_status`).

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};

use chrono::{NaiveDate, SecondsFormat, Utc};
use ownhealth_core::audit::{ChainBreakKind, ChainRead, verify_chain_with};
use ownhealth_core::blobs::{self, BlobOwner, BlobStore};
use ownhealth_core::config::Settings;
use ownhealth_core::keyfile::{self, owner_writes_only};
use ownhealth_core::keys::{self, Kek};
use ownhealth_core::{Error, Result};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use sqlx::{AssertSqlSafe, Connection, PgConnection, PgPool};
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

/// The dump leaves out the rows of `profile_key`: the profile keys live in the keyring file only.
fn dump_args(url: &str, snapshot: &str) -> (Vec<String>, Option<String>) {
    pg_args(
        url,
        &[
            "-Fc",
            &format!("--snapshot={snapshot}"),
            "--exclude-table-data=public.profile_key",
        ],
    )
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

/// Every blob a document, genome dataset, held mail or inbox item row points at, with its owner (the
/// profile, or a document with no profile yet or a held mail under its own item key), so verify can
/// tell a blob the live store had already lost from one the backup lost. An inbox item's raw bytes
/// belong to the profile it was filed to, else to its held mail ([`BlobOwner::of_item`]); a mail
/// released or deleted from the hold no longer needs its held copy.
async fn referenced_blobs(conn: &mut PgConnection) -> Result<Vec<(BlobOwner, String)>> {
    let cols: Vec<String> = sqlx::query_scalar(
        "select column_name::text from information_schema.columns where table_schema = 'public' \
         and table_name = 'genome_dataset' and data_type = 'bytea' and column_name like '%\\_blob'",
    )
    .fetch_all(&mut *conn)
    .await?;
    let mut parts = vec![
        "select profile_id as p, case when profile_id is null then id end as d, blob_sha256 as h from document"
            .to_string(),
    ];
    parts.extend(
        cols.iter()
            .map(|c| format!("select profile_id, null::uuid, \"{c}\" from genome_dataset")),
    );
    parts.push("select null::uuid, id, raw_blob from quarantine_item where status = 'held'".into());
    parts.push(
        "select i.profile_id, i.quarantine_id, i.blob_sha256 from source_item i \
         left join quarantine_item q on q.id = i.quarantine_id \
         where i.profile_id is not null or i.quarantine_id is null or q.status = 'held'"
            .into(),
    );
    let q = format!(
        "select distinct p, d, encode(h, 'hex') from ({}) x where h is not null order by 3, 1, 2",
        parts.join(" union ")
    );
    let rows: Vec<(Option<Uuid>, Option<Uuid>, String)> =
        sqlx::query_as(AssertSqlSafe(q)).fetch_all(&mut *conn).await?;
    Ok(rows
        .into_iter()
        .map(|(p, d, h)| (BlobOwner::of_item(p, d), h))
        .collect())
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
    let dir = path.parent().unwrap_or(Path::new("."));
    let checked = owner_writes_only(dir, true, "anchor").and_then(|_| owner_writes_only(path, false, "anchor"));
    match checked {
        Ok(true) => {}
        Ok(false) => return Ok(None),
        Err(why) => return Err(anchor_unreadable(path, &why)),
    }
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

/// Replace the anchor in `dir` atomically: a new file only the owner may read (0600), written and
/// synced under a temporary name, then renamed over the old one and the folder synced. A crash
/// leaves the old anchor or the new one, never half of either. A missing folder is made 0700; a
/// folder that is there keeps its mode (`read_anchor` already refused one others may write).
fn write_anchor(dir: &Path, a: &Anchor) -> Result<()> {
    let body = serde_json::to_string_pretty(&json!({
        "schema": ANCHOR_SCHEMA,
        "seq": a.seq,
        "hash": hex::encode(&a.hash),
        "at": a.at,
    }))? + "\n";
    let tmp = dir.join(format!(".{ANCHOR_FILE}.{}.tmp", ownhealth_core::ids::token_hex(6)));
    let written = (|| -> std::io::Result<()> {
        if fs::symlink_metadata(dir).is_err() {
            let mut b = fs::DirBuilder::new();
            b.recursive(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                b.mode(0o700);
            }
            b.create(dir)?;
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

/// One backup or restore at a time reads and replaces the anchor: a session advisory lock on the
/// database, held from the anchor read to the anchor write. Without it a backup that started
/// earlier, with an older head, could finish last and put its lower head over a newer one. Closing
/// the connection releases it, so a backup that fails on the way leaves nothing locked.
async fn anchor_lock(url: &str) -> Result<PgConnection> {
    let mut conn = PgConnection::connect(url).await?;
    sqlx::query("select pg_catalog.pg_advisory_lock(pg_catalog.hashtext('ownhealth.audit_anchor'))")
        .execute(&mut conn)
        .await?;
    Ok(conn)
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

/// `verify_chain_with` for the anchored link (seq 0 needs no read), with an uncheckable log as None:
/// like a broken one, it does not stop a backup or the page.
async fn chain_read(conn: &mut PgConnection, anchored: Option<&Anchor>) -> Result<Option<ChainRead>> {
    match verify_chain_with(conn, anchored.map(|a| a.seq).filter(|s| *s > 0)).await {
        Ok(r) => Ok(Some(r)),
        Err(Error::AuditUncheckable(why)) => {
            tracing::warn!("audit chain: {why}");
            Ok(None)
        }
        Err(e) => Err(e),
    }
}

/// The anchored link is still in the chain with the hash the anchor kept. None: the log cannot be
/// checked, so neither can the anchor.
fn anchor_holds(read: Option<&ChainRead>, a: &Anchor) -> Option<bool> {
    read.map(|r| a.seq == 0 || r.link.as_deref() == Some(a.hash.as_slice()))
}

/// The manifest's `audit_chain`: the newest link and what `verify_chain` found, or `uncheckable`.
fn chain_json(read: Option<&ChainRead>) -> Value {
    match read {
        Some(r) => json!({
            "seq": r.newest.as_ref().map_or(0, |l| l.0),
            "hash": r.newest.as_ref().map(|l| hex::encode(&l.1)),
            "verified": r.check.verified,
            "broken": r.check.broken.as_ref().map(|b| json!({"seq": b.seq, "event_id": b.event_id, "kind": break_kind(b.kind)})),
        }),
        None => json!({
            "seq": null,
            "hash": null,
            "verified": 0,
            "broken": {"seq": null, "event_id": null, "kind": "uncheckable"},
        }),
    }
}

/// The audit page's state of the log (`GET /api/audit/chain`): how far `verify_chain` holds, the
/// first link that does not, and whether the head the last backup anchored is still in the chain
/// (`unchecked` while the log itself cannot be checked). The anchor's hash and path stay out: the
/// page only needs the verdict.
pub async fn audit_status(settings: &Settings, conn: &mut PgConnection) -> Result<Value> {
    let anchored = read_anchor(&anchor_file(settings));
    let read = chain_read(&mut *conn, anchored.as_ref().ok().and_then(Option::as_ref)).await?;
    let (verified, broken) = match &read {
        Some(r) => (
            r.check.verified,
            r.check
                .broken
                .as_ref()
                .map(|b| json!({"seq": b.seq, "kind": break_kind(b.kind)})),
        ),
        None => (0, Some(json!({"seq": null, "kind": "uncheckable"}))),
    };
    let anchor = match anchored {
        Ok(None) => json!({"state": "none", "seq": null, "at": null}),
        Ok(Some(a)) => {
            let state = match anchor_holds(read.as_ref(), &a) {
                Some(true) => "ok",
                Some(false) => "mismatch",
                None => "unchecked",
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

/// How long the page reuses a verdict while the newest link and the anchor file stay the same.
const VERDICT_FOR: Duration = Duration::from_secs(60);

/// What a reused verdict must still match: the newest link, and the anchor file's bytes.
type Print = (Option<(i64, Vec<u8>)>, Option<Vec<u8>>);

#[derive(Default)]
struct VerdictSlot {
    last: Option<(Instant, Print, Value)>,
    checks: u64,
}

type SharedSlot = Arc<tokio::sync::Mutex<VerdictSlot>>;

/// One slot per database and anchor file, so two instances in one process (the tests) never share
/// a verdict.
static VERDICTS: LazyLock<std::sync::Mutex<HashMap<(String, PathBuf), SharedSlot>>> = LazyLock::new(Default::default);

fn verdict_slot(settings: &Settings) -> SharedSlot {
    let mut slots = VERDICTS.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    slots
        .entry((settings.sqlx_url(), anchor_file(settings)))
        .or_default()
        .clone()
}

/// `audit_status` for the page, which every signed-in person may load. A full check reads the whole
/// log, so only one runs at a time, and a verdict younger than a minute is reused while the newest
/// link and the anchor file are what they were. A new entry, a rehash or a new anchor checks again
/// at once; an edit with no new entry shows within the minute. Callers wait for the running check
/// without holding a pooled connection.
pub async fn audit_status_shared(settings: &Settings, pool: &PgPool) -> Result<Value> {
    let shared = verdict_slot(settings);
    let mut slot = shared.lock().await;
    let mut conn = pool.acquire().await?;
    let newest = sqlx::query_as("select seq, hash from only public.audit_chain order by seq desc limit 1")
        .fetch_optional(&mut *conn)
        .await?;
    let print: Print = (newest, fs::read(anchor_file(settings)).ok());
    if let Some((at, p, v)) = &slot.last
        && at.elapsed() < VERDICT_FOR
        && *p == print
    {
        return Ok(v.clone());
    }
    let v = audit_status(settings, &mut conn).await?;
    slot.checks += 1;
    slot.last = Some((Instant::now(), print, v.clone()));
    Ok(v)
}

/// How many full checks `audit_status_shared` ran for this database and anchor (for tests).
#[doc(hidden)]
pub async fn audit_status_checks(settings: &Settings) -> u64 {
    verdict_slot(settings).lock().await.checks
}

/// Write a consistent backup directory at `dest` (must be missing or empty). Returns `dest`.
///
/// It refuses, before it writes anything, when the previous anchor no longer holds (see the module
/// doc), or cannot be read. A chain that `verify_chain` finds broken, or cannot check, does not stop
/// it: a broken link stays broken, so refusing would stop every later backup of the health data. The
/// manifest records the break, the audit page shows it, and the anchor stays as it was: a backup
/// moves the anchor only onto a log that holds, and only forward.
pub async fn backup(settings: &Settings, dest: &Path) -> Result<PathBuf> {
    if dest.exists() && fs::read_dir(dest)?.next().is_some() {
        return Err(ops_error(format!("{} is not empty", dest.display())));
    }
    // The keys stay out of every backup: refuse a keyring the backup would hold, and one that cannot
    // be read, before anything is written.
    let ring = keyfile::path(settings);
    if keyfile::inside(&settings.keyring_path(), dest).map_err(|e| ops_error(e.0))? {
        return Err(ops_error(format!(
            "the keyring {} is inside the backup folder {}: set OWNHEALTH_KEYRING_DIR to a folder outside \
             every backup, see deploy/README.md",
            ring.display(),
            dest.display()
        )));
    }
    keyfile::check_folder(settings).map_err(|e| ops_error(e.0))?;
    keyfile::read_for_install(settings).map_err(|e| ops_error(e.0))?;
    if settings.keyring_dir.is_none() {
        tracing::warn!(
            "OWNHEALTH_KEYRING_DIR is not set, so the keyring is {} on the data volume: a host backup \
             of that volume keeps older copies of it, and an older copy opens a deleted profile's files \
             again. Mount a folder of its own and set OWNHEALTH_KEYRING_DIR, see deploy/README.md",
            ring.display()
        );
    }
    let url = settings.sqlx_url();
    let anchor_path = anchor_file(settings);
    let mut lock = anchor_lock(&url).await?;
    // From before the snapshot until the copy of the files below: a file a row of the snapshot names
    // stays (a held mail released, a profile deleted or a document moved meanwhile leaves its files
    // for later), so the copy has every one of them.
    blobs::hold_for_copy(&mut lock).await?;
    let anchored = read_anchor(&anchor_path)?;
    let mut conn = PgConnection::connect(&url).await?;
    let mut tx = conn.begin_with("begin isolation level repeatable read").await?;
    // The lock comes first: the snapshot is taken by the first statement that reads, so neither audit
    // table can be renamed, swapped or altered between the snapshot and the end of the backup.
    sqlx::raw_sql("lock table public.audit_event, public.audit_chain in access share mode")
        .execute(&mut *tx)
        .await?;
    // pg_dump reads the snapshot this transaction exported, so the dump, the counts, the state
    // hashes and the audit head all describe the same instant even while the app keeps writing.
    let snapshot: String = sqlx::query_scalar("select pg_export_snapshot()")
        .fetch_one(&mut *tx)
        .await?;
    let read = chain_read(&mut tx, anchored.as_ref()).await?;
    if let Some(a) = &anchored
        && anchor_holds(read.as_ref(), a) == Some(false)
    {
        return Err(anchor_mismatch(&anchor_path, a));
    }
    let broken = match &read {
        Some(r) => r.check.broken.as_ref().map(|b| {
            let at = b
                .seq
                .map_or_else(|| "an entry with no link".to_string(), |s| format!("entry {s}"));
            format!("broken at {at} ({})", break_kind(b.kind))
        }),
        None => Some("cannot be checked".to_string()),
    };
    if let Some(why) = &broken {
        tracing::warn!(
            "the audit log {why}: the backup goes on, its manifest records it, and the anchor stays as it was"
        );
    }
    fs::create_dir_all(dest)?;
    owner_only(dest)?;
    let staged_dump = tempfile::NamedTempFile::new_in(dest)?;
    let mut cmd = pg_command("pg_dump", dump_args(&url, &snapshot))?;
    let dump = cmd.stdout(staged_dump.reopen()?).stderr(Stdio::piped()).spawn()?;
    let facts = async {
        let referenced = referenced_blobs(&mut tx).await?;
        let wrapped = keys::stored_all(&mut tx).await?;
        let migrations: Vec<i32> = sqlx::query_scalar("select version from schema_migrations order by 1")
            .fetch_all(&mut *tx)
            .await?;
        let tables = table_counts(&mut tx).await?;
        let kept: i64 = sqlx::query_scalar("select count(*) from profile_key where wrapped_dek is not null")
            .fetch_one(&mut *tx)
            .await?;
        let as_of = Utc::now().date_naive();
        let states = state_hashes(&mut tx, as_of).await?;
        Ok::<_, Error>((referenced, wrapped, migrations, tables, kept, as_of, states))
    }
    .await;
    // The dump must finish before the exporting transaction ends, whatever the facts did.
    let done = dump.wait_with_output().await?;
    // build_state may store snapshots; the backup leaves the database as it was.
    tx.rollback().await?;
    conn.close().await?;
    let (referenced, wrapped, migrations, mut tables, kept, as_of, states) = facts?;
    if !done.status.success() {
        return Err(ops_error(format!(
            "pg_dump failed: {}",
            String::from_utf8_lossy(&done.stderr).trim()
        )));
    }
    let kek = settings
        .secrets_key
        .as_ref()
        .map(|_| Kek::from_settings(settings))
        .transpose()?;
    let mut deks = HashMap::new();
    let mut key_ids = BTreeMap::new();
    for (slot, wrapped, fingerprint) in &wrapped {
        let dek = kek.as_ref().map(|k| k.unwrap_slot(*slot, fingerprint, wrapped));
        if let keys::Slot::Profile(profile) = slot {
            let dek = dek
                .transpose()?
                .ok_or_else(|| ops_error("profile keys require OWNHEALTH_SECRETS_KEY"))?;
            key_ids.insert(profile.to_string(), dek.archive_id());
            deks.insert(*slot, dek);
        } else if let Some(Ok(dek)) = dek {
            deks.insert(*slot, dek);
        }
    }
    // The dump holds no row of it, so a restore never has the same count.
    tables.remove("profile_key");
    // Publish the dump only after the locked keyring rewrite succeeds. A refused rewrite drops
    // this attempt's staging file, so the same destination can be used again.
    keyfile::rewrite(&mut lock, settings)
        .await
        .map_err(|e| ops_error(e.0))?;
    staged_dump
        .persist_noclobber(dest.join("db.dump"))
        .map_err(|e| e.error)?;

    // Blobs are written before the rows that reference them commit, so every blob the snapshot can
    // see exists by now, and none of them went since (the copy lock above). Later blobs are harmless
    // extras.
    let store = BlobStore::for_settings(settings)?;
    let tree = dest.join("blobs");
    store.copy_into(&tree)?;
    blobs::end_copy(&mut lock).await?;
    let mut files = blobs::tree_files(&tree)?;
    // A file that is not where its name puts it is never read and no restore places it: it stays out
    // of the backup and its manifest, so it cannot stand in for the file its name stands for.
    files.retain(|key, p| {
        let stray = key.starts_with("?/");
        if stray {
            tracing::warn!(file = %key, "a blob file is not where its name puts it: left out of the backup");
            let _ = fs::remove_file(p);
        }
        !stray
    });
    let mut bytes = 0u64;
    let mut plain = Vec::new();
    let mut sealed = Map::new();
    for (key, p) in &files {
        bytes += fs::metadata(p)?.len();
        if key.contains('/') {
            sealed.insert(key.clone(), Value::String(sha256_file(p)?));
        } else {
            plain.push(key);
        }
    }
    // Where each referenced blob sits in this backup: its owner's sealed copy, or the plain one. The
    // keys come from the snapshot and the files from the copy, never from the live store, where a
    // delete after the copy may already have shredded a key or removed a folder. A key that does not
    // open names no sealed file here; verify-restore reports it.
    let live_slots: HashSet<_> = wrapped.iter().map(|(slot, _, _)| *slot).collect();
    let mut required = Vec::new();
    for (owner, sha) in &referenced {
        let slot = owner.slot();
        if let Some(slot) = slot
            && let Some(dek) = deks.get(&slot)
        {
            let key = blobs::sealed_key(slot, dek, sha)?;
            if files.contains_key(&key) {
                required.push(key);
                continue;
            }
        }
        let plain = blobs::sha256_hex(sha)?;
        // As in sweep_unkeyed, a sealed folder without a stored key is expendable. Its references
        // can survive a partial restore. A plain copy is still usable and stays required.
        if files.contains_key(&plain) || slot.is_none_or(|slot| live_slots.contains(&slot)) || kek.is_none() {
            required.push(plain);
        }
    }
    required.sort();
    required.dedup();
    let keyring: Vec<_> = key_ids.keys().collect();
    let fingerprint = kek.as_ref().map(|k| hex::encode(k.fingerprint()));
    let key_facts = json!({"kek_fingerprint": fingerprint, "profiles": kept, "keyring": keyring, "key_ids": key_ids});
    let created_at = Utc::now().to_rfc3339_opts(SecondsFormat::Secs, false);
    let manifest = json!({
        "schema": MANIFEST_SCHEMA,
        "created_at": created_at,
        "audit_chain": chain_json(read.as_ref()),
        "app_version": env!("CARGO_PKG_VERSION"),
        "migrations": migrations,
        "rules_version": ownhealth_rules::rules_version()?,
        "as_of": as_of.to_string(),
        "tables": tables,
        "blobs": {
            "count": files.len(),
            "bytes": bytes,
            "sha256": plain,
            "sealed": sealed,
            "referenced": required,
        },
        "keys": key_facts,
        "state_hash": states.into_iter().map(|(p, h)| (p, Value::String(h))).collect::<Map<String, Value>>(),
    });
    let text = serde_json::to_string_pretty(&sort_keys(manifest))? + "\n";
    fs::write(dest.join("manifest.json"), text)?;
    owner_only(dest)?;
    // Last, once the backup is whole: the next backup checks the live chain against this head. Only
    // a log that holds moves the anchor, and only forward (see `anchor_lock`).
    if let Some(r) = read.as_ref().filter(|r| r.check.ok()) {
        let (seq, hash) = r.newest.clone().unwrap_or((0, vec![0; 32]));
        if anchored.as_ref().is_none_or(|a| seq > a.seq) {
            write_anchor(
                &settings.anchor_path(),
                &Anchor {
                    seq,
                    hash,
                    at: created_at,
                },
            )?;
        }
    }
    lock.close().await?;
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

/// The sha256 of each sealed file's bytes on disk, as the manifest lists them.
fn sealed_list(manifest: &Value) -> BTreeMap<String, String> {
    manifest["blobs"]["sealed"]
        .as_object()
        .map(|m| {
            m.iter()
                .filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_string())))
                .collect()
        })
        .unwrap_or_default()
}

/// A blob file that is not what the manifest says: a plain one whose bytes have another sha256, a
/// sealed one whose bytes changed, or a sealed one the manifest does not list (a plain file names its
/// own hash, a sealed one cannot be checked without the key).
fn corrupt_file(key: &str, path: &Path, listed: &BTreeSet<String>, sealed: &BTreeMap<String, String>) -> Result<bool> {
    Ok(if key.contains('/') {
        sealed.get(key) != Some(&sha256_file(path)?)
    } else {
        listed.contains(key) && sha256_file(path)? != key
    })
}

/// Every blob file a backup must hold: the plain and sealed files its manifest lists, and the ones the
/// database references (older manifests have no "referenced").
fn expected_blobs(manifest: &Value) -> BTreeSet<String> {
    let mut expected: BTreeSet<String> = str_list(&manifest["blobs"]["sha256"]).into_iter().collect();
    expected.extend(sealed_list(manifest).into_keys());
    expected.extend(str_list(&manifest["blobs"]["referenced"]));
    expected
}

fn check_blobs(manifest: &Value, files: &BTreeMap<String, PathBuf>) -> Result<Value> {
    let listed: BTreeSet<String> = str_list(&manifest["blobs"]["sha256"]).into_iter().collect();
    let sealed = sealed_list(manifest);
    let expected = expected_blobs(manifest);
    let missing: Vec<&String> = expected.iter().filter(|s| !files.contains_key(*s)).collect();
    let mut corrupt = Vec::new();
    for (key, p) in files {
        if corrupt_file(key, p, &listed, &sealed)? {
            corrupt.push(key.clone());
        }
    }
    Ok(json!({"checked": files.len(), "missing": missing, "corrupt": corrupt}))
}

/// The key a backup is read with: the configured one, which must be the key the backup was made
/// under. Checked before anything is loaded.
fn backup_key(settings: &Settings, manifest: &Value) -> Result<Option<Kek>> {
    let kek = match &settings.secrets_key {
        Some(_) => Some(Kek::from_settings(settings)?),
        None => None,
    };
    if let Some(want) = manifest["keys"]["kek_fingerprint"].as_str() {
        let Some(k) = &kek else {
            // a new key never opens a backup: name the one it was made with
            return Err(ops_error(format!(
                "OWNHEALTH_SECRETS_KEY is not set: set the key the backup was made with (fingerprint {want}), \
                 see deploy/README.md"
            )));
        };
        let have = hex::encode(k.fingerprint());
        if have != want {
            return Err(ops_error(format!(
                "this backup was made under another OWNHEALTH_SECRETS_KEY (fingerprint {want}; the configured \
                 key has {have}): set the key the backup was made with"
            )));
        }
    }
    Ok(kek)
}

/// The profile keys of a backup, as the keyring holds them now.
struct RingKeys {
    legacy: bool,
    /// Rows added for tombstones the older dump did not count.
    legacy_tombstones_added: i64,
    fingerprint: Vec<u8>,
    /// the wrapped key of each profile the keyring still holds
    keys: Vec<(Uuid, Vec<u8>)>,
    /// profiles of the backup whose key the keyring lists as shredded: deleted since the backup
    shredded: Vec<Uuid>,
    /// every profile the keyring lists as shredded, also one deleted before the backup: each gets its
    /// shredded row back, so no new key is ever made for it
    tombstones: Vec<Uuid>,
    /// profiles for which the keyring has no matching key or tombstone
    missing: Vec<Uuid>,
    /// the operator chose to restore without the keys the keyring lacks (`--allow-missing-keys`)
    allow_missing: bool,
}

/// The flag that lets a restore go on without the keys the keyring lacks.
const ALLOW_MISSING: &str = "--allow-missing-keys";

/// Select and authenticate keys before the restore loads anything. Older manifests use UUIDs
/// alone. A legacy dump carries its own keys, but must still apply current tombstones.
fn ring_keys(
    settings: &Settings,
    manifest: &Value,
    kek: Option<&Kek>,
    allow_missing: bool,
) -> Result<Option<RingKeys>> {
    let ids = manifest["keys"]
        .get("keyring")
        .map(|ids| {
            ids.as_array()
                .ok_or_else(|| ops_error("bad keys.keyring list in the manifest"))
        })
        .transpose()?;
    let key_ids = manifest["keys"]
        .get("key_ids")
        .map(|map| {
            map.as_object()
                .ok_or_else(|| ops_error("bad keys.key_ids map in the manifest"))
        })
        .transpose()?;
    if key_ids.is_some_and(|map| ids.is_none_or(|ids| map.len() != ids.len())) {
        return Err(ops_error(
            "keys.key_ids does not match the manifest's keys.keyring list",
        ));
    }
    let file = keyfile::path(settings);
    let ring = match keyfile::read(&file).map_err(|e| ops_error(e.0))? {
        Some(ring) => ring,
        None if ids.is_none_or(Vec::is_empty) || allow_missing => keyfile::KeyringFile::default(),
        None => {
            return Err(ops_error(format!(
                "no keyring at {}: this backup holds no profile key, they are in keyring.json on the keyring \
                 mount (OWNHEALTH_KEYRING_DIR). Put the newest keyring.json there and run it again, or \
                 restore with {ALLOW_MISSING} to load the archive with unavailable profile files sealed. \
                 In this attempt, nothing was loaded. See deploy/README.md",
                file.display()
            )));
        }
    };
    let want = kek.map(|k| hex::encode(k.fingerprint()));
    let have = ring.kek_fingerprint.as_ref().map(hex::encode);
    if let (Some(want), Some(have)) = (&want, &have)
        && want != have
    {
        return Err(ops_error(format!(
            "the keyring {} was made under another OWNHEALTH_SECRETS_KEY (fingerprint {have}) than this \
             backup ({want}): put back this install's keyring.json; nothing was loaded",
            file.display()
        )));
    }
    if ids.is_none() && ring.shredded.is_empty() {
        return Ok(None);
    }
    let mut out = RingKeys {
        legacy: ids.is_none(),
        legacy_tombstones_added: 0,
        fingerprint: ring.kek_fingerprint.clone().unwrap_or_default(),
        keys: Vec::new(),
        shredded: Vec::new(),
        tombstones: ring.shredded.iter().copied().collect(),
        missing: Vec::new(),
        allow_missing,
    };
    for id in ids.into_iter().flatten() {
        let id = id
            .as_str()
            .and_then(|i| Uuid::parse_str(i).ok())
            .ok_or_else(|| ops_error(format!("bad profile id {id} in the manifest's keys.keyring")))?;
        let wanted = key_ids
            .map(|map| {
                map.get(&id.to_string())
                    .and_then(Value::as_str)
                    .filter(|value| value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit()))
                    .ok_or_else(|| ops_error(format!("bad key id for profile {id} in the manifest's keys.key_ids")))
            })
            .transpose()?;
        if let Some(w) = ring.profiles.get(&id) {
            let dek = kek
                .and_then(|k| k.unwrap(id, &out.fingerprint, w).ok())
                .ok_or_else(|| {
                    ops_error(format!(
                        "the keyring {} has a key for profile {id} that cannot be opened with \
                     OWNHEALTH_SECRETS_KEY: put back this install's keyring.json; nothing was loaded",
                        file.display()
                    ))
                })?;
            if wanted.is_none_or(|wanted| wanted == dek.archive_id()) {
                out.keys.push((id, w.clone()));
            } else {
                out.missing.push(id);
            }
        } else if ring.shredded.contains(&id) {
            out.shredded.push(id);
        } else {
            out.missing.push(id);
        }
    }
    Ok(Some(out))
}

/// Load selected keys into a new-format dump and apply the current tombstones to any dump with a
/// `profile_key` table. A partial restore leaves missing profiles without a key row.
async fn load_keys(url: &str, ring: &mut RingKeys) -> Result<()> {
    let mut conn = PgConnection::connect(url).await?;
    let mut tx = conn.begin().await?;
    if ring.legacy {
        let keyed: bool = sqlx::query_scalar("select to_regclass('public.profile_key') is not null")
            .fetch_one(&mut *tx)
            .await?;
        if !keyed {
            return Ok(());
        }
        ring.legacy_tombstones_added = sqlx::query_scalar(
            "select count(*) from unnest($1::uuid[]) as tombstone(id) \
             where not exists (select 1 from profile_key where profile_id = tombstone.id)",
        )
        .bind(&ring.tombstones)
        .fetch_one(&mut *tx)
        .await?;
    }
    for (id, wrapped) in &ring.keys {
        sqlx::query("insert into profile_key (profile_id, wrapped_dek, kek_fingerprint) values ($1, $2, $3)")
            .bind(id)
            .bind(wrapped)
            .bind(&ring.fingerprint)
            .execute(&mut *tx)
            .await?;
    }
    sqlx::query(
        "insert into profile_key (profile_id) select unnest($1::uuid[]) \
         on conflict (profile_id) do update set wrapped_dek = null, kek_fingerprint = null \
         where profile_key.wrapped_dek is not null",
    )
    .bind(&ring.tombstones)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    conn.close().await?;
    Ok(())
}

fn ids_text(ids: &[Uuid]) -> String {
    ids.iter().map(Uuid::to_string).collect::<Vec<_>>().join(", ")
}

/// Row counts, blob hashes and state_hash per profile against the manifest. With the keyring's keys
/// (`ring`), the key report also lists the profiles deleted since the backup (`shredded`, whose files
/// stay closed) and the ones the keyring lacks (`missing`, which fails the check).
async fn compare(
    url: &str,
    manifest: &Value,
    files: &BTreeMap<String, PathBuf>,
    kek: Option<&Kek>,
    ring: Option<&RingKeys>,
) -> Result<Value> {
    let as_of_text = manifest["as_of"].as_str().unwrap_or_default();
    let as_of = NaiveDate::parse_from_str(as_of_text, "%Y-%m-%d")
        .map_err(|e| ops_error(format!("bad as_of {as_of_text:?} in the manifest: {e}")))?;
    // A backup from before the chain has no head to compare (older manifests, the upgrade fixtures).
    let expected_chain = manifest.get("audit_chain").filter(|v| v.is_object());
    let mut conn = PgConnection::connect(url).await?;
    let mut tx = conn.begin().await?;
    let mut counts = table_counts(&mut tx).await?; // before build_state, which may add state_snapshot rows
    if let (Some(ring), Some(count)) = (ring, counts.get_mut("profile_key")) {
        *count -= ring.legacy_tombstones_added;
    }
    let actual_chain = match expected_chain {
        Some(_) => {
            let read = chain_read(&mut tx, None).await?;
            Some(chain_json(read.as_ref()))
        }
        None => None,
    };
    // every wrapped profile key must open under the configured key, or its files are lost (a dump
    // from before 1.0 has no key table: nothing to open)
    let keyed: bool = sqlx::query_scalar("select to_regclass('public.profile_key') is not null")
        .fetch_one(&mut *tx)
        .await?;
    let mut key_check = match kek.filter(|_| keyed) {
        Some(k) => {
            let (opened, failed) = keys::check_all(&mut tx, k).await?;
            json!({"checked": opened + failed.len(), "failed": failed, "ok": failed.is_empty()})
        }
        None => Value::Null,
    };
    if let Some(r) = ring {
        if key_check.is_null() {
            key_check = json!({"ok": true});
        }
        key_check["shredded"] = json!(r.shredded);
        key_check["missing"] = json!(r.missing);
        if r.allow_missing {
            key_check["missing_allowed"] = Value::Bool(true);
        } else if !r.missing.is_empty() {
            key_check["ok"] = Value::Bool(false);
        }
    }
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
    let blobs = check_blobs(manifest, files)?;
    let ok = tables.is_empty()
        && profiles.values().all(|p| p["ok"] == json!(true))
        && blobs["missing"].as_array().is_some_and(Vec::is_empty)
        && blobs["corrupt"].as_array().is_some_and(Vec::is_empty)
        && (audit.is_null() || audit["ok"] == json!(true))
        && (key_check.is_null() || key_check["ok"] == json!(true));
    Ok(json!({
        "ok": ok,
        "as_of": as_of_text,
        "tables": tables,
        "profiles": profiles,
        "blobs": blobs,
        "audit_chain": audit,
        "keys": key_check,
    }))
}

/// Load a backup into the configured, empty database and blob directory, then check it.
/// Returns the same report as `verify_restore`, computed on the restored instance, plus
/// `audit_anchor`: the head it anchored, or null.
///
/// The profile keys come from the keyring file as it is now (`ring_keys`). Without a keyring, or when
/// it lacks the key of a profile of the backup that it does not list as deleted, nothing is loaded. A
/// profile deleted since the backup comes back with its rows and a shredded key, so its files stay
/// closed (`keys.shredded` in the report).
///
/// [`restore_with`] with `allow_missing_keys` goes on in those two cases (a keyring copy older than
/// the backup, or none left after a dead disk): every row and file is loaded, the profiles whose key
/// the keyring holds open their files, and the others are listed under `keys.missing`, with their
/// records and without their files.
///
/// When every check passed and the backup's log held, the restored log now ends at the head the
/// manifest recorded, so the anchor is set to that head: the next backup and the audit page compare
/// against the log as restored, not against a newer head the restored database never had. Otherwise
/// the anchor stays as it was. A folder or file the backup would refuse gets no anchor either, and
/// `audit_anchor_error` says why.
pub async fn restore(settings: &Settings, archive: &Path) -> Result<Value> {
    restore_with(settings, archive, false).await
}

/// [`restore`], told whether to go on without the keys the keyring lacks (`--allow-missing-keys`).
pub async fn restore_with(settings: &Settings, archive: &Path, allow_missing_keys: bool) -> Result<Value> {
    let manifest = read_manifest(archive)?;
    let kek = backup_key(settings, &manifest)?;
    let ring_lock = keyfile::lock(settings).await.map_err(|e| ops_error(e.0))?;
    let mut ring = ring_keys(settings, &manifest, kek.as_ref(), allow_missing_keys)?;
    if let Some(r) = ring.as_ref().filter(|r| !r.missing.is_empty() && !r.allow_missing) {
        return Err(ops_error(format!(
            "the keyring {} holds no key for {} profile(s) of this backup and does not list them as \
             deleted ({}). The keyring may be out of date or hold a different key for the same profile. \
             Put the matching keyring.json \
             in place, or restore with {ALLOW_MISSING} to load every record and only the files whose key \
             the keyring holds; nothing was loaded",
            keyfile::path(settings).display(),
            r.missing.len(),
            ids_text(&r.missing)
        )));
    }
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
    // Check every blob before the database is loaded: a corrupt or missing one found after pg_restore
    // would leave a populated database that a second attempt refuses as "not empty".
    let backup_blobs = blobs::tree_files(&archive.join("blobs"))?;
    if let Some(gone) = expected_blobs(&manifest)
        .into_iter()
        .find(|k| !backup_blobs.contains_key(k))
    {
        return Err(ops_error(format!(
            "blob {gone} the manifest lists is missing from the backup"
        )));
    }
    // A sealed file the manifest does not list is refused too: without the key nothing else shows
    // that it was added.
    let sealed = sealed_list(&manifest);
    for (key, path) in &backup_blobs {
        let whole = match key.contains('/') {
            true => sealed.get(key) == Some(&sha256_file(path)?),
            false => sha256_file(path)? == *key,
        };
        if !whole {
            return Err(ops_error(format!("blob {key} in the backup is corrupt")));
        }
    }
    pg_restore(&url, archive).await?;
    if let Some(r) = &mut ring {
        load_keys(&url, r).await?;
    }
    drop(ring_lock);
    let store = BlobStore::for_settings(settings)?;
    for (key, path) in &backup_blobs {
        store.place(key, path)?;
    }
    let lock = anchor_lock(&url).await?;
    let mut report = compare(&url, &manifest, &store.files()?, kek.as_ref(), ring.as_ref()).await?;
    let head = &manifest["audit_chain"];
    let hash = head["hash"]
        .as_str()
        .and_then(|h| hex::decode(h).ok())
        .or_else(|| (head["seq"] == json!(0)).then(|| vec![0; 32])); // an empty log
    let anchored = match (head["seq"].as_i64(), hash) {
        (Some(seq), Some(hash)) if report["ok"] == json!(true) && head["broken"].is_null() => {
            let at = manifest["created_at"].as_str().unwrap_or_default().to_string();
            // The same folder and file check as `read_anchor`: a restore never anchors through a
            // symlink or where others may write. The data is loaded by now, so a refused or failed
            // anchor does not fail the restore; the report says why none was set.
            let dir = settings.anchor_path();
            let file = anchor_file(settings);
            let written = owner_writes_only(&dir, true, "anchor")
                .and_then(|_| owner_writes_only(&file, false, "anchor"))
                .map_err(|why| anchor_unreadable(&file, &why))
                .and_then(|_| {
                    write_anchor(
                        &dir,
                        &Anchor {
                            seq,
                            hash,
                            at: at.clone(),
                        },
                    )
                });
            match written {
                Ok(()) => json!({"seq": seq, "at": at}),
                Err(e) => {
                    tracing::warn!("restore: no audit anchor set: {e}");
                    report["audit_anchor_error"] = Value::String(e.to_string());
                    Value::Null
                }
            }
        }
        _ => Value::Null,
    };
    lock.close().await?;
    report["audit_anchor"] = anchored;
    Ok(report)
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
/// `ok` is true only when every row count, every blob hash and every profile's state_hash match, and
/// the keyring holds every key the backup needs, or lists its profile as deleted.
pub async fn verify_restore(settings: &Settings, archive: &Path) -> Result<Value> {
    let manifest = read_manifest(archive)?;
    let kek = backup_key(settings, &manifest)?;
    let ring_lock = keyfile::lock(settings).await.map_err(|e| ops_error(e.0))?;
    let mut ring = ring_keys(settings, &manifest, kek.as_ref(), false)?;
    let files = blobs::tree_files(&archive.join("blobs"))?;
    let live = settings.sqlx_url();
    let scratch = format!("{}_restore_check", db_name(&live));
    let drop_scratch = format!("drop database if exists \"{scratch}\" with (force)");
    let mut admin = PgConnection::connect(&live).await?;
    sqlx::query(AssertSqlSafe(drop_scratch.clone()))
        .execute(&mut admin)
        .await?;
    create_scratch(&mut admin, &scratch).await?;
    let target = with_db(&live, &scratch);
    let result = async {
        pg_restore(&target, archive).await?;
        if let Some(r) = &mut ring {
            load_keys(&target, r).await?;
        }
        drop(ring_lock);
        compare(&target, &manifest, &files, kek.as_ref(), ring.as_ref()).await
    }
    .await;
    let dropped = sqlx::query(AssertSqlSafe(drop_scratch)).execute(&mut admin).await;
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
