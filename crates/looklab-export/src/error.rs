// SPDX-License-Identifier: AGPL-3.0-or-later
//! Errors from baking, simulation and the writers.

use looklab_model::ModelError;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ExportError {
    #[error("the profile accepts no artifact kind this baker produces")]
    NoAcceptedArtifact,
    #[error("no artifact fits limits.max_bytes = {limit}; the smallest needs {needed}")]
    ByteLimit { needed: u64, limit: u64 },
    #[error("insertion point {point} does not take {detail}")]
    InvalidInsertion {
        point: &'static str,
        detail: &'static str,
    },
    #[error("insertion encoding `custom_1d` needs a custom curve")]
    MissingCustomCurve,
    #[error("the body transform is undefined at {cct} K")]
    TransformUndefined { cct: f64 },
    #[error("the body matrix at {cct} K is singular")]
    SingularMatrix { cct: f64 },
    #[error("look clip settings are outside the v1 range")]
    InvalidClip,
    #[error("invalid artifact: {0}")]
    InvalidArtifact(&'static str),
    #[error("invalid CCT {0}")]
    InvalidCct(f64),
    #[error("unsupported interface profile: {0}")]
    Profile(&'static str),
    #[error("unsupported: {0}")]
    Unsupported(&'static str),
    #[error("cube: {0}")]
    Cube(String),
    #[error("png: {0}")]
    Png(String),
    #[error("probe mask has {found} entries for {expected} probes")]
    ProbeMask { found: usize, expected: usize },
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("model: {0}")]
    Model(#[from] ModelError),
}

pub type Result<T, E = ExportError> = core::result::Result<T, E>;
