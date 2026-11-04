// SPDX-License-Identifier: AGPL-3.0-or-later
mod support;

use std::path::{Path, PathBuf};

#[test]
// Fixture tests read the local tree and never build for wasm32.
#[allow(clippy::disallowed_methods)]
fn fixtures_parse_and_sanitize() {
    let directory = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures");
    if !directory.is_dir() {
        return;
    }
    let mut files = Vec::new();
    collect(&directory, &mut files);
    files.sort();

    let mut checked = 0;
    for path in files {
        let extension = path
            .extension()
            .and_then(|extension| extension.to_str())
            .map(str::to_ascii_lowercase);
        if !matches!(
            extension.as_deref(),
            Some("raf" | "jpg" | "jpeg" | "tif" | "tiff" | "dng")
        ) {
            continue;
        }
        let bytes = std::fs::read(&path).expect("fixture is readable");
        let parsed = looklab_meta::parse(&bytes)
            .unwrap_or_else(|error| panic!("{}: {error}", path.display()));
        let (sanitized, _) = looklab_meta::sanitize(&parsed);
        let json = serde_json::to_string(&sanitized).expect("sanitized metadata is JSON");
        support::assert_whitelisted_keys(&json);
        checked += 1;
    }
    if checked == 0 {
        eprintln!("no metadata fixtures present");
    }
}

fn collect(directory: &Path, files: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect(&path, files);
        } else {
            files.push(path);
        }
    }
}
