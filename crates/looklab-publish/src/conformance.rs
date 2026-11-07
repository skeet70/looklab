// SPDX-License-Identifier: AGPL-3.0-or-later
//! The developer conformance download: a deterministic stored ZIP plus its digest.

use std::io::{Cursor, Write};

use serde::{Deserialize, Serialize};

use crate::hash::{ArtifactHash, SitePath};
use crate::{PublishError, SiteSink};

pub const CONFORMANCE_ARCHIVE_PATH: &str = "data/developers/conformance-v1.zip";
pub const CONFORMANCE_METADATA_PATH: &str = "data/developers/conformance-v1.json";
const ARCHIVE_PREFIX: &str = "conformance-v1";

/// One file inside the conformance archive, named relative to `conformance-v1/`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConformanceEntry {
    pub path: SitePath,
    pub bytes: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConformanceArchive {
    pub archive: SitePath,
    pub sha256: ArtifactHash,
}

/// Writes the archive and its metadata through the same sink as the site.
///
/// # Errors
/// Rejects duplicate paths, ZIP encoding failures and sink failures.
pub fn write_conformance_pack<S: SiteSink + ?Sized>(
    sink: &mut S,
    entries: &[ConformanceEntry],
) -> Result<ConformanceArchive, PublishError> {
    let bytes = build_conformance_archive(entries)?;
    let archive = SitePath::new(CONFORMANCE_ARCHIVE_PATH)?;
    sink.put(&archive, &bytes)?;
    let metadata = ConformanceArchive {
        archive,
        sha256: ArtifactHash::of(&bytes),
    };
    sink.put(
        &SitePath::new(CONFORMANCE_METADATA_PATH)?,
        &serde_json::to_vec_pretty(&metadata)?,
    )?;
    Ok(metadata)
}

/// Builds the deterministic stored ZIP for an ordered set of files.
///
/// # Errors
/// Rejects duplicate paths and ZIP encoding failures.
pub fn build_conformance_archive(entries: &[ConformanceEntry]) -> Result<Vec<u8>, PublishError> {
    let mut ordered: Vec<&ConformanceEntry> = entries.iter().collect();
    ordered.sort_by(|left, right| left.path.cmp(&right.path));
    for pair in ordered.windows(2) {
        if pair[0].path == pair[1].path {
            return Err(PublishError::Conformance(format!(
                "duplicate `{}`",
                pair[0].path
            )));
        }
    }
    let options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Stored)
        .last_modified_time(zip::DateTime::default())
        .unix_permissions(0o644);
    let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
    for entry in ordered {
        writer
            .start_file(format!("{ARCHIVE_PREFIX}/{}", entry.path.as_str()), options)
            .map_err(|error| PublishError::Conformance(error.to_string()))?;
        writer.write_all(&entry.bytes)?;
    }
    let cursor = writer
        .finish()
        .map_err(|error| PublishError::Conformance(error.to_string()))?;
    Ok(cursor.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str, bytes: &[u8]) -> ConformanceEntry {
        ConformanceEntry {
            path: SitePath::new(name).expect("site path"),
            bytes: bytes.to_vec(),
        }
    }

    #[test]
    fn archive_is_sorted_and_stable() {
        let entries = vec![
            entry("valid/card-33.llk", &[1, 2, 3]),
            entry("index.json", b"{}"),
        ];
        let first = build_conformance_archive(&entries).expect("archive");
        let mut reversed = entries;
        reversed.reverse();
        assert_eq!(
            build_conformance_archive(&reversed).expect("archive"),
            first
        );

        let mut archive =
            zip::ZipArchive::new(Cursor::new(first)).expect("readable conformance archive");
        assert_eq!(archive.len(), 2);
        assert_eq!(
            archive.by_index(0).expect("first").name(),
            "conformance-v1/index.json"
        );
        assert_eq!(
            archive.by_index(1).expect("second").name(),
            "conformance-v1/valid/card-33.llk"
        );
    }

    #[test]
    fn duplicate_paths_are_rejected() {
        let entries = [entry("index.json", b"{}"), entry("index.json", b"[]")];
        assert!(build_conformance_archive(&entries).is_err());
    }
}
