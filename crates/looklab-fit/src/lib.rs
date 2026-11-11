// SPDX-License-Identifier: AGPL-3.0-or-later
//! Single-body Look fits: staged Gauss-Newton/Levenberg-Marquardt on the tempered Student-t loss, image-clustered uncertainty, outlier scoring. See docs/02 §4.
#![allow(clippy::needless_range_loop)]

mod basis;
pub mod bootstrap;
mod cam;
mod density;
pub mod epoch;
mod fit;
pub mod gauge;
pub mod hierarchical;
mod layout;
pub mod model_error;
mod outlier;
mod predict;
mod problem;
mod solve;
mod trust_region;
mod types;
mod uncertainty;

#[cfg(feature = "synth")]
pub mod synth;

#[cfg(feature = "synth")]
pub use fit::{
    FixedLinearizationBoxSource, FixedLinearizationCandidate, FixedLinearizationDisposition,
    FixedLinearizationInput, FixedLinearizationOperations, FixedLinearizationOrdinary,
    FixedLinearizationPartial, FixedLinearizationReference, FixedLinearizationReport,
    FixedLinearizationStatus, FixedLinearizationTrial, FixedLinearizationWinner,
    PreconditionerChoice, PreconditionerGroupChoice, diagnose_fixed_linearization,
};

#[cfg(feature = "synth")]
pub use fit::fallback_ab::{
    FallbackArm, FallbackArmReport, FallbackArmStatus, FallbackComparison,
    FallbackComparisonReport, diagnose_fallback_ab,
};

#[cfg(feature = "synth")]
pub use trust_region::{
    TrustRegionIterationProgress, TrustRegionRefresh, TrustRegionRefreshReason,
    TrustRegionResidualProgress, TrustRegionTrace,
};

pub use bootstrap::{
    BOOTSTRAP_RESAMPLES, BootstrapOptions, Cov3, PublishedUncertainty, UncertaintyMethod,
    published_uncertainty,
};
pub use density::sample_density;
pub use epoch::{
    ImageProjection, PROJECTION_DIM, SampleProjection, SplitDecision, SplitOptions, SplitSample,
    detect_split, detect_split_with_effect, predicted_effect, project_images,
};
pub use gauge::{
    ComponentGauge, ExposureGauge, GaugeChange, WbGaugeChange, WbModeClass, merge_gauges,
};
pub use hierarchical::{
    BodyData, BodyGroup, BodyParameters, BodyPin, BodyResume, BodyWarmStart, FIRMWARE_PARAMETERS,
    FamilyData, FamilyFirmwareId, FamilyMode, FamilyParameters, FamilyResume, FirmwareId,
    GaugeBinding, GaugeScope, HierDiagnostics, HierFingerprint, HierImage, HierInput,
    HierLimitation, HierOptions, HierState, HierStateGeneration, HierarchicalFit, LookProvenance,
    PinnedFamily, RealizationData, RealizationFit, RealizationParameters, RealizationResume,
    RealizationWarmStart, firmware_images, firmware_look, firmware_probe_basis, fit_hierarchical,
    fit_realizations,
};
pub use looklab_color::{Mat3, RgbSpace, Xyz};
pub use looklab_model::realization::{
    ControlResponseModel, ControlValue, ResponseParamId, Settings,
};
pub use looklab_model::{Look, LookInput, Probe, ProbeId, ProbeSet};
pub use model_error::{
    MODEL_C_BINS, MODEL_HUE_BINS, MODEL_S_BINS, ModelErrorField, apply_model_error, cell,
    estimate_model_error, post_index, region,
};
pub use predict::post_clip;
pub use trust_region::TrustRadius;
pub use types::{
    CertificateOrigin, CertificateTermination, CertificateWork, ContributorId, ControlResponseFit,
    Density, Diagnostics, ExcludeReason, ExcludedImage, ExposureModeId, ExposureOffset, FitError,
    FitImage, FitOptions, FitResult, FitStage, FitState, FitStateGeneration, ImageFitStats,
    ImageIntent, ImageNuisances, ImageScores, ImageWeights, Nats, ObjectiveNats, OutlierReason,
    PLATEAU_SAMPLES, PlateauHistory, PlateauObjective, PreviousSupport, ProbeEstimate,
    ProbeUncertainty, Realization, Result, SampleEvaluations, StageImageWeights, StageLambdas,
    StationarityBounds, StationarityCertificate, StationarityReport, Verdict,
};
pub use uncertainty::small_cluster_factor;

/// Fit one body's single Look at the reference control setting.
pub fn fit(images: &[FitImage], options: &FitOptions) -> Result<FitResult> {
    fit::run(images, options, None)
}

/// Fit with the previous published support state, for hysteresis.
pub fn fit_with_history(
    images: &[FitImage],
    options: &FitOptions,
    previous: Option<&PreviousSupport>,
) -> Result<FitResult> {
    fit::run_with_history(images, options, None, Vec::new(), previous)
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

/// Joint θ/D fit for per-image scores, without publication covariance.
pub fn score_controlled(
    images: &[FitImage],
    schema: &looklab_schema::ControlSchema,
    epoch: &looklab_schema::Epoch,
    base_sim: &looklab_schema::SimKey,
    options: &FitOptions,
) -> Result<ImageScores> {
    let model = ControlResponseModel::zeroed(schema, epoch.id, base_sim, 1)?;
    let (kept, excluded) = fit::filter_spatial(images, schema, epoch.id)?;
    if !kept.iter().any(|image| image.intent == ImageIntent::Native) {
        return Err(FitError::NoNativeImages);
    }
    let result = fit::run_scoring(&kept, options, model, excluded)?;
    Ok(ImageScores {
        image_stats: result.image_stats,
        excluded: result.excluded,
        diagnostics: result.diagnostics,
    })
}

pub fn fit_with_history_resumable(
    images: &[FitImage],
    options: &FitOptions,
    previous: Option<&PreviousSupport>,
    state: Option<&FitState>,
) -> Result<(FitResult, Option<FitState>)> {
    fit::run_with_history_resumable(images, options, None, Vec::new(), previous, state)
}

/// Joint θ/D fit over all native settings on one body, with previous probe support.
pub fn fit_controlled_with_history(
    images: &[FitImage],
    schema: &looklab_schema::ControlSchema,
    epoch: u32,
    base_sim: &looklab_schema::SimKey,
    options: &FitOptions,
    previous: Option<&PreviousSupport>,
) -> Result<FitResult> {
    let model = ControlResponseModel::zeroed(schema, epoch, base_sim, 1)?;
    let (kept, excluded) = fit::filter_spatial(images, schema, epoch)?;
    if !kept.iter().any(|image| image.intent == ImageIntent::Native) {
        return Err(FitError::NoNativeImages);
    }
    fit::run_with_history(&kept, options, Some(model), excluded, previous)
}

pub fn fit_controlled_with_history_resumable(
    images: &[FitImage],
    schema: &looklab_schema::ControlSchema,
    epoch: u32,
    base_sim: &looklab_schema::SimKey,
    options: &FitOptions,
    previous: Option<&PreviousSupport>,
    state: Option<&FitState>,
) -> Result<(FitResult, Option<FitState>)> {
    let model = ControlResponseModel::zeroed(schema, epoch, base_sim, 1)?;
    let (kept, excluded) = fit::filter_spatial(images, schema, epoch)?;
    if !kept.iter().any(|image| image.intent == ImageIntent::Native) {
        return Err(FitError::NoNativeImages);
    }
    fit::run_with_history_resumable(&kept, options, Some(model), excluded, previous, state)
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
