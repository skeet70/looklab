// SPDX-License-Identifier: AGPL-3.0-or-later
//! Look representation, evaluation, look file format and probe sets.

pub mod basis;
pub mod clip;
mod deriv;
pub mod error;
pub mod file;
pub mod look;
pub mod probes;
pub mod solve;
pub mod tone;

pub use clip::{ClipFunction, ClipSettings, output_encode_clip, soft_clip};
pub use error::{ModelError, Result};
pub use file::{
    BinRef, ChromaReference, FormatId, LayersRecord, LookDocument, LookFile, LookIdentity,
    LookKind, ProbeSetVersion, WbPolicy,
};
pub use look::{
    ColorLayer, GrlComponent, GrlWeights, LayerValues, Look, LookInput, ParamLayout, Residual,
    ResidualWeights, Tint,
};
pub use probes::{Probe, ProbeGroup, ProbeId, ProbeSet, ProbeSetFile};
pub use solve::{neutral_j_prime, solve_y_ref};
pub use tone::Tone;
