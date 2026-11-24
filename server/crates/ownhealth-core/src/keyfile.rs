//! The keyring file (security-privacy.md 7.3): every profile's wrapped data key, kept out of the
//! database dump, in `keyring.json` in `OWNHEALTH_KEYRING_DIR`. That folder sits on a mount of its
//! own, outside the blob folder and every backup, and is never versioned.
//!
//! `profile_key` stays the live store: the app reads and makes the keys there (`keys.rs`). The dump
//! leaves its rows out (ownhealth-ops backup.rs), so a backup holds sealed files and rows but no key,
//! and a restore takes the keys from this file. A profile delete shreds the row and then rewrites this
//! file at once ([`rewrite`]). So once every older copy of the file is gone, no backup taken before
//! the delete opens the profile's files again; its rows stay in those dumps until they expire.
//!
//! The file holds the keys still wrapped under the KEK, never a plain key: the file alone opens
//! nothing, and neither does OWNHEALTH_SECRETS_KEY alone.
//!
//! ```text
//! {"schema": "ownhealth.keyring/v1", "kek_fingerprint": "<16 hex>",
//!  "profiles": {"<profile uuid>": "<base64 wrapped key>"}, "shredded": ["<profile uuid>"],
//!  "shredded_keys": {"<profile uuid>": ["<64 hex archive id>"]}}
//! ```
//!
//! A rewrite merges the table into the file: a key the table holds goes in unless the file's tombstone
//! closes it, a key the table shredded goes out and its profile into `shredded`, and a profile the table
//! does not know keeps what the file says. After a restore from an older backup, the table does not
//! know the profiles made since; their keys stay, so a newer backup still restores. `shredded` tells a
//! restore that a missing key was deleted on purpose, not lost.
//!
//! A tombstone names the key it closes (`shredded_keys`, by [`crate::keys::Dek::archive_id`]) when the
//! file held that key. It then closes only that key: a backup from before the keyring brings the
//! profile back with no key, and the key its next file makes is published. A tombstone that names no
//! key (the file never held it) closes every key of the profile.
//!
//! A profile with no key row takes the file's key for it, unless a tombstone closes that key
//! ([`live_key`]): a dump from before 1.0 has no key table, so its restore installs no key, and a key
//! made instead would replace the one every backup since is sealed under.
//!
//! Accepted restore losses live with their blob store (see [`crate::blobs::RestoreLosses`]).

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, TryLockError};
use std::io::{self, Write};
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use base64::Engine as _;
use serde_json::{Map, Value, json};
use sqlx::PgConnection;
use uuid::Uuid;

use crate::config::Settings;
use crate::keys::{Kek, KeyError, PROFILE_DELETED, WRAPPED_LEN};

/// The file name in the keyring folder.
pub const FILE: &str = "keyring.json";
const LOCK_FILE: &str = ".keyring.lock";
pub const SCHEMA: &str = "ownhealth.keyring/v1";

/// The profile keys of one install, wrapped under its KEK. `Debug` shows only counts.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct KeyringFile {
    /// The fingerprint of the KEK every key is wrapped under; None while the file holds no key yet.
    pub kek_fingerprint: Option<Vec<u8>>,
    /// The wrapped key of each profile (`profile_key.wrapped_dek`).
    pub profiles: BTreeMap<Uuid, Vec<u8>>,
    /// Profiles whose key was shredded: a restore reads their missing key as deleted, not lost.
    pub shredded: BTreeSet<Uuid>,
    /// The archive ids of the shredded keys, for the profiles of `shredded` whose key the file held.
    /// A profile there may hold a newer key in `profiles`.
    pub shredded_keys: BTreeMap<Uuid, BTreeSet<String>>,
}

impl KeyringFile {
    /// Whether the tombstone of `profile` closes the key with `archive_id`: a tombstone that names
    /// keys closes those, one that names none closes every key. `None` is a key of unknown id.
    pub fn closes(&self, profile: Uuid, archive_id: Option<&str>) -> bool {
        self.shredded.contains(&profile)
            && self
                .shredded_keys
                .get(&profile)
                .is_none_or(|ids| archive_id.is_none_or(|id| ids.contains(id)))
    }
}

impl std::fmt::Debug for KeyringFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "KeyringFile({} keys, {} shredded)",
            self.profiles.len(),
            self.shredded.len()
        )
    }
}

/// `keyring.json` in the configured keyring folder.
pub fn path(settings: &Settings) -> PathBuf {
    settings.keyring_path().join(FILE)
}

/// Only the owner may change the folder or file at `path`: a symlink, or a folder or file another user
/// or group may write, could hold keys someone else put there. Ok(false) when it does not exist. The
/// check looks at the path itself and never follows a symlink. `what` names it in the refusal
/// ("anchor", "keyring").
#[cfg(unix)]
pub fn owner_writes_only(path: &Path, folder: bool, what: &str) -> Result<bool, String> {
    use std::os::unix::fs::PermissionsExt;
    let meta = match fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e.to_string()),
    };
    let kind = if folder { "folder" } else { "file" };
    if meta.file_type().is_symlink() {
        return Err(format!("the {what} {kind} is a symlink"));
    }
    if (folder && !meta.is_dir()) || (!folder && !meta.is_file()) {
        return Err(format!("the {what} {kind} is not a {kind}"));
    }
    match meta.permissions().mode() & 0o022 {
        0 => Ok(true),
        0o020 => Err(format!("the {what} {kind} is group-writable")),
        _ => Err(format!("the {what} {kind} is world-writable")),
    }
}

#[cfg(not(unix))]
pub fn owner_writes_only(path: &Path, folder: bool, what: &str) -> Result<bool, String> {
    match fs::symlink_metadata(path) {
        Ok(m) if (folder && m.is_dir()) || (!folder && m.is_file()) => Ok(true),
        Ok(_) => Err(format!("the {what} has an unexpected file type")),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e.to_string()),
    }
}

fn unreadable(path: &Path, why: &str) -> KeyError {
    KeyError(format!(
        "cannot read the keyring {}: {why}. Put back the newest copy you have; without it no backup \
         opens the files of the profiles it holds, see deploy/README.md",
        path.display()
    ))
}

/// A keyring refused for where it sits or who may write it: its mode or path is the fix, never an
/// older copy of the file.
fn refused(path: &Path, why: &str) -> KeyError {
    KeyError(format!(
        "cannot read the keyring {}: {why}. Make the file and its folder plain ones that only its owner \
         may write (no symlink, no group or world write), or set OWNHEALTH_KEYRING_DIR to such a folder, \
         see deploy/README.md",
        path.display()
    ))
}

/// Resolve symlinks and '..' in the existing part of the path, as the system does when it opens it.
/// The last directories may not exist yet; a '..' among those cannot be resolved and is refused.
fn resolved(path: &Path) -> io::Result<PathBuf> {
    let absolute = std::path::absolute(path)?;
    let mut ancestor = absolute.as_path();
    let mut missing = Vec::new();
    loop {
        match fs::canonicalize(ancestor) {
            Ok(mut base) => {
                base.extend(missing.into_iter().rev());
                return Ok(base);
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                // A dangling symlink needs an operator's attention, just like a symlink loop.
                match fs::symlink_metadata(ancestor) {
                    Err(missing) if missing.kind() == io::ErrorKind::NotFound => {}
                    _ => return Err(e),
                }
                if ancestor.components().next_back() == Some(Component::ParentDir) {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "a '..' follows a folder that does not exist: use a path without '..' there",
                    ));
                }
                let Some((parent, name)) = ancestor.parent().zip(ancestor.file_name()) else {
                    return Err(e);
                };
                missing.push(name);
                ancestor = parent;
            }
            Err(e) => return Err(e),
        }
    }
}

/// Whether `path` lies in `parent`, including when their final directories have yet to be made.
pub fn inside(path: &Path, parent: &Path) -> Result<bool, KeyError> {
    let resolve =
        |p: &Path| resolved(p).map_err(|e| KeyError(format!("cannot resolve the folder {}: {e}", p.display())));
    Ok(resolve(path)?.starts_with(resolve(parent)?))
}

/// The keyring at `path`. None only when no keyring was written there yet. Anything else that is not
/// a valid keyring is an error, never an empty keyring: a damaged file must not silently drop keys.
pub fn read(path: &Path) -> Result<Option<KeyringFile>, KeyError> {
    let dir = path.parent().unwrap_or(Path::new("."));
    match owner_writes_only(dir, true, "keyring").and_then(|_| owner_writes_only(path, false, "keyring")) {
        Ok(true) => {}
        Ok(false) => return Ok(None),
        Err(why) => return Err(refused(path, &why)),
    }
    let bytes = match fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(unreadable(path, &e.to_string())),
    };
    let v: Value = serde_json::from_slice(&bytes).map_err(|e| unreadable(path, &e.to_string()))?;
    parse(&v).map(Some).ok_or_else(|| {
        unreadable(
            path,
            &format!("it is not an {SCHEMA} file with keys of {WRAPPED_LEN} bytes"),
        )
    })
}

/// Read the keyring and check that it belongs to the configured install.
pub fn read_for_install(settings: &Settings) -> Result<Option<KeyringFile>, KeyError> {
    let file = path(settings);
    let ring = read(&file)?;
    if let Some(have) = ring.as_ref().and_then(|r| r.kek_fingerprint.as_deref())
        && have != Kek::from_settings(settings)?.fingerprint()
    {
        return Err(foreign(&file));
    }
    Ok(ring)
}

fn foreign(file: &Path) -> KeyError {
    KeyError(format!(
        "the keyring {} was made under another OWNHEALTH_SECRETS_KEY: put back this install's \
         keyring.json, see deploy/README.md",
        file.display()
    ))
}

fn ensure_dir(dir: &Path) -> io::Result<()> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(dir)
}

/// Hold this guard across the read and any write or key installation. All databases and processes
/// that share the keyring take the same lock, including while `keyring.json` is replaced.
pub struct KeyringLock {
    _file: fs::File,
}

pub async fn lock(settings: &Settings) -> Result<KeyringLock, KeyError> {
    check_folder(settings)?;
    let dir = settings.keyring_path();
    let lock_path = dir.join(LOCK_FILE);
    let io_error = |e| KeyError(format!("cannot lock the keyring in {}: {e}", dir.display()));
    if !owner_writes_only(&dir, true, "keyring").map_err(|why| refused(&path(settings), &why))? {
        ensure_dir(&dir).map_err(io_error)?;
    }
    owner_writes_only(&lock_path, false, "keyring lock").map_err(|why| refused(&path(settings), &why))?;
    let mut opts = fs::OpenOptions::new();
    opts.write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let file = opts.open(&lock_path).map_err(io_error)?;
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(KeyringLock { _file: file }),
            Err(TryLockError::WouldBlock) => tokio::time::sleep(Duration::from_millis(50)).await,
            Err(TryLockError::Error(e)) => return Err(io_error(e)),
        }
    }
}

fn parse(v: &Value) -> Option<KeyringFile> {
    if v["schema"].as_str() != Some(SCHEMA) {
        return None;
    }
    let kek_fingerprint = match &v["kek_fingerprint"] {
        Value::Null => None,
        Value::String(h) => Some(hex::decode(h).ok().filter(|f| f.len() == 8)?),
        _ => return None,
    };
    let mut profiles = BTreeMap::new();
    for (id, w) in v["profiles"].as_object()? {
        let wrapped = base64::engine::general_purpose::STANDARD
            .decode(w.as_str()?)
            .ok()
            .filter(|w| w.len() == WRAPPED_LEN)?;
        profiles.insert(Uuid::parse_str(id).ok()?, wrapped);
    }
    let mut shredded = BTreeSet::new();
    for id in v["shredded"].as_array()? {
        shredded.insert(Uuid::parse_str(id.as_str()?).ok()?);
    }
    let shredded_keys = id_lists(&v["shredded_keys"])?;
    // a key with no fingerprint, ids for a profile not shredded, or a key kept under a tombstone that
    // closes every key, is not a file `write` makes
    if (kek_fingerprint.is_none() && !profiles.is_empty())
        || shredded_keys.keys().any(|p| !shredded.contains(p))
        || profiles
            .keys()
            .any(|p| shredded.contains(p) && !shredded_keys.contains_key(p))
    {
        return None;
    }
    Some(KeyringFile {
        kek_fingerprint,
        profiles,
        shredded,
        shredded_keys,
    })
}

/// `{"<profile uuid>": ["<64 hex>", ...]}`, each list not empty; absent (a file from before the field)
/// is empty.
fn id_lists(v: &Value) -> Option<BTreeMap<Uuid, BTreeSet<String>>> {
    let mut out = BTreeMap::new();
    let map = match v {
        Value::Null => return Some(out),
        v => v.as_object()?,
    };
    for (id, list) in map {
        let mut ids = BTreeSet::new();
        for hex in list.as_array()? {
            let hex = hex
                .as_str()
                .filter(|h| h.len() == 64 && h.bytes().all(|b| b.is_ascii_hexdigit()))?;
            ids.insert(hex.to_ascii_lowercase());
        }
        if ids.is_empty() {
            return None;
        }
        out.insert(Uuid::parse_str(id).ok()?, ids);
    }
    Some(out)
}

fn id_lists_json(lists: &BTreeMap<Uuid, BTreeSet<String>>) -> Map<String, Value> {
    lists.iter().map(|(id, ids)| (id.to_string(), json!(ids))).collect()
}

fn to_json(k: &KeyringFile) -> Value {
    let profiles: Map<String, Value> = k
        .profiles
        .iter()
        .map(|(id, w)| {
            (
                id.to_string(),
                Value::String(base64::engine::general_purpose::STANDARD.encode(w)),
            )
        })
        .collect();
    json!({
        "schema": SCHEMA,
        "kek_fingerprint": k.kek_fingerprint.as_ref().map(hex::encode),
        "profiles": profiles,
        "shredded": k.shredded.iter().map(Uuid::to_string).collect::<Vec<_>>(),
        "shredded_keys": id_lists_json(&k.shredded_keys),
    })
}

/// Replace the keyring in `dir` atomically: a new file only the owner may read (0600), written and
/// synced under a temporary name, then renamed over the old one and the folder synced. A crash leaves
/// the old keyring or the new one, never half of either, and the old one is gone once the rename is
/// on disk. A missing folder is made 0700.
pub fn write(dir: &Path, k: &KeyringFile) -> Result<(), KeyError> {
    let body = serde_json::to_string_pretty(&to_json(k))
        .map_err(|e| KeyError(format!("could not write the keyring: {e}")))?
        + "\n";
    let tmp = dir.join(format!(".{FILE}.{}.tmp", crate::ids::token_hex(6)));
    let written = (|| -> std::io::Result<()> {
        if fs::symlink_metadata(dir).is_err() {
            ensure_dir(dir)?;
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
        fs::rename(&tmp, dir.join(FILE))?;
        #[cfg(unix)]
        fs::File::open(dir)?.sync_all()?; // the rename is on disk only once the folder is
        Ok(())
    })();
    written.map_err(|e| {
        let _ = fs::remove_file(&tmp);
        KeyError(format!("cannot write the keyring in {}: {e}", dir.display()))
    })
}

/// The temporary copies a crash left between writing one and renaming it over the keyring: each still
/// holds every key it was written with, a shredded one too. Only a writer holding the keyring lock
/// calls this, so no other write is under way.
fn remove_leftovers(dir: &Path) -> Result<(), KeyError> {
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => {
            return Err(KeyError(format!(
                "cannot list the keyring folder {}: {e}",
                dir.display()
            )));
        }
    };
    for entry in entries {
        let entry = entry.map_err(|e| KeyError(format!("cannot list the keyring folder {}: {e}", dir.display())))?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with(&format!(".{FILE}.")) && name.ends_with(".tmp") {
            fs::remove_file(entry.path()).map_err(|e| {
                KeyError(format!(
                    "cannot remove the leftover copy {} of the keyring: {e}",
                    entry.path().display()
                ))
            })?;
        }
    }
    Ok(())
}

/// The key in the keyring at `file` that `profile` takes when it has no key row yet (a dump from
/// before 1.0 has no key table, so it brings its profiles back with none): a key made instead would
/// replace this one at the next rewrite, and every backup taken since is sealed under it. None when
/// the file holds no key for the profile or a named tombstone closes it. An unscoped tombstone
/// refuses key creation because it would also close the new key. A key that fails authentication is
/// refused so a new key cannot replace it.
pub fn live_key(file: &Path, kek: &Kek, profile: Uuid) -> Result<Option<Vec<u8>>, KeyError> {
    let Some(ring) = read(file)? else {
        return Ok(None);
    };
    if ring.shredded.contains(&profile) && !ring.shredded_keys.contains_key(&profile) {
        return Err(KeyError(PROFILE_DELETED.into()));
    }
    let Some(wrapped) = ring.profiles.get(&profile) else {
        return Ok(None);
    };
    let fingerprint = kek.fingerprint();
    if ring.kek_fingerprint.as_deref() != Some(&fingerprint[..]) {
        return Err(foreign(file));
    }
    let dek = kek.unwrap(profile, &fingerprint, wrapped).map_err(|_| {
        KeyError(format!(
            "the keyring {} holds a damaged key for profile {profile}: it does not open with \
             OWNHEALTH_SECRETS_KEY. Put back an undamaged copy of this install's keyring.json, see \
             deploy/README.md",
            file.display()
        ))
    })?;
    if ring.closes(profile, Some(&dek.archive_id())) {
        return Ok(None);
    }
    Ok(Some(wrapped.clone()))
}

/// Refuses a keyring folder inside the blob folder: every backup copies the blob folder.
pub fn check_folder(settings: &Settings) -> Result<(), KeyError> {
    let dir = settings.keyring_path();
    if crate::blobs::in_blob_folder(settings, &dir)? {
        return Err(KeyError(format!(
            "the keyring folder {} is inside the blob folder, so every backup would copy it: set \
             OWNHEALTH_KEYRING_DIR to a folder of its own, see deploy/README.md",
            dir.display()
        )));
    }
    Ok(())
}

/// A `profile_key` row: profile, wrapped key and KEK fingerprint, both null when shredded.
type KeyRow = (Uuid, Option<Vec<u8>>, Option<Vec<u8>>);

/// Merge the database keys into the current file while holding its lock. A tombstone from either
/// store wins over the key it closes ([`KeyringFile::closes`]). Profiles unknown to the database keep
/// their key in the file.
pub async fn rewrite(conn: &mut PgConnection, settings: &Settings) -> Result<KeyringFile, KeyError> {
    let _lock = lock(settings).await?;
    let dir = settings.keyring_path();
    let mut ring = read_for_install(settings)?.unwrap_or_default();
    let rows: Vec<KeyRow> =
        sqlx::query_as("select profile_id, wrapped_dek, kek_fingerprint from profile_key order by profile_id")
            .fetch_all(&mut *conn)
            .await?;
    remove_leftovers(&dir)?;
    // the id of a wrapped key, or None when it does not open (then it counts as an unknown key)
    let kek = Kek::from_settings(settings).ok();
    let archive_id = |id: Uuid, f: &[u8], w: &[u8]| {
        kek.as_ref()
            .and_then(|k| k.unwrap(id, f, w).ok())
            .map(|dek| dek.archive_id())
    };
    for (id, wrapped, fingerprint) in rows {
        match (wrapped, fingerprint) {
            (Some(w), Some(f)) => {
                if ring.shredded.contains(&id) && ring.closes(id, archive_id(id, &f, &w).as_deref()) {
                    continue;
                }
                if ring.kek_fingerprint.as_ref().is_some_and(|have| *have != f) {
                    return Err(foreign(&path(settings)));
                }
                ring.kek_fingerprint = Some(f);
                ring.profiles.insert(id, w);
            }
            _ => {
                let fingerprint = ring.kek_fingerprint.clone().unwrap_or_default();
                let named = ring.shredded_keys.contains_key(&id) || !ring.shredded.contains(&id);
                let removed = ring.profiles.remove(&id);
                ring.shredded.insert(id);
                match removed.map(|w| archive_id(id, &fingerprint, &w)) {
                    // the file held the key: the tombstone names it
                    Some(Some(key)) if named => {
                        ring.shredded_keys.entry(id).or_default().insert(key);
                    }
                    // a key the file held but cannot name: close every key of the profile
                    Some(_) => {
                        ring.shredded_keys.remove(&id);
                    }
                    // nothing new to close: the tombstone stays as it is, or closes every key
                    None => {}
                }
            }
        }
    }
    write(&dir, &ring)?;
    // The keyring is written, so no key work is pending. A loss record that cannot be read or pruned
    // stays as it is, and the next rewrite prunes it.
    if let Err(error) = prune_restore_losses(conn, settings).await {
        tracing::warn!(%error, "the keyring was rewritten; the restore loss record was left for the next rewrite");
    }
    Ok(ring)
}

/// Restoring an older dump can bring a tombstoned profile's records back. Keep its accepted losses
/// until the profile row actually goes, including on a second delete.
async fn prune_restore_losses(conn: &mut PgConnection, settings: &Settings) -> Result<(), KeyError> {
    let mut losses = crate::blobs::read_restore_losses(settings).map_err(|e| KeyError(e.to_string()))?;
    if losses.is_empty() {
        return Ok(());
    }
    let ids: Vec<_> = losses.keys().copied().collect();
    let present: Vec<Uuid> = sqlx::query_scalar("select id from profile where id = any($1)")
        .bind(&ids)
        .fetch_all(conn)
        .await?;
    if present.len() < ids.len() {
        losses.retain(|id, _| present.contains(id));
        crate::blobs::write_restore_losses(settings, &losses).map_err(|e| KeyError(e.to_string()))?;
    }
    Ok(())
}
