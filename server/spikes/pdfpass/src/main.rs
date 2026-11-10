//! cargo run -- <in.pdf> <out dir>
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use lopdf::encryption::crypt_filters::{Aes256CryptFilter, CryptFilter};
use lopdf::{Document, EncryptionState, EncryptionVersion, Permissions};
use pdfium_render::prelude::*;
use rand::Rng;

const PASSWORD: &str = "12345678Z"; // a synthetic DNI-shaped password

fn lock(input: &Path, output: &Path) -> Result<()> {
    let mut doc = Document::load(input)?;
    let mut key = [0u8; 32];
    rand::rng().fill(&mut key);
    let filter: Arc<dyn CryptFilter> = Arc::new(Aes256CryptFilter);
    let state = EncryptionState::try_from(EncryptionVersion::V5 {
        encrypt_metadata: true,
        crypt_filters: BTreeMap::from([(b"StdCF".to_vec(), filter)]),
        file_encryption_key: &key,
        stream_filter: b"StdCF".to_vec(),
        string_filter: b"StdCF".to_vec(),
        owner_password: "owner-not-used",
        user_password: PASSWORD,
        permissions: Permissions::all(),
    })?;
    doc.encrypt(&state)?;
    doc.save(output)?;
    Ok(())
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let input = PathBuf::from(args.first().context("in.pdf")?);
    let out = PathBuf::from(args.get(1).context("out dir")?);
    std::fs::create_dir_all(&out)?;
    let locked = out.join("locked-aes256.pdf");
    lock(&input, &locked)?;
    let bytes = std::fs::read(&locked)?;
    println!("locked {} bytes, /Encrypt present: {}", bytes.len(), bytes.windows(8).any(|w| w == b"/Encrypt"));

    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../vendor/pdfium/mac-arm64/lib");
    let pdfium = Pdfium::new(Pdfium::bind_to_library(Pdfium::pdfium_platform_library_name_at_path(&dir))?);

    match pdfium.load_pdf_from_byte_slice(&bytes, None) {
        Ok(_) => println!("no password: OPENED (unexpected)"),
        Err(e) => println!("no password: {e:?}"),
    }
    match pdfium.load_pdf_from_byte_slice(&bytes, Some("wrong")) {
        Ok(_) => println!("wrong password: OPENED (unexpected)"),
        Err(e) => println!("wrong password: {e:?}"),
    }
    let doc = pdfium.load_pdf_from_byte_slice(&bytes, Some(PASSWORD))?;
    let text = doc.pages().get(0)?.text()?.all();
    println!("right password: pages={} first line={:?}", doc.pages().len(), text.lines().next());
    let unlocked = doc.save_to_bytes()?;
    std::fs::write(out.join("unlocked.pdf"), &unlocked)?;
    println!("save_to_bytes: {} bytes, /Encrypt present: {}", unlocked.len(), unlocked.windows(8).any(|w| w == b"/Encrypt"));
    match pdfium.load_pdf_from_byte_slice(&unlocked, None) {
        Ok(d) => println!("unlocked copy opens without password: pages={} same text={}", d.pages().len(), d.pages().get(0)?.text()?.all() == text),
        Err(e) => println!("unlocked copy needs a password still: {e:?}"),
    }

    // lopdf: decrypt in pure Rust, save without /Encrypt, then check pdfium reads it with no password.
    let wrong = Document::load_mem_with_options(&bytes, lopdf::LoadOptions::with_password("wrong")).is_err();
    let mut ld = Document::load_mem_with_options(&bytes, lopdf::LoadOptions::with_password(PASSWORD))?;
    println!("lopdf loaded with password: encrypted flag={} encryption_state={}", ld.is_encrypted(), ld.encryption_state.is_some());
    ld.trailer.remove(b"Encrypt");
    ld.encryption_state = None;
    let mut clear = Vec::new();
    ld.save_to(&mut clear)?;
    std::fs::write(out.join("lopdf-unlocked.pdf"), &clear)?;
    println!("lopdf: wrong password refused={wrong}; decrypted {} bytes, /Encrypt present: {}", clear.len(), clear.windows(8).any(|w| w == b"/Encrypt"));
    let d = pdfium.load_pdf_from_byte_slice(&clear, None)?;
    println!("lopdf copy opens in pdfium without password: same text={}", d.pages().get(0)?.text()?.all() == text);
    Ok(())
}
