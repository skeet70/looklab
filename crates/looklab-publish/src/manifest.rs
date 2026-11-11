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

/// Gate check that marks a version's camera response models as synthetic sample data.
pub const SYNTHETIC_SAMPLE_GATE: &str = "Sample settings models";
/// Gate check that marks the browser match fixture.
pub const SYNTHETIC_FIXTURE_GATE: &str = "synthetic case";

/// What a version manifest declares about its camera response data.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CameraModelProvenance {
    /// The manifest makes no synthetic declaration.
    Unknown,
    /// The manifest declares synthetic camera response data for demonstrations or fixtures.
    Synthetic,
}

impl GateSummary {
    pub fn all_passed(&self) -> bool {
        self.checks.iter().all(|check| check.passed)
    }

    fn declares_synthetic_camera_model(&self) -> bool {
        self.checks.iter().any(|check| {
            check.name == SYNTHETIC_SAMPLE_GATE || check.name == SYNTHETIC_FIXTURE_GATE
        })
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
    /// The community snapshot whose schemas the release was built from.
    pub community_snapshot_version: u64,
    pub prep_versions: Vec<u32>,
    pub created: String,
    pub software: String,
}

pub const DOCUMENT_ARTIFACT: &str = "look.looklab";
pub const PREVIEW_LUT_ARTIFACT: &str = "preview.lut33";
pub const CARD_ARTIFACT: &str = "card.llk";
pub const RELEASE_ARTIFACT: &str = "release.json";

/// Well-known files inside a version directory.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LookArtifact {
    Document,
    PreviewLut,
    Card,
    Other,
}

impl LookArtifact {
    pub fn of(name: &ArtifactName) -> Self {
        match name.as_str() {
            DOCUMENT_ARTIFACT => Self::Document,
            PREVIEW_LUT_ARTIFACT => Self::PreviewLut,
            CARD_ARTIFACT => Self::Card,
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

impl ArtifactEntry {
    /// # Errors
    /// Returns [`HashMismatch`] when the digest or size differs.
    pub fn verify(&self, bytes: &[u8]) -> Result<(), HashMismatch> {
        if self.bytes != bytes.len() as u64 {
            return Err(HashMismatch::Size {
                name: self.name.clone(),
                expected: self.bytes,
                actual: bytes.len() as u64,
            });
        }
        let actual = ArtifactHash::of(bytes);
        if actual != self.sha256 {
            return Err(HashMismatch::Digest {
                name: self.name.clone(),
                expected: self.sha256.clone(),
                actual,
            });
        }
        Ok(())
    }
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

    /// Reports only the synthetic declaration the manifest carries.
    pub fn camera_model_provenance(&self) -> CameraModelProvenance {
        if self.gate_summary.declares_synthetic_camera_model() {
            CameraModelProvenance::Synthetic
        } else {
            CameraModelProvenance::Unknown
        }
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
    entry.verify(bytes)
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

    fn gated_manifest(status: LookStatus, gates: &[&str]) -> VersionManifest {
        let name = ArtifactName::new("look.looklab").expect("name");
        VersionManifest::from_files(
            Slug::new("provia").expect("slug"),
            LookVersion::new("1.0.0").expect("version"),
            status,
            GateSummary {
                checks: gates
                    .iter()
                    .map(|name| GateCheck {
                        name: String::from(*name),
                        passed: true,
                        detail: None,
                    })
                    .collect(),
            },
            ProvenanceSummary::default(),
            &[ArtifactBlob::new(name, vec![1, 2, 3])],
        )
        .expect("manifest")
    }

    #[test]
    fn declared_sample_and_fixture_gates_mark_synthetic_camera_models() {
        assert_eq!(
            gated_manifest(LookStatus::Published, &[SYNTHETIC_SAMPLE_GATE])
                .camera_model_provenance(),
            CameraModelProvenance::Synthetic
        );
        assert_eq!(
            gated_manifest(LookStatus::Published, &[SYNTHETIC_FIXTURE_GATE])
                .camera_model_provenance(),
            CameraModelProvenance::Synthetic
        );
    }

    #[test]
    fn provisional_or_ungated_versions_are_not_called_synthetic() {
        assert_eq!(
            gated_manifest(LookStatus::Provisional, &["identity tolerance"])
                .camera_model_provenance(),
            CameraModelProvenance::Unknown
        );
        assert_eq!(
            gated_manifest(LookStatus::Published, &[]).camera_model_provenance(),
            CameraModelProvenance::Unknown
        );
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
