// SPDX-License-Identifier: AGPL-3.0-or-later
//! Source-only LUT preview metadata (docs/02 §3.8, docs/03 §5): a display-referred LUT with an unpinned base, published without a layered approximation.

use looklab_model::baseline::is_neutral_baseline;
use looklab_model::file::{
    BaseReference, DeclaredEncoding, LicenseRecord, LookIdentity, LookKind, LutEmulation,
    OutputEncoding,
};
use serde::{Deserialize, Serialize};

use crate::PublishError;
use crate::hash::ArtifactName;
use crate::manifest::{
    ArtifactEntry, CARD_ARTIFACT, DOCUMENT_ARTIFACT, PREVIEW_LUT_ARTIFACT, RELEASE_ARTIFACT,
    VersionManifest,
};
use crate::preview::LUT_BYTES;

pub const SOURCE_CUBE_ARTIFACT: &str = "source.cube";
pub const SOURCE_LUT_ARTIFACT: &str = "source-lut.json";
pub const SOURCE_LUT_FORMAT: &str = "looklab-source-lut/1";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SourceLutFormat {
    #[serde(rename = "looklab-source-lut/1")]
    V1,
}

impl SourceLutFormat {
    pub const fn as_str(self) -> &'static str {
        SOURCE_LUT_FORMAT
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceLutPreview {
    pub format: SourceLutFormat,
    pub look: LookIdentity,
    pub name: String,
    pub license: LicenseRecord,
    /// Display-referred, with a base status that is not `pinned`.
    pub encoding: DeclaredEncoding,
    pub output: OutputEncoding,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub emulates: Option<LutEmulation>,
    pub source: ArtifactEntry,
    pub preview: SourceLutPreviewContext,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceLutPreviewContext {
    pub baseline: PreviewBaseline,
    pub lut: ArtifactEntry,
}

/// The rendering the preview LUT was composed with.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PreviewBaseline {
    /// No identified rendering of the declared base exists, so the fixed neutral baseline is used.
    NeutralFallback,
    /// A published rendering used as an identified approximation of the declared base.
    Approximated { base: BaseReference },
}

impl PreviewBaseline {
    pub fn approximation(&self) -> Option<&BaseReference> {
        match self {
            Self::NeutralFallback => None,
            Self::Approximated { base } => Some(base),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VersionLookShape {
    /// `look.looklab` with layered coefficients.
    Layered,
    /// `source-lut.json` with the source cube and composition preview.
    SourcePreview,
}

/// # Errors
/// Rejects artifact names that carry both or neither canonical look artifact.
pub fn look_shape_names<'a>(
    names: impl IntoIterator<Item = &'a str>,
) -> Result<VersionLookShape, PublishError> {
    let mut layered = false;
    let mut source = false;
    for name in names {
        layered |= name == DOCUMENT_ARTIFACT;
        source |= name == SOURCE_LUT_ARTIFACT;
    }
    match (layered, source) {
        (true, true) => Err(PublishError::ContradictoryLookArtifacts),
        (false, true) => Ok(VersionLookShape::SourcePreview),
        (true, false) => Ok(VersionLookShape::Layered),
        (false, false) => Err(PublishError::MissingLookArtifact),
    }
}

/// # Errors
/// Rejects a manifest that carries both or neither canonical look artifact.
pub fn look_shape(manifest: &VersionManifest) -> Result<VersionLookShape, PublishError> {
    look_shape_names(manifest.artifacts.iter().map(|entry| entry.name.as_str()))
}

impl SourceLutPreview {
    /// # Errors
    /// Rejects a descriptor whose declared preview is not source-only.
    pub fn validate(&self) -> Result<(), PublishError> {
        if self.look.kind != LookKind::LutDefined {
            return Err(source_lut("the look kind is not lut_defined"));
        }
        if self.name.trim().is_empty() || self.name.chars().count() > 120 {
            return Err(source_lut("the look name is empty or too long"));
        }
        self.license
            .validate()
            .map_err(|error| source_lut(error.to_string()))?;
        if let Some(emulation) = &self.emulates {
            let values = match emulation {
                LutEmulation::CameraRendering { maker, rendering } => {
                    vec![maker.as_str(), rendering.as_str()]
                }
                LutEmulation::FilmStock { name } => vec![name.as_str()],
            };
            if values
                .iter()
                .any(|value| value.trim().is_empty() || value.chars().count() > 120)
            {
                return Err(source_lut("the emulation declaration is empty or too long"));
            }
        }
        let declared = match &self.encoding {
            DeclaredEncoding::DisplayReferred { base, status } => {
                if status.is_pinned() {
                    return Err(source_lut("the display base is pinned"));
                }
                base.validate()
                    .map_err(|error| source_lut(error.to_string()))?;
                base
            }
            DeclaredEncoding::SceneReferred { .. } => {
                return Err(source_lut("the encoding is not display-referred"));
            }
        };
        match &self.preview.baseline {
            PreviewBaseline::NeutralFallback => {}
            PreviewBaseline::Approximated { base } => {
                if !matches!(base, BaseReference::Look { .. }) {
                    return Err(source_lut(
                        "the preview approximation is not a published Look rendering",
                    ));
                }
                if is_neutral_baseline(base) {
                    return Err(source_lut(
                        "the preview approximation is the neutral baseline",
                    ));
                }
                if base == declared {
                    return Err(source_lut(
                        "the preview approximation claims the declared base itself",
                    ));
                }
                base.validate()
                    .map_err(|error| source_lut(error.to_string()))?;
            }
        }
        if self.output != OutputEncoding::Srgb {
            return Err(source_lut("the output encoding is not srgb"));
        }
        if self.source.name.as_str() != SOURCE_CUBE_ARTIFACT || self.source.bytes == 0 {
            return Err(source_lut("the source artifact is not a cube"));
        }
        if self.preview.lut.name.as_str() != PREVIEW_LUT_ARTIFACT
            || self.preview.lut.bytes != LUT_BYTES as u64
        {
            return Err(source_lut("the preview artifact is not a preview LUT"));
        }
        Ok(())
    }

    /// # Errors
    /// Rejects a manifest whose entries contradict the descriptor.
    pub fn verify_artifacts(&self, manifest: &VersionManifest) -> Result<(), PublishError> {
        if manifest.look != self.look.slug || manifest.version != self.look.version {
            return Err(source_lut("the manifest names a different look version"));
        }
        if look_shape(manifest)? != VersionLookShape::SourcePreview {
            return Err(PublishError::ContradictoryLookArtifacts);
        }
        let source = manifest
            .artifact(&ArtifactName::new(SOURCE_CUBE_ARTIFACT)?)
            .ok_or_else(|| source_lut("the manifest has no source cube"))?;
        if source.sha256 != self.source.sha256 || source.bytes != self.source.bytes {
            return Err(source_lut("the source cube does not match the descriptor"));
        }
        let preview = manifest
            .artifact(&ArtifactName::new(PREVIEW_LUT_ARTIFACT)?)
            .ok_or_else(|| source_lut("the manifest has no preview LUT"))?;
        if preview.sha256 != self.preview.lut.sha256 || preview.bytes != self.preview.lut.bytes {
            return Err(source_lut("the preview LUT does not match the descriptor"));
        }
        for name in [CARD_ARTIFACT, RELEASE_ARTIFACT] {
            if manifest.artifact(&ArtifactName::new(name)?).is_some() {
                return Err(source_lut(
                    "a source-only version has a card or release artifact",
                ));
            }
        }
        self.validate()
    }

    /// # Errors
    /// Propagates a serializer failure.
    pub fn to_json(&self) -> Result<Vec<u8>, PublishError> {
        Ok(serde_json::to_vec_pretty(self)?)
    }

    /// # Errors
    /// Rejects malformed bytes or an invalid descriptor.
    pub fn decode(bytes: &[u8]) -> Result<Self, PublishError> {
        let descriptor: Self = serde_json::from_slice(bytes)?;
        descriptor.validate()?;
        Ok(descriptor)
    }
}

fn source_lut(message: impl Into<String>) -> PublishError {
    PublishError::SourceLut(message.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ArtifactHash;
    use looklab_model::file::{LookVersion, Slug};

    fn base(slug: &str, version: &str) -> BaseReference {
        BaseReference::Look {
            slug: Slug::new(slug).expect("slug"),
            version: LookVersion::new(version).expect("version"),
        }
    }

    fn descriptor(baseline: PreviewBaseline) -> SourceLutPreview {
        let preview = vec![0_u8; LUT_BYTES];
        SourceLutPreview {
            format: SourceLutFormat::V1,
            look: LookIdentity::new(
                "lut",
                "1.0.0",
                LookKind::LutDefined,
                looklab_model::WbPolicy::Adaptive,
            )
            .expect("identity"),
            name: "LUT".to_owned(),
            license: LicenseRecord {
                own_work: true,
                source_license: None,
                source_url: None,
                attribution: "a donor".to_owned(),
            },
            encoding: DeclaredEncoding::DisplayReferred {
                base: base("declared", "1.0.0"),
                status: looklab_model::file::BaseStatus::BaseIdentifiedUnmeasured,
            },
            output: OutputEncoding::Srgb,
            emulates: None,
            source: ArtifactEntry {
                name: ArtifactName::new(SOURCE_CUBE_ARTIFACT).expect("name"),
                sha256: ArtifactHash::of(b"cube"),
                bytes: 4,
            },
            preview: SourceLutPreviewContext {
                baseline,
                lut: ArtifactEntry {
                    name: ArtifactName::new(PREVIEW_LUT_ARTIFACT).expect("name"),
                    sha256: ArtifactHash::of(&preview),
                    bytes: preview.len() as u64,
                },
            },
        }
    }

    #[test]
    fn an_identified_approximation_round_trips() {
        let descriptor = descriptor(PreviewBaseline::Approximated {
            base: base("approximation", "2.0.0"),
        });
        descriptor.validate().expect("approximation");
        let decoded =
            SourceLutPreview::decode(&descriptor.to_json().expect("json")).expect("decode");
        assert_eq!(decoded.preview.baseline, descriptor.preview.baseline);
    }

    #[test]
    fn an_approximation_cannot_claim_the_declared_base_or_neutral() {
        let mut descriptor = descriptor(PreviewBaseline::Approximated {
            base: base("declared", "1.0.0"),
        });
        assert!(descriptor.validate().is_err());
        descriptor.preview.baseline = PreviewBaseline::Approximated {
            base: BaseReference::neutral_baseline(),
        };
        assert!(descriptor.validate().is_err());
    }
}
