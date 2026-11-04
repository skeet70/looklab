// SPDX-License-Identifier: AGPL-3.0-or-later
//! Version manifests and artifact verification (docs/05 §11).

use core::fmt;

use looklab_model::file::{LookVersion, Slug};
use serde::{Deserialize, Serialize};

use crate::PublishError;
use crate::hash::{ArtifactHash, ArtifactName};

/// Where a Look version sits in the publication lifecycle.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LookStatus {
    Provisional,
    Rooted,
    Published,
    Deprecated,
}

impl LookStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Provisional => "provisional",
            Self::Rooted => "rooted",
            Self::Published => "published",
            Self::Deprecated => "deprecated",
        }
    }
}

/// A release gate and its outcome.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GateCheck {
    pub name: String,
    pub passed: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GateSummary {
    pub checks: Vec<GateCheck>,
}

impl GateSummary {
    pub fn all_passed(&self) -> bool {
        self.checks.iter().all(|check| check.passed)
    }
}

/// One body's contribution to a Look version.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BodyUse {
    pub body: String,
    pub epochs: Vec<u32>,
    pub images: u32,
    pub credentials: u32,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProvenanceSummary {
    pub bodies: Vec<BodyUse>,
    pub root_admissions: Vec<String>,
    pub anchor_version: String,
    pub community_snapshot_version: u32,
    pub prep_versions: Vec<u32>,
    pub created: String,
    pub software: String,
}

pub const DOCUMENT_ARTIFACT: &str = "look.looklab";
pub const PREVIEW_LUT_ARTIFACT: &str = "preview.lut33";

/// Well-known files inside a version directory.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LookArtifact {
    Document,
    PreviewLut,
    Other,
}

impl LookArtifact {
    pub fn of(name: &ArtifactName) -> Self {
        match name.as_str() {
            DOCUMENT_ARTIFACT => Self::Document,
            PREVIEW_LUT_ARTIFACT => Self::PreviewLut,
            _ => Self::Other,
        }
    }
}

/// One published file, addressed by its SHA-256 digest.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactEntry {
    pub name: ArtifactName,
    pub sha256: ArtifactHash,
    pub bytes: u64,
}

/// The published description of one immutable Look version.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VersionManifest {
    pub look: Slug,
    pub version: LookVersion,
    pub artifacts: Vec<ArtifactEntry>,
    pub status: LookStatus,
    pub gate_summary: GateSummary,
    pub provenance: ProvenanceSummary,
}

impl VersionManifest {
    /// # Errors
    /// Rejects an empty artifact list and duplicate names.
    pub fn from_files(
        look: Slug,
        version: LookVersion,
        status: LookStatus,
        gate_summary: GateSummary,
        provenance: ProvenanceSummary,
        files: &[ArtifactBlob],
    ) -> Result<Self, PublishError> {
        if files.is_empty() {
            return Err(PublishError::NoArtifacts {
                look: look.as_str().to_owned(),
                version: version.as_str().to_owned(),
            });
        }
        let mut artifacts = Vec::with_capacity(files.len());
        for file in files {
            if artifacts
                .iter()
                .any(|entry: &ArtifactEntry| entry.name == file.name)
            {
                return Err(PublishError::DuplicateArtifact(
                    file.name.as_str().to_owned(),
                ));
            }
            artifacts.push(ArtifactEntry {
                name: file.name.clone(),
                sha256: ArtifactHash::of(&file.bytes),
                bytes: file.bytes.len() as u64,
            });
        }
        Ok(Self {
            look,
            version,
            artifacts,
            status,
            gate_summary,
            provenance,
        })
    }

    pub fn artifact(&self, name: &ArtifactName) -> Option<&ArtifactEntry> {
        self.artifacts.iter().find(|entry| &entry.name == name)
    }

    /// # Errors
    /// Propagates a serializer failure.
    pub fn to_json(&self) -> Result<Vec<u8>, PublishError> {
        Ok(serde_json::to_vec_pretty(self)?)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ArtifactBlob {
    pub name: ArtifactName,
    pub bytes: Vec<u8>,
}

impl ArtifactBlob {
    pub fn new(name: ArtifactName, bytes: Vec<u8>) -> Self {
        Self { name, bytes }
    }
}

/// A downloaded artifact whose bytes do not match the manifest entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HashMismatch {
    NotListed {
        name: ArtifactName,
    },
    Digest {
        name: ArtifactName,
        expected: ArtifactHash,
        actual: ArtifactHash,
    },
    Size {
        name: ArtifactName,
        expected: u64,
        actual: u64,
    },
}

impl fmt::Display for HashMismatch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotListed { name } => {
                write!(f, "artifact `{name}` is not listed in the version manifest")
            }
            Self::Digest {
                name,
                expected,
                actual,
            } => write!(
                f,
                "artifact `{name}` does not match the version manifest: expected {expected}, computed {actual}"
            ),
            Self::Size {
                name,
                expected,
                actual,
            } => write!(
                f,
                "artifact `{name}` has {actual} bytes, manifest lists {expected}"
            ),
        }
    }
}

impl std::error::Error for HashMismatch {}

/// # Errors
/// Returns [`HashMismatch`] when `name` is unlisted or the digest or size differs.
pub fn verify_artifact(
    manifest: &VersionManifest,
    name: &ArtifactName,
    bytes: &[u8],
) -> Result<(), HashMismatch> {
    let Some(entry) = manifest.artifact(name) else {
        return Err(HashMismatch::NotListed { name: name.clone() });
    };
    if entry.bytes != bytes.len() as u64 {
        return Err(HashMismatch::Size {
            name: name.clone(),
            expected: entry.bytes,
            actual: bytes.len() as u64,
        });
    }
    let actual = ArtifactHash::of(bytes);
    if actual != entry.sha256 {
        return Err(HashMismatch::Digest {
            name: name.clone(),
            expected: entry.sha256.clone(),
            actual,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest(files: &[ArtifactBlob]) -> VersionManifest {
        VersionManifest::from_files(
            Slug::new("provia").expect("slug"),
            LookVersion::new("1.0.0").expect("version"),
            LookStatus::Published,
            GateSummary::default(),
            ProvenanceSummary::default(),
            files,
        )
        .expect("manifest")
    }

    #[test]
    fn manifest_records_size_and_digest() {
        let name = ArtifactName::new("look.looklab").expect("name");
        let manifest = manifest(&[ArtifactBlob::new(name.clone(), vec![1, 2, 3])]);
        let entry = manifest.artifact(&name).expect("entry");
        assert_eq!(entry.bytes, 3);
        assert_eq!(entry.sha256, ArtifactHash::of(&[1, 2, 3]));
    }

    #[test]
    fn verification_accepts_matching_bytes() {
        let name = ArtifactName::new("look.looklab").expect("name");
        let manifest = manifest(&[ArtifactBlob::new(name.clone(), vec![1, 2, 3])]);
        assert!(verify_artifact(&manifest, &name, &[1, 2, 3]).is_ok());
    }

    #[test]
    fn verification_rejects_a_tampered_artifact() {
        let name = ArtifactName::new("look.looklab").expect("name");
        let manifest = manifest(&[ArtifactBlob::new(name.clone(), vec![1, 2, 3])]);
        assert!(matches!(
            verify_artifact(&manifest, &name, &[1, 2, 4]),
            Err(HashMismatch::Digest { .. })
        ));
    }

    #[test]
    fn verification_rejects_a_length_change_and_an_unlisted_name() {
        let name = ArtifactName::new("look.looklab").expect("name");
        let manifest = manifest(&[ArtifactBlob::new(name.clone(), vec![1, 2, 3])]);
        assert!(matches!(
            verify_artifact(&manifest, &name, &[1, 2]),
            Err(HashMismatch::Size { .. })
        ));
        let other = ArtifactName::new("other.bin").expect("name");
        assert!(matches!(
            verify_artifact(&manifest, &other, &[]),
            Err(HashMismatch::NotListed { .. })
        ));
    }
}
