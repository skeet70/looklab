// SPDX-License-Identifier: AGPL-3.0-or-later
use looklab_color::ucs::Ucs;
use looklab_color::{Mat3, RgbSpace};
use looklab_model::realization::{ControlResponseModel, ResponseParamId, Settings};
use looklab_model::{Look, ModelError, ProbeId};
use looklab_samples::{Error as SamplesError, SampleSet};
use looklab_schema::ControlKey;

#[derive(Debug, thiserror::Error)]
pub enum FitError {
    #[error("no images were supplied")]
    NoImages,
    #[error("image {image} has no samples")]
    EmptyImage { image: usize },
    #[error("no samples left after subsampling")]
    NoSamples,
    #[error("held-out model error has no evidence")]
    NoModelErrorEvidence,
    #[error("image {image}: sample_trust has {found} entries, expected {expected}")]
    SampleTrustLength {
        image: usize,
        found: usize,
        expected: usize,
    },
    #[error("exposure mode {mode} is not present in any image")]
    UnknownExposureMode { mode: u32 },
    #[error("samples: {0}")]
    Samples(#[from] SamplesError),
    #[error("model: {0}")]
    Model(#[from] ModelError),
    #[error("regularization matrix is singular")]
    SingularRegularization,
    #[error("control schema: {0}")]
    Schema(String),
    #[error("no native images remain after excluding unsupported settings")]
    NoNativeImages,
    #[error("control response: {0}")]
    Realization(#[from] looklab_model::realization::RealizationError),
    #[error("hierarchical fit: {0}")]
    Hierarchy(String),
    #[error("row context: {0}")]
    RowContext(String),
    #[error("firmware identity: {0}")]
    Firmware(String),
    #[error("component gauge: {0}")]
    MissingGauge(String),
}

pub type Result<T, E = FitError> = core::result::Result<T, E>;

/// Body exposure mode; mode 0 is the default mode and carries the `ξ` gauge.
#[derive(
    Clone,
    Copy,
    Debug,
    Default,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    serde::Serialize,
    serde::Deserialize,
)]
pub struct ExposureModeId(pub u32);

/// Effective-contributor id; `σ_v` is fitted per contributor.
#[derive(
    Clone,
    Copy,
    Debug,
    Default,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    serde::Serialize,
    serde::Deserialize,
)]
pub struct ContributorId(pub u32);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FitStage {
    Native,
    Published,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ImageWeights {
    pub trust_weight: f64,
    pub sample_trust: Option<Vec<f32>>,
    pub duplicates: usize,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct StageImageWeights {
    /// Native-stage weights for the body and realization block.
    pub native_body: Option<ImageWeights>,
    /// Native-stage weights for the family Look block.
    pub native_look: Option<ImageWeights>,
    /// Published-stage weights for the body and realization block.
    pub published_body: Option<ImageWeights>,
}

impl StageImageWeights {
    pub fn body(&self, stage: FitStage) -> Option<&ImageWeights> {
        match stage {
            FitStage::Native => self.native_body.as_ref(),
            FitStage::Published => self.published_body.as_ref(),
        }
    }

    pub fn look(&self) -> Option<&ImageWeights> {
        self.native_look.as_ref()
    }
}

#[derive(Clone, Debug)]
pub struct FitImage {
    pub samples: SampleSet,
    pub transform: Mat3,
    pub exposure_hint: f64,
    pub trust_weight: f64,
    /// Per-sample multipliers of `trust_weight` (docs/05 §6 caps are per region).
    pub sample_trust: Option<Vec<f32>>,
    /// Stage-specific weights override `trust_weight`/`sample_trust` when present.
    pub stage_weights: StageImageWeights,
    pub exposure_mode: ExposureModeId,
    pub contributor: ContributorId,
    pub settings: Settings,
    /// Per-control source of `settings`; provenance only, no weight effect in v1.
    pub setting_source: std::collections::BTreeMap<ControlKey, looklab_schema::SettingSource>,
    pub intent: ImageIntent,
}

impl FitImage {
    pub fn new(samples: SampleSet, transform: Mat3) -> Self {
        Self {
            samples,
            transform,
            exposure_hint: 0.0,
            trust_weight: 1.0,
            sample_trust: None,
            stage_weights: StageImageWeights::default(),
            exposure_mode: ExposureModeId::default(),
            contributor: ContributorId::default(),
            settings: Settings::default(),
            setting_source: std::collections::BTreeMap::new(),
            intent: ImageIntent::Native,
        }
    }
}

/// Recipe images are fitted after θ_L is frozen.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ImageIntent {
    #[default]
    Native,
    Recipe,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FitOptions {
    pub seed: u64,
    pub n_eff: f64,
    pub nu: f64,
    pub max_samples_per_image: usize,
    /// Samples per image for the warm-up stages; 0 uses the full set.
    pub early_samples_per_image: usize,
    /// Normal-equation rows per sweep; caps the quadrature cost, not the likelihood.
    pub hessian_samples: usize,
    pub max_sweeps: usize,
    /// Total sample evaluations allowed for the staged solve.
    pub sweep_budget: f64,
    pub sigma_prior: (f64, f64),
    pub clip_prior: (f64, f64),
    pub exposure_prior_sd: f64,
    pub wb_prior_sd: f64,
    pub exposure_mode_prior_sd: f64,
    /// Absolute tempered effective weight per KDE bin.
    pub density_floor: f64,
    /// Post-clip σ at which a probe enters support.
    pub support_sigma: f64,
    /// Post-clip σ above which a supported probe leaves support.
    pub support_sigma_exit: f64,
    /// `|b_p| / support_sigma` at which a probe enters the bias test; exit is 1.1 times this.
    pub bias_fraction: f64,
    /// Minimum images with non-negligible weight near a probe (tentative 3).
    pub min_probe_clusters: usize,
    /// Add the prior curvature floor to the probe covariance (the recovery study ablates it).
    pub prior_floor: bool,
    /// Scale on the identity prior curvature; 0 disables it (diagnostic ablation).
    pub identity_scale: f64,
    /// Scale the image-clustered sandwich for few clusters (Cameron & Miller 2015).
    pub sandwich_correction: bool,
    pub run_gcv: bool,
    /// Starting smoothness weights; GCV replaces them when `run_gcv` is set.
    pub initial_lambdas: StageLambdas,
    pub fit_residual: bool,
    /// Sweeps of the recipe stage, which holds θ_L fixed (docs/02 §4.3).
    pub recipe_sweeps: usize,
}

impl Default for FitOptions {
    fn default() -> Self {
        Self {
            seed: 0,
            n_eff: 500.0,
            nu: 4.0,
            max_samples_per_image: 10_000,
            early_samples_per_image: 64,
            hessian_samples: 8_000,
            max_sweeps: 128,
            sweep_budget: 2_000_000.0,
            sigma_prior: (4.0, 4.5),
            clip_prior: (0.03, 0.02),
            exposure_prior_sd: 0.3,
            wb_prior_sd: 0.02,
            exposure_mode_prior_sd: 0.3,
            density_floor: 0.01,
            support_sigma: 1.5,
            support_sigma_exit: 1.65,
            bias_fraction: 0.33,
            min_probe_clusters: 3,
            prior_floor: true,
            identity_scale: 1.0,
            sandwich_correction: true,
            run_gcv: true,
            initial_lambdas: StageLambdas::default(),
            fit_residual: true,
            recipe_sweeps: 4,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ImageNuisances {
    /// Log2 exposure offset relative to the body's mode offset.
    pub exposure: f64,
    pub wb_log_r: f64,
    pub wb_log_b: f64,
    pub exposure_mode: ExposureModeId,
    pub contributor: ContributorId,
    pub weight: f64,
}

#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ExposureOffset {
    pub mode: ExposureModeId,
    pub offset: f64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Realization {
    pub clip_k: f64,
    pub exposure_offsets: Vec<ExposureOffset>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct StageLambdas {
    pub tone: f64,
    pub tint: f64,
    pub grl: f64,
    pub residual: f64,
    pub response_tone: f64,
    pub response_tint: f64,
    pub response_chroma: f64,
    pub response_hue: f64,
    pub response_lightness: f64,
    pub response_wb: f64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ControlResponseFit {
    pub model: ControlResponseModel,
    pub parameter_ids: Vec<ResponseParamId>,
    /// Row-major covariance over `parameter_ids`.
    pub covariance: Vec<f64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ExcludeReason {
    SpatialControl { key: ControlKey },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExcludedImage {
    pub index: usize,
    pub reason: ExcludeReason,
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
    /// Pre-clip `J'a'b'` the Look defines.
    pub ucs: Ucs,
    /// Pre-clip covariance, UCS units squared.
    pub covariance: [[f64; 3]; 3],
    /// `sqrt(tr pre covariance)`, ΔE.
    pub sigma_de: f64,
    /// Post-clip `J'a'b'` in the reference output encoding.
    pub post_ucs: Ucs,
    /// Post-clip covariance, UCS units squared.
    pub post_covariance: [[f64; 3]; 3],
    /// `sqrt(tr post covariance)`, ΔE.
    pub post_sigma_de: f64,
    /// `|b_p|` from the prior and penalty curvature, post-clip, ΔE.
    pub bias_de: f64,
    /// `min` per-channel clip derivative is at least 0.25, so pre-clip J′ is identifiable.
    pub pre_clip_reliable: bool,
    /// Outermost hue-grid chroma knot (c=3), where G/R/Λ leverage is largest.
    pub gamut_edge: bool,
    /// Absolute tempered effective weight per KDE bin.
    pub density: f64,
    pub supported_sigma: bool,
    pub supported_density: bool,
    pub supported_clusters: bool,
    pub supported_bias: bool,
    /// Independent held-out model-error evidence exists at this encoding and cell.
    pub supported_evidence: bool,
    pub supported: bool,
    /// Images with non-negligible weight in the probe's region.
    pub clusters: usize,
}

/// Previous published state, for support hysteresis (docs/02 §5).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PreviousSupport {
    pub sigma: Vec<bool>,
    pub bias: Vec<bool>,
}

/// A finite objective value in nats of the tempered fit objective; it may be negative.
#[derive(Clone, Copy, Debug, PartialEq, PartialOrd, serde::Serialize)]
pub struct ObjectiveNats(f64);

impl ObjectiveNats {
    pub const ZERO: Self = Self(0.0);

    /// Returns `None` for a non-finite value.
    pub fn checked(value: f64) -> Option<Self> {
        value.is_finite().then_some(Self(value))
    }

    pub fn get(self) -> f64 {
        self.0
    }
}

impl<'de> serde::Deserialize<'de> for ObjectiveNats {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = <f64 as serde::Deserialize>::deserialize(deserializer)?;
        Self::checked(value).ok_or_else(|| serde::de::Error::custom("expected a finite objective"))
    }
}

/// Number of accepted-state objective samples the final-stage plateau test keeps.
pub const PLATEAU_SAMPLES: usize = 5;

/// Identity of the objective a [`PlateauHistory`] was accumulated under.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct PlateauObjective {
    /// Stage the samples belong to.
    pub stage: usize,
    /// Whether the residual grid was a free block.
    pub residual: bool,
    /// Smoothness weights in force at the checkpoint.
    pub lambdas: StageLambdas,
    /// Conditional `log sigma` of each contributor at the checkpoint.
    pub sigma: Vec<f64>,
    /// Retained eligible rows of the problem.
    pub samples: usize,
    /// Rows the eligibility probe dropped.
    pub excluded: usize,
}

/// Accepted-state objective samples of the final stage, oldest first.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct PlateauHistory {
    /// Objective the samples were stored under.
    pub objective: Option<PlateauObjective>,
    samples: [ObjectiveNats; PLATEAU_SAMPLES],
    /// Number of valid entries in `samples`.
    #[serde(default, deserialize_with = "deserialize_plateau_count")]
    count: usize,
    /// Whether the checkpoint state has an accepted cost not yet sampled.
    #[serde(default)]
    pub pending: bool,
}

fn deserialize_plateau_count<'de, D>(deserializer: D) -> std::result::Result<usize, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let count = <usize as serde::Deserialize>::deserialize(deserializer)?;
    if count > PLATEAU_SAMPLES {
        return Err(serde::de::Error::custom("too many plateau samples"));
    }
    Ok(count)
}

impl Default for PlateauHistory {
    fn default() -> Self {
        Self {
            objective: None,
            samples: [ObjectiveNats::ZERO; PLATEAU_SAMPLES],
            count: 0,
            pending: false,
        }
    }
}

impl PlateauHistory {
    /// Whether the samples may continue an objective with this identity.
    pub fn compatible(&self, objective: &PlateauObjective) -> bool {
        self.objective.as_ref() == Some(objective)
    }

    /// Stored samples, oldest first.
    pub fn samples(&self) -> &[ObjectiveNats] {
        &self.samples[..self.count]
    }

    /// Number of stored samples.
    pub fn len(&self) -> usize {
        self.count
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Snapshots the live samples. A non-finite value leaves it empty.
    pub fn snapshot(samples: &[f64], pending: bool, objective: PlateauObjective) -> Self {
        let start = samples.len().saturating_sub(PLATEAU_SAMPLES);
        let mut history = Self {
            objective: Some(objective),
            pending,
            ..Self::default()
        };
        for value in &samples[start..] {
            let Some(sample) = ObjectiveNats::checked(*value) else {
                return Self::default();
            };
            history.samples[history.count] = sample;
            history.count += 1;
        }
        history
    }
}

/// Solver generation of a saved [`FitState`].
#[derive(
    Clone,
    Copy,
    Debug,
    Default,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    serde::Serialize,
    serde::Deserialize,
)]
pub struct FitStateGeneration(pub u32);

impl FitStateGeneration {
    /// Stable clipped-white composition and fixed per-image retained rows.
    pub const CURRENT: Self = Self(6);
}

/// Everything needed to continue a fit that exhausted its budget.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct FitState {
    #[serde(default)]
    pub generation: FitStateGeneration,
    pub theta: Vec<f64>,
    pub lambdas: StageLambdas,
    /// Stage to resume at, and the first sweep within it.
    pub stage: usize,
    pub sweep: usize,
    /// Warm-up LM damping.
    pub damping: f64,
    /// `None` until the final-stage trust region is initialized.
    #[serde(default)]
    pub trust_radius: Option<crate::TrustRadius>,
    pub converged: bool,
    /// Accepted-state objective samples from the final stage.
    #[serde(default)]
    pub plateau: PlateauHistory,
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

/// A non-negative energy in nats of the tempered fit objective.
#[derive(Clone, Copy, Debug, PartialEq, PartialOrd)]
pub struct Nats(f64);

impl Nats {
    pub const ZERO: Self = Self(0.0);

    /// Returns `None` for a negative or non-finite value.
    pub fn checked(value: f64) -> Option<Self> {
        (value.is_finite() && value >= 0.0).then_some(Self(value))
    }

    pub fn get(self) -> f64 {
        self.0
    }
}

/// Where in the fit the reported certificate was computed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CertificateOrigin {
    /// The final refinement stage certified its own linearization state.
    FinalStage,
    /// The returned native objective was recertified after post-fit refinements.
    PostFitRecertification,
}

/// Why a stationarity certificate could not resolve.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CertificateTermination {
    /// The linear solve hit its iteration or stall limit.
    WorkLimit,
    /// The positive model lost curvature.
    NonpositiveCurvature,
    /// A nuisance, border or preconditioner factorization failed.
    Factorization,
    /// Non-finite arithmetic in the model or solve.
    Nonfinite,
    /// A free coordinate has no strictly positive prior floor.
    NoPriorFloor,
    /// The conditional sigma root is not bracketed or not finite.
    SigmaUnbracketed,
    /// A residual coordinate outside the free mask is nonzero.
    PinViolation,
    /// The resolved bounds straddled the stationarity threshold.
    BoundNotDecisive,
    /// The refreshed objective left the evaluator's domain or was not finite.
    InvalidObjective,
}

impl CertificateTermination {
    pub fn label(self) -> &'static str {
        match self {
            Self::WorkLimit => "work-limit",
            Self::NonpositiveCurvature => "nonpositive-curvature",
            Self::Factorization => "factorization",
            Self::Nonfinite => "nonfinite",
            Self::NoPriorFloor => "no-floor",
            Self::SigmaUnbracketed => "sigma-unbracketed",
            Self::PinViolation => "pin-violation",
            Self::BoundNotDecisive => "bound-not-decisive",
            Self::InvalidObjective => "invalid-objective",
        }
    }
}

/// First-order, model-scaled stationarity of the free coordinates.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum StationarityCertificate {
    /// The certified upper bound on the stationarity energy is at or below 1e-3 nats.
    CertifiedStationary { upper: Nats },
    /// A valid lower bound on the stationarity energy exceeds 1e-3 nats.
    CertifiedNonstationary { lower: Nats },
    /// Neither conclusion was established.
    Unresolved { termination: CertificateTermination },
}

impl StationarityCertificate {
    pub fn is_certified_stationary(self) -> bool {
        matches!(self, Self::CertifiedStationary { .. })
    }
}

/// Work and explicit residual of a certificate solve.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CertificateWork {
    pub iterations: usize,
    /// Explicit relative residual of the complete undamped system after back-substitution.
    pub relative_residual: Option<f64>,
    pub dim: usize,
    pub residual_coordinates: usize,
}

/// Certified interval on the stationarity energy, retained even when it is not decisive.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct StationarityBounds {
    pub lower: Nats,
    pub upper: Nats,
}

/// The last stationarity certification attempt for the returned native objective.
/// Recipe images are excluded and the Look is frozen for them.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct StationarityReport {
    pub origin: CertificateOrigin,
    pub certificate: StationarityCertificate,
    pub work: Option<CertificateWork>,
    /// The interval the certificate was decided from, when both bounds were computed.
    pub bounds: Option<StationarityBounds>,
}

/// Objective samples charged to the staged solve, including reference initialization and value-only trials.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Ord, PartialOrd)]
pub struct SampleEvaluations(u64);

impl SampleEvaluations {
    pub const fn get(self) -> u64 {
        self.0
    }
}

impl core::ops::AddAssign<usize> for SampleEvaluations {
    fn add_assign(&mut self, rhs: usize) {
        self.0 += rhs as u64;
    }
}

impl core::ops::AddAssign for SampleEvaluations {
    fn add_assign(&mut self, rhs: Self) {
        self.0 += rhs.0;
    }
}

/// Samples a fit dropped because they left the evaluator's domain at some permitted nuisance value.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct DomainExclusion {
    /// Exposure `s` bin, quarter stop.
    pub s_bin: i32,
    /// Input hue bin, 15 degrees.
    pub hue_bin: i32,
    /// Input `sqrt(C)` bin, unit 1.
    pub chroma_bin: i32,
    pub samples: usize,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Diagnostics {
    pub sweeps: usize,
    pub evaluations: usize,
    pub sample_evaluations: SampleEvaluations,
    pub final_cost: f64,
    pub sigma_v: Vec<(ContributorId, f64)>,
    pub gcv: StageLambdas,
    pub hessian_samples: usize,
    /// Samples excluded before refinement because a permitted nuisance left the evaluator's domain.
    pub excluded_samples: usize,
    /// Excluded samples by `(s, hue, chroma)` bin; exclusions reduce coverage evidence.
    pub excluded_regions: Vec<DomainExclusion>,
    /// Rows the final-stage curvature operator used: all active rows when they fit the retention bound.
    pub operator_rows: usize,
    pub objective_samples: usize,
    pub quarantined: usize,
    pub down_weighted: usize,
    /// Data curvature over prior plus smoothness curvature, per G/R/Λ coefficient.
    pub grl_precision_ratio: Vec<f64>,
    /// The final refinement stage met its objective and step tolerances.
    pub converged: bool,
    /// The last stationarity certificate for the returned native objective.
    pub stationarity: Option<StationarityReport>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct FitResult {
    pub look: Look,
    /// Output encoding of the post-clip probe predictions.
    pub output_space: RgbSpace,
    pub images: Vec<ImageNuisances>,
    pub realization: Realization,
    pub lambdas: StageLambdas,
    pub probes: ProbeUncertainty,
    pub density: Density,
    pub image_stats: Vec<ImageFitStats>,
    pub diagnostics: Diagnostics,
    pub control_response: Option<ControlResponseFit>,
    pub excluded: Vec<ExcludedImage>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ImageScores {
    pub image_stats: Vec<ImageFitStats>,
    pub excluded: Vec<ExcludedImage>,
    pub diagnostics: Diagnostics,
}

#[cfg(test)]
mod numeric_boundary_tests {
    use crate::{ObjectiveNats, PlateauHistory};
    use serde::Deserialize;
    use serde::de::value::{Error, F64Deserializer};

    #[test]
    fn deserialized_objectives_preserve_negative_costs_and_reject_nonfinite_values() {
        let cost = ObjectiveNats::deserialize(F64Deserializer::<Error>::new(-3.5)).unwrap();
        assert_eq!(cost.get(), -3.5);
        for value in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            assert!(ObjectiveNats::deserialize(F64Deserializer::<Error>::new(value)).is_err());
        }
    }

    #[test]
    fn checkpoint_history_rejects_an_overfull_sample_count() {
        let history: PlateauHistory = serde_json::from_str(
            r#"{"objective":null,"samples":[-5,-4,-3,-2,-1],"count":5,"pending":false}"#,
        )
        .unwrap();
        assert_eq!(history.len(), 5);
        assert_eq!(history.samples()[4].get(), -1.0);
        let error = serde_json::from_str::<PlateauHistory>(
            r#"{"objective":null,"samples":[-5,-4,-3,-2,-1],"count":6,"pending":false}"#,
        )
        .unwrap_err();
        assert_eq!(error.classify(), serde_json::error::Category::Data);
    }
}
