// SPDX-License-Identifier: AGPL-3.0-or-later
//! Community data snapshots of approved documents ([docs/03 §1](../../../docs/03-data-model.md)).

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const SNAPSHOT_MANIFEST: &str = "snapshot.json";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SnapshotManifest {
    pub version: u64,
    pub content_hash: String,
}

/// SHA-256 over the sorted `(relative path, bytes)` pairs, each followed by a NUL.
pub fn content_hash(files: &[(String, Vec<u8>)]) -> String {
    let mut sorted: Vec<&(String, Vec<u8>)> = files.iter().collect();
    sorted.sort_by(|a, b| a.0.cmp(&b.0));
    let mut hasher = Sha256::new();
    for (path, bytes) in sorted {
        hasher.update(path.as_bytes());
        hasher.update([0u8]);
        hasher.update(bytes);
        hasher.update([0u8]);
    }
    let digest = hasher.finalize();
    let mut out = String::with_capacity(64);
    for byte in digest {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

#[cfg(feature = "fs")]
mod dir {
    #![allow(clippy::disallowed_methods)]

    use std::fs;
    use std::io;
    use std::path::Path;

    use crate::error::{ErrorKind, ValidationError};
    use crate::fs::{FileErrors, classified_yaml_files, validate_dir};

    use super::{SNAPSHOT_MANIFEST, SnapshotManifest, content_hash};

    /// # Errors
    /// I/O failures while walking the snapshot.
    pub fn validate_snapshot_dir(root: &Path) -> io::Result<Vec<FileErrors>> {
        let mut failures = validate_dir(root)?;
        let paths = classified_yaml_files(root)?;
        let mut files = Vec::with_capacity(paths.len());
        for path in &paths {
            let relative = path
                .strip_prefix(root)
                .unwrap_or(path)
                .to_string_lossy()
                .replace('\\', "/");
            files.push((relative, fs::read(path)?));
        }
        let manifest = match fs::read_to_string(root.join(SNAPSHOT_MANIFEST)) {
            Ok(text) => match serde_json::from_str::<SnapshotManifest>(&text) {
                Ok(manifest) => manifest,
                Err(error) => {
                    failures.push(manifest_error(format!("not valid JSON: {error}")));
                    return Ok(failures);
                }
            },
            Err(error) => {
                failures.push(manifest_error(format!("unreadable: {error}")));
                return Ok(failures);
            }
        };
        let actual = content_hash(&files);
        if actual != manifest.content_hash {
            failures.push(FileErrors {
                file: Path::new(SNAPSHOT_MANIFEST).to_path_buf(),
                errors: vec![ValidationError::new(
                    "content_hash",
                    ErrorKind::SnapshotHashMismatch {
                        declared: manifest.content_hash,
                        found: actual,
                    },
                )],
            });
        }
        Ok(failures)
    }

    fn manifest_error(message: String) -> FileErrors {
        FileErrors {
            file: Path::new(SNAPSHOT_MANIFEST).to_path_buf(),
            errors: vec![ValidationError::new(
                "",
                ErrorKind::SnapshotManifest(message),
            )],
        }
    }
}

#[cfg(feature = "fs")]
pub use dir::validate_snapshot_dir;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_hash_is_order_independent_and_path_sensitive() {
        let a = ("bodies/fujifilm-x-t5.yaml".to_owned(), b"one".to_vec());
        let b = ("interfaces/sim.yaml".to_owned(), b"two".to_vec());
        let forward = content_hash(&[a.clone(), b.clone()]);
        let reversed = content_hash(&[b.clone(), a]);
        assert_eq!(forward, reversed);
        assert_eq!(forward.len(), 64);
        assert_ne!(forward, content_hash(&[b]));
    }

    #[test]
    fn content_hash_changes_with_the_bytes() {
        let path = "bodies/fujifilm-x-t5.body.yaml".to_owned();
        assert_ne!(
            content_hash(&[(path.clone(), b"a".to_vec())]),
            content_hash(&[(path, b"b".to_vec())])
        );
    }
}
