// SPDX-License-Identifier: AGPL-3.0-or-later
//! `native_json`: the artifact, the profile it targets and the bake, for firmware
//! test harnesses.

use looklab_schema::{InterfaceId, Interpolation, Precision};
use serde::{Deserialize, Serialize};

use crate::artifact::Artifact;
use crate::bake::{BakeIlluminant, BakeOutput, InsertionSpec};
use crate::cct::CctK;
use crate::downstream::DownstreamModel;
use crate::error::{ExportError, Result};
use crate::serde_names;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum NativeFormat {
    #[serde(rename = "looklab-native/1")]
    V1,
}

/// A schema `Interpolation` with the profile's serialized name.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct InterpolationName(pub Interpolation);

impl TryFrom<String> for InterpolationName {
    type Error = ExportError;

    fn try_from(value: String) -> Result<Self> {
        serde_names::interpolation::parse(&value)
            .map(Self)
            .ok_or(ExportError::Profile("unknown interpolation"))
    }
}

impl From<InterpolationName> for String {
    fn from(value: InterpolationName) -> Self {
        serde_names::interpolation::name(value.0).to_string()
    }
}

/// A schema `Precision` with the profile's serialized name.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct PrecisionName(pub Precision);

impl TryFrom<String> for PrecisionName {
    type Error = ExportError;

    fn try_from(value: String) -> Result<Self> {
        serde_names::precision::parse(&value)
            .map(Self)
            .ok_or(ExportError::Profile("unknown precision"))
    }
}

impl From<PrecisionName> for String {
    fn from(value: PrecisionName) -> Self {
        serde_names::precision::name(value.0).to_string()
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct NativeDocument {
    pub format: NativeFormat,
    pub producer: String,
    pub profile_id: String,
    pub profile_version: u32,
    pub bake: BakeIlluminant,
    pub insertion: InsertionSpec,
    pub downstream: DownstreamModel,
    pub interpolation: Option<InterpolationName>,
    pub precision: Option<PrecisionName>,
    pub artifacts: Vec<NativeArtifact>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct NativeArtifact {
    pub cct: CctK,
    pub artifact: Artifact,
}

impl NativeDocument {
    pub fn from_output(output: &BakeOutput) -> Self {
        Self {
            format: NativeFormat::V1,
            producer: format!("{} {}", env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION")),
            profile_id: output.profile_id.to_string(),
            profile_version: output.profile_version,
            bake: output.bake,
            insertion: output.insertion.clone(),
            downstream: output.downstream,
            interpolation: output.interpolation.map(InterpolationName),
            precision: output.precision.map(PrecisionName),
            artifacts: output
                .artifacts
                .iter()
                .map(|entry| NativeArtifact {
                    cct: entry.cct,
                    artifact: entry.artifact.clone(),
                })
                .collect(),
        }
    }

    /// # Errors
    /// [`ExportError::Profile`] for an unparsable profile id.
    pub fn into_output(self) -> Result<BakeOutput> {
        let profile_id = InterfaceId::try_from(self.profile_id.as_str())
            .map_err(|_| ExportError::Profile("invalid profile id"))?;
        Ok(BakeOutput {
            profile_id,
            profile_version: self.profile_version,
            bake: self.bake,
            insertion: self.insertion,
            downstream: self.downstream,
            interpolation: self.interpolation.map(|name| name.0),
            precision: self.precision.map(|name| name.0),
            artifacts: self
                .artifacts
                .into_iter()
                .map(|entry| crate::bake::ArtifactAtCct {
                    cct: entry.cct,
                    artifact: entry.artifact,
                })
                .collect(),
        })
    }
}

pub fn write_native_json(output: &BakeOutput) -> Result<String> {
    Ok(serde_json::to_string(&NativeDocument::from_output(output))?)
}

/// # Errors
/// [`ExportError::Json`] for malformed JSON, plus the conversion errors of
/// [`NativeDocument::into_output`].
pub fn read_native_json(text: &str) -> Result<BakeOutput> {
    serde_json::from_str::<NativeDocument>(text)?.into_output()
}
