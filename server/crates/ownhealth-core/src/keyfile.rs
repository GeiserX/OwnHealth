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
//!  "profiles": {"<profile uuid>": "<base64 wrapped key>"}, "shredded": ["<profile uuid>"]}
//! ```
//!
//! A rewrite merges the table into the file: a key the table holds goes in, a key the table shredded
//! goes out and its profile into `shredded`, and a profile the table does not know keeps what the file
//! says. After a restore from an older backup, the table does not know the profiles made since; their
//! keys stay, so a newer backup still restores. `shredded` tells a restore that a missing key was
//! deleted on purpose, not lost.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use base64::Engine as _;
use serde_json::{Map, Value, json};
use sqlx::{Connection, PgConnection};
use uuid::Uuid;

use crate::config::Settings;
use crate::keys::{KeyError, WRAPPED_LEN};

/// The file name in the keyring folder.
pub const FILE: &str = "keyring.json";
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
    if folder != meta.is_dir() {
        return Err(format!("the {what} {kind} is not a {kind}"));
    }
    match meta.permissions().mode() & 0o022 {
        0 => Ok(true),
        0o020 => Err(format!("the {what} {kind} is group-writable")),
        _ => Err(format!("the {what} {kind} is world-writable")),
    }
}

#[cfg(not(unix))]
pub fn owner_writes_only(path: &Path, _folder: bool, _what: &str) -> Result<bool, String> {
    Ok(path.exists())
}

fn unreadable(path: &Path, why: &str) -> KeyError {
    KeyError(format!(
        "cannot read the keyring {}: {why}. Put back the newest copy you have; without it no backup \
         opens the files of the profiles it holds, see deploy/README.md",
        path.display()
    ))
}

/// `path` lies in `parent`, both made absolute first (neither needs to exist).
pub fn inside(path: &Path, parent: &Path) -> bool {
    match (std::path::absolute(path), std::path::absolute(parent)) {
        (Ok(p), Ok(d)) => p.starts_with(d),
        _ => false,
    }
}

/// The keyring at `path`. None only when no keyring was written there yet. Anything else that is not
/// a valid keyring is an error, never an empty keyring: a damaged file must not silently drop keys.
pub fn read(path: &Path) -> Result<Option<KeyringFile>, KeyError> {
    let dir = path.parent().unwrap_or(Path::new("."));
    match owner_writes_only(dir, true, "keyring").and_then(|_| owner_writes_only(path, false, "keyring")) {
        Ok(true) => {}
        Ok(false) => return Ok(None),
        Err(why) => return Err(unreadable(path, &why)),
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
    // a key with no fingerprint, or a profile both kept and shredded, is not a file `write` makes
    if (kek_fingerprint.is_none() && !profiles.is_empty()) || profiles.keys().any(|p| shredded.contains(p)) {
        return None;
    }
    Some(KeyringFile {
        kek_fingerprint,
        profiles,
        shredded,
    })
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

/// Refuses a keyring folder inside the blob folder: every backup copies the blob folder.
pub fn check_folder(settings: &Settings) -> Result<(), KeyError> {
    let dir = settings.keyring_path();
    if crate::blobs::in_blob_folder(settings, &dir) {
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

/// Merge `profile_key` into the keyring file and write it: every key the table holds goes in, every
/// key it shredded goes out (its profile into `shredded`), and a profile the table does not know keeps
/// what the file says. Writers take one database lock, so the last rewrite reads the newest table and
/// a backup that read an older one never puts a shredded key back. Refuses a keyring folder inside
/// the blob folder (every backup would copy it) and a keyring made under another KEK. Returns what it
/// wrote.
pub async fn rewrite(conn: &mut PgConnection, settings: &Settings) -> Result<KeyringFile, KeyError> {
    check_folder(settings)?;
    let dir = settings.keyring_path();
    let file = dir.join(FILE);
    let mut tx = conn.begin().await?;
    sqlx::query("select pg_catalog.pg_advisory_xact_lock(pg_catalog.hashtext('ownhealth.keyring'))")
        .execute(&mut *tx)
        .await?;
    let rows: Vec<KeyRow> =
        sqlx::query_as("select profile_id, wrapped_dek, kek_fingerprint from profile_key order by profile_id")
            .fetch_all(&mut *tx)
            .await?;
    let mut ring = read(&file)?.unwrap_or_default();
    for (id, wrapped, fingerprint) in rows {
        match (wrapped, fingerprint) {
            (Some(w), Some(f)) => {
                if ring.kek_fingerprint.as_ref().is_some_and(|have| *have != f) {
                    return Err(KeyError(format!(
                        "the keyring {} was made under another OWNHEALTH_SECRETS_KEY than this install's \
                         profile keys: put back this install's keyring.json, see deploy/README.md",
                        file.display()
                    )));
                }
                ring.kek_fingerprint = Some(f);
                ring.shredded.remove(&id);
                ring.profiles.insert(id, w);
            }
            _ => {
                ring.profiles.remove(&id);
                ring.shredded.insert(id);
            }
        }
    }
    write(&dir, &ring)?;
    tx.commit().await?;
    Ok(ring)
}
