// SPDX-License-Identifier: AGPL-3.0-or-later

use thiserror::Error;

/// Errors from the Look representation, its file format and the probe set.
#[derive(Debug, Error)]
pub enum ModelError {
    #[error("tone spline coefficients must be non-negative")]
    NegativeToneBeta,
    #[error("knot vector does not match the v1 layout")]
    Knots,
    #[error("coefficient array {name:?} has length {found}, expected {expected}")]
    CoefficientLength {
        name: String,
        found: usize,
        expected: usize,
    },
    #[error("missing coefficient array {0:?}")]
    MissingCoefficient(String),
    #[error("unknown coefficient array {0:?}")]
    UnknownCoefficient(String),
    #[error("unsupported format {0:?}")]
    Format(String),
    #[error("invalid look file: {0}")]
    Invalid(String),
    #[error("viewing conditions differ from the reference conditions")]
    ViewingConditions,
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("yaml: {0}")]
    Yaml(#[from] serde_norway::Error),
    #[error("zip: {0}")]
    Zip(#[from] zip::result::ZipError),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("probe data: {0}")]
    ProbeData(String),
}

pub type Result<T, E = ModelError> = core::result::Result<T, E>;
