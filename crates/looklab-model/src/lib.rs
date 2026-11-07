// SPDX-License-Identifier: AGPL-3.0-or-later
//! Look representation, evaluation, look file format and probe sets.

pub mod baseline;
pub mod basis;
pub mod clip;
pub mod dependency;
mod deriv;
pub mod error;
pub mod file;
pub mod look;
pub mod probes;
pub mod realization;
pub mod solve;
pub mod tone;

pub use clip::{ClipFunction, ClipSettings, output_encode_clip, soft_clip};
pub use error::{ModelError, Result};
pub use file::{
    BaseField, BaseReference, BaseStatus, BinRef, CameraRendering, ChromaReference,
    DeclaredEncoding, Density, DynamicRange, FormatId, Gamut, LayersRecord, LicenseRecord,
    LogTransfer, LookDocument, LookFile, LookIdentity, LookKind, LookPin, LutData, LutDefinition,
    LutEmulation, LutRecord, MAX_LUT_SIDE, MatchRefusal, NEUTRAL_BASELINE_SLUG,
    NEUTRAL_BASELINE_VERSION, OutputEncoding, ProbeCovariance, ProbeScalars, ProbeSetVersion,
    Renderer, UncertaintyData, WbPolicy, emulation_matches_camera_rendering, lut_value_count,
};
pub use look::{
    ChromaPaths, CoefTerm, ColorLayer, GrlComponent, GrlWeights, LayerValues, Look, LookInput,
    NeutralChroma, ParamLayout, Residual, ResidualWeights, Tint, barrier, barrier_derivatives,
    domain_margin, internal_chroma_from_ucs, internal_chroma_to_xyz, j_prime_xyz_gradient,
    neutral_scale, prediction_margin, ucs_chroma_from_internal, ucs_reference_jacobian,
};
pub use probes::{Probe, ProbeGroup, ProbeId, ProbeSet, ProbeSetFile, V1_PROBE_COUNT};
pub use solve::{
    J_PRIME_MAX, SolveError, neutral_j_prime, neutral_j_prime_max, solve_y_ref, try_solve_y_ref,
};
pub use tone::Tone;
