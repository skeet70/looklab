// SPDX-License-Identifier: AGPL-3.0-or-later
//! Errors of the input-transform, CCT and chart-anchor code.

use looklab_color::dng::DngError;

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum BodyError {
    #[error("delta parameter {index} is {value}, outside [-{max}, {max}]")]
    DeltaOutOfRange { index: usize, value: f64, max: f64 },
    #[error("{what} is not finite")]
    NonFinite { what: &'static str },
    #[error("body id is empty")]
    EmptyBodyId,
    #[error("spectral sensitivity needs at least {min} samples, got {got}")]
    SsfTooFewSamples { min: usize, got: usize },
    #[error("spectral sensitivity wavelength is not ascending at row {row}")]
    SsfNotAscending { row: usize },
    #[error("spectral sensitivity row {row} has {got} columns, expected 4")]
    SsfColumnCount { row: usize, got: usize },
    #[error("spectral sensitivity header must be wavelength,r,g,b")]
    SsfHeader,
    #[error("spectral sensitivity row {row} is missing a value")]
    SsfValue { row: usize },
    #[error("spectral sensitivity row {row} is negative: {value}")]
    SsfNegative { row: usize, value: f64 },
    #[error("spectral sensitivity white response is not positive")]
    SsfWhiteResponse,
    #[error("spectral training data is malformed")]
    SpectralData,
    #[error("spectral training data column has {got} values, expected {expected}")]
    SpectralColumn { expected: usize, got: usize },
    #[error("dng: {0}")]
    Dng(#[from] DngError),
    #[error("calibration illuminant {code} has no standard white")]
    UnsupportedIlluminant { code: u16 },
    #[error("the profile has no usable colour matrices")]
    NoMatrices,
    #[error("white-balance multipliers must be finite and positive")]
    InvalidWbMultipliers,
    #[error("cct {kelvin} K is outside [{min}, {max}]")]
    CctOutOfRange { kelvin: f64, min: f64, max: f64 },
    #[error("chart patch {id} has no reference value")]
    MissingChartReference { id: u16 },
    #[error("chart session has no patches")]
    EmptyChart,
    #[error("chart weight must be finite and non-negative")]
    InvalidWeight,
    #[error("no endorsed chart session was supplied")]
    NoEndorsedSessions,
    #[error("anchor option `{what}` is invalid")]
    InvalidAnchorOptions { what: &'static str },
    #[error("matrix is singular in {what}")]
    Singular { what: &'static str },
}

pub type Result<T, E = BodyError> = core::result::Result<T, E>;
