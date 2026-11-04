// SPDX-License-Identifier: AGPL-3.0-or-later
//! The firmware interface profile file.

use serde::Deserialize;

use crate::control::Range;
use crate::error::{ErrorKind::InvalidValue, ValidationError};
use crate::ids::{BodyId, InterfaceId};
use crate::rational::Rational;

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InterfaceProfile {
    pub id: InterfaceId,
    pub name: String,
    pub version: u32,
    #[serde(default)]
    pub bodies: Vec<BodyId>,
    pub insertion: Insertion,
    pub accepts: Vec<AcceptedArtifact>,
    pub downstream: Vec<Downstream>,
    pub formats: Vec<Format>,
    pub limits: Limits,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Insertion {
    pub point: InsertionPoint,
    pub encoding: Encoding,
    pub illuminant_handling: IlluminantHandling,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Encoding {
    pub transfer: TransferFunction,
    #[serde(default)]
    pub shaper: Option<Shaper>,
    pub primaries: Primaries,
    pub range: Range,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Shaper {
    pub min_ev: Rational,
    pub max_ev: Rational,
    pub mid_gray: Rational,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize)]
pub enum InsertionPoint {
    #[serde(rename = "camera_rgb_wb_linear")]
    CameraRgbWbLinear,
    #[serde(rename = "scene_linear_rec2020")]
    SceneLinearRec2020,
    #[serde(rename = "scene_linear_xyz_d65")]
    SceneLinearXyzD65,
    #[serde(rename = "display_encoded_post_tone")]
    DisplayEncodedPostTone,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize)]
pub enum TransferFunction {
    #[serde(rename = "linear")]
    Linear,
    #[serde(rename = "log2_shaper")]
    Log2Shaper,
    #[serde(rename = "srgb")]
    Srgb,
    #[serde(rename = "custom_1d")]
    Custom1d,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize)]
pub enum Primaries {
    #[serde(rename = "camera_native")]
    CameraNative,
    #[serde(rename = "rec2020")]
    Rec2020,
    #[serde(rename = "xyz_d65")]
    XyzD65,
    #[serde(rename = "srgb")]
    Srgb,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize)]
pub enum IlluminantHandling {
    #[serde(rename = "single")]
    Single,
    #[serde(rename = "dual_interpolated")]
    DualInterpolated,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(tag = "type")]
pub enum AcceptedArtifact {
    #[serde(rename = "lut3d")]
    Lut3d(Lut3d),
    #[serde(rename = "matrix3x3")]
    Matrix3x3(Matrix3x3),
    #[serde(rename = "curve1d")]
    Curve1d(Curve1d),
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Lut3d {
    pub sizes: Vec<u32>,
    pub interpolation: Interpolation,
    pub precision: Precision,
    pub with_shaper: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Matrix3x3 {}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Curve1d {
    pub points_max: u32,
    pub per_channel: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize)]
pub enum Interpolation {
    #[serde(rename = "trilinear")]
    Trilinear,
    #[serde(rename = "tetrahedral")]
    Tetrahedral,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize)]
pub enum Precision {
    #[serde(rename = "f32")]
    F32,
    #[serde(rename = "f16")]
    F16,
    #[serde(rename = "u16")]
    U16,
    #[serde(rename = "u12")]
    U12,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Downstream {
    OutputTransform(OutputTransform),
    GamutClip(GamutClip),
}

impl<'de> Deserialize<'de> for Downstream {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let repr = DownstreamRepr::deserialize(deserializer)?;
        Ok(match repr {
            DownstreamRepr::OutputTransform(entry) => Self::OutputTransform(entry.output_transform),
            DownstreamRepr::GamutClip(entry) => Self::GamutClip(entry.gamut_clip),
        })
    }
}

#[derive(Deserialize)]
#[serde(untagged)]
enum DownstreamRepr {
    OutputTransform(OutputTransformEntry),
    GamutClip(GamutClipEntry),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct OutputTransformEntry {
    output_transform: OutputTransform,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GamutClipEntry {
    gamut_clip: GamutClip,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OutputTransform {
    pub primaries: Primaries,
    pub transfer: TransferFunction,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize)]
pub enum GamutClip {
    #[serde(rename = "hard")]
    Hard,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize)]
pub enum Format {
    #[serde(rename = "cube")]
    Cube,
    #[serde(rename = "native_json")]
    NativeJson,
    #[serde(rename = "haldclut")]
    Haldclut,
    #[serde(rename = "binary_f16")]
    BinaryF16,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Limits {
    pub max_bytes: u64,
}

pub fn validate_interface_profile(yaml: &str) -> Result<InterfaceProfile, Vec<ValidationError>> {
    crate::yaml::check(yaml)?;
    let profile: InterfaceProfile =
        serde_norway::from_str(yaml).map_err(ValidationError::syntax)?;
    let mut errors = Vec::new();
    validate_accepts(&profile.accepts, &mut errors);
    if errors.is_empty() {
        Ok(profile)
    } else {
        Err(errors)
    }
}

/// Largest grid side the bake and the artifact readers accept.
pub const MAX_LUT_SIDE: u32 = 256;

/// Largest 3D LUT the bake allocates across every accepted grid side.
pub const MAX_LUT_NODES: u64 = 1 << 22;

const MAX_LUT_SIDES: usize = 4;
const MAX_ACCEPTED_ARTIFACTS: usize = 8;
const MAX_CURVE_POINTS: u32 = 65_536;

fn validate_accepts(accepts: &[AcceptedArtifact], errors: &mut Vec<ValidationError>) {
    if accepts.len() > MAX_ACCEPTED_ARTIFACTS {
        errors.push(ValidationError::new(
            "accepts",
            InvalidValue {
                expected: "at most 8 accepted artifacts",
            },
        ));
    }
    let mut nodes = 0u64;
    for (index, accepted) in accepts.iter().enumerate() {
        let path = format!("accepts[{index}]");
        match accepted {
            AcceptedArtifact::Lut3d(lut) => {
                if lut.sizes.is_empty() {
                    errors.push(ValidationError::new(
                        format!("{path}.sizes"),
                        InvalidValue {
                            expected: "at least one grid side",
                        },
                    ));
                }
                if lut.sizes.len() > MAX_LUT_SIDES {
                    errors.push(ValidationError::new(
                        format!("{path}.sizes"),
                        InvalidValue {
                            expected: "at most 4 grid sides",
                        },
                    ));
                }
                for (side_index, side) in lut.sizes.iter().enumerate() {
                    if (2..=MAX_LUT_SIDE).contains(side) {
                        nodes += u64::from(*side).pow(3);
                    } else {
                        errors.push(ValidationError::new(
                            format!("{path}.sizes[{side_index}]"),
                            InvalidValue {
                                expected: "a grid side from 2 to 256",
                            },
                        ));
                    }
                }
                if nodes > MAX_LUT_NODES {
                    errors.push(ValidationError::new(
                        format!("{path}.sizes"),
                        InvalidValue {
                            expected: "3D LUTs under 4194304 nodes in total",
                        },
                    ));
                }
            }
            AcceptedArtifact::Curve1d(curve) => {
                if !(2..=MAX_CURVE_POINTS).contains(&curve.points_max) {
                    errors.push(ValidationError::new(
                        format!("{path}.points_max"),
                        InvalidValue {
                            expected: "between 2 and 65536 points",
                        },
                    ));
                }
            }
            AcceptedArtifact::Matrix3x3(_) => {}
        }
    }
}
