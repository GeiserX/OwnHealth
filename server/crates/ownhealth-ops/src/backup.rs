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
//! It also records the profile references backed by sealed files and the accepted omissions. A
//! restore can carry those omissions forward and identify archived files closed by an unavailable key.
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

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};

use chrono::{NaiveDate, SecondsFormat, Utc};
use ownhealth_core::audit::{ChainBreakKind, ChainRead, verify_chain_with};
use ownhealth_core::blobs::{self, BlobOwner, BlobStore};
use ownhealth_core::config::{Settings, quote_url_component};
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

/// Select another database and keep the other connection options.
fn with_db(url: &str, db: &str) -> String {
    let url = libpq_url(url);
    let (scheme, rest) = url.split_once("://").unwrap_or(("postgresql", &url));
    let (path, query) = rest.split_once('?').unwrap_or((rest, ""));
    let authority = path.split('/').next().unwrap_or_default();
    let mut target = format!("{scheme}://{authority}/{}", quote_url_component(db));
    // A dbname query parameter takes precedence over the path.
    let options = query.split('&').filter(|option| {
        !option.is_empty() && percent_decode(option.split('=').next().unwrap_or_default()) != "dbname"
    });
    for (i, option) in options.enumerate() {
        target.push(if i == 0 { '?' } else { '&' });
        target.push_str(option);
    }
    target
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
    parts.push("select profile_id, null::uuid, zip_blob from imaging_import".into());
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
    // Refuse an unreadable keyring before creating the backup directory. The locked rewrite
    // rechecks the keyring and supplies the current loss markers.
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
            let dek = dek.and_then(std::result::Result::ok).ok_or_else(|| {
                ops_error(format!(
                    "the key for profile {profile} does not open with this OWNHEALTH_SECRETS_KEY"
                ))
            })?;
            key_ids.insert(profile.to_string(), dek.archive_id());
            deks.insert(*slot, dek);
        } else if let Some(Ok(dek)) = dek {
            deks.insert(*slot, dek);
        }
    }
    // The dump holds no row of it, so a restore never has the same count.
    tables.remove("profile_key");
    keyfile::rewrite(&mut lock, settings)
        .await
        .map_err(|e| ops_error(e.0))?;
    let lost = blobs::read_restore_losses(settings)?;

    // Blobs are written before the rows that reference them commit, so every blob the snapshot can
    // see exists by now, and none of them went since (the copy lock above). Later blobs are harmless
    // extras.
    let store = BlobStore::for_settings(settings)?;
    // Keep both artifacts private to this attempt until the loss record has been updated.
    let staged_blobs = tempfile::tempdir_in(dest)?;
    let tree = staged_blobs.path().join("blobs");
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
    let mut required = Vec::new();
    let mut repaired = Vec::new();
    let mut profile_sealed = ProfileBlobs::new();
    let mut accepted_omissions = ProfileBlobs::new();
    for (owner, sha) in &referenced {
        let slot = owner.slot();
        let accepted_loss = match owner {
            BlobOwner::Profile(id) if lost.get(id).is_some_and(|losses| losses.contains(sha)) => Some(*id),
            _ => None,
        };
        if let Some(slot) = slot
            && let Some(dek) = deks.get(&slot)
        {
            let key = blobs::sealed_key(slot, dek, sha)?;
            if let Some(path) = files.get(&key) {
                if let keys::Slot::Profile(id) = slot {
                    profile_sealed.entry(id).or_default().insert(sha.clone());
                }
                if let Some(id) = accepted_loss
                    && repaired_file(path, sha, Some((id, dek)))
                {
                    repaired.push((id, sha.as_str()));
                }
                required.push(key);
                continue;
            }
        }
        let plain = blobs::sha256_hex(sha)?;
        if let Some(path) = files.get(&plain) {
            if let Some(id) = accepted_loss
                && repaired_file(path, &plain, None)
            {
                repaired.push((id, sha.as_str()));
            }
            required.push(plain);
        } else if let Some(id) = accepted_loss {
            accepted_omissions.entry(id).or_default().insert(plain);
        } else {
            required.push(plain);
        }
    }
    #[cfg(test)]
    let _ = tests::BEFORE_CLEAR_REPAIRED.try_with(|observe| observe());
    clear_repaired_losses(settings, &repaired).await?;
    staged_dump
        .persist_noclobber(dest.join("db.dump"))
        .map_err(|e| e.error)?;
    fs::rename(tree, dest.join("blobs"))?;
    staged_blobs.close()?;
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
            "profile_sealed": profile_sealed,
            "accepted_omissions": accepted_omissions,
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

type ProfileBlobs = BTreeMap<Uuid, BTreeSet<String>>;

struct ArchivedLosses {
    accepted_omissions: ProfileBlobs,
    profile_sealed: ProfileBlobs,
}

impl ArchivedLosses {
    fn for_restore(mut self, ring: &RingKeys) -> ProfileBlobs {
        for id in ring.missing.iter().chain(&ring.shredded) {
            if let Some(sealed) = self.profile_sealed.remove(id) {
                self.accepted_omissions.entry(*id).or_default().extend(sealed);
            }
        }
        self.accepted_omissions
    }
}

fn manifest_profile_blobs(manifest: &Value, field: &str) -> Result<ProfileBlobs> {
    let Some(value) = manifest["blobs"].get(field) else {
        return Ok(ProfileBlobs::new());
    };
    let invalid = || ops_error(format!("bad blobs.{field} in the manifest"));
    let lists: ProfileBlobs = serde_json::from_value(value.clone()).map_err(|_| invalid())?;
    if lists.values().flatten().any(|sha| blobs::sha256_hex(sha).is_err()) {
        return Err(invalid());
    }
    Ok(lists)
}

fn read_manifest(archive: &Path) -> Result<(Value, ArchivedLosses)> {
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
    if manifest["keys"].get("keyring").is_none() && manifest["tables"].get("profile_key").is_some() {
        return Err(ops_error(
            "this is a keyed backup from before the keyring. Restore it with the build that wrote it, \
             then take a new backup with this one",
        ));
    }
    if manifest["keys"].get("keyring").is_some() && manifest["keys"].get("key_ids").is_none() {
        return Err(ops_error(
            "this backup lists profile UUIDs without key IDs. Restore it with the build that wrote it, \
             then take a new backup with this one",
        ));
    }
    let losses = ArchivedLosses {
        accepted_omissions: manifest_profile_blobs(&manifest, "accepted_omissions")?,
        profile_sealed: manifest_profile_blobs(&manifest, "profile_sealed")?,
    };
    Ok((manifest, losses))
}

async fn pg_restore(url: &str, archive: &Path) -> Result<()> {
    let dump = fs::File::open(archive.join("db.dump"))?;
    #[cfg(test)]
    tests::OBSERVE_PG_RESTORE
        .try_with(|observe| observe(url))
        .unwrap_or(Ok(()))?;
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
/// A copied file only clears an accepted loss once the matching copy can be read in full.
fn repaired_file(path: &Path, sha: &str, sealed: Option<(Uuid, &keys::Dek)>) -> bool {
    let Some((profile, dek)) = sealed else {
        return sha256_file(path).is_ok_and(|hash| hash == sha);
    };
    let mut expected = [0; 32];
    if hex::decode_to_slice(sha, &mut expected).is_err() {
        return false;
    }
    fs::File::open(path)
        .and_then(|file| keys::SealedReader::open(file, dek, profile, &expected))
        .and_then(|mut reader| std::io::copy(&mut reader, &mut std::io::sink()))
        .is_ok()
}

async fn clear_repaired_losses(settings: &Settings, repaired: &[(Uuid, &str)]) -> Result<()> {
    if repaired.is_empty() {
        return Ok(());
    }
    let _lock = keyfile::lock(settings).await.map_err(|e| ops_error(e.0))?;
    let mut losses = blobs::read_restore_losses(settings)?;
    let mut changed = false;
    for (id, sha) in repaired {
        if let Some(files) = losses.get_mut(id) {
            changed |= files.remove(*sha);
        }
    }
    if changed {
        losses.retain(|_, files| !files.is_empty());
        blobs::write_restore_losses(settings, &losses)?;
    }
    Ok(())
}

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
    fingerprint: Vec<u8>,
    /// the wrapped keys selected for the profiles in keys.keyring
    keys: Vec<(Uuid, Vec<u8>)>,
    /// keys for profiles in the backup that stored their first sealed file later
    supplemental: Vec<(Uuid, Vec<u8>)>,
    /// profiles of the backup whose key the keyring lists as shredded: deleted since the backup
    shredded: Vec<Uuid>,
    /// profiles whose archived key the keyring closes and that hold no newer key: each gets its
    /// shredded row back, so no new key is ever made for it. A profile the archive names no key for
    /// gets no row: its next file asks the keyring, which refuses a new key only under a tombstone
    /// that names no key, so a tombstone that names a later key does not stop it.
    tombstones: Vec<Uuid>,
    /// profiles for which the keyring has no matching key or tombstone
    missing: Vec<Uuid>,
    /// the operator chose to restore without the keys the keyring lacks (`--allow-missing-keys`)
    allow_missing: bool,
}

/// The flag that lets a restore go on without the keys the keyring lacks.
const ALLOW_MISSING: &str = "--allow-missing-keys";

/// Select and authenticate keys before the restore loads anything. Keyless 0.x backups have no
/// keys to install.
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
    let Some(ids) = ids else {
        if manifest["keys"].get("key_ids").is_some() {
            return Err(ops_error("keys.key_ids requires a keys.keyring list in the manifest"));
        }
        return Ok(None);
    };
    let key_ids = manifest["keys"]["key_ids"]
        .as_object()
        .ok_or_else(|| ops_error("bad keys.key_ids map in the manifest"))?;
    if key_ids.len() != ids.len() {
        return Err(ops_error(
            "keys.key_ids does not match the manifest's keys.keyring list",
        ));
    }
    let file = keyfile::path(settings);
    let ring = match keyfile::read(&file).map_err(|e| ops_error(e.0))? {
        Some(ring) => ring,
        None if ids.is_empty() || allow_missing => keyfile::KeyringFile::default(),
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
    let mut out = RingKeys {
        fingerprint: ring.kek_fingerprint.clone().unwrap_or_default(),
        keys: Vec::new(),
        supplemental: Vec::new(),
        shredded: Vec::new(),
        tombstones: Vec::new(),
        missing: Vec::new(),
        allow_missing,
    };
    for id in ids {
        let id = id
            .as_str()
            .and_then(|i| Uuid::parse_str(i).ok())
            .ok_or_else(|| ops_error(format!("bad profile id {id} in the manifest's keys.keyring")))?;
        let wanted = key_ids
            .get(&id.to_string())
            .and_then(Value::as_str)
            .filter(|value| value.len() == 64 && value.bytes().all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f')))
            .ok_or_else(|| ops_error(format!("bad key id for profile {id} in the manifest's keys.key_ids")))?;
        if let Some(w) = ring.profiles.get(&id) {
            let dek = authenticate_ring_key(&file, kek, id, &ring, w)?;
            if wanted == dek.archive_id() {
                out.keys.push((id, w.clone()));
            } else if ring.closes(id, Some(wanted)) {
                // the archive's key was deleted, and the profile came back since with a newer key:
                // the archive's files stay closed, and the newer key stays the profile's
                out.keys.push((id, w.clone()));
                out.shredded.push(id);
            } else {
                // Another key than the archive's: the archive's files stay closed, and the profile
                // keeps the keyring's key, so its next file never replaces that key in the keyring
                // (newer backups need it).
                out.keys.push((id, w.clone()));
                out.missing.push(id);
            }
        } else if ring.closes(id, Some(wanted)) {
            out.shredded.push(id);
            out.tombstones.push(id);
        } else {
            out.missing.push(id);
        }
    }
    // Backup records a state hash for every profile in the database snapshot. Keys that
    // belong only to other backups stay in the keyring without affecting this restore.
    let selected: BTreeSet<_> = out.keys.iter().map(|(id, _)| *id).collect();
    for (&id, wrapped) in &ring.profiles {
        if manifest["state_hash"].get(id.to_string()).is_some() && !selected.contains(&id) {
            authenticate_ring_key(&file, kek, id, &ring, wrapped)?;
            out.supplemental.push((id, wrapped.clone()));
        }
    }
    Ok(Some(out))
}

fn authenticate_ring_key(
    path: &Path,
    kek: Option<&Kek>,
    id: Uuid,
    ring: &keyfile::KeyringFile,
    wrapped: &[u8],
) -> Result<keys::Dek> {
    let fingerprint = ring.kek_fingerprint.as_deref().unwrap_or_default();
    let dek = kek
        .and_then(|k| k.unwrap(id, fingerprint, wrapped).ok())
        .ok_or_else(|| {
            ops_error(format!(
                "the keyring {} holds a damaged key for profile {id}: it does not open with \
                 OWNHEALTH_SECRETS_KEY. Put back an undamaged copy of this install's keyring.json. \
                 When no such copy exists, remove that entry from keyring.json and restore with \
                 {ALLOW_MISSING}: that profile's files stay closed; nothing was loaded",
                path.display()
            ))
        })?;
    if ring.closes(id, Some(&dek.archive_id())) {
        return Err(ops_error(format!(
            "the keyring {} holds a key that a tombstone closes for profile {id}. Remove that profile's \
             entry from profiles in keyring.json, keep its tombstone, and rerun the restore. Nothing was loaded",
            path.display()
        )));
    }
    Ok(dek)
}

/// Load the selected keys and the shredded rows of the archive's closed keys. A profile in the dump
/// that stored its first sealed file after the backup gets its current key, so its next file uses
/// the same key.
async fn load_keys(url: &str, ring: &RingKeys, resume_id: Option<&str>) -> Result<()> {
    let mut conn = PgConnection::connect(url).await?;
    let mut tx = conn.begin().await?;
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
    let (held, wrapped): (Vec<Uuid>, Vec<Vec<u8>>) = ring.supplemental.iter().cloned().unzip();
    sqlx::query(
        "insert into profile_key (profile_id, wrapped_dek, kek_fingerprint) \
         select k.id, k.wrapped, $3 from unnest($1::uuid[], $2::bytea[]) as k(id, wrapped) \
         join profile p on p.id = k.id \
         on conflict (profile_id) do nothing",
    )
    .bind(&held)
    .bind(&wrapped)
    .bind(&ring.fingerprint)
    .execute(&mut *tx)
    .await?;
    if let Some(id) = resume_id {
        // Commit the checkpoint with the keys. A populated database without this checkpoint
        // cannot be resumed.
        sqlx::raw_sql(
            "create schema ownhealth_restore; create table ownhealth_restore.pending_loss (id text not null)",
        )
        .execute(&mut *tx)
        .await?;
        sqlx::query("insert into ownhealth_restore.pending_loss values ($1)")
            .bind(id)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    conn.close().await?;
    Ok(())
}

/// Bind a retry to the same backup contents, selected keys, and restore options. Restic can
/// extract the same backup into a new directory for the retry.
fn loss_resume_id(archive: &Path, manifest: &Value, ring: &RingKeys) -> Result<String> {
    let identity = json!({
        "dump": sha256_file(&archive.join("db.dump"))?,
        "manifest": manifest,
        "fingerprint": ring.fingerprint,
        "keys": ring.keys,
        "supplemental": ring.supplemental,
        "missing": ring.missing,
        "shredded": ring.shredded,
        "tombstones": ring.tombstones,
        "allow_missing": ring.allow_missing,
    });
    Ok(hex::encode(Sha256::digest(serde_json::to_vec(&identity)?)))
}

async fn can_resume_loss(conn: &mut PgConnection, resume_id: Option<&str>) -> Result<bool> {
    let Some(resume_id) = resume_id else { return Ok(false) };
    let pending: bool = sqlx::query_scalar("select to_regclass('ownhealth_restore.pending_loss') is not null")
        .fetch_one(&mut *conn)
        .await?;
    if !pending {
        return Ok(false);
    }
    let saved: Option<String> = sqlx::query_scalar("select id from ownhealth_restore.pending_loss")
        .fetch_optional(conn)
        .await?;
    Ok(saved.as_deref() == Some(resume_id))
}

/// Carry forward accepted omissions and sealed references whose key the restore cannot install.
/// Only references in the restored database without a plain copy count. Holds the keyring lock.
async fn record_lost(
    settings: &Settings,
    url: &str,
    losses: &ProfileBlobs,
    backup_blobs: &BTreeMap<String, PathBuf>,
) -> Result<()> {
    let mut conn = PgConnection::connect(url).await?;
    let referenced = referenced_blobs(&mut conn).await?;
    conn.close().await?;
    let mut expected = blobs::RestoreLosses::new();
    for (owner, sha) in referenced {
        if let BlobOwner::Profile(p) = owner
            && losses.get(&p).is_some_and(|files| files.contains(&sha))
            && !backup_blobs.contains_key(&sha)
        {
            expected.entry(p).or_default().insert(sha);
        }
    }
    blobs::record_restore_losses(settings, &expected)?;
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
    let counts = table_counts(&mut tx).await?; // before build_state, which may add state_snapshot rows
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
    let (manifest, archived_losses) = read_manifest(archive)?;
    let kek = backup_key(settings, &manifest)?;
    let ring_lock = if manifest["keys"].get("keyring").is_some() {
        Some(keyfile::lock(settings).await.map_err(|e| ops_error(e.0))?)
    } else {
        None
    };
    let ring = ring_keys(settings, &manifest, kek.as_ref(), allow_missing_keys)?;
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
    let losses = ring
        .as_ref()
        .map(|ring| archived_losses.for_restore(ring))
        .unwrap_or_default();
    let resume_id = ring
        .as_ref()
        .filter(|_| !losses.is_empty())
        .map(|ring| loss_resume_id(archive, &manifest, ring))
        .transpose()?;
    let url = settings.sqlx_url();
    let mut conn = PgConnection::connect(&url).await?;
    let tables: i64 = sqlx::query_scalar("select count(*) from pg_tables where schemaname = 'public'")
        .fetch_one(&mut conn)
        .await?;
    let resuming = tables > 0 && can_resume_loss(&mut conn, resume_id.as_deref()).await?;
    conn.close().await?;
    if tables > 0 && !resuming {
        return Err(ops_error(
            "the target database is not empty: restore needs a fresh instance (new volume, app not started). \
             It holds no checkpoint that matches this restore. If an earlier run of this restore saved a \
             checkpoint, rerun it with the same snapshot, keyring and options. Otherwise a restore that \
             stopped here cannot resume: it starts over. If this target holds only that stopped restore, \
             drop and recreate the database and empty the blob folder (or use new volumes), keep the app \
             stopped, and run the same restore again (deploy/README.md, \"Restore\"). Nothing was loaded",
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
    let expected = backup_blobs
        .keys()
        .map(|key| (key.as_str(), sealed.get(key).unwrap_or(key).as_str()))
        .collect();
    blobs::check_restore_target(settings, &expected, resuming)?;
    if !resuming {
        pg_restore(&url, archive).await?;
        if let Some(ring) = &ring {
            load_keys(&url, ring, resume_id.as_deref()).await?;
        }
    }
    // Keep the lock through blob placement when a checkpoint exists, so another invocation
    // cannot resume this restore while it is still finishing.
    let resume_lock = if resume_id.is_some() {
        record_lost(settings, &url, &losses, &backup_blobs)
            .await
            .map_err(|source| Error::RestoreLoss(Box::new(source)))?;
        ring_lock
    } else {
        drop(ring_lock);
        None
    };
    let store = BlobStore::for_settings(settings)?;
    for (key, path) in &backup_blobs {
        store.place(key, path).map_err(|error| match resume_lock {
            Some(_) => Error::RestoreLoss(Box::new(Error::from(error))),
            None => Error::from(error),
        })?;
    }
    if resume_lock.is_some() {
        let mut conn = PgConnection::connect(&url).await?;
        let mut tx = conn.begin().await?;
        sqlx::raw_sql("drop table ownhealth_restore.pending_loss; drop schema ownhealth_restore")
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        conn.close().await?;
    }
    drop(resume_lock);
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

fn database_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// The scratch database of verify-restore. A new database lets PUBLIC connect, and the server may
/// hold other roles (the wearables sidecar's own one), so only the owner may open this one. It is
/// created closed to every session and opened after the revoke: no session can get in first and
/// stay connected through the revoke.
async fn create_scratch(admin: &mut PgConnection, scratch: &str) -> Result<()> {
    let ident = database_ident(scratch);
    sqlx::query(AssertSqlSafe(format!(
        "create database {ident} with allow_connections = false"
    )))
    .execute(&mut *admin)
    .await?;
    let opened = async {
        for sql in [
            format!("revoke connect, temporary on database {ident} from public"),
            format!("alter database {ident} with allow_connections true"),
        ] {
            #[cfg(test)]
            tests::BEFORE_SCRATCH_STEP.try_with(|step| step()).unwrap_or(Ok(()))?;
            sqlx::query(AssertSqlSafe(sql)).execute(&mut *admin).await?;
        }
        Ok::<_, Error>(())
    }
    .await;
    if let Err(error) = opened {
        if let Err(cleanup) = drop_scratch(admin, scratch).await {
            tracing::warn!(%scratch, %cleanup, "could not drop the scratch database after a setup step failed");
        }
        return Err(error);
    }
    Ok(())
}

async fn drop_scratch(admin: &mut PgConnection, scratch: &str) -> Result<()> {
    let ident = database_ident(scratch);
    sqlx::query(AssertSqlSafe(format!("drop database if exists {ident} with (force)")))
        .execute(admin)
        .await?;
    Ok(())
}

/// PostgreSQL advisory locks are local to the database where the session takes them.
async fn try_lock_verification(live_conn: &mut PgConnection) -> Result<()> {
    let locked: bool =
        sqlx::query_scalar("select pg_catalog.pg_try_advisory_lock(pg_catalog.hashtext('ownhealth.verify_restore'))")
            .fetch_one(live_conn)
            .await?;
    if !locked {
        return Err(ops_error("verify-restore is already running for this database"));
    }
    Ok(())
}

/// Restore into this live database's scratch database, compare with the manifest, then drop it.
/// The live connection holds the session lock from the initial cleanup until the final drop.
/// `ok` is true only when every row count, every blob hash and every profile's state_hash match, and
/// the keyring holds every key the backup needs, or lists its profile as deleted.
pub async fn verify_restore(settings: &Settings, archive: &Path) -> Result<Value> {
    let live = settings.sqlx_url();
    let mut live_conn = PgConnection::connect(&live).await?;
    try_lock_verification(&mut live_conn).await?;
    // Check the name in the server's encoding. DDL would silently truncate an overlong identifier.
    let (scratch, fits): (String, bool) = sqlx::query_as(
        "select scratch, octet_length(scratch) <= current_setting('max_identifier_length')::integer \
         from (select pg_catalog.current_database()::text || '_restore_check' as scratch) names",
    )
    .fetch_one(&mut live_conn)
    .await?;
    if !fits {
        return Err(ops_error(format!(
            "the scratch database name {scratch:?} exceeds the PostgreSQL identifier length limit"
        )));
    }
    drop_scratch(&mut live_conn, &scratch).await?;
    let (manifest, _) = read_manifest(archive)?;
    let kek = backup_key(settings, &manifest)?;
    let ring_lock = if manifest["keys"].get("keyring").is_some() {
        Some(keyfile::lock(settings).await.map_err(|e| ops_error(e.0))?)
    } else {
        None
    };
    let ring = ring_keys(settings, &manifest, kek.as_ref(), false)?;
    // The scratch database is dropped at the end, so nothing it loads reaches the live keyring: the
    // lock covers the read alone, and a profile delete on the live install never waits on pg_restore.
    drop(ring_lock);
    let files = blobs::tree_files(&archive.join("blobs"))?;
    create_scratch(&mut live_conn, &scratch).await?;
    let target = with_db(&live, &scratch);
    let result = async {
        pg_restore(&target, archive).await?;
        if let Some(r) = &ring {
            load_keys(&target, r, None).await?;
        }
        compare(&target, &manifest, &files, kek.as_ref(), ring.as_ref()).await
    }
    .await;
    let dropped = drop_scratch(&mut live_conn, &scratch).await;
    live_conn.close().await?;
    let report = result?;
    dropped?;
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    tokio::task_local! {
        pub(super) static OBSERVE_PG_RESTORE: Box<dyn Fn(&str) -> Result<()> + Send>;
        pub(super) static BEFORE_CLEAR_REPAIRED: Box<dyn Fn() + Send>;
        pub(super) static BEFORE_SCRATCH_STEP: Box<dyn Fn() -> Result<()> + Send>;
    }

    async fn connect_outcome(url: &str) -> String {
        match PgConnection::connect(url).await {
            Ok(conn) => {
                let _ = conn.close().await;
                "connected".to_string()
            }
            Err(e) => e.to_string(),
        }
    }

    /// [`connect_outcome`] on its own thread and runtime, for a synchronous hook.
    fn connect_outcome_blocking(url: &str) -> String {
        let url = url.to_string();
        std::thread::spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_or_else(|e| e.to_string(), |runtime| runtime.block_on(connect_outcome(&url)))
        })
        .join()
        .unwrap_or_else(|_| "the connect thread panicked".to_string())
    }

    /// Settings for a live database at `url`, as `TestDb::settings` makes them.
    fn settings_for(url: &str, data_dir: &Path) -> anyhow::Result<Settings> {
        let env = HashMap::from([
            (
                "OWNHEALTH_SECRETS_KEY".to_string(),
                ownhealth_core::testing::random_secrets_key(),
            ),
            ("OWNHEALTH_DATABASE_URL".to_string(), url.to_string()),
            ("OWNHEALTH_DATA_DIR".to_string(), data_dir.display().to_string()),
            ("OWNHEALTH_DEV_AUTH".to_string(), "1".to_string()),
            ("OWNHEALTH_RUN_JOBS".to_string(), "0".to_string()),
        ]);
        Ok(Settings::from(&env)?)
    }

    #[test]
    fn restore_reports_a_database_error_and_how_to_retry() {
        let original = sqlx::Error::PoolClosed;
        let error = Error::RestoreLoss(Box::new(Error::Db(original)));
        let message = error.to_string();
        assert!(message.contains(&sqlx::Error::PoolClosed.to_string()), "{message}");
        assert!(message.contains("Resolve the reported error"), "{message}");
        assert!(message.contains("rerun the same restore"), "{message}");
        assert!(message.contains("app stopped"), "{message}");
        let source = std::error::Error::source(&error).and_then(|source| source.downcast_ref::<Error>());
        assert!(matches!(source, Some(Error::Db(sqlx::Error::PoolClosed))));
    }

    #[tokio::test]
    async fn restore_refuses_a_closed_wrapped_key_on_every_selection_path() -> anyhow::Result<()> {
        use ownhealth_core::testing::TestDb;

        let db = TestDb::empty().await?;
        let dir = tempfile::tempdir()?;
        let settings = db.settings(dir.path())?;
        let kek = Kek::from_settings(&settings)?;
        let profile = Uuid::new_v4();
        let stored = keys::Dek::generate();
        let stored_id = stored.archive_id();
        let other_id = keys::Dek::generate().archive_id();
        let mut ring = keyfile::KeyringFile {
            kek_fingerprint: Some(kek.fingerprint().to_vec()),
            ..Default::default()
        };
        let wrapped = kek.wrap(profile, &stored)?;
        ring.profiles.insert(profile, wrapped.clone());
        ring.shredded.insert(profile);
        let manifest = |requested: Option<&str>| {
            let (ids, key_ids) = requested.map_or_else(
                || (json!([]), json!({})),
                |id| (json!([profile]), json!({profile.to_string(): id})),
            );
            json!({
                "keys": {"keyring": ids, "key_ids": key_ids},
                "state_hash": {profile.to_string(): "in the backup"},
            })
        };
        let mut failures = Vec::new();
        for (label, requested, closes_other) in [
            ("matching", Some(stored_id.as_str()), false),
            ("mismatched and shredded", Some(other_id.as_str()), true),
            ("mismatched and missing", Some(other_id.as_str()), false),
            ("supplemental", None, false),
        ] {
            let mut ids = BTreeSet::from([stored_id.clone()]);
            if closes_other {
                ids.insert(other_id.clone());
            }
            ring.shredded_keys.insert(profile, ids);
            keyfile::write(&settings.keyring_path(), &ring)?;
            let result = ring_keys(&settings, &manifest(requested), Some(&kek), true);
            if !result.err().is_some_and(|e| e.to_string().contains("closes")) {
                failures.push(label);
            }
        }

        // This tombstone closes the archived key, leaving the stored newer key selectable.
        ring.shredded_keys.insert(profile, BTreeSet::from([other_id.clone()]));
        keyfile::write(&settings.keyring_path(), &ring)?;
        let selected = ring_keys(&settings, &manifest(Some(&other_id)), Some(&kek), false)?.expect("keyring manifest");
        assert_eq!(selected.keys, [(profile, wrapped)]);
        assert_eq!(selected.shredded, [profile]);
        assert!(failures.is_empty(), "closed key was not refused on {failures:?}");
        Ok(())
    }

    #[tokio::test]
    async fn verify_restore_frees_the_keyring_during_pg_restore_and_restore_does_not() -> anyhow::Result<()> {
        use ownhealth_core::testing::TestDb;
        use std::fs::TryLockError;

        crate::register();
        let db = TestDb::empty().await?;
        let dir = tempfile::tempdir()?;
        let live = TestDb::new().await?;
        let live_settings = live.settings(&dir.path().join("live"))?;
        let archive = backup(&live_settings, &dir.path().join("backup")).await?;
        let mut settings = db.settings(&dir.path().join("restored"))?;
        settings.secrets_key = live_settings.secrets_key.clone();
        settings.keyring_dir = Some(live_settings.keyring_path());
        let lock_file = settings.keyring_path().join(".keyring.lock");
        let (observed, mut checks) = tokio::sync::mpsc::channel(3);
        // Observe the lock in this process as pg_restore opens the dump. If restore returns
        // early there is no probe to cancel or join.
        let observe = Box::new(move |_: &str| {
            let lock = fs::File::options().write(true).open(&lock_file)?;
            let free = match lock.try_lock() {
                Ok(()) => true,
                Err(TryLockError::WouldBlock) => false,
                Err(TryLockError::Error(error)) => return Err(error.into()),
            };
            observed
                .try_send(free)
                .expect("the test makes at most three restore calls");
            Ok(())
        });
        OBSERVE_PG_RESTORE
            .scope(observe, async {
                assert_eq!(verify_restore(&settings, &archive).await?["ok"], true);
                assert_eq!(checks.try_recv(), Ok(true), "verify-restore held the keyring lock");
                assert_eq!(restore(&settings, &archive).await?["ok"], true);
                assert_eq!(checks.try_recv(), Ok(false), "restore released the keyring lock");
                let early = restore(&settings, &archive).await.expect_err("target now has tables");
                assert!(early.to_string().contains("target database is not empty"), "{early}");
                assert_eq!(checks.try_recv(), Err(tokio::sync::mpsc::error::TryRecvError::Empty));
                Ok::<_, Error>(())
            })
            .await?;
        Ok(())
    }

    async fn scratch_exists(admin: &mut PgConnection, scratch: &str) -> sqlx::Result<bool> {
        sqlx::query_scalar("select exists(select 1 from pg_database where datname = $1)")
            .bind(scratch)
            .fetch_one(admin)
            .await
    }

    #[tokio::test]
    async fn round_eight_verification_refuses_overlap_without_dropping_the_scratch() -> anyhow::Result<()> {
        use ownhealth_core::testing::TestDb;

        crate::register();
        let db = TestDb::new().await?;
        let dir = tempfile::tempdir()?;
        let settings = db.settings(dir.path())?;
        let archive = backup(&settings, &dir.path().join("backup")).await?;
        let scratch = format!("{}_restore_check", db.name);
        let mut lock = PgConnection::connect(&db.url).await?;
        create_scratch(&mut lock, &scratch).await?;
        let result = tokio::time::timeout(Duration::from_secs(30), async {
            let held: bool = sqlx::query_scalar(
                "select pg_catalog.pg_try_advisory_lock(pg_catalog.hashtext('ownhealth.verify_restore'))",
            )
            .fetch_one(&mut lock)
            .await?;
            anyhow::ensure!(held, "the test must own the lock");
            let result = verify_restore(&settings, &archive).await;
            Ok::<_, anyhow::Error>((result, scratch_exists(&mut lock, &scratch).await?))
        })
        .await;
        lock.close().await?; // releases the test's lock before asserting the result
        let mut cleanup = PgConnection::connect(&db.url).await?;
        if scratch_exists(&mut cleanup, &scratch).await? {
            drop_scratch(&mut cleanup, &scratch).await?;
        }
        let (result, preserved) = result??;
        let error = result.expect_err("overlapping verification must fail");
        assert!(
            error.to_string().contains("verify-restore is already running"),
            "{error}"
        );
        assert!(preserved, "overlap dropped the scratch database");
        Ok(())
    }

    #[tokio::test]
    async fn round_eight_verifications_for_two_live_databases_can_overlap() -> anyhow::Result<()> {
        use ownhealth_core::testing::TestDb;

        crate::register();
        let dir = tempfile::tempdir()?;
        let first = TestDb::new().await?;
        let second = TestDb::new().await?;
        let first_settings = first.settings(&dir.path().join("first"))?;
        let second_settings = second.settings(&dir.path().join("second"))?;
        let first_archive = backup(&first_settings, &dir.path().join("backup-first")).await?;
        let second_archive = backup(&second_settings, &dir.path().join("backup-second")).await?;
        let first_scratch = format!("{}_restore_check", first.name);
        let second_scratch = format!("{}_restore_check", second.name);

        let mut conn = PgConnection::connect(&first.url).await?;
        // These two leftovers belong to this test. Each check should remove only its own.
        create_scratch(&mut conn, &first_scratch).await?;
        create_scratch(&mut conn, &second_scratch).await?;

        let (at_restore, mut reached) = tokio::sync::mpsc::unbounded_channel();
        let result = tokio::time::timeout(Duration::from_secs(60), async {
            let first_check = OBSERVE_PG_RESTORE.scope(
                Box::new(move |url| {
                    at_restore.send(url.to_string()).expect("record scratch database");
                    Ok(())
                }),
                verify_restore(&first_settings, &first_archive),
            );
            tokio::pin!(first_check);
            let target = tokio::select! {
                result = &mut first_check => anyhow::bail!("first check finished before pg_restore: {result:?}"),
                target = reached.recv() => target.expect("observe pg_restore"),
            };
            let second_preserved = scratch_exists(&mut conn, &second_scratch).await?;
            // Keep the first verification future alive, still holding its lock, while the second runs.
            let second_report = verify_restore(&second_settings, &second_archive).await;
            let first_report = first_check.await?;
            Ok::<_, anyhow::Error>((target, second_preserved, first_report, second_report))
        })
        .await;
        let first_left = scratch_exists(&mut conn, &first_scratch).await?;
        let second_left = scratch_exists(&mut conn, &second_scratch).await?;
        if first_left {
            drop_scratch(&mut conn, &first_scratch).await?;
        }
        if second_left {
            drop_scratch(&mut conn, &second_scratch).await?;
        }

        let (target, second_preserved, first_report, second_report) = result??;
        assert_eq!(second_report?["ok"], true);
        assert_eq!(first_report["ok"], true);
        assert!(target.ends_with(&format!("/{first_scratch}")), "incorrect scratch name");
        assert!(
            second_preserved,
            "the first check dropped the second database's scratch"
        );
        assert!(!first_left && !second_left, "a scratch database was left behind");
        Ok(())
    }

    /// The lock is held through pg_restore: a second check of the same database refuses and
    /// leaves the first check's scratch database alone.
    #[tokio::test]
    async fn round_nine_a_second_verification_refuses_while_the_first_restores() -> anyhow::Result<()> {
        use ownhealth_core::testing::TestDb;

        crate::register();
        let db = TestDb::new().await?;
        let dir = tempfile::tempdir()?;
        let settings = db.settings(dir.path())?;
        let archive = backup(&settings, &dir.path().join("backup")).await?;
        let scratch = format!("{}_restore_check", db.name);
        let (at_restore, mut reached) = tokio::sync::mpsc::unbounded_channel();
        let result = tokio::time::timeout(Duration::from_secs(60), async {
            let first_check = OBSERVE_PG_RESTORE.scope(
                Box::new(move |url| {
                    at_restore.send(url.to_string()).expect("record the scratch database");
                    Ok(())
                }),
                verify_restore(&settings, &archive),
            );
            tokio::pin!(first_check);
            tokio::select! {
                result = &mut first_check => anyhow::bail!("first check finished before pg_restore: {result:?}"),
                target = reached.recv() => target.expect("observe pg_restore"),
            };
            // The first check is now inside pg_restore and is not polled while the second runs.
            let second = verify_restore(&settings, &archive).await;
            let mut conn = PgConnection::connect(&db.url).await?;
            let kept = scratch_exists(&mut conn, &scratch).await?;
            conn.close().await?;
            let first = first_check.await?;
            Ok::<_, anyhow::Error>((second, kept, first))
        })
        .await;
        let mut cleanup = PgConnection::connect(&db.url).await?;
        let left = scratch_exists(&mut cleanup, &scratch).await?;
        if left {
            drop_scratch(&mut cleanup, &scratch).await?;
        }
        cleanup.close().await?;

        let (second, kept, first) = result??;
        let error = second.expect_err("a second check of the same database must refuse");
        assert!(
            error.to_string().contains("verify-restore is already running"),
            "{error}"
        );
        assert!(kept, "the second check dropped the first check's scratch database");
        assert_eq!(first["ok"], true);
        assert!(!left, "the first check left its scratch database");
        Ok(())
    }

    /// A live database whose name needs quoting and percent-encoding (mixed case, space, double
    /// quote, slash, percent): the check replaces its own leftover scratch and keeps another
    /// database's scratch.
    #[tokio::test]
    async fn round_nine_verification_quotes_the_live_database_name() -> anyhow::Result<()> {
        use ownhealth_core::testing::{TestDb, admin_url};

        crate::register();
        let base = TestDb::new().await?;
        base.pool.close().await;
        let live = format!("{}_Mx \"q/%", base.name);
        let scratch = format!("{live}_restore_check");
        let neighbour = format!("{}_restore_check", base.name);
        let mut admin = PgConnection::connect(&admin_url()).await?;
        sqlx::query(AssertSqlSafe(format!(
            "create database {} template {}",
            database_ident(&live),
            database_ident(&base.name)
        )))
        .execute(&mut admin)
        .await?;
        let dir = tempfile::tempdir()?;
        let outcome = async {
            create_scratch(&mut admin, &scratch).await?; // a leftover of this live database
            create_scratch(&mut admin, &neighbour).await?; // the scratch of another live database
            let settings = settings_for(&with_db(&base.url, &live), dir.path())?;
            let archive = backup(&settings, &dir.path().join("backup")).await?;
            Ok::<_, anyhow::Error>(verify_restore(&settings, &archive).await?)
        }
        .await;
        let scratch_left = scratch_exists(&mut admin, &scratch).await?;
        let neighbour_left = scratch_exists(&mut admin, &neighbour).await?;
        for name in [&scratch, &neighbour, &live] {
            sqlx::query(AssertSqlSafe(format!(
                "drop database if exists {} with (force)",
                database_ident(name)
            )))
            .execute(&mut admin)
            .await?;
        }
        admin.close().await?;
        assert_eq!(outcome?["ok"], true);
        assert!(!scratch_left, "the check left its scratch database");
        assert!(neighbour_left, "the check dropped another database's scratch");
        Ok(())
    }

    /// PostgreSQL cuts a longer name back to 63 bytes, so `<live>_restore_check` of a 63-byte
    /// live name would name the live database itself. The check refuses before any DDL.
    #[tokio::test]
    async fn round_nine_verification_refuses_a_scratch_name_past_the_identifier_limit() -> anyhow::Result<()> {
        use ownhealth_core::testing::{TestDb, admin_url};

        let base = TestDb::empty().await?;
        let live = format!("{}_{}", base.name, "x".repeat(62 - base.name.len()));
        assert_eq!(live.len(), 63);
        let mut admin = PgConnection::connect(&admin_url()).await?;
        sqlx::query(AssertSqlSafe(format!("create database {}", database_ident(&live))))
            .execute(&mut admin)
            .await?;
        let dir = tempfile::tempdir()?;
        let result = async {
            let settings = settings_for(&with_db(&base.url, &live), dir.path())?;
            Ok::<_, anyhow::Error>(verify_restore(&settings, &dir.path().join("no-backup")).await)
        }
        .await;
        let live_left = scratch_exists(&mut admin, &live).await?;
        sqlx::query(AssertSqlSafe(format!(
            "drop database if exists {} with (force)",
            database_ident(&live)
        )))
        .execute(&mut admin)
        .await?;
        admin.close().await?;
        let error = result?.expect_err("an overlong scratch name must be refused");
        assert!(
            error
                .to_string()
                .contains("exceeds the PostgreSQL identifier length limit"),
            "{error}"
        );
        assert!(live_left, "the live database is gone");
        Ok(())
    }

    /// A failure after the create, in the revoke or in opening the database, drops the scratch.
    #[tokio::test]
    async fn round_nine_a_failed_scratch_step_leaves_no_scratch_database() -> anyhow::Result<()> {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let db = ownhealth_core::testing::TestDb::empty().await?;
        let scratch = format!("{}_restore_check", db.name);
        let mut admin = PgConnection::connect(&db.url).await?;
        let mut kept = Vec::new();
        for fail_at in 1..=2 {
            let calls = Arc::new(AtomicUsize::new(0));
            let seen = Arc::clone(&calls);
            let result = BEFORE_SCRATCH_STEP
                .scope(
                    Box::new(move || match seen.fetch_add(1, Ordering::SeqCst) + 1 == fail_at {
                        true => Err(ops_error("injected failure")),
                        false => Ok(()),
                    }),
                    create_scratch(&mut admin, &scratch),
                )
                .await;
            let left = scratch_exists(&mut admin, &scratch).await?;
            if left {
                drop_scratch(&mut admin, &scratch).await?;
            }
            if result.is_ok() || left {
                kept.push((fail_at, calls.load(Ordering::SeqCst), left));
            }
        }
        admin.close().await?;
        assert!(
            kept.is_empty(),
            "a failed step did not fail or kept the scratch (step, calls, left): {kept:?}"
        );
        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_failed_repair_write_leaves_the_backup_destination_retryable() -> anyhow::Result<()> {
        use ownhealth_core::{demo::seed_demo, testing::TestDb};
        use std::os::unix::fs::PermissionsExt;

        crate::register();
        let db = TestDb::new().await?;
        let dir = tempfile::tempdir()?;
        let settings = db.settings(dir.path())?;
        let store = BlobStore::for_settings(&settings)?;
        assert!(seed_demo(&db.pool, &settings, &store, false).await?);
        let (profile, sha): (Uuid, String) = sqlx::query_as(
            "select profile_id, encode(blob_sha256, 'hex') from document where profile_id is not null limit 1",
        )
        .fetch_one(&db.pool)
        .await?;
        let losses = blobs::RestoreLosses::from([(profile, BTreeSet::from([sha]))]);
        blobs::write_restore_losses(&settings, &losses)?;
        let folder = settings.blobs_path();

        let dest = dir.path().join("backup");
        let read_only = folder.clone();
        let result = BEFORE_CLEAR_REPAIRED
            .scope(
                Box::new(move || {
                    fs::set_permissions(&read_only, fs::Permissions::from_mode(0o500))
                        .expect("make the loss record read-only")
                }),
                backup(&settings, &dest),
            )
            .await;
        fs::set_permissions(&folder, fs::Permissions::from_mode(0o700))?;
        let error = result.expect_err("backup could not record the repair");
        assert!(error.to_string().contains("cannot write restore losses"), "{error}");
        backup(&settings, &dest).await?;
        assert_eq!(verify_restore(&settings, &dest).await?["ok"], true);
        assert!(blobs::read_restore_losses(&settings)?.is_empty());
        Ok(())
    }

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
        let (scheme, rest) = db.url.split_once("://").unwrap_or(("postgres", &db.url));
        let host = rest.rsplit_once('@').map_or(rest, |(_, h)| h);
        let plain = with_db(&format!("{scheme}://{role}:{password}@{host}"), &scratch);
        // The first attempt comes right after the create, before the revoke.
        let early = Arc::new(std::sync::Mutex::new(None));
        let (seen, url) = (Arc::clone(&early), plain.clone());
        let created = BEFORE_SCRATCH_STEP
            .scope(
                Box::new(move || {
                    let mut seen = seen.lock().map_err(|_| ops_error("the hook's lock is poisoned"))?;
                    if seen.is_none() {
                        *seen = Some(connect_outcome_blocking(&url));
                    }
                    Ok(())
                }),
                create_scratch(&mut admin, &scratch),
            )
            .await;
        let opened = connect_outcome(&plain).await;
        let early = early.lock().map_err(|_| anyhow::anyhow!("poisoned"))?.take();
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
        let early = early.unwrap_or_default();
        assert!(
            early.contains("is not currently accepting connections"),
            "a plain role could open the new scratch database before the revoke: {early}"
        );
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
    }

    #[test]
    fn with_db_ignores_a_slash_in_the_query() {
        let url = "postgresql://u@h/live?sslrootcert=/etc/ca.pem";
        assert_eq!(with_db(url, "x"), "postgresql://u@h/x?sslrootcert=/etc/ca.pem");
        assert_eq!(
            with_db("postgresql://u@h/live?host=/var/run/postgresql", "x"),
            "postgresql://u@h/x?host=/var/run/postgresql"
        );
    }

    #[test]
    fn with_db_encodes_the_name_and_replaces_a_dbname_option() {
        assert_eq!(
            with_db(
                "postgresql://local?dbname=live&sslrootcert=/etc/ca.pem&db%6Eame=also_live",
                "live /%\"_restore_check"
            ),
            "postgresql://local/live%20%2F%25%22_restore_check?sslrootcert=/etc/ca.pem"
        );
        assert_eq!(
            database_ident("live /%\"_restore_check"),
            "\"live /%\"\"_restore_check\""
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
