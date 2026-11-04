// SPDX-License-Identifier: AGPL-3.0-or-later
//! Single-body Look fits: staged Gauss-Newton/Levenberg-Marquardt on the tempered Student-t loss, image-clustered uncertainty, outlier scoring. See docs/02 §4.
#![allow(clippy::needless_range_loop)]

mod basis;
mod cam;
mod density;
pub mod epoch;
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

pub use epoch::{SplitDecision, SplitOptions, SplitSample, detect_split};
pub use looklab_color::{Mat3, RgbSpace, Xyz};
pub use looklab_model::realization::{
    ControlResponseModel, ControlValue, ResponseParamId, Settings,
};
pub use looklab_model::{Look, LookInput, Probe, ProbeId, ProbeSet};
pub use types::{
    ContributorId, ControlResponseFit, Density, Diagnostics, ExcludeReason, ExcludedImage,
    ExposureModeId, ExposureOffset, FitError, FitImage, FitOptions, FitResult, ImageFitStats,
    ImageIntent, ImageNuisances, OutlierReason, ProbeEstimate, ProbeUncertainty, Realization,
    Result, StageLambdas, Verdict,
};

/// Fit one body's single Look at the reference control setting.
pub fn fit(images: &[FitImage], options: &FitOptions) -> Result<FitResult> {
    fit::run(images, options, None)
}

/// Joint θ/D fit over all native settings on one body.
pub fn fit_controlled(
    images: &[FitImage],
    schema: &looklab_schema::ControlSchema,
    epoch: u32,
    base_sim: &looklab_schema::SimKey,
    options: &FitOptions,
) -> Result<FitResult> {
    let model = ControlResponseModel::zeroed(schema, epoch, base_sim, 1)?;
    let (kept, excluded) = fit::filter_spatial(images, schema, epoch)?;
    if !kept.iter().any(|image| image.intent == ImageIntent::Native) {
        return Err(FitError::NoNativeImages);
    }
    fit::run_with(&kept, options, Some(model), excluded)
}

/// Refit the control response with θ_L fixed.
pub fn refit_responses(
    look: &Look,
    images: &[FitImage],
    schema: &looklab_schema::ControlSchema,
    epoch: u32,
    base_sim: &looklab_schema::SimKey,
    model: &ControlResponseModel,
    options: &FitOptions,
) -> Result<ControlResponseFit> {
    let (kept, _excluded) = fit::filter_spatial(images, schema, epoch)?;
    if kept.is_empty() {
        return Err(FitError::NoSamples);
    }
    fit::run_responses(look, &kept, schema, epoch, base_sim, model, options)
}
