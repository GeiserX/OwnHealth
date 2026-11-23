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

/// Readers handed scopes that every caller builds with `Permission::read_scopes_for_sensitive`, so
/// read:sensitive in them already means the same-source rule held: `check_drug` (REST drug_check.rs
/// and the MCP check_drug tool).
const FILTERED_SCOPES: [&str; 1] = ["ownhealth-rules/src/drugs.rs"];

#[test]
fn sensitive_facts_must_use_category_aware_permissions() {
    let crates = Path::new(env!("CARGO_MANIFEST_DIR")).parent().expect("server crates");
    let access = crates.join("ownhealth-core/src/access.rs");
    // a scope set asked for read:sensitive alone, whatever the method (has, contains)
    let direct =
        regex::Regex::new(r#"\.\s*(?:has|contains)\s*\(\s*&?"read:sensitive"\s*[,)]"#).expect("scope check pattern");
    let mut violations = Vec::new();
    for krate in crates.read_dir().expect("server crates") {
        let krate = krate.expect("crate path").path();
        if !krate.is_dir() {
            continue;
        }
        rust_files(&krate, &mut |path| {
            let relative = path.strip_prefix(crates).expect("crate source path");
            if path == access || FILTERED_SCOPES.iter().any(|f| relative == Path::new(f)) {
                return;
            }
            let source = std::fs::read_to_string(path).expect("read Rust source");
            if direct.is_match(&source) {
                violations.push(relative.to_path_buf());
            }
        });
    }
    assert!(
        violations.is_empty(),
        "Use may_read_sensitive(category_scope): {violations:?}"
    );
}
