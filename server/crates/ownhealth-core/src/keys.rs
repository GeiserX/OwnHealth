//! Per-profile data keys (security-privacy.md 7.1): each profile's files are encrypted under that
//! profile's own key, so a copy of the blob folder alone opens nothing, and deleting a profile's key
//! leaves its files unreadable.
//!
//! The key hierarchy:
//!
//! - The key-encryption key (KEK) is HKDF-SHA256(OWNHEALTH_SECRETS_KEY, info `ownhealth kek v1`). It
//!   lives only in memory; the operator keeps OWNHEALTH_SECRETS_KEY with the backups.
//! - Each profile has one random 256-bit data key (DEK), stored in `profile_key` wrapped with
//!   AES-256-GCM under the KEK (the profile id is the associated data), with the KEK's fingerprint.
//! - Each pending item (a held mail, a document with no profile yet) has a DEK of its own the same
//!   way, in `pending_key` under its item id. When the item gets a profile, its files are sealed again
//!   under the profile's DEK and the item's key is shredded ([`shred_pending`]).
//! - A profile's DEK also seals the secrets of its connections (`secrets.rs`, a key derived for
//!   secrets alone).
//! - The DEK derives the file names (HMAC-SHA256 of the plaintext sha256, so a name says nothing about
//!   the bytes) and, with a random salt per file, each file's own record key.
//!
//! A file is a header and records of 64 KiB of plaintext, each sealed with AES-256-GCM:
//!
//! ```text
//! header  magic "OHB\x01" | salt (16) | nonce prefix (7) | binding (32)
//! record  ciphertext of up to 64 KiB | tag (16)      nonce = prefix | counter u32 BE | last flag
//! ```
//!
//! The record key is HKDF(DEK, salt), so a nonce never repeats under one key: the counter counts the
//! records of one file, and every file has its own key. Every record but the last holds exactly 64 KiB;
//! the last holds 1 to 64 KiB, and is empty only in an empty file. The last flag is part of the nonce,
//! and the reader opens the last record before it hands out a byte, so a cut file fails when it opens
//! instead of reading short; the counter makes a reordered, repeated or dropped record fail. The binding
//! is an HMAC over the owner and the plaintext sha256, so a file copied under another name or another
//! profile does not open.
//!
//! No key, DEK or record key is ever logged, printed or put in an error: `Debug` of every key type
//! shows at most the KEK's fingerprint, and the key bytes are zeroed when they drop.

use std::collections::HashMap;
use std::io::{self, Read, Seek, SeekFrom};
use std::sync::{Arc, Mutex};

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Nonce};
use base64::Engine as _;
use hkdf::Hkdf;
use hmac::{Hmac, Mac};
use sha2::Sha256;
use sqlx::{Connection, PgConnection};
use uuid::Uuid;
use zeroize::Zeroizing;

use crate::config::Settings;

type HmacSha256 = Hmac<Sha256>;

/// Plaintext bytes per record.
pub const RECORD: usize = 64 * 1024;
const TAG: usize = 16;
const MAGIC: [u8; 4] = *b"OHB\x01";
const SALT: usize = 16;
const PREFIX: usize = 7;
const BIND: usize = 32;
/// Header bytes before the first record.
pub const HEADER: usize = MAGIC.len() + SALT + PREFIX + BIND;
const WRAP_VERSION: u8 = 1;
const WRAP_NONCE: usize = 12;
/// `version | nonce | ciphertext of the 32-byte DEK | tag`: the length migration 1290 checks.
pub const WRAPPED_LEN: usize = 1 + WRAP_NONCE + 32 + TAG;

/// A key problem, said without any key material.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct KeyError(pub String);

impl KeyError {
    fn new(msg: impl Into<String>) -> KeyError {
        KeyError(msg.into())
    }
}

impl From<sqlx::Error> for KeyError {
    fn from(e: sqlx::Error) -> Self {
        KeyError(format!("could not read the profile keys: {e}"))
    }
}

/// Whose data key: a profile's, or one pending item's (a held mail, a document with no profile yet).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Slot {
    Profile(Uuid),
    Pending(Uuid),
}

impl Slot {
    pub fn id(self) -> Uuid {
        match self {
            Slot::Profile(id) | Slot::Pending(id) => id,
        }
    }

    /// The table and the id column that hold this slot's wrapped key.
    fn table(self) -> (&'static str, &'static str) {
        match self {
            Slot::Profile(_) => ("profile_key", "profile_id"),
            Slot::Pending(_) => ("pending_key", "item_id"),
        }
    }

    /// The associated data of the wrapped key: a profile key never opens as an item's, or back.
    fn aad(self) -> Vec<u8> {
        let mut a = match self {
            Slot::Profile(_) => b"ownhealth profile dek v1".to_vec(),
            Slot::Pending(_) => b"ownhealth pending dek v1".to_vec(),
        };
        a.extend_from_slice(self.id().as_bytes());
        a
    }
}

/// The one line a missing key prints: what to set and how to make one.
pub const MISSING_KEY: &str = "OWNHEALTH_SECRETS_KEY is not set: it encrypts every profile's files and inbox secrets; \
     generate one with `openssl rand -base64 32` and set it (or OWNHEALTH_SECRETS_KEY_FILE), see deploy/README.md";

/// The one line `restore` and `verify-restore` print without a key: a new key never opens a backup.
pub const MISSING_BACKUP_KEY: &str = "OWNHEALTH_SECRETS_KEY is not set: set the key the backup was made with \
     (or OWNHEALTH_SECRETS_KEY_FILE); its fingerprint is keys.kek_fingerprint in the backup's manifest.json, \
     and a new key never opens the backup, see deploy/README.md";

fn hkdf32(ikm: &[u8], salt: Option<&[u8]>, info: &[u8]) -> Zeroizing<[u8; 32]> {
    let mut out = Zeroizing::new([0u8; 32]);
    // 32 bytes is far below HKDF-SHA256's limit of 255 * 32, so expand cannot fail
    let _ = Hkdf::<Sha256>::new(salt, ikm).expand(info, &mut out[..]);
    out
}

fn hmac(key: &[u8], parts: &[&[u8]]) -> HmacSha256 {
    // HMAC takes a key of any length
    let mut mac = <HmacSha256 as KeyInit>::new_from_slice(key).unwrap_or_else(|_| unreachable!());
    for p in parts {
        mac.update(p);
    }
    mac
}

fn cipher(key: &[u8; 32]) -> Aes256Gcm {
    Aes256Gcm::new(key.into())
}

/// The key-encryption key. Cheap to clone; `Debug` shows only its fingerprint.
#[derive(Clone)]
pub struct Kek {
    key: Arc<Zeroizing<[u8; 32]>>,
    fingerprint: [u8; 8],
}

impl std::fmt::Debug for Kek {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Kek({})", hex::encode(self.fingerprint))
    }
}

impl Kek {
    /// The KEK of the 32 bytes OWNHEALTH_SECRETS_KEY decodes to.
    pub fn from_secret(secret: &[u8]) -> Result<Kek, KeyError> {
        if secret.len() != 32 {
            return Err(KeyError(format!(
                "OWNHEALTH_SECRETS_KEY must be 32 bytes, got {}",
                secret.len()
            )));
        }
        let key = hkdf32(secret, None, b"ownhealth kek v1");
        let mac = hmac(&key[..], &[b"ownhealth kek fingerprint v1"])
            .finalize()
            .into_bytes();
        let mut fingerprint = [0u8; 8];
        fingerprint.copy_from_slice(&mac[..8]);
        Ok(Kek {
            key: Arc::new(key),
            fingerprint,
        })
    }

    /// The KEK of OWNHEALTH_SECRETS_KEY as it is set: standard base64 of 32 bytes.
    pub fn from_base64(value: &str) -> Result<Kek, KeyError> {
        let secret = Zeroizing::new(
            base64::engine::general_purpose::STANDARD
                .decode(value.trim())
                .map_err(|_| KeyError::new("OWNHEALTH_SECRETS_KEY is not standard base64"))?,
        );
        Kek::from_secret(&secret)
    }

    /// The configured KEK; the one-line fix when OWNHEALTH_SECRETS_KEY is not set.
    pub fn from_settings(settings: &Settings) -> Result<Kek, KeyError> {
        Kek::from_base64(
            settings
                .secrets_key
                .as_deref()
                .ok_or_else(|| KeyError::new(MISSING_KEY))?,
        )
    }

    /// 8 bytes that name this KEK without revealing it (an HMAC under the KEK).
    pub fn fingerprint(&self) -> [u8; 8] {
        self.fingerprint
    }

    /// `dek` wrapped for `profile`: `version | nonce | ciphertext | tag`.
    pub fn wrap(&self, profile: Uuid, dek: &Dek) -> Result<Vec<u8>, KeyError> {
        self.wrap_slot(Slot::Profile(profile), dek)
    }

    /// `dek` wrapped for `slot`.
    pub fn wrap_slot(&self, slot: Slot, dek: &Dek) -> Result<Vec<u8>, KeyError> {
        let mut nonce = [0u8; WRAP_NONCE];
        rand::fill(&mut nonce[..]);
        let ct = cipher(&self.key)
            .encrypt(
                &Nonce::from(nonce),
                Payload {
                    msg: &dek.key[..],
                    aad: &slot.aad(),
                },
            )
            .map_err(|_| KeyError::new("could not wrap a profile key"))?;
        let mut out = Vec::with_capacity(WRAPPED_LEN);
        out.push(WRAP_VERSION);
        out.extend_from_slice(&nonce);
        out.extend_from_slice(&ct);
        Ok(out)
    }

    /// The DEK `wrap` made for `profile`. Another KEK (by its fingerprint), another profile or a
    /// changed byte is refused, without detail.
    pub fn unwrap(&self, profile: Uuid, fingerprint: &[u8], wrapped: &[u8]) -> Result<Dek, KeyError> {
        self.unwrap_slot(Slot::Profile(profile), fingerprint, wrapped)
    }

    /// The DEK `wrap_slot` made for `slot`.
    pub fn unwrap_slot(&self, slot: Slot, fingerprint: &[u8], wrapped: &[u8]) -> Result<Dek, KeyError> {
        if fingerprint != self.fingerprint {
            return Err(KeyError::new(WRONG_KEY));
        }
        let refused = || KeyError::new("a profile key does not open with this OWNHEALTH_SECRETS_KEY");
        if wrapped.len() != WRAPPED_LEN || wrapped[0] != WRAP_VERSION {
            return Err(refused());
        }
        let mut nonce = [0u8; WRAP_NONCE];
        nonce.copy_from_slice(&wrapped[1..1 + WRAP_NONCE]);
        let plain = Zeroizing::new(
            cipher(&self.key)
                .decrypt(
                    &Nonce::from(nonce),
                    Payload {
                        msg: &wrapped[1 + WRAP_NONCE..],
                        aad: &slot.aad(),
                    },
                )
                .map_err(|_| refused())?,
        );
        let mut key = Zeroizing::new([0u8; 32]);
        key.copy_from_slice(&plain);
        Ok(Dek::from_key(key))
    }
}

/// One profile's data key, with the file-name key it derives. Never cloned or printed.
pub struct Dek {
    key: Zeroizing<[u8; 32]>,
    name: Zeroizing<[u8; 32]>,
}

impl std::fmt::Debug for Dek {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Dek(***)")
    }
}

impl Dek {
    fn from_key(key: Zeroizing<[u8; 32]>) -> Dek {
        let name = hkdf32(&key[..], None, b"ownhealth blob name v1");
        Dek { key, name }
    }

    /// A fresh random key.
    pub fn generate() -> Dek {
        let mut key = Zeroizing::new([0u8; 32]);
        rand::fill(&mut key[..]);
        Dek::from_key(key)
    }

    /// The on-disk name of the file whose plaintext hashes to `sha` (32 bytes): 64 hex characters.
    pub fn address(&self, sha: &[u8; 32]) -> String {
        hex::encode(
            hmac(&self.name[..], &[b"ownhealth blob address v1", sha])
                .finalize()
                .into_bytes(),
        )
    }

    /// `value` sealed under a key this DEK derives for secrets alone: `nonce | ciphertext | tag`,
    /// bound to `aad`. For a profile's connection secrets (`secrets.rs`).
    pub fn seal_value(&self, value: &[u8], aad: &[u8]) -> Result<Vec<u8>, KeyError> {
        let key = hkdf32(&self.key[..], None, b"ownhealth profile secret v1");
        let mut nonce = [0u8; WRAP_NONCE];
        rand::fill(&mut nonce[..]);
        let ct = cipher(&key)
            .encrypt(&Nonce::from(nonce), Payload { msg: value, aad })
            .map_err(|_| KeyError::new("could not seal a secret"))?;
        let mut out = Vec::with_capacity(WRAP_NONCE + ct.len());
        out.extend_from_slice(&nonce);
        out.extend_from_slice(&ct);
        Ok(out)
    }

    /// What `seal_value` made with this DEK and `aad`. Another key, other associated data or a
    /// changed byte is the same error, without detail.
    pub fn open_value(&self, sealed: &[u8], aad: &[u8]) -> Result<Zeroizing<Vec<u8>>, KeyError> {
        let refused = || KeyError::new("the stored secret does not open with its profile's key");
        if sealed.len() < WRAP_NONCE + TAG {
            return Err(refused());
        }
        let key = hkdf32(&self.key[..], None, b"ownhealth profile secret v1");
        let mut nonce = [0u8; WRAP_NONCE];
        nonce.copy_from_slice(&sealed[..WRAP_NONCE]);
        cipher(&key)
            .decrypt(
                &Nonce::from(nonce),
                Payload {
                    msg: &sealed[WRAP_NONCE..],
                    aad,
                },
            )
            .map(Zeroizing::new)
            .map_err(|_| refused())
    }

    /// The record key and the binding key of one file, from its salt.
    fn file_keys(&self, salt: &[u8]) -> (Aes256Gcm, Zeroizing<[u8; 32]>) {
        let key = hkdf32(&self.key[..], Some(salt), b"ownhealth blob records v1");
        let bind = hkdf32(&self.key[..], Some(salt), b"ownhealth blob binding v1");
        (cipher(&key), bind)
    }
}

fn record_aad(header: &[u8], owner: Uuid) -> Vec<u8> {
    // the header without the binding, which is written last
    let mut a = header[..MAGIC.len() + SALT + PREFIX].to_vec();
    a.extend_from_slice(owner.as_bytes());
    a
}

fn nonce(prefix: &[u8], counter: u32, last: bool) -> aes_gcm::aead::Nonce<Aes256Gcm> {
    let mut n = [0u8; 12];
    n[..PREFIX].copy_from_slice(prefix);
    n[PREFIX..PREFIX + 4].copy_from_slice(&counter.to_be_bytes());
    n[11] = u8::from(last);
    Nonce::from(n)
}

fn binding(bind: &[u8; 32], owner: Uuid, sha: &[u8; 32]) -> HmacSha256 {
    hmac(&bind[..], &[owner.as_bytes(), sha])
}

fn corrupt() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        "an encrypted blob does not open: it was changed, cut or reordered, or belongs to another profile",
    )
}

/// Seals one file, record by record. The caller writes `HEADER` placeholder bytes, then every
/// `seal` output in order (the last one with `last`), then puts `header(sha)` at offset 0.
pub struct Sealer {
    cipher: Aes256Gcm,
    bind: Zeroizing<[u8; 32]>,
    header: [u8; HEADER],
    aad: Vec<u8>,
    owner: Uuid,
    counter: u32,
    done: bool,
}

impl std::fmt::Debug for Sealer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Sealer(***)")
    }
}

impl Sealer {
    pub fn new(dek: &Dek, owner: Uuid) -> Sealer {
        let mut header = [0u8; HEADER];
        header[..MAGIC.len()].copy_from_slice(&MAGIC);
        rand::fill(&mut header[MAGIC.len()..MAGIC.len() + SALT + PREFIX]);
        let (cipher, bind) = dek.file_keys(&header[MAGIC.len()..MAGIC.len() + SALT]);
        Sealer {
            cipher,
            bind,
            aad: record_aad(&header, owner),
            header,
            owner,
            counter: 0,
            done: false,
        }
    }

    /// The sealed record of `chunk`: exactly `RECORD` bytes, except the last record (`last`), which
    /// holds 1 to `RECORD` bytes, or none in an empty file. Nothing may follow the last record.
    pub fn seal(&mut self, chunk: &[u8], last: bool) -> io::Result<Vec<u8>> {
        let fits = if last {
            chunk.len() <= RECORD && (self.counter == 0 || !chunk.is_empty())
        } else {
            chunk.len() == RECORD
        };
        if self.done || !fits {
            return Err(io::Error::other(
                "a sealed blob takes records of exactly 64 KiB, then one last record of at most 64 KiB",
            ));
        }
        let prefix = &self.header[MAGIC.len() + SALT..MAGIC.len() + SALT + PREFIX];
        let out = self
            .cipher
            .encrypt(
                &nonce(prefix, self.counter, last),
                Payload {
                    msg: chunk,
                    aad: &self.aad,
                },
            )
            .map_err(|_| io::Error::other("could not encrypt a blob record"))?;
        if last {
            self.done = true;
        } else {
            self.counter = self
                .counter
                .checked_add(1)
                .ok_or_else(|| io::Error::other("a blob larger than 256 TiB"))?;
        }
        Ok(out)
    }

    /// The header to write at offset 0 once the plaintext sha256 is known.
    pub fn header(&self, sha: &[u8; 32]) -> [u8; HEADER] {
        let mut h = self.header;
        let tag = binding(&self.bind, self.owner, sha).finalize().into_bytes();
        h[HEADER - BIND..].copy_from_slice(&tag);
        h
    }
}

/// Reads a sealed file as its plaintext, with seeks. Every record is checked when it is read; a
/// failed check is an `InvalidData` error, never short or changed bytes.
pub struct SealedReader<F> {
    file: F,
    cipher: Aes256Gcm,
    prefix: [u8; PREFIX],
    aad: Vec<u8>,
    records: u64,
    last_len: u64,
    plain_len: u64,
    pos: u64,
    current: Option<(u64, Zeroizing<Vec<u8>>)>,
}

impl<F> std::fmt::Debug for SealedReader<F> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "SealedReader(len {}, at {})", self.plain_len, self.pos)
    }
}

const SEALED: u64 = (RECORD + TAG) as u64;

impl<F: Read + Seek> SealedReader<F> {
    /// Opens `file`, sealed for `owner` over plaintext whose sha256 is `sha`: checks the binding, and
    /// opens the last record, so a file cut anywhere, or with a changed last record, fails here.
    pub fn open(mut file: F, dek: &Dek, owner: Uuid, sha: &[u8; 32]) -> io::Result<SealedReader<F>> {
        let total = file.seek(SeekFrom::End(0))?;
        file.seek(SeekFrom::Start(0))?;
        let mut header = [0u8; HEADER];
        if total < (HEADER + TAG) as u64 {
            return Err(corrupt());
        }
        file.read_exact(&mut header)?;
        if header[..MAGIC.len()] != MAGIC {
            return Err(corrupt());
        }
        let (cipher, bind) = dek.file_keys(&header[MAGIC.len()..MAGIC.len() + SALT]);
        binding(&bind, owner, sha)
            .verify_slice(&header[HEADER - BIND..])
            .map_err(|_| corrupt())?;
        let body = total - HEADER as u64;
        let records = body.div_ceil(SEALED);
        let last_len = body - (records - 1) * SEALED;
        // an empty last record only in an empty file: after whole records, the length is a cut
        if last_len < TAG as u64 || (records > 1 && last_len == TAG as u64) || records > u64::from(u32::MAX) + 1 {
            return Err(corrupt());
        }
        let mut prefix = [0u8; PREFIX];
        prefix.copy_from_slice(&header[MAGIC.len() + SALT..MAGIC.len() + SALT + PREFIX]);
        let mut reader = SealedReader {
            file,
            cipher,
            prefix,
            aad: record_aad(&header, owner),
            records,
            last_len,
            plain_len: body - records * TAG as u64,
            pos: 0,
            current: None,
        };
        // the last record says the file ends there: without this, a file cut to whole records plus
        // 16 bytes would read short, and an empty file would never check its tag
        reader.load(records - 1)?;
        Ok(reader)
    }

    /// The plaintext size, from the file length (the records check it as they are read).
    pub fn len(&self) -> u64 {
        self.plain_len
    }

    pub fn is_empty(&self) -> bool {
        self.plain_len == 0
    }

    fn load(&mut self, index: u64) -> io::Result<()> {
        if self.current.as_ref().is_some_and(|(i, _)| *i == index) {
            return Ok(());
        }
        let last = index + 1 == self.records;
        let len = if last { self.last_len } else { SEALED };
        self.file.seek(SeekFrom::Start(HEADER as u64 + index * SEALED))?;
        let mut sealed = vec![0u8; usize::try_from(len).map_err(|_| corrupt())?];
        self.file.read_exact(&mut sealed).map_err(|_| corrupt())?;
        let counter = u32::try_from(index).map_err(|_| corrupt())?;
        let plain = self
            .cipher
            .decrypt(
                &nonce(&self.prefix, counter, last),
                Payload {
                    msg: &sealed,
                    aad: &self.aad,
                },
            )
            .map_err(|_| corrupt())?;
        // only the last record may be short
        if !last && plain.len() != RECORD {
            return Err(corrupt());
        }
        self.current = Some((index, Zeroizing::new(plain)));
        Ok(())
    }
}

impl<F: Read + Seek> Read for SealedReader<F> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() || self.pos >= self.plain_len {
            return Ok(0);
        }
        let index = self.pos / RECORD as u64;
        self.load(index)?;
        let offset = usize::try_from(self.pos % RECORD as u64).map_err(|_| corrupt())?;
        let Some((_, plain)) = &self.current else {
            return Err(corrupt());
        };
        let n = buf.len().min(plain.len().saturating_sub(offset));
        if n == 0 {
            return Err(corrupt());
        }
        buf[..n].copy_from_slice(&plain[offset..offset + n]);
        self.pos += n as u64;
        Ok(n)
    }
}

impl<F: Read + Seek> Seek for SealedReader<F> {
    fn seek(&mut self, to: SeekFrom) -> io::Result<u64> {
        let next = match to {
            SeekFrom::Start(n) => Some(n),
            SeekFrom::End(d) => self.plain_len.checked_add_signed(d),
            SeekFrom::Current(d) => self.pos.checked_add_signed(d),
        };
        self.pos = next.ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "seek before the start"))?;
        Ok(self.pos)
    }
}

enum Source {
    /// `profile_key` in this database, read on a connection of its own (blob calls are not async).
    Db(String),
    /// Keys kept only in memory, for tests of the blob layer.
    #[cfg(any(test, feature = "testing"))]
    Memory,
}

/// The keys this process opened, and how many times `forget` ran: a key read while `forget` ran is
/// never kept, so a deleted profile's key cannot come back into the cache.
#[derive(Default)]
struct Cache {
    keys: HashMap<Slot, Arc<Dek>>,
    forgets: u64,
}

/// The profile and pending-item keys this process has opened, and where to find the others.
pub struct Keyring {
    kek: Kek,
    source: Source,
    cache: Mutex<Cache>,
}

impl std::fmt::Debug for Keyring {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Keyring({:?})", self.kek)
    }
}

impl Keyring {
    /// Keys in the `profile_key` and `pending_key` tables of the database at `url` (a sqlx URL).
    pub fn new(kek: Kek, url: impl Into<String>) -> Keyring {
        Keyring {
            kek,
            source: Source::Db(url.into()),
            cache: Mutex::new(Cache::default()),
        }
    }

    /// Keys kept in memory only, for the blob layer's own tests.
    #[cfg(any(test, feature = "testing"))]
    pub fn in_memory(kek: Kek) -> Keyring {
        Keyring {
            kek,
            source: Source::Memory,
            cache: Mutex::new(Cache::default()),
        }
    }

    pub fn kek(&self) -> &Kek {
        &self.kek
    }

    /// Whether the keys live in a database (the app), not only in memory (blob tests).
    pub fn stored(&self) -> bool {
        matches!(self.source, Source::Db(_))
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Cache>, KeyError> {
        self.cache
            .lock()
            .map_err(|_| KeyError::new("the keyring lock is poisoned"))
    }

    /// Drop the key this process kept for `profile`, after its row went with the profile. A lookup
    /// that started before this call keeps nothing.
    pub fn forget(&self, profile: Uuid) {
        self.forget_slot(Slot::Profile(profile));
    }

    /// Drop the key this process kept for `slot` (see [`Keyring::forget`]).
    pub fn forget_slot(&self, slot: Slot) {
        if let Ok(mut c) = self.cache.lock() {
            c.keys.remove(&slot);
            c.forgets = c.forgets.wrapping_add(1);
        }
    }

    /// The cached key of `slot`, and the `forget` count a lookup that starts now must see again.
    fn cached(&self, slot: Slot) -> Result<(Option<Arc<Dek>>, u64), KeyError> {
        let c = self.lock()?;
        Ok((c.keys.get(&slot).cloned(), c.forgets))
    }

    /// Keep `dek` for `slot` unless `forget` ran since the lookup began (`seen`): the check and
    /// the insert hold one lock. The key goes back to the caller either way.
    fn publish(&self, slot: Slot, dek: Arc<Dek>, seen: u64) -> Result<Arc<Dek>, KeyError> {
        let mut c = self.lock()?;
        if c.forgets != seen {
            return Ok(dek);
        }
        Ok(c.keys.entry(slot).or_insert(dek).clone())
    }

    /// The DEK of `profile`. With `create`, a profile with none gets one (two processes racing keep
    /// the first); without, None when the profile has none.
    pub fn dek(&self, profile: Uuid, create: bool) -> Result<Option<Arc<Dek>>, KeyError> {
        self.dek_for(Slot::Profile(profile), create)
    }

    /// The DEK of `slot`, as [`Keyring::dek`] does for a profile. A shredded key (a deleted profile,
    /// a pending item that got its profile or was deleted) is None, and refused with `create`.
    pub fn dek_for(&self, slot: Slot, create: bool) -> Result<Option<Arc<Dek>>, KeyError> {
        let (cached, seen) = self.cached(slot)?;
        if let Some(d) = cached {
            return Ok(Some(d));
        }
        let found = match &self.source {
            Source::Db(url) => {
                let new = if create {
                    Some(self.kek.wrap_slot(slot, &Dek::generate())?)
                } else {
                    None
                };
                match side_lookup(url, slot, new, self.kek.fingerprint())? {
                    Some((wrapped, fp)) => Some(self.kek.unwrap_slot(slot, &fp, &wrapped)?),
                    None => None,
                }
            }
            #[cfg(any(test, feature = "testing"))]
            Source::Memory => create.then(Dek::generate),
        };
        let Some(dek) = found else {
            return Ok(None);
        };
        Ok(Some(self.publish(slot, Arc::new(dek), seen)?))
    }
}

/// A `profile_key` or `pending_key` row: `wrapped_dek`, `kek_fingerprint`.
type Stored = (Vec<u8>, Vec<u8>);

/// Whether a key made under another KEK is stored in either key table.
const OTHER_KEK: &str = "(exists (select 1 from profile_key where kek_fingerprint <> $1) \
     or exists (select 1 from pending_key where kek_fingerprint <> $1))";

/// The stored key of `slot` (`wrapped_dek`, `kek_fingerprint`), after inserting `new` when given
/// and none is stored. Blob reads and writes are not async, so this runs on a thread of its own with
/// its own connection: it never waits on the caller's pool or runtime. It runs once per key per
/// process. The row has no foreign key to its profile or item: the profile may still be in the
/// caller's open transaction, and a held mail's key is made before its row. `new` is stored only
/// while every stored key was made under the same KEK, so a key swapped while the app runs cannot add
/// keys the right one never opens. A shredded key's row stays with no key ([`shred`],
/// [`shred_pending`]): the insert waits on a shred still in progress, then finds the row, so no key is
/// ever made again for a deleted profile or a settled item (None without `new`, an error with it).
fn side_lookup(url: &str, slot: Slot, new: Option<Vec<u8>>, fingerprint: [u8; 8]) -> Result<Option<Stored>, KeyError> {
    let url = url.to_string();
    let (table, column) = slot.table();
    let id = slot.id();
    let lookup = std::thread::Builder::new()
        .name("ownhealth-key-lookup".into())
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|e| KeyError(format!("could not start the key lookup: {e}")))?;
            rt.block_on(async move {
                let mut conn = PgConnection::connect(&url).await?;
                if let Some(wrapped) = &new {
                    let sql = format!(
                        "insert into {table} ({column}, wrapped_dek, kek_fingerprint) select $2, $3, $1 \
                         where not {OTHER_KEK} on conflict ({column}) do nothing"
                    );
                    sqlx::query(sqlx::AssertSqlSafe(sql))
                        .bind(&fingerprint[..])
                        .bind(id)
                        .bind(wrapped)
                        .execute(&mut conn)
                        .await?;
                }
                // waits for a shred in progress, then reads the row it left
                let sql = format!("select wrapped_dek, kek_fingerprint from {table} where {column} = $1 for share");
                let row: Option<StoredRow> = sqlx::query_as(sqlx::AssertSqlSafe(sql))
                    .bind(id)
                    .fetch_optional(&mut conn)
                    .await?;
                if matches!(row, Some((None, _) | (_, None))) {
                    let _ = conn.close().await;
                    return match new {
                        Some(_) => Err(KeyError::new(shredded(slot))),
                        None => Ok(None),
                    };
                }
                let row = row.and_then(|(w, f)| Some((w?, f?)));
                if row.is_none() && new.is_some() {
                    let other: bool = sqlx::query_scalar(sqlx::AssertSqlSafe(format!("select {OTHER_KEK}")))
                        .bind(&fingerprint[..])
                        .fetch_one(&mut conn)
                        .await?;
                    let _ = conn.close().await;
                    return Err(if other {
                        KeyError::new(WRONG_KEY)
                    } else {
                        KeyError::new(shredded(slot))
                    });
                }
                let _ = conn.close().await;
                Ok(row)
            })
        })
        .map_err(|e| KeyError(format!("could not start the key lookup: {e}")))?;
    lookup.join().map_err(|_| KeyError::new("the key lookup stopped"))?
}

/// Why no key could be stored for a slot whose row is shredded.
fn shredded(slot: Slot) -> &'static str {
    match slot {
        Slot::Profile(_) => "no key could be stored for the profile: it was deleted",
        Slot::Pending(_) => "no key could be stored for the item: it was assigned, released or deleted",
    }
}

/// A key row as stored: wrapped DEK and fingerprint, both null on a shredded key.
type StoredRow = (Option<Vec<u8>>, Option<Vec<u8>>);

/// The refusal of a key that is not the one the stored profile keys were made with.
const WRONG_KEY: &str = "the profile keys were made with another OWNHEALTH_SECRETS_KEY: start with the key this \
     install (or this backup) was made with";

/// Destroy `profile`'s key in the caller's transaction (the profile delete): the row stays with no
/// key, so a key write that races the delete waits for it and then refuses ([`side_lookup`]). A
/// profile that never stored a file gets the same row. Returns 1 when a key was there.
pub async fn shred(conn: &mut PgConnection, profile: Uuid) -> Result<u64, KeyError> {
    let had: Option<bool> =
        sqlx::query_scalar("select wrapped_dek is not null from profile_key where profile_id = $1 for update")
            .bind(profile)
            .fetch_optional(&mut *conn)
            .await?;
    sqlx::query(
        "insert into profile_key (profile_id) values ($1) \
         on conflict (profile_id) do update set wrapped_dek = null, kek_fingerprint = null",
    )
    .bind(profile)
    .execute(&mut *conn)
    .await?;
    Ok(u64::from(had == Some(true)))
}

/// Destroy the keys of pending items `items` in the caller's transaction: a held mail released or
/// deleted, a document that got its profile. The rows stay with no key, as [`shred`] leaves a
/// profile's, so an item's file sealed later by a process that still holds the key in memory never
/// opens, and the sweep ([`crate::blobs::BlobStore::sweep_unkeyed`]) removes it. Returns how many
/// keys were there.
pub async fn shred_pending(conn: &mut PgConnection, items: &[Uuid]) -> Result<u64, KeyError> {
    if items.is_empty() {
        return Ok(0);
    }
    let had: i64 = sqlx::query_scalar(
        "select count(*) from (select 1 from pending_key where item_id = any($1) and wrapped_dek is not null \
         for update) x",
    )
    .bind(items)
    .fetch_one(&mut *conn)
    .await?;
    sqlx::query(
        "insert into pending_key (item_id) select unnest($1::uuid[]) \
         on conflict (item_id) do update set wrapped_dek = null, kek_fingerprint = null",
    )
    .bind(items)
    .execute(&mut *conn)
    .await?;
    Ok(u64::try_from(had).unwrap_or(0))
}

/// Destroy the keys whose profile is gone without a delete (a profile create that rolled back after
/// its first file), once a day old: a younger one may belong to a profile still in an open
/// transaction. Returns how many went.
pub async fn shred_orphans(conn: &mut PgConnection) -> Result<u64, KeyError> {
    Ok(sqlx::query(
        "update profile_key k set wrapped_dek = null, kek_fingerprint = null \
         where k.wrapped_dek is not null and k.created_at < now() - interval '1 day' \
         and not exists (select 1 from profile p where p.id = k.profile_id)",
    )
    .execute(conn)
    .await?
    .rows_affected())
}

/// Destroy the pending keys whose item no longer waits for a profile, once a day old: a mail whose
/// hold rolled back after its key was made, or an item settled without its key being shredded. An
/// item waits while it is a held mail or a document with no profile. Returns how many went.
pub async fn shred_pending_orphans(conn: &mut PgConnection) -> Result<u64, KeyError> {
    Ok(sqlx::query(
        "update pending_key k set wrapped_dek = null, kek_fingerprint = null \
         where k.wrapped_dek is not null and k.created_at < now() - interval '1 day' \
         and not exists (select 1 from quarantine_item q where q.id = k.item_id and q.status = 'held') \
         and not exists (select 1 from document d where d.id = k.item_id and d.profile_id is null)",
    )
    .execute(conn)
    .await?
    .rows_affected())
}

/// The slots of `slots` whose key is stored and not shredded.
pub async fn live(conn: &mut PgConnection, slots: &[Slot]) -> Result<std::collections::HashSet<Slot>, KeyError> {
    let mut out = std::collections::HashSet::new();
    for kind in [Slot::Profile(Uuid::nil()), Slot::Pending(Uuid::nil())] {
        let ids: Vec<Uuid> = slots
            .iter()
            .filter(|s| std::mem::discriminant(*s) == std::mem::discriminant(&kind))
            .map(|s| s.id())
            .collect();
        if ids.is_empty() {
            continue;
        }
        let (table, column) = kind.table();
        let sql = format!("select {column} from {table} where {column} = any($1) and wrapped_dek is not null");
        let found: Vec<Uuid> = sqlx::query_scalar(sqlx::AssertSqlSafe(sql))
            .bind(&ids)
            .fetch_all(&mut *conn)
            .await?;
        out.extend(found.into_iter().map(|id| match kind {
            Slot::Profile(_) => Slot::Profile(id),
            Slot::Pending(_) => Slot::Pending(id),
        }));
    }
    Ok(out)
}

/// Whether `pending_key` exists: a database restored from before it has none.
async fn has_pending(conn: &mut PgConnection) -> Result<bool, KeyError> {
    Ok(
        sqlx::query_scalar("select to_regclass('public.pending_key') is not null")
            .fetch_one(conn)
            .await?,
    )
}

/// Refuses a start whose OWNHEALTH_SECRETS_KEY does not open every data key the database holds,
/// profiles' and pending items', before anything serves or stores a key: files stored under keys the
/// right key never opens would be lost. A database with no key yet (a fresh install, or one from
/// before 1.0) passes.
pub async fn check_start(conn: &mut PgConnection, kek: &Kek) -> Result<(), KeyError> {
    let keyed: bool = sqlx::query_scalar("select to_regclass('public.profile_key') is not null")
        .fetch_one(&mut *conn)
        .await?;
    if !keyed {
        return Ok(());
    }
    let (opened, failed) = check_all(conn, kek).await?;
    if failed.is_empty() {
        return Ok(());
    }
    let sql = if has_pending(conn).await? {
        "select distinct f from (select kek_fingerprint f from profile_key union all \
         select kek_fingerprint from pending_key) x where f is not null order by 1"
    } else {
        "select distinct kek_fingerprint from profile_key where kek_fingerprint is not null order by 1"
    };
    let made: Vec<Vec<u8>> = sqlx::query_scalar(sql).fetch_all(&mut *conn).await?;
    let made: Vec<String> = made.iter().map(hex::encode).collect();
    Err(KeyError(format!(
        "OWNHEALTH_SECRETS_KEY (fingerprint {have}) does not open {bad} of the {all} profile keys this \
         install holds (made under fingerprint {made}): start with the key this install was made with, \
         from secrets/secrets_key, the OWNHEALTH_SECRETS_KEY line of an older .env, or the copy kept \
         with the backups; a new key never opens the files already stored, see deploy/README.md",
        have = hex::encode(kek.fingerprint()),
        bad = failed.len(),
        all = opened + failed.len(),
        made = made.join(", "),
    )))
}

/// Every data key the database holds, as (slot, `wrapped_dek`, `kek_fingerprint`); a shredded row
/// holds no key and is left out. A backup reads them in its snapshot, so the files it names are
/// the ones its own rows open, whatever a delete did to the live rows meanwhile.
pub async fn stored_all(conn: &mut PgConnection) -> Result<Vec<(Slot, Vec<u8>, Vec<u8>)>, KeyError> {
    let mut rows: Vec<(Slot, Vec<u8>, Vec<u8>)> = sqlx::query_as::<_, (Uuid, Vec<u8>, Vec<u8>)>(
        "select profile_id, wrapped_dek, kek_fingerprint from profile_key where wrapped_dek is not null \
         order by profile_id",
    )
    .fetch_all(&mut *conn)
    .await?
    .into_iter()
    .map(|(id, w, f)| (Slot::Profile(id), w, f))
    .collect();
    if has_pending(conn).await? {
        let pending: Vec<(Uuid, Vec<u8>, Vec<u8>)> = sqlx::query_as(
            "select item_id, wrapped_dek, kek_fingerprint from pending_key where wrapped_dek is not null \
             order by item_id",
        )
        .fetch_all(&mut *conn)
        .await?;
        rows.extend(pending.into_iter().map(|(id, w, f)| (Slot::Pending(id), w, f)));
    }
    Ok(rows)
}

/// Every stored data key opened with `kek`: (keys that open, the profiles and items whose key does
/// not). For verify-restore and restore, which must not trust a backup the configured key cannot
/// read. A shredded row holds no key and is not counted.
pub async fn check_all(conn: &mut PgConnection, kek: &Kek) -> Result<(usize, Vec<Uuid>), KeyError> {
    let rows = stored_all(conn).await?;
    let mut bad = Vec::new();
    for (slot, wrapped, fp) in &rows {
        if kek.unwrap_slot(*slot, fp, wrapped).is_err() {
            bad.push(slot.id());
        }
    }
    Ok((rows.len() - bad.len(), bad))
}

#[cfg(test)]
mod tests {
    use std::io::{Cursor, Write};

    use sha2::Digest;

    use super::*;

    /// A test KEK from bytes made at run time: no key-shaped literal sits in the repository.
    fn kek() -> Kek {
        let mut secret = [0u8; 32];
        rand::fill(&mut secret[..]);
        Kek::from_secret(&secret).expect("kek")
    }

    fn seal_all(dek: &Dek, owner: Uuid, data: &[u8]) -> Vec<u8> {
        let mut s = Sealer::new(dek, owner);
        let mut out = vec![0u8; HEADER];
        let chunks: Vec<&[u8]> = if data.is_empty() {
            vec![&[]]
        } else {
            data.chunks(RECORD).collect()
        };
        for (i, c) in chunks.iter().enumerate() {
            out.write_all(&s.seal(c, i + 1 == chunks.len()).expect("seal"))
                .expect("write");
        }
        let sha: [u8; 32] = Sha256::digest(data).into();
        out[..HEADER].copy_from_slice(&s.header(&sha));
        out
    }

    fn open_all(dek: &Dek, owner: Uuid, data: &[u8], sealed: Vec<u8>) -> io::Result<Vec<u8>> {
        let sha: [u8; 32] = Sha256::digest(data).into();
        let mut r = SealedReader::open(Cursor::new(sealed), dek, owner, &sha)?;
        let mut out = Vec::new();
        r.read_to_end(&mut out)?;
        Ok(out)
    }

    fn sample(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i % 251) as u8).collect()
    }

    #[test]
    fn records_round_trip_at_every_size_and_seek() -> io::Result<()> {
        let (dek, owner) = (Dek::generate(), Uuid::new_v4());
        for len in [0, 1, RECORD - 1, RECORD, RECORD + 1, 3 * RECORD, 3 * RECORD + 17] {
            let data = sample(len);
            let sealed = seal_all(&dek, owner, &data);
            assert_eq!(sealed.len(), HEADER + len + len.div_ceil(RECORD).max(1) * TAG, "{len}");
            assert_eq!(open_all(&dek, owner, &data, sealed.clone())?, data, "{len}");
            let sha: [u8; 32] = Sha256::digest(&data).into();
            let mut r = SealedReader::open(Cursor::new(sealed), &dek, owner, &sha)?;
            assert_eq!(r.len(), len as u64);
            if len > 10 {
                let at = len as u64 - 10;
                r.seek(SeekFrom::Start(at))?;
                let mut tail = Vec::new();
                r.read_to_end(&mut tail)?;
                assert_eq!(tail, &data[len - 10..], "{len}");
                r.seek(SeekFrom::End(-(len as i64)))?;
                let mut head = [0u8; 5];
                r.read_exact(&mut head)?;
                assert_eq!(head, data[..5], "{len}");
            }
        }
        Ok(())
    }

    #[test]
    fn a_cut_reordered_repeated_or_flipped_file_fails() {
        let (dek, owner) = (Dek::generate(), Uuid::new_v4());
        let data = sample(3 * RECORD + 100);
        let sealed = seal_all(&dek, owner, &data);
        let rec = |i: usize| HEADER + i * (RECORD + TAG)..HEADER + (i + 1) * (RECORD + TAG);
        let bad = |v: Vec<u8>, what: &str| {
            let err = open_all(&dek, owner, &data, v).expect_err(what);
            assert_eq!(err.kind(), io::ErrorKind::InvalidData, "{what}");
        };
        // cut at a record boundary: the new last record was not sealed as last, so it never reads short
        bad(sealed[..rec(3).start].to_vec(), "cut after 3 records");
        bad(sealed[..rec(1).start].to_vec(), "cut after 1 record");
        // cut inside a record
        bad(sealed[..sealed.len() - 5].to_vec(), "cut inside the last record");
        bad(sealed[..HEADER + 10].to_vec(), "cut inside the first record");
        bad(sealed[..HEADER].to_vec(), "header only");
        // two records swapped
        let mut swapped = sealed[..rec(0).start].to_vec();
        swapped.extend_from_slice(&sealed[rec(1)]);
        swapped.extend_from_slice(&sealed[rec(0)]);
        swapped.extend_from_slice(&sealed[rec(2).start..]);
        bad(swapped, "records reordered");
        // a record repeated in place of the next
        let mut repeated = sealed[..rec(1).end].to_vec();
        repeated.extend_from_slice(&sealed[rec(1)]);
        repeated.extend_from_slice(&sealed[rec(3).start..]);
        bad(repeated, "a record repeated");
        // a record dropped from the middle
        let mut dropped = sealed[..rec(1).start].to_vec();
        dropped.extend_from_slice(&sealed[rec(2).start..]);
        bad(dropped, "a record dropped");
        // an extra record appended after the last
        let mut appended = sealed.clone();
        appended.extend_from_slice(&sealed[rec(0)]);
        bad(appended, "a record appended");
        // one bit flipped anywhere: header, record body, tag
        for at in [0, 5, HEADER - 1, HEADER + 7, rec(1).end - 1, sealed.len() - 1] {
            let mut flipped = sealed.clone();
            flipped[at] ^= 1;
            bad(flipped, &format!("bit flipped at {at}"));
        }
    }

    #[test]
    fn a_file_opens_only_for_its_owner_its_bytes_and_its_key() {
        let (dek, owner) = (Dek::generate(), Uuid::new_v4());
        let data = b"Carmen Ortega, LabNorte ferritin".to_vec();
        let sealed = seal_all(&dek, owner, &data);
        assert!(open_all(&dek, owner, &data, sealed.clone()).is_ok());
        assert!(
            open_all(&dek, Uuid::new_v4(), &data, sealed.clone()).is_err(),
            "another owner"
        );
        assert!(
            open_all(&Dek::generate(), owner, &data, sealed.clone()).is_err(),
            "another key"
        );
        let sha_other: [u8; 32] = Sha256::digest(b"other").into();
        assert!(
            SealedReader::open(Cursor::new(sealed.clone()), &dek, owner, &sha_other).is_err(),
            "copied under another file's name"
        );
        assert!(
            !sealed.windows(13).any(|w| w == b"Carmen Ortega"),
            "plaintext in the sealed bytes"
        );
    }

    #[test]
    fn nonces_never_repeat_under_one_key() {
        let dek = Dek::generate();
        let owner = Uuid::new_v4();
        // two files of the same bytes get different salts, so different record keys and bytes
        let a = seal_all(&dek, owner, b"same bytes");
        let b = seal_all(&dek, owner, b"same bytes");
        assert_ne!(a[MAGIC.len()..MAGIC.len() + SALT], b[MAGIC.len()..MAGIC.len() + SALT]);
        assert_ne!(a[HEADER..], b[HEADER..]);
        // within a file, every record has its own counter, and the last record its own flag
        let mut s = Sealer::new(&dek, owner);
        let one = s.seal(&[0u8; RECORD], false).expect("seal");
        let two = s.seal(&[0u8; RECORD], false).expect("seal");
        let three = s.seal(&[0u8; RECORD], true).expect("seal");
        assert!(one != two && two != three && one != three);
        assert!(s.seal(b"after the last", true).is_err(), "a record after the last");
    }

    #[test]
    fn only_the_last_record_may_be_short_and_only_an_empty_file_has_an_empty_one() {
        let (dek, owner) = (Dek::generate(), Uuid::new_v4());
        let mut s = Sealer::new(&dek, owner);
        assert!(
            s.seal(&[0u8; 16], false).is_err(),
            "a short record that is not the last"
        );
        assert!(s.seal(&[], false).is_err(), "an empty record that is not the last");
        s.seal(&[0u8; RECORD], false).expect("a full record");
        assert!(s.seal(&[], true).is_err(), "an empty last record after a full one");
        s.seal(&[1u8; 9], true).expect("a short last record");
        // an empty file is one empty last record
        Sealer::new(&dek, owner).seal(&[], true).expect("an empty file");
    }

    #[test]
    fn a_file_cut_to_a_record_boundary_plus_one_tag_fails_when_it_opens() {
        let (dek, owner) = (Dek::generate(), Uuid::new_v4());
        let data = sample(3 * RECORD + 100);
        let sha: [u8; 32] = Sha256::digest(&data).into();
        let sealed = seal_all(&dek, owner, &data);
        let boundary = |i: usize| HEADER + i * (RECORD + TAG);
        // after 0, 1 or 2 whole records, the next 16 bytes as they were, or anything else
        for records in 0..3 {
            for junk in [false, true] {
                let mut cut = sealed[..boundary(records) + TAG].to_vec();
                if junk {
                    cut[boundary(records)..].fill(0xa5);
                }
                let what = format!("{records} records + 16 bytes, junk {junk}");
                let err = SealedReader::open(Cursor::new(cut.clone()), &dek, owner, &sha).expect_err(&what);
                assert_eq!(err.kind(), io::ErrorKind::InvalidData, "{what}");
                assert!(open_all(&dek, owner, &data, cut).is_err(), "{what}");
            }
        }
        // a valid empty blob reads as nothing; with its tag changed it does not open
        let empty = seal_all(&dek, owner, b"");
        assert_eq!(empty.len(), HEADER + TAG);
        assert_eq!(open_all(&dek, owner, b"", empty.clone()).expect("empty"), b"");
        let mut bad = empty;
        let last = bad.len() - 1;
        bad[last] ^= 1;
        let empty_sha: [u8; 32] = Sha256::digest(b"").into();
        assert!(
            SealedReader::open(Cursor::new(bad), &dek, owner, &empty_sha).is_err(),
            "a changed tag on an empty blob"
        );
    }

    #[test]
    fn a_dek_wraps_under_the_kek_for_its_profile_only() -> Result<(), KeyError> {
        let (k, profile) = (kek(), Uuid::new_v4());
        let dek = Dek::generate();
        let wrapped = k.wrap(profile, &dek)?;
        assert_eq!(wrapped.len(), WRAPPED_LEN);
        let back = k.unwrap(profile, &k.fingerprint(), &wrapped)?;
        let sha = [7u8; 32];
        assert_eq!(back.address(&sha), dek.address(&sha));
        assert!(
            k.unwrap(Uuid::new_v4(), &k.fingerprint(), &wrapped).is_err(),
            "another profile"
        );
        let other = kek();
        let err = other
            .unwrap(profile, &k.fingerprint(), &wrapped)
            .expect_err("another kek");
        assert!(err.to_string().contains("another OWNHEALTH_SECRETS_KEY"), "{err}");
        assert!(
            other.unwrap(profile, &other.fingerprint(), &wrapped).is_err(),
            "another kek with a copied fingerprint"
        );
        let mut flipped = wrapped.clone();
        flipped[20] ^= 1;
        assert!(k.unwrap(profile, &k.fingerprint(), &flipped).is_err(), "a changed byte");
        assert_ne!(k.wrap(profile, &dek)?, wrapped, "nonce reused");
        Ok(())
    }

    #[test]
    fn the_kek_comes_from_the_secrets_key_and_nothing_prints_a_key() {
        let mut secret = [0u8; 32];
        rand::fill(&mut secret[..]);
        let b64 = base64::engine::general_purpose::STANDARD.encode(secret);
        let a = Kek::from_base64(&b64).expect("kek");
        let b = Kek::from_secret(&secret).expect("kek");
        assert_eq!(a.fingerprint(), b.fingerprint());
        assert_ne!(a.fingerprint(), kek().fingerprint());
        // the KEK is derived, never the secrets key itself
        assert_ne!(a.key[..], secret[..]);
        assert!(
            Kek::from_base64("c2hvcnQ=")
                .expect_err("short")
                .to_string()
                .contains("32 bytes")
        );
        let missing = Kek::from_settings(&Settings::default()).expect_err("missing");
        assert_eq!(missing.to_string(), MISSING_KEY);
        assert!(!MISSING_KEY.contains('\n'), "the fix is one line");
        let dek = Dek::generate();
        let shown = format!(
            "{a:?} {dek:?} {:?} {:?}",
            Sealer::new(&dek, Uuid::nil()),
            Keyring::in_memory(a.clone())
        );
        assert!(!shown.contains(&hex::encode(&a.key[..])) && !shown.contains(&hex::encode(&dek.key[..])));
        assert!(!shown.contains(&b64) && !shown.contains(&hex::encode(secret)));
        assert_eq!(shown.matches("***").count(), 2, "{shown}");
    }

    #[test]
    fn a_key_read_while_forget_ran_is_never_kept() -> Result<(), KeyError> {
        let ring = Keyring::in_memory(kek());
        let p = Uuid::new_v4();
        // a lookup began, then the profile was deleted and forgotten, then the lookup ended
        let (none, seen) = ring.cached(Slot::Profile(p))?;
        assert!(none.is_none());
        ring.forget(p);
        let late = ring.publish(Slot::Profile(p), Arc::new(Dek::generate()), seen)?;
        assert!(
            ring.cached(Slot::Profile(p))?.0.is_none(),
            "a deleted profile's key came back into the cache"
        );
        // a lookup with no forget in between keeps its key, and the first one stays
        let (_, seen) = ring.cached(Slot::Profile(p))?;
        let kept = ring.publish(Slot::Profile(p), late.clone(), seen)?;
        assert!(Arc::ptr_eq(&kept, &late));
        let again = ring.publish(Slot::Profile(p), Arc::new(Dek::generate()), seen)?;
        assert!(Arc::ptr_eq(&again, &late), "a second lookup replaced the kept key");
        assert!(ring.cached(Slot::Profile(p))?.0.is_some_and(|c| Arc::ptr_eq(&c, &late)));
        Ok(())
    }

    #[test]
    fn the_memory_keyring_creates_a_key_only_when_asked() -> Result<(), KeyError> {
        let ring = Keyring::in_memory(kek());
        let p = Uuid::new_v4();
        assert!(ring.dek(p, false)?.is_none());
        let made = ring.dek(p, true)?.expect("made");
        let again = ring.dek(p, false)?.expect("kept");
        assert!(Arc::ptr_eq(&made, &again));
        Ok(())
    }

    #[test]
    fn a_pending_items_key_never_opens_as_a_profiles_key() -> Result<(), KeyError> {
        let (k, id) = (kek(), Uuid::new_v4());
        let dek = Dek::generate();
        let wrapped = k.wrap_slot(Slot::Pending(id), &dek)?;
        let back = k.unwrap_slot(Slot::Pending(id), &k.fingerprint(), &wrapped)?;
        assert_eq!(back.address(&[3u8; 32]), dek.address(&[3u8; 32]));
        assert!(
            k.unwrap_slot(Slot::Profile(id), &k.fingerprint(), &wrapped).is_err(),
            "opened as a profile key"
        );
        assert!(
            k.unwrap(id, &k.fingerprint(), &wrapped).is_err(),
            "opened as a profile key"
        );
        let profile = k.wrap(id, &dek)?;
        assert!(
            k.unwrap_slot(Slot::Pending(id), &k.fingerprint(), &profile).is_err(),
            "opened as an item key"
        );
        Ok(())
    }

    #[test]
    fn a_value_sealed_under_a_dek_opens_only_with_it_and_its_associated_data() -> Result<(), KeyError> {
        let dek = Dek::generate();
        let sealed = dek.seal_value(b"fake-refresh-LabNorte", b"row-1")?;
        assert_eq!(&dek.open_value(&sealed, b"row-1")?[..], b"fake-refresh-LabNorte");
        assert!(dek.open_value(&sealed, b"row-2").is_err(), "other associated data");
        assert!(Dek::generate().open_value(&sealed, b"row-1").is_err(), "another key");
        let mut flipped = sealed.clone();
        flipped[WRAP_NONCE + 2] ^= 1;
        assert!(dek.open_value(&flipped, b"row-1").is_err(), "a changed byte");
        assert!(dek.open_value(&sealed[..10], b"row-1").is_err(), "cut");
        assert_ne!(dek.seal_value(b"x", b"a")?, dek.seal_value(b"x", b"a")?, "nonce reused");
        assert!(
            !sealed.windows(12).any(|w| w == b"fake-refresh"),
            "plaintext in the sealed bytes"
        );
        Ok(())
    }

    #[test]
    fn forgetting_one_slot_keeps_the_others() -> Result<(), KeyError> {
        let ring = Keyring::in_memory(kek());
        let id = Uuid::new_v4();
        let profile = ring.dek(id, true)?.expect("made");
        let item = ring.dek_for(Slot::Pending(id), true)?.expect("made");
        assert!(
            !Arc::ptr_eq(&profile, &item),
            "one key for a profile and an item of the same id"
        );
        ring.forget_slot(Slot::Pending(id));
        assert!(ring.dek_for(Slot::Pending(id), false)?.is_none());
        assert!(ring.dek(id, false)?.is_some_and(|d| Arc::ptr_eq(&d, &profile)));
        Ok(())
    }
}
