// SPDX-License-Identifier: AGPL-3.0-or-later
//! Community data snapshots of approved documents ([docs/03 §1](../../../docs/03-data-model.md)).

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::ids::{BodyId, InterfaceId};

pub const SNAPSHOT_MANIFEST: &str = "snapshot.json";

/// The fixture consent list a snapshot carries.
pub const FIXTURE_MANIFEST: &str = "fixtures/manifest.toml";

pub fn body_export_path(id: &BodyId) -> String {
    format!("bodies/{}.body.yaml", export_stem(id.as_str()))
}

pub fn schema_export_path(id: &BodyId) -> String {
    format!("bodies/{}.yaml", export_stem(id.as_str()))
}

pub fn interface_export_path(id: &InterfaceId) -> String {
    format!("interfaces/{}.yaml", export_stem(id.as_str()))
}

/// Encodes an identifier into one file-name component: every byte outside
/// `[a-z0-9._-]` becomes `%XX`, so the mapping is injective.
fn export_stem(id: &str) -> String {
    let mut out = String::with_capacity(id.len());
    for byte in id.bytes() {
        if byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'_' | b'.')
        {
            out.push(char::from(byte));
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

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
    use std::path::{Path, PathBuf};

    use serde::Deserialize;
    use sha2::{Digest, Sha256};

    use crate::body::{MappingStatus, validate_body};
    use crate::error::{ErrorKind, ValidationError};
    use crate::fs::{FileErrors, Kind, classified_yaml_files, classify, validate_dir};
    use crate::interface::validate_interface_profile;
    use crate::proposal::{FixtureFileName, Placement, Sha256Hex};
    use crate::schema::validate_control_schema;

    use super::{
        FIXTURE_MANIFEST, SNAPSHOT_MANIFEST, SnapshotManifest, body_export_path, content_hash,
        interface_export_path, schema_export_path,
    };

    #[derive(Deserialize)]
    struct FixtureManifest {
        #[serde(default)]
        fixture: Vec<FixtureEntry>,
    }

    #[derive(Deserialize)]
    struct FixtureEntry {
        file: String,
        make: String,
        model: String,
        #[serde(default)]
        sha256: Option<Sha256Hex>,
        #[serde(default)]
        source_url: Option<String>,
        #[serde(default, rename = "committed")]
        placement: Placement,
    }

    /// # Errors
    /// I/O failures while walking the snapshot.
    pub fn validate_snapshot_dir(root: &Path) -> io::Result<Vec<FileErrors>> {
        let mut failures = validate_dir(root)?;
        let paths = classified_yaml_files(root)?;
        let mut files = Vec::with_capacity(paths.len() + 1);
        for path in &paths {
            files.push((relative(root, path), fs::read(path)?));
        }
        check_ownership(root, &paths, &mut failures)?;
        check_fixtures(root, &paths, &mut failures)?;
        if let Ok(bytes) = fs::read(root.join(FIXTURE_MANIFEST)) {
            files.push((FIXTURE_MANIFEST.to_owned(), bytes));
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

    fn relative(root: &Path, path: &Path) -> String {
        path.strip_prefix(root)
            .unwrap_or(path)
            .to_string_lossy()
            .replace('\\', "/")
    }

    fn check_ownership(
        root: &Path,
        paths: &[PathBuf],
        failures: &mut Vec<FileErrors>,
    ) -> io::Result<()> {
        for path in paths {
            let text = fs::read_to_string(path)?;
            let declared = match classify(path) {
                Some(Kind::Body) => validate_body(&text)
                    .ok()
                    .map(|body| body_export_path(&body.id)),
                Some(Kind::Schema) => validate_control_schema(&text)
                    .ok()
                    .map(|schema| schema_export_path(&schema.body)),
                Some(Kind::Interface) => validate_interface_profile(&text)
                    .ok()
                    .map(|profile| interface_export_path(&profile.id)),
                Some(Kind::Manual) => None,
                None => None,
            };
            let Some(expected) = declared else {
                continue;
            };
            let actual = relative(root, path);
            if expected != actual {
                failures.push(FileErrors {
                    file: path.clone(),
                    errors: vec![ValidationError::new(
                        "",
                        ErrorKind::DocumentOwner {
                            declared: expected,
                            path: actual,
                        },
                    )],
                });
            }
        }
        Ok(())
    }

    fn check_fixtures(
        root: &Path,
        paths: &[PathBuf],
        failures: &mut Vec<FileErrors>,
    ) -> io::Result<()> {
        let manifest_path = root.join(FIXTURE_MANIFEST);
        let entries = match fs::read_to_string(&manifest_path) {
            Ok(text) => match toml::from_str::<FixtureManifest>(&text) {
                Ok(manifest) => {
                    let mut errors = fixture_entry_errors(&manifest);
                    errors.extend(fixture_storage_errors(root, &manifest));
                    if !errors.is_empty() {
                        failures.push(FileErrors {
                            file: PathBuf::from(FIXTURE_MANIFEST),
                            errors,
                        });
                    }
                    Some(manifest)
                }
                Err(error) => {
                    failures.push(fixture_error(format!("not valid TOML: {error}")));
                    None
                }
            },
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => {
                failures.push(fixture_error(format!("unreadable: {error}")));
                None
            }
        };
        for path in paths {
            if !matches!(classify(path), Some(Kind::Body)) {
                continue;
            }
            let text = fs::read_to_string(path)?;
            let Ok(body) = validate_body(&text) else {
                continue;
            };
            if body.mapping_status == MappingStatus::Provisional {
                continue;
            }
            let covered = entries.as_ref().is_some_and(|manifest| {
                manifest
                    .fixture
                    .iter()
                    .any(|fixture| body.matches_fixture(&fixture.make, &fixture.model))
            });
            if !covered {
                failures.push(FileErrors {
                    file: path.clone(),
                    errors: vec![ValidationError::new(
                        "mapping_status",
                        ErrorKind::FixtureEvidenceMissing {
                            entity: body.id.to_string(),
                        },
                    )],
                });
            }
        }
        Ok(())
    }

    fn fixture_entry_errors(manifest: &FixtureManifest) -> Vec<ValidationError> {
        let mut errors = Vec::new();
        for (index, entry) in manifest.fixture.iter().enumerate() {
            if FixtureFileName::parse(&entry.file).is_err() {
                errors.push(fixture_entry_error(index, "a plain file name"));
            }
            if entry.sha256.is_none() {
                errors.push(fixture_entry_error(index, "a sha256 digest"));
            }
            if entry.make.trim().is_empty() || entry.model.trim().is_empty() {
                errors.push(fixture_entry_error(
                    index,
                    "make and model, so the fixture names a body",
                ));
            }
            let fetch_only = entry.placement == Placement::Fetch;
            match &entry.source_url {
                Some(url) if fetch_only && !url.starts_with("https://") => {
                    errors.push(fixture_entry_error(index, "an https source URL"));
                }
                None if fetch_only => {
                    errors.push(fixture_entry_error(
                        index,
                        "a source URL for a fetch-only fixture",
                    ));
                }
                _ => {}
            }
        }
        errors
    }

    fn fixture_entry_error(index: usize, detail: &str) -> ValidationError {
        ValidationError::new(
            format!("fixture[{index}]"),
            ErrorKind::FixtureManifest(detail.to_owned()),
        )
    }

    fn fixture_storage_errors(root: &Path, manifest: &FixtureManifest) -> Vec<ValidationError> {
        let mut errors = Vec::new();
        for (index, entry) in manifest.fixture.iter().enumerate() {
            let Ok(file) = FixtureFileName::parse(&entry.file) else {
                continue;
            };
            let Some(relative) = entry.placement.snapshot_path(file) else {
                continue;
            };
            let Ok(bytes) = fs::read(root.join(&relative)) else {
                errors.push(fixture_entry_error(
                    index,
                    &format!("fixture bytes at {relative}"),
                ));
                continue;
            };
            if let Some(expected) = entry.sha256
                && Sha256::digest(&bytes).as_slice() != expected.as_bytes()
            {
                errors.push(fixture_entry_error(
                    index,
                    &format!("fixture bytes at {relative} matching the recorded sha256"),
                ));
            }
        }
        errors
    }

    fn fixture_error(message: String) -> FileErrors {
        FileErrors {
            file: PathBuf::from(FIXTURE_MANIFEST),
            errors: vec![ValidationError::new(
                "",
                ErrorKind::FixtureManifest(message),
            )],
        }
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
