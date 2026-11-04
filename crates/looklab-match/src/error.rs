// SPDX-License-Identifier: AGPL-3.0-or-later

use looklab_model::ModelError;
use looklab_samples::Error as SamplesError;
use looklab_schema::ValidationError;

#[derive(Debug, thiserror::Error)]
pub enum MatchError {
    #[error("model: {0}")]
    Model(#[from] ModelError),
    #[error("samples: {0}")]
    Samples(#[from] SamplesError),
    #[error("schema: {0}")]
    Schema(String),
    #[error("invalid control schema: {0:?}")]
    ControlSchema(Vec<ValidationError>),
    #[error("epoch {0} is not in the schema")]
    UnknownEpoch(u32),
    #[error("control {0} is not in the epoch")]
    UnknownControl(String),
    #[error("no native sims in the epoch")]
    NoSims,
    #[error("no supported probes")]
    NoSupportedProbes,
    #[error("no control-response model for sim {0}")]
    MissingModel(String),
    #[error("control-response model {0} has {1} values for control {2}")]
    ResponseLength(String, usize, String),
    #[error("control-response model {0} is for epoch {1}, not {2}")]
    WrongEpoch(String, u32, u32),
    #[error("control-response model {0} has base coefficients outside the Look layout")]
    CoefficientIndex(String),
    #[error("control-response transform is singular")]
    SingularTransform,
    #[error("permutation target is unreachable")]
    UnreachableTarget,
    #[error("permalink: {0}")]
    Permalink(String),
    #[error("observed render has no samples")]
    EmptyObserved,
    #[error("linear system is singular")]
    SingularSystem,
    #[error("dimension mismatch: {0}")]
    Dimension(String),
}

pub type Result<T, E = MatchError> = core::result::Result<T, E>;
