// SPDX-License-Identifier: AGPL-3.0-or-later
//! Single-body Look fits: staged Gauss-Newton/Levenberg-Marquardt on the tempered Student-t loss, image-clustered uncertainty, outlier scoring. See docs/02 §4.
#![allow(clippy::needless_range_loop)]

mod basis;
mod cam;
mod density;
mod fit;
mod layout;
mod outlier;
mod predict;
mod problem;
mod solve;
mod types;
mod uncertainty;

#[cfg(feature = "synth")]
pub mod synth;

pub use looklab_color::{Mat3, RgbSpace, Xyz};
pub use looklab_model::{Look, LookInput, Probe, ProbeId, ProbeSet};
pub use types::{
    ContributorId, Density, Diagnostics, ExposureModeId, ExposureOffset, FitError, FitImage,
    FitOptions, FitResult, ImageFitStats, ImageNuisances, OutlierReason, ProbeEstimate,
    ProbeUncertainty, Realization, Result, StageLambdas, Verdict,
};

/// Fit one body's single Look from its images.
pub fn fit(images: &[FitImage], options: &FitOptions) -> Result<FitResult> {
    fit::run(images, options)
}
