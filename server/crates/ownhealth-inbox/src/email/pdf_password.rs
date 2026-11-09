//! Lane A: per-sender PDF passwords (ingestion.md 3.2, SPIKES-v0.2.md c). pdfium-render opens a
//! locked PDF but cannot save it unlocked (it saves with flags 0, so `/Encrypt` stays); lopdf opens it
//! with the password (`LoadOptions::with_password`), and saving without the `/Encrypt` trailer entry
//! gives an unlocked copy that PDFium reads with no password. CPU work: call from `spawn_blocking`.

use ownhealth_core::error::{Error, Result};
use ownhealth_core::secrets::SecretString;

/// What unlocking a PDF gave.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Unlock {
    /// Not encrypted: import the bytes as they are.
    NotEncrypted,
    /// The password at `index` opened it; `pdf` is the unlocked copy.
    Unlocked { pdf: Vec<u8>, index: usize },
    /// Encrypted and none of the sender's passwords opened it: review as `encrypted_unknown_password`.
    NoPasswordWorked,
}

/// True when the PDF has an `/Encrypt` dictionary.
pub fn is_encrypted(_pdf: &[u8]) -> bool {
    false
}

/// Try the sender's passwords in order. Never logs a password or says which one failed.
pub fn unlock(_pdf: &[u8], _passwords: &[SecretString]) -> Result<Unlock> {
    Err(Error::NotImplemented("email::pdf_password::unlock"))
}
