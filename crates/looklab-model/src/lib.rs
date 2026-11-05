// SPDX-License-Identifier: AGPL-3.0-or-later
//! Look representation, evaluation, look file format and probe sets.

pub mod basis;
pub mod clip;
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
    BinRef, ChromaReference, Density, FormatId, LayersRecord, LookDocument, LookFile, LookIdentity,
    LookKind, ProbeCovariance, ProbeScalars, ProbeSetVersion, UncertaintyData, WbPolicy,
};
pub use look::{
    CoefTerm, ColorLayer, GrlComponent, GrlWeights, LayerValues, Look, LookInput, ParamLayout,
    Residual, ResidualWeights, Tint,
};
pub use probes::{Probe, ProbeGroup, ProbeId, ProbeSet, ProbeSetFile, V1_PROBE_COUNT};
pub use solve::{
    J_PRIME_MAX, SolveError, neutral_j_prime, neutral_j_prime_max, solve_y_ref, try_solve_y_ref,
};
pub use tone::Tone;
