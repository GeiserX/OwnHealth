//! Where blobs live is `blobs.rs`'s business (plan, shared foundations 5): no production code
//! outside the store gets the blob folder from `Settings::blobs_path()` or builds a path inside the
//! store's own layout. The per-profile encryption (slice 46) and the S3 backend (slice 64) change
//! that layout, so a new raw use would read the wrong bytes. Only the store itself may; the backup
//! copies, lists and places files through it (`BlobStore::copy_into`, `tree_files`, `place`). A new
//! use fails here.

use std::path::{Path, PathBuf};

/// Files that may reach the blob folder directly, relative to `server/crates`.
const ALLOWED: &[&str] = &["ownhealth-core/src/blobs.rs"];

/// What a raw use looks like in source.
const RAW: &[&str] = &["blobs_path(", "join(\"sha256\")", "\"blobs/sha256"];

fn rs_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let entries = std::fs::read_dir(dir).unwrap_or_else(|e| panic!("cannot list {}: {e}", dir.display()));
    for e in entries {
        let p = e
            .unwrap_or_else(|e| panic!("cannot list {}: {e}", dir.display()))
            .path();
        if p.is_dir() {
            rs_files(&p, out);
        } else if p.extension().is_some_and(|x| x == "rs") {
            out.push(p);
        }
    }
}

/// Every raw use in the production code (`<crate>/src`, up to a file's `#[cfg(test)] mod`) of the
/// crates under `crates`, as `path:line: text`, skipping the files in `allowed`. Also how many files
/// were read.
fn raw_uses(crates: &Path, allowed: &[&str]) -> (Vec<String>, usize) {
    let mut files = Vec::new();
    for c in std::fs::read_dir(crates).unwrap_or_else(|e| panic!("cannot list {}: {e}", crates.display())) {
        let src = c.expect("crate dir").path().join("src");
        if src.is_dir() {
            rs_files(&src, &mut files);
        }
    }
    files.sort();
    let mut found = Vec::new();
    for f in &files {
        let rel = f
            .strip_prefix(crates)
            .expect("under crates")
            .to_string_lossy()
            .replace('\\', "/");
        if allowed.contains(&rel.as_str()) {
            continue;
        }
        let text = std::fs::read_to_string(f).unwrap_or_else(|e| panic!("cannot read {rel}: {e}"));
        let production = text.split("#[cfg(test)]\nmod ").next().unwrap_or_default();
        for (i, line) in production.lines().enumerate() {
            let code = line.trim_start();
            // a comment, or where the setting itself is defined
            if code.starts_with("//") || code.starts_with("pub fn blobs_path(") {
                continue;
            }
            if RAW.iter().any(|r| code.contains(r)) {
                found.push(format!("{rel}:{}: {code}", i + 1));
            }
        }
    }
    (found, files.len())
}

fn crates_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("..")
}

#[test]
fn no_production_code_outside_the_store_reaches_the_blob_folder() {
    let (found, read) = raw_uses(&crates_dir(), ALLOWED);
    assert!(
        read > 100,
        "only {read} source files read: the scan looked in the wrong place"
    );
    assert!(
        found.is_empty(),
        "raw blob paths outside blobs.rs (use the BlobStore API):\n{}",
        found.join("\n")
    );
}

/// The scan sees the use it allows: without the allow list it reports blobs.rs.
#[test]
fn the_scan_finds_the_allowed_uses_when_they_are_not_allowed() {
    let (found, _) = raw_uses(&crates_dir(), &[]);
    let file = "ownhealth-core/src/blobs.rs:";
    assert!(
        found.iter().any(|f| f.starts_with(file)),
        "{file} not seen in {found:#?}"
    );
}

#[test]
fn a_planted_use_is_reported_and_test_code_is_not() {
    let dir = tempfile::tempdir().expect("tmp");
    let src = dir.path().join("ownhealth-planted").join("src");
    std::fs::create_dir_all(src.join("routes")).expect("mkdir");
    std::fs::write(
        src.join("routes").join("files.rs"),
        "// settings.blobs_path() in a comment is fine\n\
         fn f(s: &Settings) -> PathBuf {\n    s.blobs_path().join(\"sha256\")\n}\n\
         #[cfg(test)]\nmod tests {\n    fn t(s: &Settings) { let _ = s.blobs_path(); }\n}\n",
    )
    .expect("write");
    let (found, read) = raw_uses(dir.path(), &[]);
    assert_eq!(read, 1);
    assert_eq!(
        found,
        ["ownhealth-planted/src/routes/files.rs:3: s.blobs_path().join(\"sha256\")"]
    );
    let (found, _) = raw_uses(dir.path(), &["ownhealth-planted/src/routes/files.rs"]);
    assert!(found.is_empty(), "{found:?}");
}

/// The store's own path helpers stay private to blobs.rs, so no caller, in core or outside, can
/// open a stored file by its path. The helpers must still be there, so a rename cannot pass this.
#[test]
fn the_store_keeps_its_path_helpers_private() {
    let src = std::fs::read_to_string(crates_dir().join("ownhealth-core/src/blobs.rs")).expect("blobs.rs");
    for (name, sig) in [
        ("path", "fn path(&self, sha256: &str)"),
        ("sealed_path", "fn sealed_path(&self, profile: Uuid, address: &str)"),
        ("find", "fn find(&self, owner: BlobOwner, sha256: &str)"),
    ] {
        let decls: Vec<&str> = src.lines().filter(|l| l.contains(sig)).map(str::trim).collect();
        let private = decls.len() == 1 && decls[0].starts_with("fn ");
        assert!(private, "blobs.rs: `{name}` must be declared once, private: {decls:?}");
    }
    assert!(
        !src.contains("fn root("),
        "blobs.rs: a root() hands out the store's folder"
    );
}
