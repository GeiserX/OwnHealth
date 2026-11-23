//! Inbox credentials at rest (security-privacy.md 7.1, ingestion.md 3.2): an IMAP password, a
//! Paperless or Nightscout token, a per-sender PDF password. AES-256-GCM with the key in
//! `OWNHEALTH_SECRETS_KEY` (or `OWNHEALTH_SECRETS_KEY_FILE`): 32 random bytes, standard base64
//! (`openssl rand -base64 32`).
//!
//! The `secret` row id and purpose are the associated data, so a ciphertext copied into another row
//! does not open. A value read back is a [`SecretString`], whose `Debug` and `Display` print `***`.
//! Nothing here logs, and no error message carries a value.
//!
//! A connection's refresh token belongs to one profile (slice 46): it is sealed under that profile's
//! data key instead ([`store_profile_secret`], the purposes in [`PROFILE_SEALED`]), so deleting the
//! profile's key leaves the token unreadable too. Its ciphertext is version 2 and names the profile:
//! `2 | profile id (16) | nonce (12) | ciphertext and tag`, with the row id, purpose and profile id as
//! associated data. [`read_secret`] reads both versions, so a token sealed before stays readable; the
//! next write of it seals it under the profile.

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Nonce};
use base64::Engine as _;
use sha2::{Digest, Sha256};
use sqlx::PgConnection;
use uuid::Uuid;

use std::sync::Arc;

use crate::config::Settings;
use crate::error::{Error, Result};
use crate::keys::{Dek, Keyring};

/// The `secret.purpose` check constraint.
pub const PURPOSES: [&str; 7] = [
    "imap_password",
    "paperless_token",
    "nightscout_token",
    "pdf_password",
    "connector_refresh_token",
    "connector_client_secret",
    "openwearables_api_key",
];

/// The purposes sealed under the profile's own data key: a value that belongs to one profile's
/// connection. A household's vendor client secret (`connector_client_secret`) serves every profile's
/// connection to that vendor, so it stays under the secrets key.
pub const PROFILE_SEALED: [&str; 1] = ["connector_refresh_token"];

const VERSION: u8 = 1;
/// A value sealed under a profile's data key.
const PROFILE_VERSION: u8 = 2;
const NONCE_LEN: usize = 12;

/// A secret in memory. Never printed; `expose()` is the one way to read it.
#[derive(Clone, PartialEq, Eq)]
pub struct SecretString(String);

impl SecretString {
    pub fn new(value: impl Into<String>) -> Self {
        SecretString(value.into())
    }

    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for SecretString {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SecretString(***)")
    }
}

impl std::fmt::Display for SecretString {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("***")
    }
}

/// The sealing key, and the keyring a profile's secrets are sealed with. Cheap to clone; `Debug`
/// shows only the key fingerprint.
#[derive(Clone)]
pub struct SecretBox {
    cipher: Aes256Gcm,
    fingerprint: [u8; 8],
    keys: Option<Arc<Keyring>>,
}

impl std::fmt::Debug for SecretBox {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "SecretBox({})", hex::encode(self.fingerprint))
    }
}

fn not_configured() -> Error {
    Error::Invalid(
        "OWNHEALTH_SECRETS_KEY is not set: generate one with `openssl rand -base64 32` and set it (or \
         OWNHEALTH_SECRETS_KEY_FILE) before saving a password or token"
            .into(),
    )
}

impl SecretBox {
    /// A key from 32 raw bytes.
    pub fn new(key: &[u8]) -> Result<SecretBox> {
        if key.len() != 32 {
            return Err(Error::Invalid(format!(
                "OWNHEALTH_SECRETS_KEY must be 32 bytes, got {}",
                key.len()
            )));
        }
        let cipher = Aes256Gcm::new_from_slice(key).map_err(|_| Error::Invalid("bad secrets key".into()))?;
        let digest = Sha256::digest(key);
        let mut fingerprint = [0u8; 8];
        fingerprint.copy_from_slice(&digest[..8]);
        Ok(SecretBox {
            cipher,
            fingerprint,
            keys: None,
        })
    }

    /// This box with the keyring that seals a profile's secrets ([`store_profile_secret`]): the blob
    /// store's (`BlobStore::keyring`), so one process keeps one copy of each profile key.
    pub fn with_keys(mut self, keys: Option<Arc<Keyring>>) -> SecretBox {
        self.keys = keys;
        self
    }

    /// The key of `OWNHEALTH_SECRETS_KEY`: standard base64 of 32 bytes.
    pub fn from_base64(value: &str) -> Result<SecretBox> {
        let key = base64::engine::general_purpose::STANDARD
            .decode(value.trim())
            .map_err(|_| Error::Invalid("OWNHEALTH_SECRETS_KEY is not standard base64".into()))?;
        SecretBox::new(&key)
    }

    /// The configured key; `Error::Invalid` with the fix when it is not set.
    pub fn from_settings(settings: &Settings) -> Result<SecretBox> {
        SecretBox::from_base64(settings.secrets_key.as_deref().ok_or_else(not_configured)?)
    }

    /// Whether this box carries the keyring a profile's secrets are sealed with.
    pub fn has_keys(&self) -> bool {
        self.keys.is_some()
    }

    pub fn fingerprint(&self) -> &[u8; 8] {
        &self.fingerprint
    }

    /// `version || nonce || ciphertext+tag`, bound to `aad`.
    pub fn seal(&self, plaintext: &[u8], aad: &[u8]) -> Result<Vec<u8>> {
        let mut nonce = [0u8; NONCE_LEN];
        rand::fill(&mut nonce[..]);
        let ct = self
            .cipher
            .encrypt(&Nonce::from(nonce), Payload { msg: plaintext, aad })
            .map_err(|_| Error::Invalid("could not seal the secret".into()))?;
        let mut out = Vec::with_capacity(1 + NONCE_LEN + ct.len());
        out.push(VERSION);
        out.extend_from_slice(&nonce);
        out.extend_from_slice(&ct);
        Ok(out)
    }

    /// Open a version 1 secret from its stored identity, purpose, and ciphertext.
    pub fn open_legacy_secret(&self, id: Uuid, purpose: &str, ciphertext: &[u8]) -> Result<SecretString> {
        secret_text(self.open(ciphertext, &aad(id, purpose))?)
    }

    /// Open what `seal` made with the same key and `aad`. A wrong key, other associated data or a
    /// changed byte is the same error, without detail.
    pub fn open(&self, sealed: &[u8], aad: &[u8]) -> Result<Vec<u8>> {
        let refused = || Error::Invalid("the stored secret does not open with this key".into());
        if sealed.len() < 1 + NONCE_LEN + 16 || sealed[0] != VERSION {
            return Err(refused());
        }
        let mut nonce = [0u8; NONCE_LEN];
        nonce.copy_from_slice(&sealed[1..1 + NONCE_LEN]);
        self.cipher
            .decrypt(
                &Nonce::from(nonce),
                Payload {
                    msg: &sealed[1 + NONCE_LEN..],
                    aad,
                },
            )
            .map_err(|_| refused())
    }
}

fn aad(id: Uuid, purpose: &str) -> Vec<u8> {
    let mut a = id.as_bytes().to_vec();
    a.extend_from_slice(purpose.as_bytes());
    a
}

fn profile_aad(id: Uuid, purpose: &str, profile: Uuid) -> Vec<u8> {
    let mut a = aad(id, purpose);
    a.extend_from_slice(profile.as_bytes());
    a
}

/// The data key of `profile` from the box's keyring, off the async runtime (a first lookup reads the
/// database). With `create`, a profile with none gets one.
async fn profile_dek(sb: &SecretBox, profile: Uuid, create: bool) -> Result<Arc<Dek>> {
    let keys = sb
        .keys
        .clone()
        .ok_or_else(|| Error::Invalid("a profile's secret needs the profile keys, which are not loaded".into()))?;
    tokio::task::spawn_blocking(move || keys.dek(profile, create))
        .await
        .map_err(|e| Error::Other(anyhow::anyhow!("key lookup task failed: {e}")))??
        .ok_or_else(|| Error::Invalid("the profile's key is gone, so its stored secret no longer opens".into()))
}

/// Open `profile`'s data key (a profile with none gets one): a caller checks that it can seal under
/// the key before it does work whose result must be sealed there.
pub async fn open_profile_key(sb: &SecretBox, profile: Uuid) -> Result<()> {
    profile_dek(sb, profile, true).await.map(|_| ())
}

/// `value` sealed for row `id` under `profile`'s data key (version 2).
async fn seal_for_profile(
    sb: &SecretBox,
    id: Uuid,
    purpose: &str,
    profile: Uuid,
    value: &SecretString,
) -> Result<Vec<u8>> {
    if !PROFILE_SEALED.contains(&purpose) {
        return Err(Error::Invalid(format!(
            "a {purpose} secret is not sealed under a profile's key"
        )));
    }
    let dek = profile_dek(sb, profile, true).await?;
    let sealed = dek.seal_value(value.expose().as_bytes(), &profile_aad(id, purpose, profile))?;
    let mut out = Vec::with_capacity(1 + 16 + sealed.len());
    out.push(PROFILE_VERSION);
    out.extend_from_slice(profile.as_bytes());
    out.extend_from_slice(&sealed);
    Ok(out)
}

fn check_purpose(purpose: &str) -> Result<()> {
    if PURPOSES.contains(&purpose) {
        Ok(())
    } else {
        Err(Error::Invalid(format!("unknown secret purpose {purpose:?}")))
    }
}

/// Seal and store a new secret; returns its id.
pub async fn store_secret(
    conn: &mut PgConnection,
    sb: &SecretBox,
    household_id: Uuid,
    purpose: &str,
    label: Option<&str>,
    value: &SecretString,
) -> Result<Uuid> {
    check_purpose(purpose)?;
    let id = Uuid::new_v4();
    let sealed = sb.seal(value.expose().as_bytes(), &aad(id, purpose))?;
    sqlx::query(
        "insert into secret (id, household_id, purpose, label, key_fingerprint, ciphertext) values ($1, $2, $3, $4, $5, $6)",
    )
    .bind(id)
    .bind(household_id)
    .bind(purpose)
    .bind(label)
    .bind(&sb.fingerprint[..])
    .bind(&sealed)
    .execute(conn)
    .await?;
    Ok(id)
}

/// Seal and store a new secret of `profile` under the profile's data key; returns its id. For the
/// purposes in [`PROFILE_SEALED`]; the box must carry the keyring ([`SecretBox::with_keys`]).
pub async fn store_profile_secret(
    conn: &mut PgConnection,
    sb: &SecretBox,
    household_id: Uuid,
    profile: Uuid,
    purpose: &str,
    value: &SecretString,
) -> Result<Uuid> {
    check_purpose(purpose)?;
    let id = Uuid::new_v4();
    let sealed = seal_for_profile(sb, id, purpose, profile, value).await?;
    sqlx::query(
        "insert into secret (id, household_id, purpose, key_fingerprint, ciphertext) values ($1, $2, $3, $4, $5)",
    )
    .bind(id)
    .bind(household_id)
    .bind(purpose)
    .bind(&sb.fingerprint[..])
    .bind(&sealed)
    .execute(conn)
    .await?;
    Ok(id)
}

/// Replace the value of a secret of `profile`, sealed under the profile's data key: a rotated refresh
/// token. A value sealed before under the secrets key is sealed under the profile from now on.
pub async fn replace_profile_secret(
    conn: &mut PgConnection,
    sb: &SecretBox,
    id: Uuid,
    profile: Uuid,
    value: &SecretString,
) -> Result<()> {
    let purpose: Option<String> = sqlx::query_scalar("select purpose from secret where id = $1")
        .bind(id)
        .fetch_optional(&mut *conn)
        .await?;
    let purpose = purpose.ok_or_else(|| Error::NotFound("Unknown secret.".into()))?;
    let sealed = seal_for_profile(sb, id, &purpose, profile, value).await?;
    sqlx::query("update secret set ciphertext = $2, key_fingerprint = $3, updated_at = now() where id = $1")
        .bind(id)
        .bind(&sealed)
        .bind(&sb.fingerprint[..])
        .execute(conn)
        .await?;
    Ok(())
}

/// True when secret `id` is sealed under `profile`'s data key (version 2 naming that profile).
pub async fn sealed_for_profile(conn: &mut PgConnection, id: Uuid, profile: Uuid) -> Result<bool> {
    let sealed: Option<Vec<u8>> = sqlx::query_scalar("select ciphertext from secret where id = $1")
        .bind(id)
        .fetch_optional(conn)
        .await?;
    Ok(sealed.is_some_and(|s| s.first() == Some(&PROFILE_VERSION) && s.get(1..17) == Some(profile.as_bytes())))
}

/// Replace the value of an existing secret (a changed password), keeping its id and purpose.
pub async fn replace_secret(conn: &mut PgConnection, sb: &SecretBox, id: Uuid, value: &SecretString) -> Result<()> {
    let purpose: Option<String> = sqlx::query_scalar("select purpose from secret where id = $1")
        .bind(id)
        .fetch_optional(&mut *conn)
        .await?;
    let purpose = purpose.ok_or_else(|| Error::NotFound("Unknown secret.".into()))?;
    let sealed = sb.seal(value.expose().as_bytes(), &aad(id, &purpose))?;
    sqlx::query("update secret set ciphertext = $2, key_fingerprint = $3, updated_at = now() where id = $1")
        .bind(id)
        .bind(&sealed)
        .bind(&sb.fingerprint[..])
        .execute(conn)
        .await?;
    Ok(())
}

/// Read a secret back. `household_id` must own it. A key other than the one that sealed it is
/// `Error::Invalid` naming the problem (rotated key), never the value. A secret sealed under a
/// profile's data key (version 2) opens with that profile's key from the box's keyring.
pub async fn read_secret(
    conn: &mut PgConnection,
    sb: &SecretBox,
    household_id: Uuid,
    id: Uuid,
) -> Result<SecretString> {
    let row: Option<(String, Vec<u8>, Vec<u8>)> =
        sqlx::query_as("select purpose, key_fingerprint, ciphertext from secret where id = $1 and household_id = $2")
            .bind(id)
            .bind(household_id)
            .fetch_optional(conn)
            .await?;
    let (purpose, fp, sealed) = row.ok_or_else(|| Error::NotFound("Unknown secret.".into()))?;
    if fp != sb.fingerprint {
        return Err(Error::Invalid(
            "this secret was sealed with another OWNHEALTH_SECRETS_KEY; enter it again".into(),
        ));
    }
    let plain = if sealed.first() == Some(&PROFILE_VERSION) {
        let refused = || Error::Invalid("the stored secret does not open with this key".into());
        let profile = sealed
            .get(1..17)
            .and_then(|b| Uuid::from_slice(b).ok())
            .ok_or_else(refused)?;
        let dek = profile_dek(sb, profile, false).await?;
        dek.open_value(&sealed[17..], &profile_aad(id, &purpose, profile))
            .map_err(|_| refused())?
            .to_vec()
    } else {
        return sb.open_legacy_secret(id, &purpose, &sealed);
    };
    secret_text(plain)
}

fn secret_text(plain: Vec<u8>) -> Result<SecretString> {
    String::from_utf8(plain)
        .map(SecretString)
        .map_err(|_| Error::Invalid("the stored secret is not text".into()))
}

pub async fn delete_secret(conn: &mut PgConnection, household_id: Uuid, id: Uuid) -> Result<bool> {
    let n = sqlx::query("delete from secret where id = $1 and household_id = $2")
        .bind(id)
        .bind(household_id)
        .execute(conn)
        .await?
        .rows_affected();
    Ok(n == 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sb() -> SecretBox {
        SecretBox::new(&[7u8; 32]).expect("key")
    }

    #[test]
    fn seal_and_open_round_trip_and_bind_the_associated_data() {
        let b = sb();
        let sealed = b.seal(b"12345678Z", b"row-1").expect("seal");
        assert_eq!(b.open(&sealed, b"row-1").expect("open"), b"12345678Z");
        assert!(
            b.open(&sealed, b"row-2").is_err(),
            "another row's associated data opens it"
        );
        let other = SecretBox::new(&[8u8; 32]).expect("key");
        assert!(other.open(&sealed, b"row-1").is_err(), "another key opens it");
        let mut flipped = sealed.clone();
        let last = flipped.len() - 1;
        flipped[last] ^= 1;
        assert!(b.open(&flipped, b"row-1").is_err(), "a changed byte opens");
        assert_ne!(
            b.seal(b"x", b"a").expect("seal"),
            b.seal(b"x", b"a").expect("seal"),
            "nonce reused"
        );
        assert!(
            !sealed.windows(9).any(|w| w == b"12345678Z"),
            "plaintext in the sealed bytes"
        );
    }

    #[test]
    fn the_key_comes_from_base64_of_32_bytes_and_errors_name_no_value() {
        assert!(SecretBox::from_base64("BwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwc=").is_ok());
        let short = SecretBox::from_base64("c2hvcnQ=").expect_err("refused");
        assert!(short.to_string().contains("32 bytes"));
        let missing = SecretBox::from_settings(&Settings::default()).expect_err("refused");
        assert!(missing.to_string().contains("OWNHEALTH_SECRETS_KEY"));
    }

    #[test]
    fn a_secret_never_prints() {
        let s = SecretString::new("12345678Z");
        assert_eq!(format!("{s} {s:?}"), "*** SecretString(***)");
        assert!(format!("{:?}", sb()).starts_with("SecretBox("));
    }

    #[tokio::test]
    async fn a_profiles_secret_is_sealed_under_its_key_bound_to_its_row_and_profile() -> Result<()> {
        let mut secret = [0u8; 32];
        rand::fill(&mut secret[..]); // made at run time, never a literal
        let ring = Arc::new(Keyring::in_memory(crate::keys::Kek::from_secret(&secret)?));
        let b = sb().with_keys(Some(ring.clone()));
        let (id, profile) = (Uuid::new_v4(), Uuid::new_v4());
        let value = SecretString::new("fake-refresh-LabNorte-0001");
        let sealed = seal_for_profile(&b, id, "connector_refresh_token", profile, &value).await?;
        assert_eq!(sealed[0], PROFILE_VERSION);
        assert_eq!(&sealed[1..17], profile.as_bytes());
        assert!(
            !sealed.windows(12).any(|w| w == b"fake-refresh"),
            "plaintext in the sealed bytes"
        );
        let dek = ring.dek(profile, false)?.expect("the profile's key");
        let bound = profile_aad(id, "connector_refresh_token", profile);
        assert_eq!(&dek.open_value(&sealed[17..], &bound)?[..], value.expose().as_bytes());
        // another row, purpose or profile does not open it
        for other in [
            profile_aad(Uuid::new_v4(), "connector_refresh_token", profile),
            profile_aad(id, "connector_client_secret", profile),
            profile_aad(id, "connector_refresh_token", Uuid::new_v4()),
        ] {
            assert!(dek.open_value(&sealed[17..], &other).is_err());
        }
        // the secrets key alone never opens it, and only the profile-sealed purposes seal this way
        assert!(b.open(&sealed, &aad(id, "connector_refresh_token")).is_err());
        assert!(
            seal_for_profile(&b, id, "imap_password", profile, &value)
                .await
                .is_err()
        );
        assert!(
            seal_for_profile(&sb(), id, "connector_refresh_token", profile, &value)
                .await
                .is_err(),
            "sealed with no keyring"
        );
        assert!(format!("{b:?}").starts_with("SecretBox(") && !format!("{b:?}").contains("Keyring"));
        Ok(())
    }
}
