// SPDX-License-Identifier: AGPL-3.0-or-later
//! Published software interface profiles and their reference round-trip reports
//! (docs/04 §6-§7).

use looklab_color::Mat3;
use looklab_export::{BakeIlluminant, Baker, CctK, InputTransform, ValidationReport, validate};
use looklab_model::Look;
use looklab_model::probes;
use looklab_schema::{
    AcceptedArtifact, IlluminantHandling, InterfaceId, InterfaceProfile, validate_interface_profile,
};
use serde::{Deserialize, Serialize};

use crate::PublishError;
use crate::audit::ProposalRef;
use crate::hash::{ArtifactHash, ArtifactName, SitePath};
use crate::manifest::ArtifactEntry;
use crate::site::MANIFEST_NAME;

pub const INTERFACE_MANIFEST_FORMAT: &str = "looklab-interface-manifest/1";
pub const INTERFACE_DEFINITION_ARTIFACT: &str = "definition.yaml";
pub const INTERFACE_REPORT_ARTIFACT: &str = "report.json";

/// The reference simulation's bounded LUT-node budget.
pub const MAX_REFERENCE_LUT_NODES: u64 = 2_000_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum InterfaceManifestFormat {
    #[serde(rename = "looklab-interface-manifest/1")]
    V1,
}

/// The provenance of a published definition.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "source", rename_all = "snake_case")]
pub enum InterfaceProvenance {
    /// Shipped with the looklab data baseline.
    Bootstrap,
    /// Moderated approval published in a snapshot.
    Approved {
        proposal: ProposalRef,
        snapshot_version: u64,
        snapshot_sha256: ArtifactHash,
    },
}

/// One immutable published definition revision.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct InterfaceProfileManifest {
    pub format: InterfaceManifestFormat,
    pub id: InterfaceId,
    pub name: String,
    /// The definition's declared version; it is not a revision counter.
    pub version: u32,
    pub provenance: InterfaceProvenance,
    pub definition: ArtifactEntry,
    pub report: ArtifactEntry,
}

impl InterfaceProfileManifest {
    /// # Errors
    /// Propagates a serializer failure.
    pub fn to_json(&self) -> Result<Vec<u8>, PublishError> {
        Ok(serde_json::to_vec_pretty(self)?)
    }

    /// # Errors
    /// Rejects a definition whose artifact name, digest, size or identity differs.
    pub fn verify_definition(&self, bytes: &[u8]) -> Result<InterfaceProfile, PublishError> {
        if self.definition.name.as_str() != INTERFACE_DEFINITION_ARTIFACT {
            return Err(PublishError::Interface(format!(
                "interface `{}` definition artifact is `{}`",
                self.id, self.definition.name
            )));
        }
        self.definition.verify(bytes)?;
        let text = std::str::from_utf8(bytes)
            .map_err(|_| PublishError::Interface("the definition is not UTF-8".to_owned()))?;
        let profile = validate_interface_profile(text).map_err(|errors| {
            PublishError::Interface(format!("the definition is invalid: {errors:?}"))
        })?;
        if profile.id != self.id || profile.name != self.name || profile.version != self.version {
            return Err(PublishError::Interface(format!(
                "interface `{}` manifest does not describe its definition",
                self.id
            )));
        }
        Ok(profile)
    }

    /// # Errors
    /// Rejects a report whose artifact name, digest, size, JSON shape or scalar values differ.
    pub fn verify_report(&self, bytes: &[u8]) -> Result<RoundTripReport, PublishError> {
        if self.report.name.as_str() != INTERFACE_REPORT_ARTIFACT {
            return Err(PublishError::Interface(format!(
                "interface `{}` report artifact is `{}`",
                self.id, self.report.name
            )));
        }
        self.report.verify(bytes)?;
        let report: RoundTripReport = serde_json::from_slice(bytes).map_err(|error| {
            PublishError::Interface(format!(
                "interface `{}` has an unreadable report: {error}",
                self.id
            ))
        })?;
        report.validate()?;
        Ok(report)
    }
}

/// A catalog entry for the current revision of one interface profile.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct InterfaceProfileEntry {
    pub id: InterfaceId,
    pub name: String,
    pub manifest: SitePath,
    pub manifest_sha256: ArtifactHash,
    pub manifest_bytes: u64,
}

/// One interface revision to publish, with its typed report.
#[derive(Clone, Debug, PartialEq)]
pub struct PublishedInterfaceProfile {
    pub manifest: InterfaceProfileManifest,
    pub manifest_bytes: Vec<u8>,
    pub definition: Vec<u8>,
    pub report: RoundTripReport,
    pub report_bytes: Vec<u8>,
    pub current: bool,
}

impl PublishedInterfaceProfile {
    /// # Errors
    /// Rejects a definition that is not UTF-8 or does not validate.
    pub fn new(
        provenance: InterfaceProvenance,
        definition: Vec<u8>,
        report: RoundTripReport,
        current: bool,
    ) -> Result<Self, PublishError> {
        let report_bytes = serde_json::to_vec_pretty(&report)?;
        Self::from_report_bytes(provenance, definition, report_bytes, current)
    }

    /// Builds a revision, rejecting definition or report bytes that do not verify.
    pub fn from_report_bytes(
        provenance: InterfaceProvenance,
        definition: Vec<u8>,
        report_bytes: Vec<u8>,
        current: bool,
    ) -> Result<Self, PublishError> {
        let text = std::str::from_utf8(&definition)
            .map_err(|_| PublishError::Interface("the definition is not UTF-8".to_owned()))?;
        let profile = validate_interface_profile(text).map_err(|errors| {
            PublishError::Interface(format!("the definition is invalid: {errors:?}"))
        })?;
        let manifest = InterfaceProfileManifest {
            format: InterfaceManifestFormat::V1,
            id: profile.id,
            name: profile.name,
            version: profile.version,
            provenance,
            definition: ArtifactEntry {
                name: ArtifactName::new(INTERFACE_DEFINITION_ARTIFACT)?,
                sha256: ArtifactHash::of(&definition),
                bytes: definition.len() as u64,
            },
            report: ArtifactEntry {
                name: ArtifactName::new(INTERFACE_REPORT_ARTIFACT)?,
                sha256: ArtifactHash::of(&report_bytes),
                bytes: report_bytes.len() as u64,
            },
        };
        let report = manifest.verify_report(&report_bytes)?;
        let manifest_bytes = manifest.to_json()?;
        Ok(Self {
            manifest,
            manifest_bytes,
            definition,
            report,
            report_bytes,
            current,
        })
    }

    /// Rebuilds a revision, rejecting published bytes that do not verify.
    pub fn from_published(
        manifest_bytes: Vec<u8>,
        definition: Vec<u8>,
        report_bytes: Vec<u8>,
        current: bool,
    ) -> Result<Self, PublishError> {
        let manifest: InterfaceProfileManifest =
            serde_json::from_slice(&manifest_bytes).map_err(|error| {
                PublishError::Interface(format!("unreadable interface manifest: {error}"))
            })?;
        manifest.verify_definition(&definition)?;
        let report = manifest.verify_report(&report_bytes)?;
        Ok(Self {
            manifest,
            manifest_bytes,
            definition,
            report,
            report_bytes,
            current,
        })
    }

    /// The content address of the manifest bytes.
    ///
    /// # Errors
    /// Rejects a hash that is not a safe path component.
    pub fn revision(&self) -> Result<ArtifactHash, PublishError> {
        let revision = ArtifactHash::of(&self.manifest_bytes);
        interface_dir(&revision)?;
        Ok(revision)
    }

    /// # Errors
    /// Rejects manifest bytes that do not encode this revision's manifest.
    pub fn verify_manifest(&self) -> Result<(), PublishError> {
        let parsed: InterfaceProfileManifest = serde_json::from_slice(&self.manifest_bytes)
            .map_err(|error| {
                PublishError::Interface(format!("unreadable interface manifest: {error}"))
            })?;
        if parsed != self.manifest {
            return Err(PublishError::Interface(format!(
                "interface `{}` manifest bytes differ from the manifest",
                self.manifest.id
            )));
        }
        Ok(())
    }
}

/// # Errors
/// Rejects a hash that is not a safe path component.
pub fn interface_dir(revision: &ArtifactHash) -> Result<SitePath, PublishError> {
    SitePath::new(format!("data/interfaces/{}", revision.as_str()))
}

/// # Errors
/// Rejects a hash that is not a safe path component.
pub fn interface_manifest_path(revision: &ArtifactHash) -> Result<SitePath, PublishError> {
    Ok(interface_dir(revision)?.join(MANIFEST_NAME))
}

/// One CCT's reference round-trip error distribution.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CctSummary {
    pub cct: f64,
    pub count: usize,
    pub median: f64,
    pub p95: f64,
    pub max: f64,
    pub unsupported: usize,
    pub clipped: usize,
}

/// The reference pipeline's round-trip check of a baked profile.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RoundTripReport {
    pub probes: usize,
    pub target_clipped: usize,
    pub ccts: Vec<CctSummary>,
    pub flagged: bool,
}

impl RoundTripReport {
    pub fn from_validation(report: &ValidationReport) -> Self {
        let ccts = std::iter::once(&report.primary)
            .chain(&report.others)
            .map(|entry| CctSummary {
                cct: entry.cct.kelvin(),
                count: entry.stats.count,
                median: entry.stats.median,
                p95: entry.stats.p95,
                max: entry.stats.max,
                unsupported: entry.unsupported,
                clipped: entry.clipped,
            })
            .collect();
        Self {
            probes: report.probes,
            target_clipped: report.target_clipped,
            ccts,
            flagged: report.flagged(),
        }
    }

    fn validate(&self) -> Result<(), PublishError> {
        if self.ccts.is_empty() {
            return Err(PublishError::Interface(
                "the reference report has no CCT entries".to_owned(),
            ));
        }
        for entry in &self.ccts {
            for (name, value) in [
                ("cct", entry.cct),
                ("median", entry.median),
                ("p95", entry.p95),
                ("max", entry.max),
            ] {
                if !value.is_finite() || value < 0.0 {
                    return Err(PublishError::Interface(format!(
                        "the reference report has a non-finite or negative {name}"
                    )));
                }
            }
        }
        Ok(())
    }
}

/// A fixed camera model for the reference round-trip simulation.
#[derive(Clone, Copy)]
struct ReferenceTransform(Mat3);

impl InputTransform for ReferenceTransform {
    fn camera_to_xyz(&self, _cct: CctK) -> Option<Mat3> {
        Some(self.0)
    }
}

/// Bakes and validates a profile through the bounded reference pipeline.
///
/// # Errors
/// A profile the exporter cannot bake, or grids over the reference node budget.
pub fn reference_round_trip(profile: &InterfaceProfile) -> Result<RoundTripReport, PublishError> {
    let nodes: u64 = profile
        .accepts
        .iter()
        .filter_map(|accepted| match accepted {
            AcceptedArtifact::Lut3d(lut) => Some(lut),
            AcceptedArtifact::Matrix3x3(_) | AcceptedArtifact::Curve1d(_) => None,
        })
        .flat_map(|lut| lut.sizes.iter())
        .map(|side| u64::from(*side).pow(3))
        .sum();
    if nodes > MAX_REFERENCE_LUT_NODES {
        return Err(PublishError::Interface(
            "the profile's LUT grids exceed the reference simulation's node budget".to_owned(),
        ));
    }
    let look = Look::colorimetric();
    let transform = ReferenceTransform(Mat3::IDENTITY);
    let illuminant = match profile.insertion.illuminant_handling {
        IlluminantHandling::Single => BakeIlluminant::Single { cct: CctK::D65 },
        IlluminantHandling::DualInterpolated => BakeIlluminant::DualInterpolated,
    };
    let output = Baker::new(profile, &look, &transform)
        .bake(illuminant)
        .map_err(|error| PublishError::Interface(error.to_string()))?;
    let report = validate(&output, &look, &transform, &probes::v1(), None)
        .map_err(|error| PublishError::Interface(error.to_string()))?;
    Ok(RoundTripReport::from_validation(&report))
}
