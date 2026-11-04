// SPDX-License-Identifier: AGPL-3.0-or-later
//! Firmware interface profiles, LUT baking, pipeline simulation and writers.
//!
//! The bake realizes docs/04 §7: `F = Downstream⁻¹ ∘ OutputEncode ∘ CAM16UCS⁻¹ ∘
//! L ∘ ToSceneXYZ_{P,B}(T) ∘ Decode_P`. An artifact value is linear RGB in the
//! downstream output primaries.

pub mod artifact;
pub mod bake;
pub mod camera;
pub mod cct;
pub mod downstream;
pub mod error;
pub mod native;
pub mod quantize;
pub mod sim;
pub mod writer;

mod serde_names;

pub use artifact::{Artifact, Curve1d, Lut3d, Matrix3x3, Shaper1d};
pub use bake::{
    ArtifactAtCct, BakeIlluminant, BakeOutput, Baker, InsertionSpec, ShaperParams, baked_look,
};
pub use camera::{DngInputTransform, InputTransform};
pub use cct::{BAKE_CCTS, CctK};
pub use downstream::{
    DownstreamModel, OutputSpace, TargetOutput, Transfer, in_gamut, perceptual_clip, ucs_to_xyz,
};
pub use error::{ExportError, Result};
pub use looklab_schema::{Format, InsertionPoint, Interpolation, Precision};
pub use native::{NativeDocument, read_native_json, write_native_json};
pub use sim::{CctReport, DeltaStats, Support, ValidationReport, validate};
pub use writer::{
    Cube1dFile, CubeFile, hald_level, read_cube, read_cube_1d, read_haldclut, write_cube,
    write_cube_1d, write_haldclut, written_bytes,
};
