// SPDX-License-Identifier: AGPL-3.0-or-later
//! Body input transforms: spectral and DNG priors, CCT estimation from
//! white-balance multipliers, and chart anchoring (docs/02 §2).
#![allow(clippy::needless_range_loop)]

pub mod anchor;
pub mod cct;
pub mod chart;
pub mod delta;
pub mod spectral;
pub mod transform;

mod error;
mod linalg;

pub use anchor::{
    AnchorOptions, AnchorSolution, AnchorTerms, ChartSession, anchor_from_charts,
    chart_anchor_terms,
};
pub use cct::{
    CCT_A, CCT_D65, CalibrationIlluminant, CameraProfile, Cct, camera_to_xyz_at_illuminant,
    camera_to_xyz_at_wb, estimate_cct, neutral_from_wb,
};
pub use chart::{ChartError, ChartReference, chart_error};
pub use delta::{Cov6, Delta6, PARAMETERS};
pub use error::{BodyError, Result};
pub use spectral::{
    SpectralData, SpectralSensitivity, SsfIlluminant, camera_responses, ssf_matrix,
    synthetic_sensitivity, training_data,
};
pub use transform::{
    A0Source, BodyId, BodyPrior, CHART_PRIOR_SD, DB_PRIOR_SD, DNG_PRIOR_SD, InputTransform,
    SSF_PRIOR_SD, TransformVersion, prior_covariance, prior_from_database, prior_from_dng,
    prior_from_ssf, select_prior,
};
