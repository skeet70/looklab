// SPDX-License-Identifier: AGPL-3.0-or-later
use looklab_color::Mat3;
use looklab_color::ucs::Ucs;
use looklab_model::{Look, ModelError, ProbeId};
use looklab_samples::{Error as SamplesError, SampleSet};

#[derive(Debug, thiserror::Error)]
pub enum FitError {
    #[error("no images were supplied")]
    NoImages,
    #[error("image {image} has no samples")]
    EmptyImage { image: usize },
    #[error("no samples left after subsampling")]
    NoSamples,
    #[error("exposure mode {mode} is not present in any image")]
    UnknownExposureMode { mode: u32 },
    #[error("samples: {0}")]
    Samples(#[from] SamplesError),
    #[error("model: {0}")]
    Model(#[from] ModelError),
    #[error("regularization matrix is singular")]
    SingularRegularization,
}

pub type Result<T, E = FitError> = core::result::Result<T, E>;

/// Body exposure mode; mode 0 is the default mode and carries the `ξ` gauge.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ExposureModeId(pub u32);

/// Effective-contributor id; `σ_v` is fitted per contributor.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ContributorId(pub u32);

#[derive(Clone, Debug)]
pub struct FitImage {
    pub samples: SampleSet,
    pub transform: Mat3,
    pub exposure_hint: f64,
    pub trust_weight: f64,
    pub exposure_mode: ExposureModeId,
    pub contributor: ContributorId,
}

impl FitImage {
    pub fn new(samples: SampleSet, transform: Mat3) -> Self {
        Self {
            samples,
            transform,
            exposure_hint: 0.0,
            trust_weight: 1.0,
            exposure_mode: ExposureModeId::default(),
            contributor: ContributorId::default(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FitOptions {
    pub seed: u64,
    pub n_eff: f64,
    pub nu: f64,
    pub max_samples_per_image: usize,
    /// Normal-equation rows per sweep; caps the quadrature cost, not the likelihood.
    pub hessian_samples: usize,
    pub max_sweeps: usize,
    pub sigma_prior: (f64, f64),
    pub clip_prior: (f64, f64),
    pub exposure_prior_sd: f64,
    pub wb_prior_sd: f64,
    pub exposure_mode_prior_sd: f64,
    pub density_floor_fraction: f64,
    pub support_sigma: f64,
    pub run_gcv: bool,
    pub fit_residual: bool,
}

impl Default for FitOptions {
    fn default() -> Self {
        Self {
            seed: 0,
            n_eff: 500.0,
            nu: 4.0,
            max_samples_per_image: 10_000,
            hessian_samples: 8_000,
            max_sweeps: 24,
            sigma_prior: (4.0, 4.5),
            clip_prior: (0.03, 0.02),
            exposure_prior_sd: 0.3,
            wb_prior_sd: 0.02,
            exposure_mode_prior_sd: 0.3,
            density_floor_fraction: 0.02,
            support_sigma: 1.5,
            run_gcv: true,
            fit_residual: true,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ImageNuisances {
    /// Log2 exposure offset relative to the body's mode offset.
    pub exposure: f64,
    pub wb_log_r: f64,
    pub wb_log_b: f64,
    pub exposure_mode: ExposureModeId,
    pub contributor: ContributorId,
    pub weight: f64,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ExposureOffset {
    pub mode: ExposureModeId,
    pub offset: f64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Realization {
    pub clip_k: f64,
    pub exposure_offsets: Vec<ExposureOffset>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct StageLambdas {
    pub tone: f64,
    pub tint: f64,
    pub grl: f64,
    pub residual: f64,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Verdict {
    Accept,
    DownWeight,
    Quarantine,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OutlierReason {
    ExposureLike,
    HueUniformCast,
    ShoulderOnly,
    ContentCorrelated,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ImageFitStats {
    pub rms_de: f64,
    pub z: f64,
    pub verdict: Verdict,
    pub reason: Option<OutlierReason>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ProbeEstimate {
    pub id_hint: usize,
    pub ucs: Ucs,
    pub covariance: [[f64; 3]; 3],
    pub sigma_de: f64,
    pub density: f64,
    pub supported: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ProbeUncertainty {
    pub probes: Vec<ProbeEstimate>,
    pub ids: Vec<ProbeId>,
    pub supported_fraction: f64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Density {
    pub s_bins: usize,
    pub hue_bins: usize,
    pub c_bins: usize,
    pub counts: Vec<f64>,
    pub total: f64,
    pub floor: f64,
}

impl Density {
    /// Weighted sample density in `(s, h_in, √C_in)`, trilinearly interpolated (docs/02 §3.6).
    pub fn at(&self, s: f64, hue: f64, c: f64) -> f64 {
        crate::density::lookup(self, s, hue, c)
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Diagnostics {
    pub sweeps: usize,
    pub evaluations: usize,
    pub final_cost: f64,
    pub sigma_v: Vec<(ContributorId, f64)>,
    pub gcv: StageLambdas,
    pub hessian_samples: usize,
    pub objective_samples: usize,
    pub quarantined: usize,
    pub down_weighted: usize,
}

#[derive(Clone, Debug, PartialEq)]
pub struct FitResult {
    pub look: Look,
    pub images: Vec<ImageNuisances>,
    pub realization: Realization,
    pub lambdas: StageLambdas,
    pub probes: ProbeUncertainty,
    pub density: Density,
    pub image_stats: Vec<ImageFitStats>,
    pub diagnostics: Diagnostics,
}
