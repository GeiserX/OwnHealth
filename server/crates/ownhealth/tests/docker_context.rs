//! The image build context. `.dockerignore` drops `data/*` and lets back in only the folders it
//! names, and the server compiles those files in (`include_str!`). A data folder the file does not
//! name is missing inside the image build, so the release build fails there and nowhere else.

use std::path::PathBuf;

#[test]
fn every_data_folder_reaches_the_image_build() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../..");
    let ignore = std::fs::read_to_string(root.join(".dockerignore")).expect(".dockerignore");
    let included: Vec<&str> = ignore.lines().filter_map(|l| l.trim().strip_prefix("!data/")).collect();
    let mut missing = Vec::new();
    for entry in std::fs::read_dir(root.join("data")).expect("data/") {
        let entry = entry.expect("data/ entry");
        if entry.file_type().expect("file type").is_dir() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if !included.contains(&name.as_str()) {
                missing.push(name);
            }
        }
    }
    missing.sort();
    assert!(
        missing.is_empty(),
        "data folders .dockerignore leaves out of the image build: {missing:?}"
    );
}
