use std::path::Path;

fn rust_files(dir: &Path, check: &mut impl FnMut(&Path)) {
    for entry in dir.read_dir().expect("read source directory") {
        let entry = entry.expect("source directory entry");
        let path = entry.path();
        if entry.file_type().expect("source file type").is_dir() {
            rust_files(&path, check);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            check(&path);
        }
    }
}

#[test]
fn sensitive_facts_must_use_category_aware_permissions() {
    let crates = Path::new(env!("CARGO_MANIFEST_DIR")).parent().expect("server crates");
    let access = crates.join("ownhealth-core/src/access.rs");
    let direct = regex::Regex::new(r#"\.\s*has\s*\(\s*"read:sensitive"\s*[,)]"#).expect("scope check pattern");
    let mut violations = Vec::new();
    for krate in crates.read_dir().expect("server crates") {
        let krate = krate.expect("crate path").path();
        if !krate.is_dir() {
            continue;
        }
        rust_files(&krate, &mut |path| {
            if path == access {
                return;
            }
            let source = std::fs::read_to_string(path).expect("read Rust source");
            if direct.is_match(&source) {
                violations.push(path.strip_prefix(crates).expect("crate source path").to_path_buf());
            }
        });
    }
    assert!(
        violations.is_empty(),
        "Use may_read_sensitive(category_scope): {violations:?}"
    );
}
