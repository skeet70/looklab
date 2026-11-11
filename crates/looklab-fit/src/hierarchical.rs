// SPDX-License-Identifier: AGPL-3.0-or-later
//! Global hierarchical fit across bodies (docs/02 §4.3): block coordinate descent
//! between body/realization parameters and the shared family target, then a recipe
//! stage with the family target frozen.

use std::collections::{BTreeMap, BTreeSet};

use looklab_body::{
    A0Source, AnchorOptions, Delta6, InputTransform, TransformVersion, chart_anchor_terms,
};
use looklab_color::{Mat3, RgbSpace};
use looklab_model::basis::TONE_COEFS;
use looklab_model::domain_penalty::{DOMAIN_AXES, PenaltyDerivatives};
use looklab_model::realization::{ControlResponseModel, Delta, ResponseComponent};
use looklab_model::{Look, Tone};
use looklab_schema::{
    BodyId, BodyRealizationId, ControlEpochId, FamilyTargetId, FirmwareVersion, LookId,
};

use crate::fit;
use crate::gauge::{ComponentGauge, WbModeClass};
use crate::layout::{Layout, Params, softplus_inverse, softplus_slope};
use crate::predict::{ColumnMap, ImageCtx, Jac, ResponseTerm, evaluate_sample};
use crate::problem::{Problem, RowModel, Sample};
use crate::solve::{factor_in_place, solve_in_place};
use crate::types::{
    CertificateOrigin, CertificateTermination, CertificateWork, ContributorId, Density,
    ExposureModeId, ExposureOffset, FitError, FitImage, FitOptions, FitStage, ImageIntent,
    ImageNuisances, Nats, PreviousSupport, ProbeUncertainty, Result, StageLambdas,
    StationarityBounds, StationarityCertificate, StationarityReport,
};

/// Two firmware offset coordinates: a constant `T` shift and a constant `log G` shift.
pub const FIRMWARE_PARAMETERS: usize = 2;

const RESPONSE_PRIOR_PRECISION: f64 = 10.0;

/// Globally consistent firmware identity: body, resolved control epoch and normalized version.
#[derive(
    Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
pub struct FirmwareId {
    pub body: BodyId,
    pub epoch: ControlEpochId,
    pub version: FirmwareVersion,
}

impl FirmwareId {
    /// # Errors
    /// An invalid firmware version string.
    pub fn new(body: BodyId, epoch: ControlEpochId, version: &str) -> Result<Self> {
        let version = version
            .parse::<FirmwareVersion>()
            .map_err(|error| FitError::Firmware(error.to_string()))?;
        Ok(Self {
            body,
            epoch,
            version,
        })
    }
}

#[derive(
    Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
pub struct FamilyFirmwareId {
    pub family: LookId,
    pub firmware: FirmwareId,
}

#[derive(Clone, Debug)]
pub struct HierImage {
    pub image: FitImage,
    pub firmware: FirmwareId,
    pub wb_mode: WbModeClass,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct BodyWarmStart {
    pub delta: Delta6,
    pub exposure: Vec<ExposureOffset>,
    pub wb: Vec<(WbModeClass, [f64; 2])>,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct RealizationWarmStart {
    pub clip_k: f64,
    pub nuisances: Vec<ImageNuisances>,
    /// LM damping at the saved state.
    pub damping: f64,
    #[serde(default)]
    pub response: Option<ControlResponseModel>,
    #[serde(default)]
    pub coefficients: Vec<f64>,
}

#[derive(Clone, Debug)]
pub struct RealizationData {
    pub id: BodyRealizationId,
    pub target: FamilyTargetId,
    pub model: Option<ControlResponseModel>,
    pub images: Vec<HierImage>,
    pub warm: Option<RealizationWarmStart>,
}

#[derive(Clone, Debug)]
pub struct BodyData {
    pub id: BodyId,
    pub transform: InputTransform,
    pub default_mode: ExposureModeId,
    pub default_wb_mode: WbModeClass,
    pub charts: Vec<looklab_body::ChartSession>,
    pub warm: Option<BodyWarmStart>,
    pub realizations: Vec<RealizationData>,
}

impl BodyData {
    /// Chart- and SSF-sourced priors are tight enough to define the global `Δ` gauge.
    pub fn anchored(&self) -> bool {
        matches!(self.transform.source(), A0Source::Ssf | A0Source::Chart)
    }

    fn native_samples(&self) -> usize {
        self.realizations
            .iter()
            .flat_map(|realization| realization.images.iter())
            .filter(|image| image.image.intent == ImageIntent::Native)
            .map(|image| image.image.samples.n())
            .sum()
    }
}

#[derive(Clone, Debug)]
pub enum FamilyMode {
    Fit { initial: Option<Look> },
    Pinned(Look),
}

#[derive(Clone, Debug)]
pub struct FamilyData {
    pub target: FamilyTargetId,
    pub mode: FamilyMode,
    pub previous_support: Option<PreviousSupport>,
}

impl FamilyData {
    pub fn pinned(target: FamilyTargetId, look: Look) -> Self {
        Self {
            target,
            mode: FamilyMode::Pinned(look),
            previous_support: None,
        }
    }
}

#[derive(Clone, Debug)]
pub struct HierInput {
    pub families: Vec<FamilyData>,
    pub bodies: Vec<BodyData>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct HierStateGeneration(pub u32);

impl HierStateGeneration {
    /// Generation 3: family-scoped firmware keys, frozen retained rows and free/fixed body coordinates.
    pub const CURRENT: Self = Self(3);
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct HierFingerprint(pub u64);

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct BodyResume {
    pub id: BodyId,
    pub state: BodyWarmStart,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct RealizationResume {
    pub id: BodyRealizationId,
    pub target: FamilyTargetId,
    pub state: RealizationWarmStart,
    #[serde(default)]
    pub recipe_fitted: bool,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct FamilyResume {
    pub target: FamilyTargetId,
    pub theta: Vec<f64>,
    pub lambdas: StageLambdas,
}

/// Subject-bound hierarchical solver state for resuming an unfinished solve.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct HierState {
    pub generation: HierStateGeneration,
    pub fingerprint: HierFingerprint,
    pub outer_iterations: usize,
    pub bodies: Vec<BodyResume>,
    pub realizations: Vec<RealizationResume>,
    #[serde(default)]
    pub published_bodies: Vec<BodyResume>,
    #[serde(default)]
    pub published_realizations: Vec<RealizationResume>,
    pub families: Vec<FamilyResume>,
    pub firmware: Vec<(FamilyFirmwareId, [f64; FIRMWARE_PARAMETERS])>,
    /// Contributor variance per id.
    pub sigma: Vec<(ContributorId, f64)>,
    pub body_decrement: Option<f64>,
    pub families_certified: bool,
    pub converged: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub enum BodyPin {
    /// Shared body Δ/ξ/ω are free coordinates of the conditional objective.
    Free,
    /// Shared body Δ/ξ/ω are fixed at the authoritative native-stage states.
    Native(Vec<BodyResume>),
    /// Shared body Δ/ξ/ω are fixed at the authoritative published states.
    Published(Vec<BodyResume>),
}

impl BodyPin {
    pub fn fixed(&self) -> bool {
        !matches!(self, Self::Free)
    }

    fn resumes(&self) -> Option<&[BodyResume]> {
        match self {
            Self::Free => None,
            Self::Native(resumes) | Self::Published(resumes) => Some(resumes),
        }
    }
}

#[derive(Clone, Debug)]
pub struct HierOptions {
    pub fit: FitOptions,
    pub outer_iterations: usize,
    pub body_iterations: usize,
    pub firmware_prior_sd: [f64; FIRMWARE_PARAMETERS],
    pub anchor: AnchorOptions,
    /// Persisted component gauges; a designated body in the member set is kept.
    pub gauges: Vec<ComponentGauge>,
    /// Persisted component membership for exact gauge reuse and missing-gauge detection.
    pub gauge_scopes: Vec<GaugeScope>,
    /// Initial contributor variances for the free stage and fixed scales for the realization stage.
    pub shared_sigma: Vec<(ContributorId, f64)>,
    /// Firmware offsets in force; the free stage updates them, the pinned stage never does.
    pub firmware: Vec<(FamilyFirmwareId, [f64; FIRMWARE_PARAMETERS])>,
    /// Whether shared body Δ/ξ/ω are free or fixed at an authoritative body-stage state.
    pub body_pin: BodyPin,
    pub resume: Option<HierState>,
}

impl Default for HierOptions {
    fn default() -> Self {
        Self {
            fit: FitOptions::default(),
            outer_iterations: 3,
            body_iterations: 6,
            firmware_prior_sd: [0.5, 0.03],
            anchor: AnchorOptions::default(),
            gauges: Vec::new(),
            gauge_scopes: Vec::new(),
            shared_sigma: Vec::new(),
            firmware: Vec::new(),
            body_pin: BodyPin::Free,
            resume: None,
        }
    }
}

#[derive(Clone, Debug)]
pub struct BodyParameters {
    pub id: BodyId,
    /// Fitted `Δ` with the source prior covariance.
    pub transform: InputTransform,
    pub exposure: Vec<ExposureOffset>,
    pub wb: Vec<(WbModeClass, [f64; 2])>,
}

#[derive(Clone, Debug)]
pub struct RealizationParameters {
    pub id: BodyRealizationId,
    pub target: FamilyTargetId,
    /// Fitted realization clip; distinct from the family's nominal rendering clip.
    pub clip_k: f64,
    pub images: Vec<ImageNuisances>,
    pub model: Option<ControlResponseModel>,
    pub recipe_fitted: bool,
    /// LM damping at the returned state, for subject-bound resume.
    pub damping: f64,
}

#[derive(Clone, Debug)]
pub struct LookProvenance {
    pub body: BodyId,
    pub realization: BodyRealizationId,
    pub target: FamilyTargetId,
    pub transform_version: TransformVersion,
    pub delta: Delta6,
    pub exposure: Vec<ExposureOffset>,
    pub wb: Vec<(WbModeClass, [f64; 2])>,
    pub clip_k: f64,
    pub model_version: Option<u32>,
}

/// Image indices of one body inside a pooled family fit.
#[derive(Clone, Debug)]
pub struct BodyGroup {
    pub body: BodyId,
    pub images: Vec<usize>,
}

#[derive(Clone, Debug)]
pub struct FamilyParameters {
    pub target: FamilyTargetId,
    pub look: Look,
    pub firmware: Vec<(FirmwareId, [f64; FIRMWARE_PARAMETERS])>,
    pub probes: ProbeUncertainty,
    pub density: Density,
    /// Nominal canonical rendering clip, not a realization's fitted `k`.
    pub clip_k: f64,
    /// Output encoding of the post-clip probe predictions.
    pub output_space: RgbSpace,
    pub provenance: Vec<LookProvenance>,
    /// Native images with the native-stage body/realization parameters the family was fitted against.
    pub pool: Vec<FitImage>,
    pub groups: Vec<BodyGroup>,
    /// Full conditional solver state, for subject-bound resume.
    pub theta: Vec<f64>,
    pub lambdas: StageLambdas,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum HierLimitation {
    NotConverged,
    NoFamilyCertificate,
    FamilyNotStationary,
    /// The active body block has no validated interval certificate.
    BodyCertificateUnavailable,
    /// The final body block certificate is not certified stationary.
    BodyNotStationary,
    /// The finalized native body snapshot certificate is not certified stationary.
    NativeBodyNotStationary,
    /// Family probe uncertainty uses the nominal target clip, not each realization's fitted k.
    NominalClipForProbeUncertainty,
}

#[derive(Clone, Debug)]
pub struct HierDiagnostics {
    pub evaluations: usize,
    pub outer_iterations: usize,
    pub sigma_v: Vec<(ContributorId, f64)>,
    pub converged: bool,
    pub family_certificate: Option<StationarityReport>,
    pub body_certificate: Option<StationarityReport>,
    pub native_body_certificate: Option<StationarityReport>,
    pub body_decrement: Option<f64>,
    pub limitations: Vec<HierLimitation>,
}

#[derive(Clone, Debug)]
pub struct HierarchicalFit {
    pub families: Vec<FamilyParameters>,
    pub bodies: Vec<BodyParameters>,
    pub realizations: Vec<RealizationParameters>,
    pub components: Vec<ComponentGauge>,
    pub diagnostics: HierDiagnostics,
    pub state: HierState,
}

#[derive(Clone, Debug)]
pub struct PinnedFamily {
    pub target: FamilyTargetId,
    pub look: Look,
}

#[derive(Clone, Debug)]
pub struct GaugeBinding {
    pub gauge: ComponentGauge,
    /// `true` only when the binding came from `HierOptions::gauges`.
    pub persisted: bool,
}

/// Persisted gauge plus the bodies of the component it was established for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GaugeScope {
    pub gauge: ComponentGauge,
    pub bodies: Vec<BodyId>,
}

#[derive(Clone, Debug)]
pub struct RealizationFit {
    pub families: Vec<PinnedFamily>,
    pub bodies: Vec<BodyParameters>,
    pub realizations: Vec<RealizationParameters>,
    pub gauges: Vec<GaugeBinding>,
    pub diagnostics: HierDiagnostics,
    pub state: HierState,
}

impl HierarchicalFit {
    /// Every family whose native body set intersects `dirty`.
    pub fn refit_cascade(&self, dirty: &[BodyId]) -> Vec<FamilyTargetId> {
        let dirty: std::collections::HashSet<&str> =
            dirty.iter().map(|body| body.as_str()).collect();
        let mut out: Vec<FamilyTargetId> = self
            .families
            .iter()
            .filter(|family| {
                family
                    .provenance
                    .iter()
                    .any(|entry| dirty.contains(entry.body.as_str()))
            })
            .map(|family| family.target.clone())
            .collect();
        out.sort();
        out.dedup();
        out
    }

    /// Realization subjects whose body intersects `dirty`.
    pub fn affected_realizations(
        &self,
        dirty: &[BodyId],
    ) -> Vec<(BodyRealizationId, FamilyTargetId)> {
        let dirty: std::collections::HashSet<&str> =
            dirty.iter().map(|body| body.as_str()).collect();
        self.realizations
            .iter()
            .filter(|realization| dirty.contains(realization.id.body.as_str()))
            .map(|realization| (realization.id.clone(), realization.target.clone()))
            .collect()
    }

    pub fn family(&self, target: &FamilyTargetId) -> Option<&FamilyParameters> {
        self.families.iter().find(|family| &family.target == target)
    }

    pub fn family_of(&self, family: &LookId) -> Option<&FamilyParameters> {
        self.families
            .iter()
            .find(|candidate| &candidate.target.family == family)
    }

    pub fn body(&self, id: &BodyId) -> Option<&BodyParameters> {
        self.bodies.iter().find(|body| &body.id == id)
    }

    pub fn realization(
        &self,
        id: &BodyRealizationId,
        target: &FamilyTargetId,
    ) -> Option<&RealizationParameters> {
        self.realizations
            .iter()
            .find(|realization| &realization.id == id && &realization.target == target)
    }

    pub fn body_warm(&self, id: &BodyId) -> Option<BodyWarmStart> {
        self.state
            .bodies
            .iter()
            .find(|body| &body.id == id)
            .map(|body| body.state.clone())
    }

    pub fn realization_warm(
        &self,
        id: &BodyRealizationId,
        target: &FamilyTargetId,
    ) -> Option<RealizationWarmStart> {
        self.state
            .realizations
            .iter()
            .find(|realization| &realization.id == id && &realization.target == target)
            .map(|realization| realization.state.clone())
    }

    pub fn family_warm(&self, target: &FamilyTargetId) -> Option<FamilyResume> {
        self.state
            .families
            .iter()
            .find(|family| &family.target == target)
            .cloned()
    }

    pub fn published_body_warm(&self, id: &BodyId) -> Option<BodyWarmStart> {
        self.state
            .published_bodies
            .iter()
            .find(|body| &body.id == id)
            .map(|body| body.state.clone())
    }

    pub fn published_realization_warm(
        &self,
        id: &BodyRealizationId,
        target: &FamilyTargetId,
    ) -> Option<RealizationWarmStart> {
        self.state
            .published_realizations
            .iter()
            .find(|realization| &realization.id == id && &realization.target == target)
            .map(|realization| realization.state.clone())
    }

    pub fn numerically_publishable(&self) -> bool {
        self.diagnostics.converged
            && self
                .diagnostics
                .body_certificate
                .is_some_and(|report| report.certificate.is_certified_stationary())
            && self
                .diagnostics
                .native_body_certificate
                .is_some_and(|report| report.certificate.is_certified_stationary())
    }
}

impl RealizationFit {
    pub fn body(&self, id: &BodyId) -> Option<&BodyParameters> {
        self.bodies.iter().find(|body| &body.id == id)
    }

    pub fn realization(
        &self,
        id: &BodyRealizationId,
        target: &FamilyTargetId,
    ) -> Option<&RealizationParameters> {
        self.realizations
            .iter()
            .find(|realization| &realization.id == id && &realization.target == target)
    }

    pub fn body_warm(&self, id: &BodyId) -> Option<BodyWarmStart> {
        self.state
            .bodies
            .iter()
            .find(|body| &body.id == id)
            .map(|body| body.state.clone())
    }

    pub fn realization_warm(
        &self,
        id: &BodyRealizationId,
        target: &FamilyTargetId,
    ) -> Option<RealizationWarmStart> {
        self.state
            .realizations
            .iter()
            .find(|realization| &realization.id == id && &realization.target == target)
            .map(|realization| realization.state.clone())
    }

    pub fn published_body_warm(&self, id: &BodyId) -> Option<BodyWarmStart> {
        self.state
            .published_bodies
            .iter()
            .find(|body| &body.id == id)
            .map(|body| body.state.clone())
    }

    pub fn published_realization_warm(
        &self,
        id: &BodyRealizationId,
        target: &FamilyTargetId,
    ) -> Option<RealizationWarmStart> {
        self.state
            .published_realizations
            .iter()
            .find(|realization| &realization.id == id && &realization.target == target)
            .map(|realization| realization.state.clone())
    }

    pub fn numerically_publishable(&self) -> bool {
        self.diagnostics.converged
            && self
                .diagnostics
                .body_certificate
                .is_some_and(|report| report.certificate.is_certified_stationary())
            && self
                .diagnostics
                .native_body_certificate
                .is_some_and(|report| report.certificate.is_certified_stationary())
    }
}

/// A constant `T` offset and a constant `log G` offset over the whole layer.
pub fn firmware_look(look: &Look, offset: [f64; FIRMWARE_PARAMETERS]) -> Look {
    let mut out = look.clone();
    if let Tone::Spline { c0, beta } = &look.tone
        && let Ok(tone) = Tone::spline(c0 + offset[0], *beta)
    {
        out.tone = tone;
    }
    for value in &mut out.color.g.plane {
        *value += offset[1];
    }
    for value in &mut out.color.g.volume {
        *value += offset[1];
    }
    out
}

#[derive(Clone, Debug)]
struct BodyState {
    delta: Delta6,
    xi: Vec<(ExposureModeId, f64)>,
    omega: Vec<(WbModeClass, [f64; 2])>,
}

impl BodyState {
    fn initial(body: &BodyData) -> Self {
        match &body.warm {
            Some(warm) => Self::from_warm(warm),
            None => Self {
                delta: body.transform.delta(),
                xi: Vec::new(),
                omega: Vec::new(),
            },
        }
    }

    fn from_warm(warm: &BodyWarmStart) -> Self {
        Self {
            delta: warm.delta,
            xi: warm
                .exposure
                .iter()
                .map(|offset| (offset.mode, offset.offset))
                .collect(),
            omega: warm.wb.clone(),
        }
    }

    fn to_warm(&self) -> BodyWarmStart {
        BodyWarmStart {
            delta: self.delta,
            exposure: self
                .xi
                .iter()
                .map(|(mode, offset)| ExposureOffset {
                    mode: *mode,
                    offset: *offset,
                })
                .collect(),
            wb: self.omega.clone(),
        }
    }

    fn xi(&self, mode: ExposureModeId) -> f64 {
        self.xi
            .iter()
            .find(|(candidate, _)| *candidate == mode)
            .map_or(0.0, |(_, value)| *value)
    }

    fn omega(&self, class: WbModeClass) -> [f64; 2] {
        self.omega
            .iter()
            .find(|(candidate, _)| *candidate == class)
            .map_or([0.0; 2], |(_, value)| *value)
    }
}

#[derive(Clone, Debug)]
struct RealizationState {
    clip: f64,
    nuisances: Vec<[f64; 3]>,
    response: Option<ControlResponseModel>,
    coefficients: Vec<f64>,
    damping: f64,
}

impl RealizationState {
    fn initial(realization: &RealizationData, clip: f64) -> Self {
        match &realization.warm {
            Some(warm) => Self::from_warm(warm, realization.images.len()),
            None => Self {
                clip,
                nuisances: vec![[0.0; 3]; realization.images.len()],
                response: realization.model.clone(),
                coefficients: Vec::new(),
                damping: 1e-3,
            },
        }
    }

    fn from_warm(warm: &RealizationWarmStart, images: usize) -> Self {
        let nuisances = if warm.nuisances.len() == images {
            warm.nuisances
                .iter()
                .map(|nuisance| [nuisance.exposure, nuisance.wb_log_r, nuisance.wb_log_b])
                .collect()
        } else {
            vec![[0.0; 3]; images]
        };
        Self {
            clip: warm.clip_k,
            nuisances,
            response: warm.response.clone(),
            coefficients: warm.coefficients.clone(),
            damping: warm.damping,
        }
    }

    fn to_warm(&self, realization: &RealizationData) -> RealizationWarmStart {
        let nuisances = realization
            .images
            .iter()
            .enumerate()
            .map(|(index, tag)| ImageNuisances {
                exposure: self.nuisances.get(index).copied().unwrap_or([0.0; 3])[0],
                wb_log_r: self.nuisances.get(index).copied().unwrap_or([0.0; 3])[1],
                wb_log_b: self.nuisances.get(index).copied().unwrap_or([0.0; 3])[2],
                exposure_mode: tag.image.exposure_mode,
                contributor: tag.image.contributor,
                weight: tag.image.trust_weight,
            })
            .collect();
        RealizationWarmStart {
            clip_k: self.clip,
            nuisances,
            damping: self.damping,
            response: self.response.clone(),
            coefficients: self.coefficients.clone(),
        }
    }
}

#[derive(Clone, Debug)]
struct BodyLayout {
    delta: [usize; 6],
    xi: Vec<Option<usize>>,
    omega: Vec<[Option<usize>; 2]>,
    mode_ids: Vec<ExposureModeId>,
    wb_ids: Vec<WbModeClass>,
    fixed: bool,
    clip: usize,
    image: Vec<[usize; 3]>,
    active: Vec<usize>,
    response: Vec<usize>,
    total: usize,
    offset: usize,
}

impl BodyLayout {
    /// The shared coordinates occupy `0..self.clip`.
    fn shared_total(&self) -> usize {
        self.clip
    }

    fn local_total(&self) -> usize {
        self.total - self.shared_total()
    }
}

struct BodyPass {
    gradient: Vec<f64>,
    hessian: Vec<f64>,
    /// Forward error bound from sample and prior accumulation.
    gradient_error: Vec<f64>,
    /// Absolute row sum of the accumulated Hessian.
    hessian_abs: Vec<f64>,
    /// Sequential operation count used by the Hessian error bound.
    hessian_terms: usize,
    cost: f64,
    pairs: Vec<Vec<(f64, f64)>>,
    infeasible: bool,
}

/// One realization's inputs inside the joint body objective.
struct BodyBlock {
    realization_index: usize,
    state_index: usize,
    active: Vec<usize>,
    problem: Problem,
    spaces: Vec<RgbSpace>,
    layout: BodyLayout,
    template: Option<ControlResponseModel>,
}

/// Shared body coordinates plus every affected realization, evaluated as one objective.
struct BodyObjective {
    blocks: Vec<BodyBlock>,
    shared_total: usize,
    sample_cost: f64,
}

/// A joint step: one shared vector and one realization-local vector per block.
struct JointStep {
    shared: Vec<f64>,
    locals: Vec<Vec<f64>>,
}

struct BodyOutcome {
    pairs: Vec<Vec<(f64, f64)>>,
    evaluations: usize,
    decrement: Option<f64>,
    exhausted: bool,
    plateau: bool,
}

fn contributor_index(contributors: &[ContributorId], id: ContributorId) -> usize {
    contributors
        .iter()
        .position(|candidate| *candidate == id)
        .unwrap_or(0)
}

fn unique_modes(images: &[HierImage], active: &[usize]) -> Vec<ExposureModeId> {
    let mut modes = Vec::new();
    for index in active {
        let mode = images[*index].image.exposure_mode;
        if !modes.contains(&mode) {
            modes.push(mode);
        }
    }
    modes
}

fn unique_wb_modes(images: &[HierImage], active: &[usize]) -> Vec<WbModeClass> {
    let mut modes = Vec::new();
    for index in active {
        let mode = images[*index].wb_mode;
        if !modes.contains(&mode) {
            modes.push(mode);
        }
    }
    modes
}

#[allow(clippy::too_many_arguments)]
fn body_layout(
    active: &[usize],
    modes: &[ExposureModeId],
    wb_modes: &[WbModeClass],
    gauge: &ComponentGauge,
    designated: bool,
    fixed: bool,
    response_count: usize,
    offset: usize,
) -> BodyLayout {
    let mut next = 0usize;
    let mut alloc = || {
        let index = next;
        next += 1;
        index
    };
    let (delta, xi, omega, mode_ids, wb_ids) = if fixed {
        ([0usize; 6], Vec::new(), Vec::new(), Vec::new(), Vec::new())
    } else {
        let delta = core::array::from_fn(|_| alloc());
        let xi: Vec<Option<usize>> = modes
            .iter()
            .map(|mode| {
                if designated && *mode == gauge.exposure_mode {
                    None
                } else {
                    Some(alloc())
                }
            })
            .collect();
        let omega: Vec<[Option<usize>; 2]> = wb_modes
            .iter()
            .map(|mode| {
                if designated && *mode == gauge.wb_mode {
                    [None, None]
                } else {
                    [Some(alloc()), Some(alloc())]
                }
            })
            .collect();
        (delta, xi, omega, modes.to_vec(), wb_modes.to_vec())
    };
    let clip = alloc();
    let image = active.iter().map(|_| [alloc(), alloc(), alloc()]).collect();
    let response = (0..response_count).map(|_| alloc()).collect();
    BodyLayout {
        delta,
        xi,
        omega,
        mode_ids,
        wb_ids,
        fixed,
        clip,
        image,
        active: active.to_vec(),
        response,
        total: next,
        offset,
    }
}

fn tone_slopes(look: &Look) -> [f64; TONE_COEFS] {
    let mut slopes = [1.0_f64; TONE_COEFS];
    if let Tone::Spline { beta, .. } = &look.tone {
        for (slope, value) in slopes.iter_mut().zip(beta.iter()) {
            *slope = softplus_slope(softplus_inverse(*value));
        }
    }
    slopes
}

/// Adds the sample-weighted domain-penalty gradient and Gauss-Newton curvature
#[allow(clippy::too_many_arguments)]
fn accumulate_domain_penalty(
    gradient: &mut [f64],
    gradient_abs: &mut [f64],
    hessian: &mut [f64],
    hessian_abs: &mut [f64],
    total: usize,
    buffer: &[(u32, [f64; 3], [f64; DOMAIN_AXES])],
    weight: f64,
    penalty: &PenaltyDerivatives,
) {
    for (position, (left, _, left_axis)) in buffer.iter().enumerate() {
        let mut term = 0.0;
        for axis in 0..DOMAIN_AXES {
            term += penalty.gradient[axis] * left_axis[axis];
        }
        gradient[*left as usize] += weight * term;
        gradient_abs[*left as usize] += (weight * term).abs();
        let base = *left as usize * total;
        for (right, _, right_axis) in buffer.iter().skip(position) {
            let mut curvature = 0.0;
            for axis in 0..DOMAIN_AXES {
                if left_axis[axis] == 0.0 {
                    continue;
                }
                for other in 0..DOMAIN_AXES {
                    curvature +=
                        penalty.gn_hessian[axis][other] * left_axis[axis] * right_axis[other];
                }
            }
            let value = weight * curvature;
            hessian[base + *right as usize] += value;
            hessian_abs[*left as usize] += value.abs();
            if right != left {
                hessian[*right as usize * total + *left as usize] += value;
                hessian_abs[*right as usize] += value.abs();
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn body_pass(
    body: &BodyData,
    realization: &RealizationData,
    active: &[usize],
    samples: &[Sample],
    problem_layout: &Layout,
    spaces: &[RgbSpace],
    terms: &[Vec<ResponseTerm>],
    looks: &BTreeMap<LookId, Look>,
    firmware: &BTreeMap<FamilyFirmwareId, [f64; FIRMWARE_PARAMETERS]>,
    layout: &BodyLayout,
    body_state: &BodyState,
    state: &RealizationState,
    sigma: &[f64],
    contributors: &[ContributorId],
    options: &HierOptions,
    delta_precision: &[[f64; 6]; 6],
    include_body_terms: bool,
) -> Result<BodyPass> {
    let total = layout.total;
    let mut gradient = vec![0.0_f64; total];
    let mut gradient_abs = vec![0.0_f64; total];
    let mut hessian = vec![0.0_f64; total * total];
    let mut hessian_abs = vec![0.0_f64; total];
    let mut cost = 0.0;
    let mut infeasible = false;
    let mut pairs: Vec<Vec<(f64, f64)>> = vec![Vec::new(); contributors.len()];
    let mut map = vec![u32::MAX; problem_layout.total + total];
    let mut local_of = vec![u32::MAX; total];
    for (local, global) in local_indices(layout).iter().enumerate() {
        map[layout.offset + global] = local as u32;
        local_of[*global] = local as u32;
    }
    for (index, global) in layout.response.iter().enumerate() {
        map[problem_layout.response.start + index] = local_of[*global];
    }

    struct ActiveLook {
        look: Look,
        slopes: [f64; TONE_COEFS],
        tone_clamp: [crate::predict::ToneClamp; TONE_COEFS],
    }
    let firmware_ids: Vec<FamilyFirmwareId> = active
        .iter()
        .map(|index| FamilyFirmwareId {
            family: realization.target.family.clone(),
            firmware: realization.images[*index].firmware.clone(),
        })
        .collect();
    let active_looks: Vec<ActiveLook> = (0..active.len())
        .map(|position| {
            let base = looks
                .get(&realization.target.family)
                .cloned()
                .unwrap_or_else(Look::colorimetric);
            let offset = firmware
                .get(&firmware_ids[position])
                .copied()
                .unwrap_or([0.0; 2]);
            let mut look = firmware_look(&base, offset);
            let mut delta = Delta::default();
            for term in &terms[position] {
                if let Some(layer) = term.component.layer() {
                    let value = state.coefficients.get(term.index).copied().unwrap_or(0.0);
                    delta.add(layer, term.feature, value * term.phi);
                }
            }
            let tone_clamp = match look.tone.betas() {
                Some(beta)
                    if terms[position].iter().any(|term| {
                        term.component == ResponseComponent::Tone && term.feature > 0
                    }) =>
                {
                    crate::problem::response_tone_clamp(
                        core::array::from_fn(|k| beta[k]),
                        &terms[position],
                        &state.coefficients,
                    )
                }
                _ => [crate::predict::ToneClamp::Interior; TONE_COEFS],
            };
            let slopes = tone_slopes(&look);
            look = delta.apply(&look)?;
            Ok(ActiveLook {
                look,
                slopes,
                tone_clamp,
            })
        })
        .collect::<Result<_>>()?;

    let mut jac = Jac::default();
    for sample in samples {
        let position = sample.image as usize;
        let body_index = active[position];
        let tag = &realization.images[body_index];
        let nuisance = state.nuisances[body_index];
        let mode_position = layout
            .mode_ids
            .iter()
            .position(|mode| *mode == tag.image.exposure_mode)
            .unwrap_or(0);
        let wb_position = layout
            .wb_ids
            .iter()
            .position(|mode| *mode == tag.wb_mode)
            .unwrap_or(0);
        let xi = body_state.xi(tag.image.exposure_mode);
        let omega = body_state.omega(tag.wb_mode);
        let mut log_r = omega[0] + nuisance[1];
        let mut log_b = omega[1] + nuisance[2];
        for term in &terms[position] {
            let value = state.coefficients.get(term.index).copied().unwrap_or(0.0) * term.phi;
            match term.component {
                ResponseComponent::WbR => log_r += value,
                ResponseComponent::WbB => log_b += value,
                _ => {}
            }
        }
        let ActiveLook {
            look,
            slopes,
            tone_clamp,
        } = &active_looks[position];
        let a0 = tag.image.transform;
        let transform = a0.mul(&body_state.delta.transform());
        let context = ImageCtx {
            transform,
            a0,
            space: spaces[position],
            exposure_scale: libm::pow(2.0, xi + nuisance[0]),
            wb_r: libm::exp(log_r),
            wb_b: libm::exp(log_b),
            tone_clamp: *tone_clamp,
        };
        let columns = ColumnMap {
            mode: None,
            image: Some(layout.image[position][0] + layout.offset),
            wb_r: Some(layout.image[position][1] + layout.offset),
            wb_b: Some(layout.image[position][2] + layout.offset),
            clip: None,
            exposure_extra: if layout.fixed {
                None
            } else {
                layout
                    .xi
                    .get(mode_position)
                    .copied()
                    .flatten()
                    .map(|index| index + layout.offset)
            },
            wb_r_extra: if layout.fixed {
                None
            } else {
                layout
                    .omega
                    .get(wb_position)
                    .and_then(|pair| pair[0])
                    .map(|index| index + layout.offset)
            },
            wb_b_extra: if layout.fixed {
                None
            } else {
                layout
                    .omega
                    .get(wb_position)
                    .and_then(|pair| pair[1])
                    .map(|index| index + layout.offset)
            },
            clip_extra: Some(layout.clip + layout.offset),
            delta: (!layout.fixed).then(|| layout.delta.map(|index| index + layout.offset)),
            second: false,
            curvature: false,
            clip_geometry: crate::predict::ClipGeometry::Linear,
        };
        let evaluated = evaluate_sample(
            look,
            problem_layout,
            &context,
            &columns,
            sample.rgb,
            state.clip,
            1.0,
            slopes,
            &terms[position],
            &sample.target,
            &mut jac,
        );
        let residual = evaluated.residual;
        let Some(penalty) = evaluated.penalty.as_ref() else {
            infeasible = true;
            cost = f64::INFINITY;
            continue;
        };
        let feasible = penalty.value.is_finite()
            && penalty.gradient.iter().all(|value| value.is_finite())
            && penalty
                .hessian
                .iter()
                .flatten()
                .all(|value| value.is_finite())
            && penalty
                .gn_hessian
                .iter()
                .flatten()
                .all(|value| value.is_finite())
            && evaluated.axis_terms.len() == jac.cols.len()
            && evaluated
                .axis_terms
                .iter()
                .all(|axis| axis.iter().all(|value| value.is_finite()))
            && jac
                .derivs
                .iter()
                .all(|derivative| derivative.iter().all(|value| value.is_finite()));
        if !feasible {
            infeasible = true;
            cost = f64::INFINITY;
            continue;
        }
        let squared =
            residual[0] * residual[0] + residual[1] * residual[1] + residual[2] * residual[2];
        let contributor = contributor_index(contributors, tag.image.contributor);
        let variance = libm::exp(2.0 * sigma[contributor]);
        let nu = options.fit.nu;
        let ratio = squared / (nu * variance);
        let alpha = (nu + 3.0) / (nu * variance + squared);
        let weight = sample.weight;
        cost += weight
            * (0.5 * (nu + 3.0) * libm::log1p(ratio) + 1.5 * libm::log(variance) + penalty.value);
        pairs[contributor].push((weight, squared));
        let contribution = weight * alpha;

        let mut buffer: Vec<(u32, [f64; 3], [f64; DOMAIN_AXES])> =
            Vec::with_capacity(jac.cols.len());
        for (index, (column, derivative)) in jac.cols.iter().zip(jac.derivs.iter()).enumerate() {
            let local = map[*column as usize];
            if local != u32::MAX {
                buffer.push((local, *derivative, evaluated.axis_terms[index]));
            }
        }
        for (a, (left, left_derivative, _)) in buffer.iter().enumerate() {
            let base = *left as usize * total;
            for (right, right_derivative, _) in buffer.iter().skip(a) {
                let dot = left_derivative[0] * right_derivative[0]
                    + left_derivative[1] * right_derivative[1]
                    + left_derivative[2] * right_derivative[2];
                let value = contribution * dot;
                hessian[base + *right as usize] += value;
                hessian_abs[*left as usize] += value.abs();
                if right != left {
                    hessian[*right as usize * total + *left as usize] += value;
                    hessian_abs[*right as usize] += value.abs();
                }
            }
            let term = left_derivative[0] * residual[0]
                + left_derivative[1] * residual[1]
                + left_derivative[2] * residual[2];
            gradient[*left as usize] += contribution * term;
            gradient_abs[*left as usize] += (contribution * term).abs();
        }
        accumulate_domain_penalty(
            &mut gradient,
            &mut gradient_abs,
            &mut hessian,
            &mut hessian_abs,
            total,
            &buffer,
            weight,
            penalty,
        );
    }

    let exposure_sd = options.fit.exposure_prior_sd;
    let wb_sd = options.fit.wb_prior_sd;
    let mode_sd = options.fit.exposure_mode_prior_sd;
    if include_body_terms {
        let delta_params = body_state.delta.params();
        for (index, value) in delta_params.iter().enumerate() {
            let mut quadratic = 0.0;
            for (other, precision) in delta_precision[index].iter().enumerate() {
                gradient[layout.delta[index]] += precision * delta_params[other];
                gradient_abs[layout.delta[index]] += (precision * delta_params[other]).abs();
                hessian[layout.delta[index] * total + layout.delta[other]] += precision;
                hessian_abs[layout.delta[index]] += precision.abs();
                if index != other {
                    hessian_abs[layout.delta[other]] += precision.abs();
                }
                quadratic += precision * delta_params[other];
            }
            cost += 0.5 * value * quadratic;
        }
        for (position, mode) in layout.mode_ids.iter().enumerate() {
            if let Some(index) = layout.xi[position] {
                let value = body_state.xi(*mode);
                gradient[index] += value / (mode_sd * mode_sd);
                gradient_abs[index] += (value / (mode_sd * mode_sd)).abs();
                hessian[index * total + index] += 1.0 / (mode_sd * mode_sd);
                hessian_abs[index] += 1.0 / (mode_sd * mode_sd);
                cost += 0.5 * value * value / (mode_sd * mode_sd);
            }
        }
        for (position, class) in layout.wb_ids.iter().enumerate() {
            let values = body_state.omega(*class);
            for channel in 0..2 {
                if let Some(index) = layout.omega[position][channel] {
                    let value = values[channel];
                    gradient[index] += value / (wb_sd * wb_sd);
                    gradient_abs[index] += (value / (wb_sd * wb_sd)).abs();
                    hessian[index * total + index] += 1.0 / (wb_sd * wb_sd);
                    hessian_abs[index] += 1.0 / (wb_sd * wb_sd);
                    cost += 0.5 * value * value / (wb_sd * wb_sd);
                }
            }
        }
    }
    let (clip_mean, clip_sd) = options.fit.clip_prior;
    {
        let value = state.clip - clip_mean;
        gradient[layout.clip] += value / (clip_sd * clip_sd);
        gradient_abs[layout.clip] += (value / (clip_sd * clip_sd)).abs();
        hessian[layout.clip * total + layout.clip] += 1.0 / (clip_sd * clip_sd);
        hessian_abs[layout.clip] += 1.0 / (clip_sd * clip_sd);
        cost += 0.5 * value * value / (clip_sd * clip_sd);
    }
    for (position, body_index) in active.iter().enumerate() {
        let nuisance = state.nuisances[*body_index];
        let hint = realization.images[*body_index]
            .image
            .exposure_hint
            .clamp(-8.0, 8.0);
        let columns = layout.image[position];
        let delta = nuisance[0] - hint;
        gradient[columns[0]] += delta / (exposure_sd * exposure_sd);
        gradient_abs[columns[0]] += (delta / (exposure_sd * exposure_sd)).abs();
        hessian[columns[0] * total + columns[0]] += 1.0 / (exposure_sd * exposure_sd);
        hessian_abs[columns[0]] += 1.0 / (exposure_sd * exposure_sd);
        cost += 0.5 * delta * delta / (exposure_sd * exposure_sd);
        for channel in 0..2 {
            let value = nuisance[1 + channel];
            gradient[columns[1 + channel]] += value / (wb_sd * wb_sd);
            gradient_abs[columns[1 + channel]] += (value / (wb_sd * wb_sd)).abs();
            hessian[columns[1 + channel] * total + columns[1 + channel]] += 1.0 / (wb_sd * wb_sd);
            hessian_abs[columns[1 + channel]] += 1.0 / (wb_sd * wb_sd);
            cost += 0.5 * value * value / (wb_sd * wb_sd);
        }
    }

    if include_body_terms && !body.charts.is_empty() {
        let transform = body
            .transform
            .with_delta(body_state.delta, body.transform.covariance());
        let terms = chart_anchor_terms(&transform, &body.charts, &options.anchor);
        cost += terms.value;
        for (index, column) in terms.gradient.iter().enumerate() {
            gradient[layout.delta[index]] += column;
            gradient_abs[layout.delta[index]] += column.abs();
            for (other, value) in terms.hessian[index].iter().enumerate() {
                hessian[layout.delta[index] * total + layout.delta[other]] += value;
                hessian_abs[layout.delta[index]] += value.abs();
                if index != other {
                    hessian_abs[layout.delta[other]] += value.abs();
                }
            }
        }
    }

    for (local, value) in state.coefficients.iter().enumerate() {
        if local >= layout.response.len() {
            break;
        }
        let slot = layout.response[local];
        gradient[slot] += RESPONSE_PRIOR_PRECISION * value;
        gradient_abs[slot] += (RESPONSE_PRIOR_PRECISION * value).abs();
        hessian[slot * total + slot] += RESPONSE_PRIOR_PRECISION;
        hessian_abs[slot] += RESPONSE_PRIOR_PRECISION;
        cost += 0.5 * RESPONSE_PRIOR_PRECISION * value * value;
    }

    let terms = samples
        .len()
        .saturating_mul(total + 1)
        .saturating_add(total + 1);
    let gradient_error = gradient_abs
        .iter()
        .map(|absolute| roundoff_gamma(terms) * *absolute)
        .collect();
    Ok(BodyPass {
        gradient,
        hessian,
        gradient_error,
        hessian_abs,
        hessian_terms: terms,
        cost,
        pairs,
        infeasible,
    })
}

fn local_indices(layout: &BodyLayout) -> Vec<usize> {
    let mut indices = Vec::new();
    if !layout.fixed {
        indices.extend(layout.delta);
        indices.extend(layout.xi.iter().flatten().copied());
        for pair in &layout.omega {
            indices.extend(pair.iter().flatten().copied());
        }
    }
    indices.push(layout.clip);
    for image in &layout.image {
        indices.extend(image.iter().copied());
    }
    indices.extend(layout.response.iter().copied());
    indices
}

#[allow(clippy::too_many_arguments)]
fn solve_body_step(passes: &[BodyPass], layouts: &[BodyLayout], damping: f64) -> Option<JointStep> {
    let s = layouts.first().map_or(0, BodyLayout::shared_total);
    if layouts.iter().any(|layout| layout.shared_total() != s) {
        return None;
    }
    let mut a = vec![0.0_f64; s * s];
    let mut rhs = vec![0.0_f64; s];
    let mut factored: Vec<(&BodyPass, &BodyLayout, Vec<f64>)> = Vec::with_capacity(passes.len());
    for (pass, layout) in passes.iter().zip(layouts) {
        let l = layout.local_total();
        let n = layout.total;
        let mut b = vec![0.0_f64; l * l];
        for i in 0..l {
            for j in 0..l {
                b[i * l + j] = pass.hessian[(s + i) * n + s + j];
            }
        }
        for i in 0..l {
            let diagonal = b[i * l + i].max(1e-12);
            b[i * l + i] += damping * diagonal;
        }
        if !factor_in_place(&mut b, l) {
            return None;
        }
        for i in 0..s {
            rhs[i] -= pass.gradient[i];
            for j in 0..s {
                a[i * s + j] += pass.hessian[i * n + j];
            }
        }
        let mut local = vec![0.0_f64; l];
        local.copy_from_slice(&pass.gradient[s..]);
        solve_in_place(&b, l, &mut local);
        for i in 0..s {
            let mut value = 0.0;
            for k in 0..l {
                value += pass.hessian[i * n + s + k] * local[k];
            }
            rhs[i] += value;
        }
        factored.push((pass, layout, b));
    }
    for i in 0..s {
        let diagonal = a[i * s + i].max(1e-12);
        a[i * s + i] += damping * diagonal;
    }
    for (pass, layout, b) in &factored {
        let l = layout.local_total();
        let n = layout.total;
        for i in 0..s {
            let mut column = vec![0.0_f64; l];
            for k in 0..l {
                column[k] = pass.hessian[i * n + s + k];
            }
            solve_in_place(b, l, &mut column);
            for j in 0..s {
                let mut value = 0.0;
                for k in 0..l {
                    value += pass.hessian[j * n + s + k] * column[k];
                }
                a[i * s + j] -= value;
            }
        }
    }
    let mut shared = rhs;
    if s > 0 {
        if !factor_in_place(&mut a, s) {
            return None;
        }
        solve_in_place(&a, s, &mut shared);
    }
    let mut locals = Vec::with_capacity(factored.len());
    for (pass, layout, b) in &factored {
        let l = layout.local_total();
        let n = layout.total;
        let mut local = vec![0.0_f64; l];
        local.copy_from_slice(&pass.gradient[s..]);
        for j in 0..s {
            for k in 0..l {
                local[k] += pass.hessian[j * n + s + k] * shared[j];
            }
        }
        solve_in_place(b, l, &mut local);
        for value in &mut local {
            *value = -*value;
        }
        locals.push(local);
    }
    Some(JointStep { shared, locals })
}

/// Quadratic-model stationarity energy of the complete joint body block.
fn body_decrement(passes: &[BodyPass], layouts: &[BodyLayout]) -> Option<f64> {
    if layouts.is_empty() {
        return Some(0.0);
    }
    let step = solve_body_step(passes, layouts, 0.0)?;
    let s = layouts[0].shared_total();
    let mut value = 0.0;
    for index in 0..s {
        let gradient: f64 = passes.iter().map(|pass| pass.gradient[index]).sum();
        value += gradient * step.shared[index];
    }
    for (position, (pass, layout)) in passes.iter().zip(layouts).enumerate() {
        for index in 0..layout.local_total() {
            value += pass.gradient[s + index] * step.locals[position][index];
        }
    }
    value.is_finite().then_some(value.abs())
}

fn apply_shared_step(
    layout: &BodyLayout,
    body_state: &mut BodyState,
    step: &[f64],
    factor: f64,
) -> bool {
    let mut params = body_state.delta.params();
    for index in 0..6 {
        params[index] += factor * step[layout.delta[index]];
    }
    let Ok(delta) = Delta6::new(params) else {
        return false;
    };
    body_state.delta = delta;
    for (position, id) in layout.mode_ids.iter().enumerate() {
        if let Some(index) = layout.xi[position]
            && let Some((_, value)) = body_state
                .xi
                .iter_mut()
                .find(|(candidate, _)| candidate == id)
        {
            *value += factor * step[index];
        }
    }
    for (position, id) in layout.wb_ids.iter().enumerate() {
        if let Some((_, value)) = body_state
            .omega
            .iter_mut()
            .find(|(candidate, _)| candidate == id)
        {
            for channel in 0..2 {
                if let Some(index) = layout.omega[position][channel] {
                    value[channel] += factor * step[index];
                }
            }
        }
    }
    true
}

fn apply_local_step(layout: &BodyLayout, state: &mut RealizationState, step: &[f64], factor: f64) {
    let base = layout.shared_total();
    state.clip = (state.clip + factor * step[layout.clip - base]).clamp(0.005, 0.1);
    for (position, local) in layout.response.iter().enumerate() {
        if position < state.coefficients.len() {
            state.coefficients[position] += factor * step[*local - base];
        }
    }
    for (position, columns) in layout.image.iter().enumerate() {
        let image = layout.active[position];
        for channel in 0..3 {
            state.nuisances[image][channel] += factor * step[columns[channel] - base];
        }
    }
}

fn ensure_modes(state: &mut BodyState, images: &[HierImage], active: &[usize]) {
    for mode in unique_modes(images, active) {
        if !state.xi.iter().any(|(candidate, _)| *candidate == mode) {
            state.xi.push((mode, 0.0));
        }
    }
    for class in unique_wb_modes(images, active) {
        if !state.omega.iter().any(|(candidate, _)| *candidate == class) {
            state.omega.push((class, [0.0; 2]));
        }
    }
}

/// The designated body's default exposure mode and WB class are the component reference.
fn enforce_body_gauge(state: &mut BodyState, gauge: &ComponentGauge, designated: bool) {
    if !designated {
        return;
    }
    if let Some((_, value)) = state
        .xi
        .iter_mut()
        .find(|(candidate, _)| *candidate == gauge.exposure_mode)
    {
        *value = 0.0;
    }
    if let Some((_, value)) = state
        .omega
        .iter_mut()
        .find(|(candidate, _)| *candidate == gauge.wb_mode)
    {
        *value = [0.0; 2];
    }
}

#[allow(clippy::too_many_arguments)]
fn build_body_objective(
    body: &BodyData,
    state_indices: &[usize],
    realization_states: &mut [RealizationState],
    body_state: &mut BodyState,
    gauge: &ComponentGauge,
    designated: bool,
    options: &HierOptions,
    include_recipe: bool,
    fixed_body: bool,
) -> Result<BodyObjective> {
    let actives: Vec<Vec<usize>> = body
        .realizations
        .iter()
        .map(|realization| {
            realization
                .images
                .iter()
                .enumerate()
                .filter(|(_, tag)| include_recipe || tag.image.intent == ImageIntent::Native)
                .map(|(index, _)| index)
                .collect()
        })
        .collect();
    let mut modes = Vec::new();
    let mut wb_modes = Vec::new();
    for (realization, active) in body.realizations.iter().zip(&actives) {
        if active.is_empty() {
            continue;
        }
        if !fixed_body {
            ensure_modes(body_state, &realization.images, active);
        }
        for index in active {
            let mode = realization.images[*index].image.exposure_mode;
            if !modes.contains(&mode) {
                modes.push(mode);
            }
            let wb = realization.images[*index].wb_mode;
            if !wb_modes.contains(&wb) {
                wb_modes.push(wb);
            }
        }
    }
    let mut blocks = Vec::new();
    let mut sample_cost = 0.0;
    let stage = if include_recipe {
        FitStage::Published
    } else {
        FitStage::Native
    };
    if !fixed_body {
        enforce_body_gauge(body_state, gauge, designated);
    }
    for (realization_index, realization) in body.realizations.iter().enumerate() {
        let active = &actives[realization_index];
        if active.is_empty() {
            continue;
        }
        let state_index = state_indices[realization_index];
        let mut state = realization_states[state_index].clone();
        if state.nuisances.len() != realization.images.len() {
            state.nuisances = vec![[0.0; 3]; realization.images.len()];
        }
        let filtered: Vec<FitImage> = active
            .iter()
            .map(|index| {
                let mut image = realization.images[*index].image.clone();
                if let Some(weights) = image.stage_weights.body(stage) {
                    image.trust_weight = weights.trust_weight;
                    image.sample_trust = weights.sample_trust.clone();
                }
                image.exposure_hint =
                    body_state.xi(image.exposure_mode) + image.exposure_hint.clamp(-8.0, 8.0);
                image
            })
            .collect();
        let template = state.response.clone().or_else(|| realization.model.clone());
        let rows: Vec<RowModel<'_>> = active
            .iter()
            .map(|index| {
                let tag = &realization.images[*index];
                RowModel {
                    group: 0,
                    model: template.as_ref(),
                    clip_k: 0.0,
                    firmware: [0.0; 2],
                    eligible_transform: tag.image.transform,
                    eligible_hint: tag.image.exposure_hint.clamp(-8.0, 8.0),
                }
            })
            .collect();
        let problem = Problem::new_with_rows(&filtered, &rows, &options.fit)?;
        let response_count = problem.parameter_ids.len();
        if state.coefficients.len() != response_count {
            let mut params = Params::initial(problem.layout.clone(), problem.contributors.len());
            if let Some(model) = &template {
                fit::seed_response_params(&problem, &mut params, model);
            }
            state.coefficients = params.theta[problem.layout.response.clone()].to_vec();
        }
        realization_states[state_index] = state;
        let layout = body_layout(
            active,
            &modes,
            &wb_modes,
            gauge,
            designated,
            fixed_body,
            response_count,
            problem.layout.total,
        );
        sample_cost += problem.samples.len() as f64;
        let spaces: Vec<RgbSpace> = problem.images.iter().map(|image| image.space).collect();
        blocks.push(BodyBlock {
            realization_index,
            state_index,
            active: active.clone(),
            problem,
            spaces,
            layout,
            template,
        });
    }
    let shared_total = blocks
        .first()
        .map_or(0, |block| block.layout.shared_total());
    Ok(BodyObjective {
        blocks,
        shared_total,
        sample_cost,
    })
}

#[allow(clippy::too_many_arguments)]
fn body_pass_objective<'a>(
    objective: &BodyObjective,
    body: &BodyData,
    body_state: &BodyState,
    states: impl Fn(usize) -> &'a RealizationState,
    looks: &BTreeMap<LookId, Look>,
    firmware: &BTreeMap<FamilyFirmwareId, [f64; FIRMWARE_PARAMETERS]>,
    sigma: &[f64],
    contributors: &[ContributorId],
    options: &HierOptions,
    precision: &[[f64; 6]; 6],
    fixed_body: bool,
) -> Result<Vec<BodyPass>> {
    let mut passes = Vec::with_capacity(objective.blocks.len());
    for (index, block) in objective.blocks.iter().enumerate() {
        let realization = &body.realizations[block.realization_index];
        passes.push(body_pass(
            body,
            realization,
            &block.active,
            &block.problem.samples,
            &block.problem.layout,
            &block.spaces,
            &block.problem.terms,
            looks,
            firmware,
            &block.layout,
            body_state,
            states(block.state_index),
            sigma,
            contributors,
            options,
            precision,
            index == 0 && !fixed_body,
        )?);
    }
    Ok(passes)
}

fn step_factor(step: &JointStep) -> f64 {
    let mut largest = step.shared.iter().fold(0.0_f64, |acc, value| {
        if value.is_finite() {
            acc.max(value.abs())
        } else {
            acc
        }
    });
    for local in &step.locals {
        largest = local.iter().fold(largest, |acc, value| {
            if value.is_finite() {
                acc.max(value.abs())
            } else {
                acc
            }
        });
    }
    if largest > 2.0 { 2.0 / largest } else { 1.0 }
}

#[allow(clippy::too_many_arguments)]
fn fit_body(
    body: &BodyData,
    state_indices: &[usize],
    realization_states: &mut [RealizationState],
    body_state: &mut BodyState,
    looks: &BTreeMap<LookId, Look>,
    firmware: &BTreeMap<FamilyFirmwareId, [f64; FIRMWARE_PARAMETERS]>,
    gauge: &ComponentGauge,
    designated: bool,
    sigma: &[f64],
    contributors: &[ContributorId],
    options: &HierOptions,
    include_recipe: bool,
    remaining: &mut f64,
) -> Result<BodyOutcome> {
    let fixed_body = options.body_pin.fixed();
    let mut pairs = vec![Vec::new(); contributors.len()];
    let mut evaluations = 0usize;
    let precision = body
        .transform
        .covariance()
        .precision()
        .map_err(|error| FitError::Hierarchy(format!("delta prior: {error}")))?;
    let objective = build_body_objective(
        body,
        state_indices,
        realization_states,
        body_state,
        gauge,
        designated,
        options,
        include_recipe,
        fixed_body,
    )?;
    if objective.blocks.is_empty() {
        return Ok(BodyOutcome {
            pairs,
            evaluations,
            decrement: Some(0.0),
            exhausted: false,
            plateau: true,
        });
    }
    let layouts: Vec<BodyLayout> = objective
        .blocks
        .iter()
        .map(|block| block.layout.clone())
        .collect();
    let samples = objective.sample_cost;
    if *remaining < samples {
        return Ok(BodyOutcome {
            pairs,
            evaluations,
            decrement: None,
            exhausted: true,
            plateau: false,
        });
    }
    *remaining -= samples;
    let mut passes = body_pass_objective(
        &objective,
        body,
        body_state,
        |index| &realization_states[index],
        looks,
        firmware,
        sigma,
        contributors,
        options,
        &precision,
        fixed_body,
    )?;
    evaluations += 1;
    if passes.iter().any(|pass| pass.infeasible) {
        return Err(FitError::Hierarchy(
            "body samples left the inverse domain".to_string(),
        ));
    }
    let mut cost: f64 = passes.iter().map(|pass| pass.cost).sum();
    let mut recent = vec![cost];
    let mut damping = 1e-3_f64;
    let mut exhausted = false;
    let mut trial_slot = vec![usize::MAX; realization_states.len()];
    for (position, block) in objective.blocks.iter().enumerate() {
        trial_slot[block.state_index] = position;
    }
    for _ in 0..options.body_iterations.max(1) {
        let Some(step) = solve_body_step(&passes, &layouts, damping) else {
            damping = (damping * 10.0).min(1e6);
            if damping >= 1e6 {
                break;
            }
            continue;
        };
        let factor = step_factor(&step);
        let mut trial_body = body_state.clone();
        let mut trial_states: Vec<RealizationState> = objective
            .blocks
            .iter()
            .map(|block| realization_states[block.state_index].clone())
            .collect();
        if !fixed_body && !apply_shared_step(&layouts[0], &mut trial_body, &step.shared, factor) {
            damping = (damping * 10.0).min(1e6);
            continue;
        }
        for (position, (block, local)) in objective.blocks.iter().zip(&step.locals).enumerate() {
            apply_local_step(&block.layout, &mut trial_states[position], local, factor);
        }
        if *remaining < samples {
            exhausted = true;
            break;
        }
        *remaining -= samples;
        let trial_passes = body_pass_objective(
            &objective,
            body,
            &trial_body,
            |index| {
                let slot = trial_slot[index];
                if slot == usize::MAX {
                    &realization_states[index]
                } else {
                    &trial_states[slot]
                }
            },
            looks,
            firmware,
            sigma,
            contributors,
            options,
            &precision,
            fixed_body,
        )?;
        evaluations += 1;
        let trial_cost: f64 = trial_passes.iter().map(|pass| pass.cost).sum();
        let trial_feasible = trial_passes.iter().all(|pass| !pass.infeasible);
        if trial_feasible && trial_cost <= cost + 1e-12 {
            *body_state = trial_body;
            for (block, state) in objective.blocks.iter().zip(trial_states) {
                realization_states[block.state_index] = state;
            }
            passes = trial_passes;
            cost = trial_cost;
            recent.push(cost);
            if recent.len() > crate::types::PLATEAU_SAMPLES {
                recent.remove(0);
            }
            damping = (damping * 0.4).max(1e-9);
            if fit::converged(&recent) {
                break;
            }
        } else {
            damping = (damping * 10.0).min(1e6);
            if damping >= 1e6 {
                break;
            }
        }
    }
    let decrement = body_decrement(&passes, &layouts);
    let plateau = fit::converged(&recent)
        || decrement.is_some_and(|value| value <= fit::STATIONARY_DECREMENT_NATS);
    for block in &objective.blocks {
        realization_states[block.state_index].damping = damping;
        if let Some(template) = &block.template {
            let state = &realization_states[block.state_index];
            let mut params = Params::initial(
                block.problem.layout.clone(),
                block.problem.contributors.len(),
            );
            for (index, value) in state.coefficients.iter().enumerate() {
                params.theta[block.problem.layout.response.start + index] = *value;
            }
            let response = fit::build_control_response(
                template.clone(),
                &block.problem,
                &params,
                block.problem.parameter_ids.clone(),
                Vec::new(),
            )
            .model;
            realization_states[block.state_index].response = Some(response);
        }
    }
    for pass in &passes {
        for (slot, list) in pairs.iter_mut().enumerate() {
            list.extend(pass.pairs[slot].iter().copied());
        }
    }
    Ok(BodyOutcome {
        pairs,
        evaluations,
        decrement,
        exhausted,
        plateau,
    })
}

/// A diagonal Loewner lower bound on the six-parameter `Δ` prior precision.
fn delta_floor(precision: &[[f64; 6]; 6]) -> f64 {
    let mut floor = f64::INFINITY;
    for (row, values) in precision.iter().enumerate() {
        let mut bound = values[row];
        for (column, value) in values.iter().enumerate() {
            if column != row {
                bound -= value.abs();
            }
        }
        floor = floor.min(bound);
    }
    floor.max(0.0)
}

fn body_floors(
    objective: &BodyObjective,
    precision: &[[f64; 6]; 6],
    options: &HierOptions,
) -> (Vec<f64>, Vec<Vec<f64>>) {
    let s = objective.shared_total;
    let mut shared = vec![0.0_f64; s];
    if s >= 6 {
        let delta = delta_floor(precision);
        for value in &mut shared[..6] {
            *value = delta;
        }
    }
    let layout = &objective.blocks[0].layout;
    for index in layout.xi.iter().flatten() {
        shared[*index] =
            1.0 / (options.fit.exposure_mode_prior_sd * options.fit.exposure_mode_prior_sd);
    }
    for pair in &layout.omega {
        for index in pair.iter().flatten() {
            shared[*index] = 1.0 / (options.fit.wb_prior_sd * options.fit.wb_prior_sd);
        }
    }
    let exposure = 1.0 / (options.fit.exposure_prior_sd * options.fit.exposure_prior_sd);
    let wb = 1.0 / (options.fit.wb_prior_sd * options.fit.wb_prior_sd);
    let (_, clip_sd) = options.fit.clip_prior;
    let locals = objective
        .blocks
        .iter()
        .map(|block| {
            let mut floor = vec![0.0_f64; block.layout.local_total()];
            floor[block.layout.clip - s] = 1.0 / (clip_sd * clip_sd);
            for columns in &block.layout.image {
                floor[columns[0] - s] = exposure;
                floor[columns[1] - s] = wb;
                floor[columns[2] - s] = wb;
            }
            for index in &block.layout.response {
                floor[*index - s] = RESPONSE_PRIOR_PRECISION;
            }
            floor
        })
        .collect();
    (shared, locals)
}

fn certificate_work(
    passes: &[BodyPass],
    layouts: &[BodyLayout],
    residual: &[f64],
) -> CertificateWork {
    let dim = layouts.first().map_or(0, BodyLayout::shared_total)
        + layouts.iter().map(BodyLayout::local_total).sum::<usize>();
    let gradient = passes
        .iter()
        .flat_map(|pass| pass.gradient.iter())
        .fold(0.0_f64, |acc, value| acc.max(value.abs()));
    let residual = residual
        .iter()
        .fold(0.0_f64, |acc, value| acc.max(value.abs()));
    CertificateWork {
        iterations: 0,
        relative_residual: Some(residual / gradient.max(1.0)),
        dim,
        residual_coordinates: 0,
    }
}

/// Forward relative error bound for `terms` sequential floating-point operations.
fn roundoff_gamma(terms: usize) -> f64 {
    let product = terms as f64 * (f64::EPSILON / 2.0);
    if product >= 1.0 {
        f64::INFINITY
    } else {
        product / (1.0 - product)
    }
}

/// Stationarity interval of the complete joint body block at one accepted state.
fn certify_body_pass(
    passes: &[BodyPass],
    layouts: &[BodyLayout],
    floors_shared: &[f64],
    floors_local: &[Vec<f64>],
) -> StationarityReport {
    let report = |certificate: StationarityCertificate,
                  work: Option<CertificateWork>,
                  bounds: Option<StationarityBounds>| StationarityReport {
        origin: CertificateOrigin::PostFitRecertification,
        certificate,
        work,
        bounds,
    };
    let Some(step) = solve_body_step(passes, layouts, 0.0) else {
        return report(
            StationarityCertificate::Unresolved {
                termination: CertificateTermination::Factorization,
            },
            None,
            None,
        );
    };
    let s = layouts[0].shared_total();
    let mut delta_inf = step
        .shared
        .iter()
        .fold(0.0_f64, |acc, value| acc.max(value.abs()));
    for local in &step.locals {
        delta_inf = local
            .iter()
            .fold(delta_inf, |acc, value| acc.max(value.abs()));
    }
    let mut residual = Vec::new();
    let mut correction = 0.0;
    let mut first = 0.0;
    let mut quadratic = 0.0;
    let mut energy_scale = 0.0;
    let mut first_order_error = 0.0;
    let mut floor_min = f64::INFINITY;
    let mut gradient = vec![0.0_f64; s];
    for pass in passes {
        for (index, value) in pass.gradient.iter().take(s).enumerate() {
            gradient[index] += *value;
        }
    }
    let mut err_gradient = vec![0.0_f64; s];
    let mut gradient_norm_sq = 0.0;
    let mut err_gradient_norm_sq = 0.0;
    for index in 0..s {
        let scale: f64 = passes.iter().map(|pass| pass.gradient[index].abs()).sum();
        let block_error: f64 = passes.iter().map(|pass| pass.gradient_error[index]).sum();
        err_gradient[index] = roundoff_gamma(passes.len()) * scale + block_error;
        gradient_norm_sq += gradient[index] * gradient[index];
        err_gradient_norm_sq += err_gradient[index] * err_gradient[index];
    }
    for pass in passes {
        for value in &pass.gradient[s..] {
            gradient_norm_sq += *value * *value;
        }
        for value in &pass.gradient_error[s..] {
            err_gradient_norm_sq += *value * *value;
        }
    }
    for index in 0..s {
        let mut curvature = 0.0;
        let mut curvature_scale = 0.0;
        let mut terms = 0usize;
        for (position, (pass, layout)) in passes.iter().zip(layouts).enumerate() {
            let n = layout.total;
            for other in 0..s {
                let product = pass.hessian[index * n + other] * step.shared[other];
                curvature += product;
                curvature_scale += product.abs();
                terms += 1;
            }
            for other in 0..layout.local_total() {
                let product = pass.hessian[index * n + s + other] * step.locals[position][other];
                curvature += product;
                curvature_scale += product.abs();
                terms += 1;
            }
        }
        let input_error: f64 = passes
            .iter()
            .map(|pass| roundoff_gamma(pass.hessian_terms) * pass.hessian_abs[index] * delta_inf)
            .sum();
        let err_curvature = roundoff_gamma(terms) * curvature_scale + input_error;
        let value = -gradient[index] - curvature;
        residual.push(value);
        let floor = floors_shared[index] * (1.0 - 8.0 * f64::EPSILON);
        if floor <= 0.0 || !floor.is_finite() {
            return report(
                StationarityCertificate::Unresolved {
                    termination: CertificateTermination::NoPriorFloor,
                },
                None,
                None,
            );
        }
        floor_min = floor_min.min(floor);
        let err_value = err_gradient[index] + err_curvature;
        let value_error = value.abs() + err_value;
        quadratic += step.shared[index] * curvature;
        first += step.shared[index] * value;
        correction += value_error * value_error / floor;
        energy_scale += 0.5 * step.shared[index].abs() * (curvature.abs() + err_curvature)
            + step.shared[index].abs() * value_error;
        first_order_error +=
            0.5 * step.shared[index].abs() * err_curvature + step.shared[index].abs() * err_value;
    }
    for (position, (pass, layout)) in passes.iter().zip(layouts).enumerate() {
        let n = layout.total;
        for index in 0..layout.local_total() {
            let mut curvature = 0.0;
            let mut curvature_scale = 0.0;
            let mut terms = 0usize;
            for other in 0..s {
                let product = pass.hessian[(s + index) * n + other] * step.shared[other];
                curvature += product;
                curvature_scale += product.abs();
                terms += 1;
            }
            for other in 0..layout.local_total() {
                let product =
                    pass.hessian[(s + index) * n + s + other] * step.locals[position][other];
                curvature += product;
                curvature_scale += product.abs();
                terms += 1;
            }
            let err_curvature = roundoff_gamma(terms) * curvature_scale
                + roundoff_gamma(pass.hessian_terms) * pass.hessian_abs[s + index] * delta_inf;
            let err_value = pass.gradient_error[s + index] + err_curvature;
            let value = -pass.gradient[s + index] - curvature;
            residual.push(value);
            let floor = floors_local[position][index] * (1.0 - 8.0 * f64::EPSILON);
            if floor <= 0.0 || !floor.is_finite() {
                return report(
                    StationarityCertificate::Unresolved {
                        termination: CertificateTermination::NoPriorFloor,
                    },
                    None,
                    None,
                );
            }
            floor_min = floor_min.min(floor);
            let value_error = value.abs() + err_value;
            quadratic += step.locals[position][index] * curvature;
            first += step.locals[position][index] * value;
            correction += value_error * value_error / floor;
            energy_scale +=
                0.5 * step.locals[position][index].abs() * (curvature.abs() + err_curvature)
                    + step.locals[position][index].abs() * value_error;
            first_order_error += 0.5 * step.locals[position][index].abs() * err_curvature
                + step.locals[position][index].abs() * err_value;
        }
    }
    let dim = s + layouts.iter().map(BodyLayout::local_total).sum::<usize>();
    let arithmetic_error = roundoff_gamma(4 * dim.max(1)) * energy_scale;
    let gradient_error_energy = if floor_min.is_finite() && floor_min > 0.0 {
        let norm = gradient_norm_sq.sqrt();
        let error = err_gradient_norm_sq.sqrt();
        (norm * error + 0.5 * error * error) / floor_min
    } else {
        f64::INFINITY
    };
    let lower = (0.5 * quadratic + first).max(0.0);
    let lower_enclosed =
        (lower - gradient_error_energy - arithmetic_error - first_order_error).max(0.0);
    let upper_enclosed =
        lower + 0.5 * correction + gradient_error_energy + arithmetic_error + first_order_error;
    if !(lower_enclosed.is_finite() && upper_enclosed.is_finite()) {
        return report(
            StationarityCertificate::Unresolved {
                termination: CertificateTermination::Nonfinite,
            },
            None,
            None,
        );
    }
    let work = Some(certificate_work(passes, layouts, &residual));
    let bounds = match (Nats::checked(lower_enclosed), Nats::checked(upper_enclosed)) {
        (Some(lower), Some(upper)) => Some(StationarityBounds { lower, upper }),
        _ => None,
    };
    let certificate = if lower_enclosed > fit::STATIONARY_DECREMENT_NATS {
        match Nats::checked(lower_enclosed) {
            Some(lower) => StationarityCertificate::CertifiedNonstationary { lower },
            None => StationarityCertificate::Unresolved {
                termination: CertificateTermination::Nonfinite,
            },
        }
    } else if upper_enclosed <= fit::STATIONARY_DECREMENT_NATS {
        match Nats::checked(upper_enclosed) {
            Some(upper) => StationarityCertificate::CertifiedStationary { upper },
            None => StationarityCertificate::Unresolved {
                termination: CertificateTermination::Nonfinite,
            },
        }
    } else {
        StationarityCertificate::Unresolved {
            termination: CertificateTermination::BoundNotDecisive,
        }
    };
    report(certificate, work, bounds)
}

fn certificate_lower(report: &StationarityReport) -> f64 {
    match report.certificate {
        StationarityCertificate::CertifiedNonstationary { lower } => lower.get(),
        _ => report.bounds.map_or(0.0, |bounds| bounds.lower.get()),
    }
}

fn certificate_upper(report: &StationarityReport) -> Option<f64> {
    match report.certificate {
        StationarityCertificate::CertifiedStationary { upper } => Some(upper.get()),
        StationarityCertificate::CertifiedNonstationary { .. } => None,
        StationarityCertificate::Unresolved { .. } => {
            report.bounds.map(|bounds| bounds.upper.get())
        }
    }
}

/// Complete body-block certificate over every body: additive energy bounds.
fn combine_body_certificates(reports: Vec<StationarityReport>) -> StationarityReport {
    let lower: f64 = reports.iter().map(certificate_lower).sum();
    let uppers: Option<Vec<f64>> = reports.iter().map(certificate_upper).collect();
    let upper = uppers.map(|values| values.iter().sum::<f64>());
    let bounds = upper.and_then(|upper| {
        Some(StationarityBounds {
            lower: Nats::checked(lower)?,
            upper: Nats::checked(upper)?,
        })
    });
    let certificate = if lower > fit::STATIONARY_DECREMENT_NATS {
        Nats::checked(lower).map_or(
            StationarityCertificate::Unresolved {
                termination: CertificateTermination::Nonfinite,
            },
            |lower| StationarityCertificate::CertifiedNonstationary { lower },
        )
    } else if upper.is_some_and(|upper| upper <= fit::STATIONARY_DECREMENT_NATS) {
        upper.and_then(Nats::checked).map_or(
            StationarityCertificate::Unresolved {
                termination: CertificateTermination::Nonfinite,
            },
            |upper| StationarityCertificate::CertifiedStationary { upper },
        )
    } else {
        let termination = reports
            .iter()
            .find_map(|report| match report.certificate {
                StationarityCertificate::Unresolved { termination } => Some(termination),
                _ => None,
            })
            .unwrap_or(CertificateTermination::BoundNotDecisive);
        StationarityCertificate::Unresolved { termination }
    };
    let work = reports.iter().filter_map(|report| report.work).fold(
        CertificateWork {
            iterations: 0,
            relative_residual: None,
            dim: 0,
            residual_coordinates: 0,
        },
        |mut total, work| {
            total.iterations += work.iterations;
            total.dim += work.dim;
            total.residual_coordinates += work.residual_coordinates;
            total.relative_residual = match (total.relative_residual, work.relative_residual) {
                (Some(left), Some(right)) => Some(left.max(right)),
                (left, right) => left.or(right),
            };
            total
        },
    );
    StationarityReport {
        origin: CertificateOrigin::PostFitRecertification,
        certificate,
        work: (work.dim > 0).then_some(work),
        bounds,
    }
}

#[allow(clippy::too_many_arguments)]
fn certify_bodies(
    input: &HierInput,
    state_indices: &[Vec<usize>],
    body_states: &[BodyState],
    realization_states: &[RealizationState],
    looks: &BTreeMap<LookId, Look>,
    firmware: &BTreeMap<FamilyFirmwareId, [f64; FIRMWARE_PARAMETERS]>,
    groups: &[Vec<usize>],
    gauges: &[ComponentGauge],
    designated: &[bool],
    sigma: &[f64],
    contributors: &[ContributorId],
    options: &HierOptions,
    include_recipe: bool,
    remaining: &mut f64,
) -> StationarityReport {
    let fixed_body = options.body_pin.fixed();
    let mut reports = Vec::new();
    for (body_index, body) in input.bodies.iter().enumerate() {
        let Some(component) = groups
            .iter()
            .position(|members| members.contains(&body_index))
        else {
            continue;
        };
        let gauge = &gauges[component];
        let mut states = realization_states.to_vec();
        let mut body_state = body_states[body_index].clone();
        let precision = match body.transform.covariance().precision() {
            Ok(precision) => precision,
            Err(_) => {
                reports.push(StationarityReport {
                    origin: CertificateOrigin::PostFitRecertification,
                    certificate: StationarityCertificate::Unresolved {
                        termination: CertificateTermination::Nonfinite,
                    },
                    work: None,
                    bounds: None,
                });
                continue;
            }
        };
        let objective = match build_body_objective(
            body,
            &state_indices[body_index],
            &mut states,
            &mut body_state,
            gauge,
            designated[body_index],
            options,
            include_recipe,
            fixed_body,
        ) {
            Ok(objective) => objective,
            Err(_) => {
                reports.push(StationarityReport {
                    origin: CertificateOrigin::PostFitRecertification,
                    certificate: StationarityCertificate::Unresolved {
                        termination: CertificateTermination::InvalidObjective,
                    },
                    work: None,
                    bounds: None,
                });
                continue;
            }
        };
        if objective.blocks.is_empty() {
            continue;
        }
        if *remaining < objective.sample_cost {
            reports.push(StationarityReport {
                origin: CertificateOrigin::PostFitRecertification,
                certificate: StationarityCertificate::Unresolved {
                    termination: CertificateTermination::WorkLimit,
                },
                work: None,
                bounds: None,
            });
            continue;
        }
        *remaining -= objective.sample_cost;
        let layouts: Vec<BodyLayout> = objective
            .blocks
            .iter()
            .map(|block| block.layout.clone())
            .collect();
        let passes = match body_pass_objective(
            &objective,
            body,
            &body_state,
            |index| &states[index],
            looks,
            firmware,
            sigma,
            contributors,
            options,
            &precision,
            fixed_body,
        ) {
            Ok(passes) => passes,
            Err(_) => {
                reports.push(StationarityReport {
                    origin: CertificateOrigin::PostFitRecertification,
                    certificate: StationarityCertificate::Unresolved {
                        termination: CertificateTermination::InvalidObjective,
                    },
                    work: None,
                    bounds: None,
                });
                continue;
            }
        };
        if passes.iter().any(|pass| pass.infeasible) {
            reports.push(StationarityReport {
                origin: CertificateOrigin::PostFitRecertification,
                certificate: StationarityCertificate::Unresolved {
                    termination: CertificateTermination::InvalidObjective,
                },
                work: None,
                bounds: None,
            });
            continue;
        }
        let (floors_shared, floors_local) = body_floors(&objective, &precision, options);
        reports.push(certify_body_pass(
            &passes,
            &layouts,
            &floors_shared,
            &floors_local,
        ));
    }
    if reports.is_empty() {
        return StationarityReport {
            origin: CertificateOrigin::PostFitRecertification,
            certificate: StationarityCertificate::Unresolved {
                termination: CertificateTermination::InvalidObjective,
            },
            work: None,
            bounds: None,
        };
    }
    combine_body_certificates(reports)
}

/// Recertify every family against the finalized body, firmware and gauge state.
#[allow(clippy::too_many_arguments)]
fn certify_families(
    input: &HierInput,
    state_indices: &[Vec<usize>],
    body_states: &[BodyState],
    realization_states: &[RealizationState],
    firmware: &BTreeMap<FamilyFirmwareId, [f64; FIRMWARE_PARAMETERS]>,
    groups: &[Vec<usize>],
    gauges: &[ComponentGauge],
    sigma: &[f64],
    contributors: &[ContributorId],
    family_theta: &BTreeMap<LookId, Vec<f64>>,
    family_lambdas: &BTreeMap<LookId, StageLambdas>,
    family_results: &BTreeMap<LookId, fit::LookOnlyResult>,
    options: &HierOptions,
    evaluations: &mut usize,
    remaining: &mut f64,
) -> Result<BTreeMap<LookId, StationarityReport>> {
    let mut reports = BTreeMap::new();
    for family in &input.families {
        let family_id = &family.target.family;
        let Some(result) = family_results.get(family_id) else {
            continue;
        };
        let designated_body = family_designated(&input.bodies, groups, gauges, family_id);
        let pool = family_pool(
            family_id,
            None,
            &input.bodies,
            state_indices,
            body_states,
            realization_states,
            false,
            designated_body.as_deref(),
            firmware,
        );
        if pool.images.is_empty() {
            continue;
        }
        let theta = family_theta.get(family_id).map(Vec::as_slice);
        let lambdas = family_lambdas
            .get(family_id)
            .copied()
            .unwrap_or(options.fit.initial_lambdas);
        let report = match fit::certify_look_rows(
            &pool.images,
            &options.fit,
            Some(&result.look),
            pool.nominal_clip.unwrap_or(0.03),
            &pool.rows,
            theta,
            lambdas,
            &sigma_pairs(contributors, sigma),
            evaluations,
            remaining,
        ) {
            Ok(report) => report,
            Err(_) => StationarityReport {
                origin: CertificateOrigin::PostFitRecertification,
                certificate: StationarityCertificate::Unresolved {
                    termination: CertificateTermination::InvalidObjective,
                },
                work: None,
                bounds: None,
            },
        };
        reports.insert(family_id.clone(), report);
    }
    Ok(reports)
}

fn certified_reports(
    reports: &BTreeMap<LookId, StationarityReport>,
) -> (Option<StationarityReport>, bool) {
    let certificate = reports
        .values()
        .copied()
        .find(|report| !report.certificate.is_certified_stationary())
        .or_else(|| reports.values().next().copied());
    let certified = !reports.is_empty()
        && reports
            .values()
            .all(|report| report.certificate.is_certified_stationary());
    (certificate, certified)
}

fn apply_component_gauges(
    body_states: &mut [BodyState],
    groups: &[Vec<usize>],
    gauges: &[ComponentGauge],
    bodies: &[BodyData],
    include_recipe: bool,
    recenter_delta: bool,
) {
    for (component, members) in groups.iter().enumerate() {
        let gauge = &gauges[component];
        for index in members {
            let designated = bodies[*index].id.as_str() == gauge.designated.as_str();
            enforce_body_gauge(&mut body_states[*index], gauge, designated);
        }
        if recenter_delta && !gauge.anchored_delta {
            apply_component_gauge(body_states, members, bodies, include_recipe);
        }
    }
}

fn apply_delta_gauge(states: &mut [BodyState], active: &[Vec<usize>]) {
    let total: f64 = active.iter().map(|indices| indices.len() as f64).sum();
    if total <= 0.0 {
        return;
    }
    let mut mean = [0.0_f64; 6];
    for (state, indices) in states.iter().zip(active.iter()) {
        let weight = indices.len() as f64;
        for (slot, value) in mean.iter_mut().zip(state.delta.params().iter()) {
            *slot += weight * value / total;
        }
    }
    for state in states.iter_mut() {
        let mut params = state.delta.params();
        for (value, shift) in params.iter_mut().zip(mean.iter()) {
            *value -= shift;
        }
        if let Ok(delta) = Delta6::new(params) {
            state.delta = delta;
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn apply_component_gauge(
    body_states: &mut [BodyState],
    members: &[usize],
    bodies: &[BodyData],
    include_recipe: bool,
) {
    let active: Vec<Vec<usize>> = members
        .iter()
        .map(|body_index| {
            let count = bodies[*body_index]
                .realizations
                .iter()
                .flat_map(|realization| realization.images.iter())
                .filter(|tag| include_recipe || tag.image.intent == ImageIntent::Native)
                .count();
            (0..count).collect()
        })
        .collect();
    let mut local: Vec<BodyState> = members
        .iter()
        .map(|index| body_states[*index].clone())
        .collect();
    apply_delta_gauge(&mut local, &active);
    for (position, index) in members.iter().enumerate() {
        body_states[*index] = local[position].clone();
    }
}

fn update_sigma(sigma: &mut [f64], pairs: &[Vec<(f64, f64)>], options: &FitOptions) {
    let (a, b) = options.sigma_prior;
    let nu = options.nu;
    for (index, list) in pairs.iter().enumerate() {
        if list.is_empty() {
            continue;
        }
        let mut low = -6.0_f64;
        let mut high = 6.0_f64;
        for _ in 0..48 {
            let mid = 0.5 * (low + high);
            let variance = libm::exp(2.0 * mid);
            let mut derivative = 2.0 * a - 2.0 * b * libm::exp(-2.0 * mid);
            for (weight, squared) in list {
                let u = squared / (nu * variance);
                derivative += weight * (3.0 - (nu + 3.0) * u / (1.0 + u));
            }
            if derivative > 0.0 {
                high = mid;
            } else {
                low = mid;
            }
        }
        sigma[index] = 0.5 * (low + high);
    }
}

fn require_feasible_projection(evaluated: &crate::predict::EvalOutput, what: &str) -> Result<()> {
    if evaluated.penalty.is_some() {
        return Ok(());
    }
    Err(FitError::Hierarchy(match evaluated.violation {
        Some(violation) => format!("{what} sample left the inverse domain: {violation}"),
        None => format!("{what} sample left the inverse domain"),
    }))
}

/// Per-image projections of the residual onto the firmware-offset basis, for
/// [`crate::detect_split`]; a retained sample outside the inverse domain is an error.
pub fn firmware_images(
    look: &Look,
    images: &[FitImage],
    nuisances: &[ImageNuisances],
    firmware: &[FirmwareId],
    space: looklab_color::RgbSpace,
    clips: &[f64],
    options: &FitOptions,
) -> Result<Vec<crate::epoch::ImageProjection>> {
    if images.len() != nuisances.len()
        || images.len() != firmware.len()
        || images.len() != clips.len()
    {
        return Err(FitError::Hierarchy(
            "images, nuisances, firmware tags and clips must have equal length".to_string(),
        ));
    }
    let Ok(problem) = Problem::new_with(images, None, options) else {
        return Err(FitError::Hierarchy(
            "firmware projection pool is empty".to_string(),
        ));
    };
    let layout = problem.layout.clone();
    let slopes = tone_slopes(look);
    let mut per_image: Vec<Vec<crate::epoch::SampleProjection>> = vec![Vec::new(); images.len()];
    let mut jac = Jac::default();
    for sample in &problem.samples {
        let index = sample.image as usize;
        let nuisance = nuisances[index];
        let context = ImageCtx {
            transform: images[index].transform,
            a0: images[index].transform,
            space,
            exposure_scale: libm::pow(2.0, nuisance.exposure),
            wb_r: libm::exp(nuisance.wb_log_r),
            wb_b: libm::exp(nuisance.wb_log_b),
            tone_clamp: [crate::predict::ToneClamp::Interior; TONE_COEFS],
        };
        let evaluated = evaluate_sample(
            look,
            &layout,
            &context,
            &ColumnMap::default(),
            sample.rgb,
            clips[index],
            1.0,
            &slopes,
            &[],
            &sample.target,
            &mut jac,
        );
        require_feasible_projection(&evaluated, "firmware projection")?;
        let residual = evaluated.residual;
        let mut basis = [[0.0_f64; 3]; crate::epoch::PROJECTION_DIM];
        for (column, derivative) in jac.cols.iter().zip(jac.derivs.iter()) {
            let column = *column as usize;
            if column == layout.tone_c0 {
                for channel in 0..3 {
                    basis[0][channel] += derivative[channel];
                }
            } else if layout.g.contains(&column) {
                for channel in 0..3 {
                    basis[1][channel] += derivative[channel];
                }
            }
        }
        per_image[index].push(crate::epoch::SampleProjection {
            residual,
            basis,
            weight: sample.weight,
        });
    }
    let groups = firmware_groups(firmware);
    Ok(images
        .iter()
        .enumerate()
        .map(|(index, _)| crate::epoch::ImageProjection {
            group: groups[&firmware[index]],
            samples: per_image[index].clone(),
        })
        .collect())
}

fn firmware_groups(firmware: &[FirmwareId]) -> BTreeMap<FirmwareId, u32> {
    let mut ids: Vec<FirmwareId> = firmware.to_vec();
    ids.sort();
    ids.dedup();
    ids.into_iter()
        .enumerate()
        .map(|(index, id)| (id, index as u32))
        .collect()
}

/// Firmware basis at every v1 probe, for the predicted-difference effect gate.
pub fn firmware_probe_basis(look: &Look) -> Vec<[[f64; 3]; FIRMWARE_PARAMETERS]> {
    let layout = Layout::with_response(1, 0, 1, 0);
    let slopes = tone_slopes(look);
    let probes = looklab_model::probes::v1();
    let mut jac = Jac::default();
    let mut out = Vec::with_capacity(probes.probes.len());
    for probe in &probes.probes {
        let _ = crate::predict::evaluate_probe(look, &layout, probe.xyz, &slopes, &mut jac);
        let mut basis = [[0.0_f64; 3]; FIRMWARE_PARAMETERS];
        for (column, derivative) in jac.cols.iter().zip(jac.derivs.iter()) {
            let column = *column as usize;
            if column == layout.tone_c0 {
                for channel in 0..3 {
                    basis[0][channel] += derivative[channel];
                }
            } else if layout.g.contains(&column) {
                for channel in 0..3 {
                    basis[1][channel] += derivative[channel];
                }
            }
        }
        out.push(basis);
    }
    out
}

struct FamilyPool<'a> {
    images: Vec<FitImage>,
    rows: Vec<RowModel<'a>>,
    clips: Vec<f64>,
    groups: Vec<BodyGroup>,
    firmware: Vec<FirmwareId>,
    nominal_clip: Option<f64>,
}

#[allow(clippy::too_many_arguments)]
fn family_pool<'a>(
    family: &LookId,
    exact_target: Option<&FamilyTargetId>,
    bodies: &[BodyData],
    state_indices: &[Vec<usize>],
    body_states: &[BodyState],
    realization_states: &'a [RealizationState],
    include_recipe: bool,
    designated: Option<&str>,
    firmware_offsets: &BTreeMap<FamilyFirmwareId, [f64; FIRMWARE_PARAMETERS]>,
) -> FamilyPool<'a> {
    let mut images = Vec::new();
    let mut rows = Vec::new();
    let mut clips = Vec::new();
    let mut groups = Vec::new();
    let mut firmware = Vec::new();
    let mut nominal_clip = None;
    let mut group = 0usize;
    for (body_index, body) in bodies.iter().enumerate() {
        let body_state = &body_states[body_index];
        let mut indices = Vec::new();
        for (realization_index, realization) in body.realizations.iter().enumerate() {
            if &realization.target.family != family {
                continue;
            }
            if let Some(exact) = exact_target
                && &realization.target != exact
            {
                continue;
            }
            let state_index = state_indices[body_index][realization_index];
            let state = &realization_states[state_index];
            let mut included = false;
            for (index, tag) in realization.images.iter().enumerate() {
                if !include_recipe && tag.image.intent != ImageIntent::Native {
                    continue;
                }
                let omega = body_state.omega(tag.wb_mode);
                let nuisance = state.nuisances[index];
                let gain = Mat3::diag([
                    libm::exp(omega[0] + nuisance[1]),
                    1.0,
                    libm::exp(omega[1] + nuisance[2]),
                ]);
                let mut image = tag.image.clone();
                if let Some(weights) = image.stage_weights.look() {
                    image.trust_weight = weights.trust_weight;
                    image.sample_trust = weights.sample_trust.clone();
                }
                image.transform = tag
                    .image
                    .transform
                    .mul(&body_state.delta.transform())
                    .mul(&gain);
                image.exposure_hint =
                    body_state.xi(image.exposure_mode) + nuisance[0].clamp(-8.0, 8.0);
                image.exposure_mode = ExposureModeId(0);
                indices.push(images.len());
                images.push(image);
                rows.push(RowModel {
                    group,
                    model: state.response.as_ref(),
                    clip_k: state.clip,
                    firmware: firmware_offsets
                        .get(&FamilyFirmwareId {
                            family: family.clone(),
                            firmware: tag.firmware.clone(),
                        })
                        .copied()
                        .unwrap_or([0.0; 2]),
                    eligible_transform: tag.image.transform,
                    eligible_hint: tag.image.exposure_hint.clamp(-8.0, 8.0),
                });
                clips.push(state.clip);
                firmware.push(tag.firmware.clone());
                included = true;
            }
            if included {
                let is_designated = designated.is_some_and(|id| body.id.as_str() == id);
                if nominal_clip.is_none() || is_designated {
                    nominal_clip = Some(state.clip);
                }
                group += 1;
            }
        }
        if !indices.is_empty() {
            groups.push(BodyGroup {
                body: body.id.clone(),
                images: indices,
            });
        }
    }
    FamilyPool {
        images,
        rows,
        clips,
        groups,
        firmware,
        nominal_clip,
    }
}

#[allow(clippy::too_many_arguments)]
fn estimate_firmware(
    base_look: &Look,
    pool: &[FitImage],
    firmware: &[FirmwareId],
    clips: &[f64],
    sigma: &[f64],
    contributors: &[ContributorId],
    options: &FitOptions,
    firmware_prior_sd: [f64; FIRMWARE_PARAMETERS],
    remaining: &mut f64,
) -> Result<Vec<(FirmwareId, [f64; FIRMWARE_PARAMETERS])>> {
    if pool.is_empty() {
        return Ok(Vec::new());
    }
    let Ok(problem) = Problem::new_with(pool, None, options) else {
        return Ok(Vec::new());
    };
    let samples = problem.samples.len() as f64;
    if *remaining < samples {
        return Ok(Vec::new());
    }
    *remaining -= samples;
    let layout = problem.layout.clone();
    let slopes = tone_slopes(base_look);
    let mut blocks: BTreeMap<FirmwareId, ([[f64; 2]; 2], [f64; 2])> = BTreeMap::new();
    let mut jac = Jac::default();
    for sample in &problem.samples {
        let index = sample.image as usize;
        let context = ImageCtx {
            transform: pool[index].transform,
            a0: pool[index].transform,
            space: problem.images[index].space,
            exposure_scale: libm::pow(2.0, pool[index].exposure_hint),
            wb_r: 1.0,
            wb_b: 1.0,
            tone_clamp: [crate::predict::ToneClamp::Interior; TONE_COEFS],
        };
        let columns = ColumnMap::default();
        let evaluated = evaluate_sample(
            base_look,
            &layout,
            &context,
            &columns,
            sample.rgb,
            clips[index],
            1.0,
            &slopes,
            &[],
            &sample.target,
            &mut jac,
        );
        require_feasible_projection(&evaluated, "firmware estimate")?;
        let residual = evaluated.residual;
        let squared =
            residual[0] * residual[0] + residual[1] * residual[1] + residual[2] * residual[2];
        let contributor = contributor_index(contributors, pool[index].contributor);
        let variance = libm::exp(2.0 * sigma[contributor]);
        let alpha = (options.nu + 3.0) / (options.nu * variance + squared);
        let weight = sample.weight * alpha;
        let mut basis = [[0.0_f64; 3]; FIRMWARE_PARAMETERS];
        for (column, value) in jac.cols.iter().zip(jac.derivs.iter()) {
            let column = *column as usize;
            if column == layout.tone_c0 {
                for channel in 0..3 {
                    basis[0][channel] += value[channel];
                }
            } else if layout.g.contains(&column) {
                for channel in 0..3 {
                    basis[1][channel] += value[channel];
                }
            }
        }
        let mut derivative = [0.0_f64; FIRMWARE_PARAMETERS];
        for (index, row) in basis.iter().enumerate() {
            for channel in 0..3 {
                derivative[index] += row[channel] * residual[channel];
            }
        }
        let entry = blocks
            .entry(firmware[index].clone())
            .or_insert(([[0.0; 2]; 2], [0.0; 2]));
        for a in 0..2 {
            entry.1[a] += weight * derivative[a];
            for b in 0..2 {
                let mut dot = 0.0;
                for channel in 0..3 {
                    dot += basis[a][channel] * basis[b][channel];
                }
                entry.0[a][b] += weight * dot;
            }
        }
    }
    let mut out = Vec::new();
    for (id, (hessian, gradient)) in blocks {
        let mut matrix = hessian;
        let rhs = [-gradient[0], -gradient[1]];
        for (index, sd) in firmware_prior_sd.iter().enumerate() {
            matrix[index][index] += 1.0 / (sd * sd);
        }
        if let Some(solution) = solve_2x2(matrix, rhs) {
            out.push((id, solution));
        }
    }
    Ok(out)
}

fn solve_2x2(matrix: [[f64; 2]; 2], rhs: [f64; 2]) -> Option<[f64; 2]> {
    let determinant = matrix[0][0] * matrix[1][1] - matrix[0][1] * matrix[1][0];
    if determinant.abs() < 1e-18 {
        return None;
    }
    Some([
        (matrix[1][1] * rhs[0] - matrix[0][1] * rhs[1]) / determinant,
        (matrix[0][0] * rhs[1] - matrix[1][0] * rhs[0]) / determinant,
    ])
}

fn contributors_of(bodies: &[BodyData]) -> Vec<ContributorId> {
    let mut contributors = Vec::new();
    for body in bodies {
        for realization in &body.realizations {
            for tag in &realization.images {
                let id = tag.image.contributor;
                if !contributors.contains(&id) {
                    contributors.push(id);
                }
            }
        }
    }
    contributors
}

fn initial_sigma(contributors: &[ContributorId], shared: &[(ContributorId, f64)]) -> Vec<f64> {
    contributors
        .iter()
        .map(|id| {
            shared
                .iter()
                .find(|(candidate, _)| candidate == id)
                .map_or(libm::log(1.5), |(_, variance)| {
                    libm::log((*variance).max(1e-6))
                })
        })
        .collect()
}

fn sigma_pairs(contributors: &[ContributorId], sigma: &[f64]) -> Vec<(ContributorId, f64)> {
    contributors
        .iter()
        .copied()
        .zip(sigma.iter().copied())
        .collect()
}

fn state_indices(bodies: &[BodyData]) -> Vec<Vec<usize>> {
    let mut next = 0usize;
    bodies
        .iter()
        .map(|body| {
            body.realizations
                .iter()
                .map(|_| {
                    let index = next;
                    next += 1;
                    index
                })
                .collect()
        })
        .collect()
}

fn components(bodies: &[BodyData]) -> Vec<Vec<usize>> {
    let n = bodies.len();
    let mut parent: Vec<usize> = (0..n).collect();
    fn find(parent: &mut [usize], index: usize) -> usize {
        let mut root = index;
        while parent[root] != root {
            root = parent[root];
        }
        let mut current = index;
        while parent[current] != current {
            let next = parent[current];
            parent[current] = root;
            current = next;
        }
        root
    }
    let mut by_family: BTreeMap<LookId, usize> = BTreeMap::new();
    for (index, body) in bodies.iter().enumerate() {
        for realization in &body.realizations {
            match by_family.get(&realization.target.family) {
                Some(first) => {
                    let a = find(&mut parent, *first);
                    let b = find(&mut parent, index);
                    if a != b {
                        parent[a] = b;
                    }
                }
                None => {
                    by_family.insert(realization.target.family.clone(), index);
                }
            }
        }
    }
    let mut groups: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
    for index in 0..n {
        let root = find(&mut parent, index);
        groups.entry(root).or_default().push(index);
    }
    groups.into_values().collect()
}

fn designated_body(bodies: &[BodyData], members: &[usize]) -> usize {
    let anchored: Vec<usize> = members
        .iter()
        .copied()
        .filter(|index| bodies[*index].anchored())
        .collect();
    let candidates = if anchored.is_empty() {
        members
    } else {
        &anchored
    };
    candidates
        .iter()
        .copied()
        .max_by_key(|index| (bodies[*index].native_samples(), usize::MAX - *index))
        .unwrap_or(members[0])
}

fn gauge_bindings(
    bodies: &[BodyData],
    groups: &[Vec<usize>],
    persisted: &[ComponentGauge],
    scopes: &[GaugeScope],
) -> Result<Vec<GaugeBinding>> {
    let known = |body: &looklab_body::BodyId| {
        bodies
            .iter()
            .any(|candidate| candidate.id.as_str() == body.as_str())
    };
    for gauge in persisted {
        if !known(&gauge.designated) {
            return Err(FitError::MissingGauge(format!(
                "persisted gauge designates absent body {}",
                gauge.designated.as_str()
            )));
        }
    }
    for scope in scopes {
        if !known(&scope.gauge.designated) {
            return Err(FitError::MissingGauge(format!(
                "gauge scope designates absent body {}",
                scope.gauge.designated.as_str()
            )));
        }
    }
    let mut bindings = Vec::new();
    for members in groups {
        let found = persisted.iter().find(|gauge| {
            members
                .iter()
                .any(|index| bodies[*index].id.as_str() == gauge.designated.as_str())
        });
        let binding = match found {
            Some(gauge) => GaugeBinding {
                gauge: gauge.clone(),
                persisted: true,
            },
            None => {
                let scope_members = scopes.iter().any(|scope| {
                    scope.bodies.iter().any(|scope_body| {
                        members
                            .iter()
                            .any(|index| bodies[*index].id.as_str() == scope_body.as_str())
                    })
                });
                if scope_members {
                    return Err(FitError::MissingGauge(format!(
                        "persisted gauge reference left component [{}]",
                        members
                            .iter()
                            .map(|index| bodies[*index].id.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    )));
                }
                let chosen = designated_body(bodies, members);
                GaugeBinding {
                    gauge: ComponentGauge {
                        designated: looklab_body::BodyId::new(bodies[chosen].id.as_str())
                            .map_err(|error| FitError::Hierarchy(format!("gauge body: {error}")))?,
                        exposure_mode: bodies[chosen].default_mode,
                        wb_mode: bodies[chosen].default_wb_mode,
                        anchored_delta: members.iter().any(|index| bodies[*index].anchored()),
                    },
                    persisted: false,
                }
            }
        };
        bindings.push(binding);
    }
    Ok(bindings)
}

fn designated_bodies(bodies: &[BodyData], gauges: &[ComponentGauge]) -> Vec<bool> {
    let mut designated = vec![false; bodies.len()];
    for gauge in gauges {
        for (index, body) in bodies.iter().enumerate() {
            if body.id.as_str() == gauge.designated.as_str() {
                designated[index] = true;
            }
        }
    }
    designated
}

fn body_parameters(body: &BodyData, state: &BodyState) -> BodyParameters {
    let mut modes: Vec<ExposureModeId> = state.xi.iter().map(|(mode, _)| *mode).collect();
    modes.sort();
    modes.dedup();
    let exposure = modes
        .iter()
        .map(|mode| ExposureOffset {
            mode: *mode,
            offset: state.xi(*mode),
        })
        .collect();
    let mut classes: Vec<WbModeClass> = state.omega.iter().map(|(class, _)| *class).collect();
    classes.sort();
    classes.dedup();
    let wb = classes
        .iter()
        .map(|class| (*class, state.omega(*class)))
        .collect();
    BodyParameters {
        id: body.id.clone(),
        transform: body
            .transform
            .with_delta(state.delta, body.transform.covariance()),
        exposure,
        wb,
    }
}

fn realization_parameters(
    realization: &RealizationData,
    state: &RealizationState,
    recipe_fitted: bool,
) -> RealizationParameters {
    let images = realization
        .images
        .iter()
        .enumerate()
        .map(|(index, tag)| {
            let nuisance = state.nuisances.get(index).copied().unwrap_or([0.0; 3]);
            ImageNuisances {
                exposure: nuisance[0],
                wb_log_r: nuisance[1],
                wb_log_b: nuisance[2],
                exposure_mode: tag.image.exposure_mode,
                contributor: tag.image.contributor,
                weight: tag.image.trust_weight,
            }
        })
        .collect();
    RealizationParameters {
        id: realization.id.clone(),
        target: realization.target.clone(),
        clip_k: state.clip,
        images,
        model: state.response.clone(),
        recipe_fitted,
        damping: state.damping,
    }
}

fn look_provenance(
    body: &BodyData,
    body_state: &BodyState,
    realization: &RealizationData,
    state: &RealizationState,
) -> LookProvenance {
    let modes: BTreeSet<ExposureModeId> = realization
        .images
        .iter()
        .filter(|tag| tag.image.intent == ImageIntent::Native)
        .map(|tag| tag.image.exposure_mode)
        .collect();
    LookProvenance {
        body: body.id.clone(),
        realization: realization.id.clone(),
        target: realization.target.clone(),
        transform_version: body.transform.version(),
        delta: body_state.delta,
        exposure: modes
            .into_iter()
            .map(|mode| ExposureOffset {
                mode,
                offset: body_state.xi(mode),
            })
            .collect(),
        wb: body_state.omega.clone(),
        clip_k: state.clip,
        model_version: state.response.as_ref().map(|model| model.version),
    }
}

fn look_distance(a: &Look, b: &Look) -> f64 {
    let left = a.coef_vector();
    let right = b.coef_vector();
    if left.len() != right.len() {
        return f64::INFINITY;
    }
    left.iter()
        .zip(right.iter())
        .map(|(x, y)| (x - y).abs())
        .fold(0.0_f64, f64::max)
}

fn body_distance(a: &BodyState, b: &BodyState) -> f64 {
    let mut change = 0.0_f64;
    for (left, right) in a.delta.params().iter().zip(b.delta.params().iter()) {
        change = change.max((left - right).abs());
    }
    for ((_, left), (_, right)) in a.xi.iter().zip(b.xi.iter()) {
        change = change.max((left - right).abs());
    }
    for ((_, left), (_, right)) in a.omega.iter().zip(b.omega.iter()) {
        for channel in 0..2 {
            change = change.max((left[channel] - right[channel]).abs());
        }
    }
    change
}

fn realization_distance(a: &RealizationState, b: &RealizationState) -> f64 {
    let mut change = (a.clip - b.clip).abs();
    for (left, right) in a.nuisances.iter().zip(b.nuisances.iter()) {
        for channel in 0..3 {
            change = change.max((left[channel] - right[channel]).abs());
        }
    }
    for (left, right) in a.coefficients.iter().zip(b.coefficients.iter()) {
        change = change.max((left - right).abs());
    }
    change
}

fn has_recipe(bodies: &[BodyData]) -> bool {
    bodies.iter().any(|body| {
        body.realizations.iter().any(|realization| {
            realization
                .images
                .iter()
                .any(|tag| tag.image.intent == ImageIntent::Recipe)
        })
    })
}

fn validate(input: &HierInput) -> Result<()> {
    if input.bodies.is_empty() || input.families.is_empty() {
        return Err(FitError::NoImages);
    }
    let mut families = BTreeSet::new();
    let mut targets = BTreeSet::new();
    for family in &input.families {
        if !families.insert(family.target.family.clone()) {
            return Err(FitError::Hierarchy(format!(
                "family {} is declared more than once",
                family.target.family
            )));
        }
        targets.insert(family.target.clone());
    }
    for body in &input.bodies {
        if body.realizations.is_empty() {
            return Err(FitError::Hierarchy(format!(
                "body {} has no realizations",
                body.id
            )));
        }
        let mut seen = BTreeSet::new();
        for realization in &body.realizations {
            if realization.id.body != body.id {
                return Err(FitError::Hierarchy(format!(
                    "realization body {} does not match body {}",
                    realization.id.body, body.id
                )));
            }
            if !families.contains(&realization.target.family) {
                return Err(FitError::Hierarchy(format!(
                    "realization target {} has no declared family",
                    realization.target.family
                )));
            }
            if realization.images.is_empty() {
                return Err(FitError::NoImages);
            }
            if !seen.insert((realization.id.clone(), realization.target.clone())) {
                return Err(FitError::Hierarchy(format!(
                    "duplicate realization subject {:?}",
                    realization.id
                )));
            }
        }
    }
    Ok(())
}

fn fingerprint(input: &HierInput, options: &HierOptions) -> HierFingerprint {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    for family in &input.families {
        family.target.hash(&mut hasher);
        match &family.mode {
            FamilyMode::Fit { .. } => 0u8.hash(&mut hasher),
            FamilyMode::Pinned(_) => 1u8.hash(&mut hasher),
        }
    }
    for body in &input.bodies {
        body.id.hash(&mut hasher);
        format!("{:?}", body.transform).hash(&mut hasher);
        format!("{:?}", body.charts).hash(&mut hasher);
        for realization in &body.realizations {
            realization.id.hash(&mut hasher);
            realization.target.hash(&mut hasher);
            if let Some(model) = &realization.model {
                format!("{:?}", model).hash(&mut hasher);
            }
            realization.images.len().hash(&mut hasher);
            for tag in &realization.images {
                tag.firmware.body.hash(&mut hasher);
                tag.firmware.epoch.get().hash(&mut hasher);
                tag.firmware.version.as_str().hash(&mut hasher);
                tag.wb_mode.hash(&mut hasher);
                let image = &tag.image;
                image.contributor.hash(&mut hasher);
                image.exposure_mode.hash(&mut hasher);
                image.exposure_hint.to_bits().hash(&mut hasher);
                image.trust_weight.to_bits().hash(&mut hasher);
                (image.intent == ImageIntent::Recipe).hash(&mut hasher);
                format!("{:?}", image.settings).hash(&mut hasher);
                format!("{:?}", image.setting_source).hash(&mut hasher);
                format!("{:?}", image.samples.header()).hash(&mut hasher);
                for row in &image.transform.0 {
                    for value in row {
                        value.to_bits().hash(&mut hasher);
                    }
                }
                for sample in image.samples.in_rgb() {
                    for value in sample {
                        value.to_bits().hash(&mut hasher);
                    }
                }
                for sample in image.samples.out_rgb() {
                    for value in sample {
                        value.to_bits().hash(&mut hasher);
                    }
                }
                for value in image.samples.density_w() {
                    value.to_bits().hash(&mut hasher);
                }
                if let Some(trust) = &image.sample_trust {
                    trust.len().hash(&mut hasher);
                    for value in trust {
                        value.to_bits().hash(&mut hasher);
                    }
                }
                format!("{:?}", image.stage_weights).hash(&mut hasher);
            }
        }
    }
    format!("{:?}", options.fit).hash(&mut hasher);
    options.outer_iterations.hash(&mut hasher);
    options.body_iterations.hash(&mut hasher);
    format!("{:?}", options.body_pin).hash(&mut hasher);
    for value in options.firmware_prior_sd {
        value.to_bits().hash(&mut hasher);
    }
    format!("{:?}", options.anchor).hash(&mut hasher);
    format!("{:?}", options.gauges).hash(&mut hasher);
    format!("{:?}", options.gauge_scopes).hash(&mut hasher);
    for (id, variance) in &options.shared_sigma {
        id.hash(&mut hasher);
        variance.to_bits().hash(&mut hasher);
    }
    for (id, offset) in &options.firmware {
        id.hash(&mut hasher);
        for value in offset {
            value.to_bits().hash(&mut hasher);
        }
    }
    HierStateGeneration::CURRENT.0.hash(&mut hasher);
    env!("CARGO_PKG_VERSION").hash(&mut hasher);
    HierFingerprint(hasher.finish())
}

/// Stationarity evidence recorded in the diagnostics.
struct Certificates {
    family: Option<StationarityReport>,
    body: Option<StationarityReport>,
    native_body: Option<StationarityReport>,
    body_decrement: Option<f64>,
}

fn diagnostics(
    evaluations: usize,
    outer_iterations: usize,
    contributors: &[ContributorId],
    sigma: &[f64],
    converged: bool,
    certificates: Certificates,
    free_families: bool,
) -> HierDiagnostics {
    let Certificates {
        family: family_certificate,
        body: body_certificate,
        native_body: native_body_certificate,
        body_decrement,
    } = certificates;
    let mut limitations = Vec::new();
    if !converged {
        limitations.push(HierLimitation::NotConverged);
    }
    if free_families {
        match family_certificate {
            None => limitations.push(HierLimitation::NoFamilyCertificate),
            Some(report) if !report.certificate.is_certified_stationary() => {
                limitations.push(HierLimitation::FamilyNotStationary);
            }
            Some(_) => {}
        }
        limitations.push(HierLimitation::NominalClipForProbeUncertainty);
    }
    if body_certificate.is_none() && native_body_certificate.is_none() {
        limitations.push(HierLimitation::BodyCertificateUnavailable);
    }
    if body_certificate.is_some_and(|report| !report.certificate.is_certified_stationary()) {
        limitations.push(HierLimitation::BodyNotStationary);
    }
    if native_body_certificate.is_some_and(|report| !report.certificate.is_certified_stationary()) {
        limitations.push(HierLimitation::NativeBodyNotStationary);
    }
    HierDiagnostics {
        evaluations,
        outer_iterations,
        sigma_v: contributors
            .iter()
            .zip(sigma.iter())
            .map(|(id, log)| (*id, libm::exp(*log)))
            .collect(),
        converged,
        family_certificate,
        body_certificate,
        native_body_certificate,
        body_decrement,
        limitations,
    }
}

fn family_firmware_offsets(
    family: &LookId,
    tags: &[FirmwareId],
    firmware: &BTreeMap<FamilyFirmwareId, [f64; FIRMWARE_PARAMETERS]>,
) -> Vec<(FirmwareId, [f64; FIRMWARE_PARAMETERS])> {
    let mut out: Vec<(FirmwareId, [f64; FIRMWARE_PARAMETERS])> = Vec::new();
    for id in tags {
        if let Some(offset) = firmware.get(&FamilyFirmwareId {
            family: family.clone(),
            firmware: id.clone(),
        }) && !out.iter().any(|(known, _)| known == id)
        {
            out.push((id.clone(), *offset));
        }
    }
    out
}

fn family_designated(
    bodies: &[BodyData],
    groups: &[Vec<usize>],
    gauges: &[ComponentGauge],
    family: &LookId,
) -> Option<String> {
    groups
        .iter()
        .zip(gauges)
        .find(|(members, _)| {
            members.iter().any(|index| {
                bodies[*index]
                    .realizations
                    .iter()
                    .any(|realization| &realization.target.family == family)
            })
        })
        .map(|(_, gauge)| gauge.designated.as_str().to_owned())
}

/// Solver state restored from a resume snapshot.
struct ResumeTargets<'a> {
    body_states: &'a mut [BodyState],
    realization_states: &'a mut [RealizationState],
    sigma: &'a mut [f64],
    firmware: &'a mut BTreeMap<FamilyFirmwareId, [f64; FIRMWARE_PARAMETERS]>,
    family_theta: &'a mut BTreeMap<LookId, Vec<f64>>,
    family_lambdas: &'a mut BTreeMap<LookId, StageLambdas>,
}

fn resume_states(
    input: &HierInput,
    state_indices: &[Vec<usize>],
    state: &HierState,
    targets: ResumeTargets<'_>,
) {
    let ResumeTargets {
        body_states,
        realization_states,
        sigma,
        firmware,
        family_theta,
        family_lambdas,
    } = targets;
    for resume in &state.bodies {
        if let Some(index) = input.bodies.iter().position(|body| body.id == resume.id) {
            body_states[index] = BodyState::from_warm(&resume.state);
        }
    }
    for resume in &state.realizations {
        for (body_index, body) in input.bodies.iter().enumerate() {
            if let Some(realization_index) = body.realizations.iter().position(|realization| {
                realization.id == resume.id && realization.target == resume.target
            }) {
                let state_index = state_indices[body_index][realization_index];
                realization_states[state_index] = RealizationState::from_warm(
                    &resume.state,
                    body.realizations[realization_index].images.len(),
                );
            }
        }
    }
    for resume in &state.families {
        family_theta.insert(resume.target.family.clone(), resume.theta.clone());
        family_lambdas.insert(resume.target.family.clone(), resume.lambdas);
    }
    for (index, (_, variance)) in state.sigma.iter().enumerate() {
        if index < sigma.len() {
            sigma[index] = libm::log((*variance).max(1e-6));
        }
    }
    firmware.clear();
    firmware.extend(state.firmware.iter().cloned());
}

fn apply_body_pin(
    input: &HierInput,
    options: &HierOptions,
    body_states: &mut [BodyState],
) -> Result<()> {
    let Some(resumes) = options.body_pin.resumes() else {
        return Ok(());
    };
    for resume in resumes {
        let Some(index) = input.bodies.iter().position(|body| body.id == resume.id) else {
            return Err(FitError::Hierarchy(format!(
                "pinned body {} is absent",
                resume.id
            )));
        };
        body_states[index] = BodyState::from_warm(&resume.state);
    }
    for body in &input.bodies {
        if !resumes.iter().any(|resume| resume.id == body.id) {
            return Err(FitError::Hierarchy(format!(
                "body {} has no pinned body state",
                body.id
            )));
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn build_state(
    fingerprint: HierFingerprint,
    outer_iterations: usize,
    input: &HierInput,
    native_body_states: &[BodyState],
    native_realization_states: &[RealizationState],
    published_body_states: &[BodyState],
    published_realization_states: &[RealizationState],
    recipe_fitted: bool,
    family_results: &BTreeMap<LookId, fit::LookOnlyResult>,
    firmware: &BTreeMap<FamilyFirmwareId, [f64; FIRMWARE_PARAMETERS]>,
    contributors: &[ContributorId],
    sigma: &[f64],
    body_decrement: Option<f64>,
    families_certified: bool,
    converged: bool,
) -> HierState {
    let mut bodies = Vec::new();
    let mut published_bodies = Vec::new();
    for (index, body) in input.bodies.iter().enumerate() {
        bodies.push(BodyResume {
            id: body.id.clone(),
            state: native_body_states[index].to_warm(),
        });
        published_bodies.push(BodyResume {
            id: body.id.clone(),
            state: published_body_states[index].to_warm(),
        });
    }
    let mut realizations = Vec::new();
    let mut published_realizations = Vec::new();
    let mut state_index = 0usize;
    for body in &input.bodies {
        for realization in &body.realizations {
            realizations.push(RealizationResume {
                id: realization.id.clone(),
                target: realization.target.clone(),
                state: native_realization_states[state_index].to_warm(realization),
                recipe_fitted: false,
            });
            published_realizations.push(RealizationResume {
                id: realization.id.clone(),
                target: realization.target.clone(),
                state: published_realization_states[state_index].to_warm(realization),
                recipe_fitted,
            });
            state_index += 1;
        }
    }
    let families = family_results
        .iter()
        .map(|(family, result)| FamilyResume {
            target: FamilyTargetId {
                family: family.clone(),
                version: input
                    .families
                    .iter()
                    .find(|candidate| &candidate.target.family == family)
                    .map_or_else(
                        || {
                            looklab_schema::TargetVersion::new(1)
                                .expect("one is a positive target version")
                        },
                        |candidate| candidate.target.version,
                    ),
            },
            theta: result.theta.clone(),
            lambdas: result.lambdas,
        })
        .collect();
    HierState {
        generation: HierStateGeneration::CURRENT,
        fingerprint,
        outer_iterations,
        bodies,
        realizations,
        published_bodies,
        published_realizations,
        families,
        firmware: firmware
            .iter()
            .map(|(id, offset)| (id.clone(), *offset))
            .collect(),
        sigma: contributors
            .iter()
            .zip(sigma.iter())
            .map(|(id, log)| (*id, libm::exp(*log)))
            .collect(),
        body_decrement,
        families_certified,
        converged,
    }
}

/// Fit every family from its native evidence, then run the recipe stage with
/// every family target frozen.
pub fn fit_hierarchical(input: &HierInput, options: &HierOptions) -> Result<HierarchicalFit> {
    validate(input)?;
    if options.body_pin.fixed() {
        return Err(FitError::Hierarchy(
            "fit_hierarchical requires free body coordinates".to_string(),
        ));
    }
    if input
        .families
        .iter()
        .any(|family| matches!(family.mode, FamilyMode::Pinned(_)))
    {
        return Err(FitError::Hierarchy(
            "fit_hierarchical requires free family modes".to_string(),
        ));
    }
    let state_indices = state_indices(&input.bodies);
    let groups = components(&input.bodies);
    let bindings = gauge_bindings(
        &input.bodies,
        &groups,
        &options.gauges,
        &options.gauge_scopes,
    )?;
    let gauges: Vec<ComponentGauge> = bindings
        .iter()
        .map(|binding| binding.gauge.clone())
        .collect();
    let designated = designated_bodies(&input.bodies, &gauges);
    let contributors = contributors_of(&input.bodies);
    let fingerprint = fingerprint(input, options);
    if let Some(state) = &options.resume
        && (state.generation != HierStateGeneration::CURRENT || state.fingerprint != fingerprint)
    {
        return Err(FitError::Hierarchy(
            "resume state does not match the current hierarchy layout".to_string(),
        ));
    }
    let mut body_states: Vec<BodyState> = input.bodies.iter().map(BodyState::initial).collect();
    let mut realization_states: Vec<RealizationState> = input
        .bodies
        .iter()
        .flat_map(|body| {
            body.realizations
                .iter()
                .map(|realization| RealizationState::initial(realization, 0.03))
        })
        .collect();
    let mut sigma = initial_sigma(&contributors, &options.shared_sigma);
    let mut firmware: BTreeMap<FamilyFirmwareId, [f64; FIRMWARE_PARAMETERS]> =
        options.firmware.iter().cloned().collect();
    let mut family_theta: BTreeMap<LookId, Vec<f64>> = BTreeMap::new();
    let mut family_lambdas: BTreeMap<LookId, StageLambdas> = BTreeMap::new();
    let mut family_results: BTreeMap<LookId, fit::LookOnlyResult> = BTreeMap::new();
    let mut look_map: BTreeMap<LookId, Look> = BTreeMap::new();
    let mut completed_outer = 0usize;
    let mut last_body_decrement = None;
    if let Some(state) = &options.resume {
        completed_outer = state.outer_iterations;
        last_body_decrement = state.body_decrement;
        resume_states(
            input,
            &state_indices,
            state,
            ResumeTargets {
                body_states: &mut body_states,
                realization_states: &mut realization_states,
                sigma: &mut sigma,
                firmware: &mut firmware,
                family_theta: &mut family_theta,
                family_lambdas: &mut family_lambdas,
            },
        );
    }
    let mut evaluations = 0usize;
    let mut remaining = options.fit.sweep_budget;
    for family in &input.families {
        let family_id = &family.target.family;
        if look_map.contains_key(family_id) {
            continue;
        }
        let designated_body = family_designated(&input.bodies, &groups, &gauges, family_id);
        let pool = family_pool(
            family_id,
            None,
            &input.bodies,
            &state_indices,
            &body_states,
            &realization_states,
            false,
            designated_body.as_deref(),
            &firmware,
        );
        if pool.images.is_empty() {
            return Err(FitError::NoImages);
        }
        let initial = match &family.mode {
            FamilyMode::Fit { initial } => initial.as_ref(),
            FamilyMode::Pinned(_) => unreachable!("validated free family mode"),
        };
        let theta = family_theta.get(family_id).map(Vec::as_slice);
        let lambdas = family_lambdas.get(family_id).copied();
        let scales = sigma_pairs(&contributors, &sigma);
        let result = fit::run_look_only_rows(
            &pool.images,
            &options.fit,
            initial,
            pool.nominal_clip.unwrap_or(0.03),
            &pool.rows,
            family.previous_support.as_ref(),
            theta,
            lambdas,
            &scales,
            &mut remaining,
        )?;
        evaluations += result.evaluations;
        look_map.insert(family_id.clone(), result.look.clone());
        family_results.insert(family_id.clone(), result);
    }

    let mut outer_iterations = completed_outer;
    let mut converged = false;
    let mut exhausted = false;
    let invocation_end = completed_outer + options.outer_iterations.max(1);
    for iteration in completed_outer..invocation_end {
        outer_iterations = iteration + 1;
        let previous_bodies = body_states.clone();
        let previous_realizations = realization_states.clone();
        let mut all_pairs: Vec<Vec<(f64, f64)>> = vec![Vec::new(); contributors.len()];
        let mut body_decrement: Option<f64> = Some(0.0);
        let mut body_plateau = true;
        for (component, members) in groups.iter().enumerate() {
            let gauge = &gauges[component];
            for index in members {
                let outcome = fit_body(
                    &input.bodies[*index],
                    &state_indices[*index],
                    &mut realization_states,
                    &mut body_states[*index],
                    &look_map,
                    &firmware,
                    gauge,
                    designated[*index],
                    &sigma,
                    &contributors,
                    options,
                    false,
                    &mut remaining,
                )?;
                for (slot, list) in all_pairs.iter_mut().enumerate() {
                    list.extend(outcome.pairs[slot].iter().copied());
                }
                evaluations += outcome.evaluations;
                body_plateau &= outcome.plateau;
                if outcome.exhausted {
                    exhausted = true;
                    break;
                }
                body_decrement = match (body_decrement, outcome.decrement) {
                    (Some(current), Some(value)) => Some(current.max(value)),
                    _ => None,
                };
            }
            if exhausted {
                break;
            }
            if !gauge.anchored_delta {
                apply_component_gauge(&mut body_states, members, &input.bodies, false);
            }
        }
        if exhausted {
            break;
        }
        let previous_sigma = sigma.clone();
        update_sigma(&mut sigma, &all_pairs, &options.fit);
        last_body_decrement = body_decrement;
        if remaining <= 0.0 {
            exhausted = true;
            break;
        }

        let mut change = body_states
            .iter()
            .zip(previous_bodies.iter())
            .map(|(current, previous)| body_distance(current, previous))
            .fold(0.0_f64, f64::max);
        change = change.max(
            realization_states
                .iter()
                .zip(previous_realizations.iter())
                .map(|(current, previous)| realization_distance(current, previous))
                .fold(0.0_f64, f64::max),
        );
        change = change.max(
            sigma
                .iter()
                .zip(previous_sigma.iter())
                .map(|(current, previous)| (current - previous).abs())
                .fold(0.0_f64, f64::max),
        );
        for family in &input.families {
            let family_id = &family.target.family;
            let designated_body = family_designated(&input.bodies, &groups, &gauges, family_id);
            let pool = family_pool(
                family_id,
                None,
                &input.bodies,
                &state_indices,
                &body_states,
                &realization_states,
                false,
                designated_body.as_deref(),
                &firmware,
            );
            if pool.images.is_empty() {
                continue;
            }
            let warm = look_map.get(family_id);
            let theta = if iteration == completed_outer {
                family_theta.get(family_id).map(Vec::as_slice)
            } else {
                None
            };
            let result = fit::run_look_only_rows(
                &pool.images,
                &options.fit,
                warm,
                pool.nominal_clip.unwrap_or(0.03),
                &pool.rows,
                family.previous_support.as_ref(),
                theta,
                family_lambdas.get(family_id).copied(),
                &sigma_pairs(&contributors, &sigma),
                &mut remaining,
            )?;
            evaluations += result.evaluations;
            if let Some(previous) = warm {
                change = change.max(look_distance(previous, &result.look));
            }
            family_theta.insert(family_id.clone(), result.theta.clone());
            family_lambdas.insert(family_id.clone(), result.lambdas);
            let offsets = estimate_firmware(
                &result.look,
                &pool.images,
                &pool.firmware,
                &pool.clips,
                &sigma,
                &contributors,
                &options.fit,
                options.firmware_prior_sd,
                &mut remaining,
            )?;
            for (id, offset) in &offsets {
                firmware.insert(
                    FamilyFirmwareId {
                        family: family_id.clone(),
                        firmware: id.clone(),
                    },
                    *offset,
                );
            }
            look_map.insert(family_id.clone(), result.look.clone());
            family_results.insert(family_id.clone(), result);
        }
        if remaining <= 0.0 {
            exhausted = true;
        }
        if change < 1e-5 && body_plateau && !exhausted {
            converged = true;
            break;
        }
        if exhausted {
            break;
        }
    }

    if outer_iterations == completed_outer {
        converged = options.resume.as_ref().is_some_and(|state| state.converged);
    }
    apply_component_gauges(
        &mut body_states,
        &groups,
        &gauges,
        &input.bodies,
        false,
        true,
    );
    let native_body_certificate = Some(certify_bodies(
        input,
        &state_indices,
        &body_states,
        &realization_states,
        &look_map,
        &firmware,
        &groups,
        &gauges,
        &designated,
        &sigma,
        &contributors,
        options,
        false,
        &mut remaining,
    ));
    let native_body_certified =
        native_body_certificate.is_some_and(|report| report.certificate.is_certified_stationary());
    let native_family_reports = certify_families(
        input,
        &state_indices,
        &body_states,
        &realization_states,
        &firmware,
        &groups,
        &gauges,
        &sigma,
        &contributors,
        &family_theta,
        &family_lambdas,
        &family_results,
        options,
        &mut evaluations,
        &mut remaining,
    )?;
    let (family_certificate, certified_families) = certified_reports(&native_family_reports);
    let mut final_family_publications: BTreeMap<LookId, (ProbeUncertainty, Density, RgbSpace)> =
        BTreeMap::new();
    let mut probes_bound = true;
    for family in &input.families {
        let family_id = &family.target.family;
        let Some(result) = family_results.get(family_id) else {
            continue;
        };
        let designated_body = family_designated(&input.bodies, &groups, &gauges, family_id);
        let pool = family_pool(
            family_id,
            None,
            &input.bodies,
            &state_indices,
            &body_states,
            &realization_states,
            false,
            designated_body.as_deref(),
            &firmware,
        );
        if pool.images.is_empty() {
            continue;
        }
        let problem = match Problem::new_with_rows(&pool.images, &pool.rows, &options.fit) {
            Ok(problem) => problem,
            Err(_) => {
                probes_bound = false;
                continue;
            }
        };
        let theta = family_theta.get(family_id).map(Vec::as_slice);
        let lambdas = family_lambdas
            .get(family_id)
            .copied()
            .unwrap_or(options.fit.initial_lambdas);
        let params = match fit::seed_look_only_params(
            &problem,
            &options.fit,
            Some(&result.look),
            pool.nominal_clip.unwrap_or(0.03),
            &pool.rows,
            theta,
            &sigma_pairs(&contributors, &sigma),
        ) {
            Ok(params) => params,
            Err(_) => {
                probes_bound = false;
                continue;
            }
        };
        match fit::look_only_probes(
            &problem,
            &params,
            &lambdas,
            family.previous_support.as_ref(),
            &options.fit,
        ) {
            Ok((probes, density, output_space)) => {
                evaluations += 1;
                final_family_publications
                    .insert(family_id.clone(), (probes, density, output_space));
            }
            Err(_) => probes_bound = false,
        }
    }

    let native_body_states = body_states.clone();
    let native_realization_states = realization_states.clone();
    let native_looks = look_map.clone();
    let native_results = family_results.clone();

    let recipe = has_recipe(&input.bodies);
    if recipe {
        let mut recipe_decrement: Option<f64> = Some(0.0);
        'recipe: for (component, members) in groups.iter().enumerate() {
            let gauge = &gauges[component];
            for index in members {
                let outcome = fit_body(
                    &input.bodies[*index],
                    &state_indices[*index],
                    &mut realization_states,
                    &mut body_states[*index],
                    &look_map,
                    &firmware,
                    gauge,
                    designated[*index],
                    &sigma,
                    &contributors,
                    options,
                    true,
                    &mut remaining,
                )?;
                evaluations += outcome.evaluations;
                if outcome.exhausted {
                    exhausted = true;
                    break 'recipe;
                }
                recipe_decrement = match (recipe_decrement, outcome.decrement) {
                    (Some(current), Some(value)) => Some(current.max(value)),
                    _ => None,
                };
            }
            if !gauge.anchored_delta {
                apply_component_gauge(&mut body_states, members, &input.bodies, true);
            }
        }
        last_body_decrement = recipe_decrement;
    }
    if exhausted {
        converged = false;
    }
    apply_component_gauges(
        &mut body_states,
        &groups,
        &gauges,
        &input.bodies,
        recipe,
        true,
    );

    let body_certificate = Some(certify_bodies(
        input,
        &state_indices,
        &body_states,
        &realization_states,
        &look_map,
        &firmware,
        &groups,
        &gauges,
        &designated,
        &sigma,
        &contributors,
        options,
        recipe,
        &mut remaining,
    ));
    let published_body_certified =
        body_certificate.is_some_and(|report| report.certificate.is_certified_stationary());
    if let Some(bounds) = body_certificate.and_then(|report| report.bounds) {
        last_body_decrement = Some(bounds.upper.get());
    }
    let certified_body = native_body_certified && published_body_certified;
    let numerically_converged = converged && certified_families && certified_body && probes_bound;

    let mut families = Vec::new();
    for family in &input.families {
        let family_id = &family.target.family;
        let Some(result) = native_results.get(family_id) else {
            continue;
        };
        let designated_body = family_designated(&input.bodies, &groups, &gauges, family_id);
        let pool = family_pool(
            family_id,
            None,
            &input.bodies,
            &state_indices,
            &native_body_states,
            &native_realization_states,
            false,
            designated_body.as_deref(),
            &firmware,
        );
        let mut provenance = Vec::new();
        for (body_index, body) in input.bodies.iter().enumerate() {
            for (realization_index, realization) in body.realizations.iter().enumerate() {
                if &realization.target.family != family_id {
                    continue;
                }
                if !realization
                    .images
                    .iter()
                    .any(|tag| tag.image.intent == ImageIntent::Native)
                {
                    continue;
                }
                let state_index = state_indices[body_index][realization_index];
                provenance.push(look_provenance(
                    body,
                    &native_body_states[body_index],
                    realization,
                    &native_realization_states[state_index],
                ));
            }
        }
        let (probes, density, output_space) = match final_family_publications.get(family_id) {
            Some((probes, density, output_space)) => {
                ((*probes).clone(), (*density).clone(), *output_space)
            }
            None => (
                result.probes.clone(),
                result.density.clone(),
                result.output_space,
            ),
        };
        families.push(FamilyParameters {
            target: family.target.clone(),
            look: native_looks
                .get(family_id)
                .cloned()
                .unwrap_or_else(Look::colorimetric),
            firmware: family_firmware_offsets(family_id, &pool.firmware, &firmware),
            probes,
            density,
            clip_k: result.clip_k,
            output_space,
            provenance,
            pool: pool.images,
            groups: pool.groups,
            theta: result.theta.clone(),
            lambdas: result.lambdas,
        });
    }

    let bodies_out = input
        .bodies
        .iter()
        .zip(body_states.iter())
        .map(|(body, state)| body_parameters(body, state))
        .collect();
    let mut realizations_out = Vec::new();
    let mut state_index = 0usize;
    for body in &input.bodies {
        for realization in &body.realizations {
            realizations_out.push(realization_parameters(
                realization,
                &realization_states[state_index],
                has_recipe(&input.bodies),
            ));
            state_index += 1;
        }
    }
    let diagnostics = diagnostics(
        evaluations,
        outer_iterations,
        &contributors,
        &sigma,
        numerically_converged,
        Certificates {
            family: family_certificate,
            body: body_certificate,
            native_body: native_body_certificate,
            body_decrement: last_body_decrement,
        },
        true,
    );
    let state = build_state(
        fingerprint,
        outer_iterations,
        input,
        &native_body_states,
        &native_realization_states,
        &body_states,
        &realization_states,
        recipe,
        &native_results,
        &firmware,
        &contributors,
        &sigma,
        last_body_decrement,
        certified_families,
        numerically_converged,
    );
    Ok(HierarchicalFit {
        families,
        bodies: bodies_out,
        realizations: realizations_out,
        components: gauges,
        diagnostics,
        state,
    })
}

/// Fit realization parameters against pinned family targets without moving them.
pub fn fit_realizations(input: &HierInput, options: &HierOptions) -> Result<RealizationFit> {
    validate(input)?;
    let mut pinned = Vec::new();
    let mut look_map = BTreeMap::new();
    for family in &input.families {
        let FamilyMode::Pinned(look) = &family.mode else {
            return Err(FitError::Hierarchy(
                "fit_realizations requires pinned family modes".to_string(),
            ));
        };
        pinned.push(PinnedFamily {
            target: family.target.clone(),
            look: look.clone(),
        });
        look_map.insert(family.target.family.clone(), look.clone());
    }
    for body in &input.bodies {
        for realization in &body.realizations {
            if !pinned
                .iter()
                .any(|family| family.target == realization.target)
            {
                return Err(FitError::Hierarchy(format!(
                    "realization target {} is not a pinned family",
                    realization.target.family
                )));
            }
        }
    }
    let state_indices = state_indices(&input.bodies);
    let groups = components(&input.bodies);
    let bindings = gauge_bindings(
        &input.bodies,
        &groups,
        &options.gauges,
        &options.gauge_scopes,
    )?;
    let gauges: Vec<ComponentGauge> = bindings
        .iter()
        .map(|binding| binding.gauge.clone())
        .collect();
    let designated = designated_bodies(&input.bodies, &gauges);
    let contributors = contributors_of(&input.bodies);
    let fingerprint = fingerprint(input, options);
    if let Some(state) = &options.resume
        && (state.generation != HierStateGeneration::CURRENT || state.fingerprint != fingerprint)
    {
        return Err(FitError::Hierarchy(
            "resume state does not match the current hierarchy layout".to_string(),
        ));
    }
    let mut body_states: Vec<BodyState> = input.bodies.iter().map(BodyState::initial).collect();
    let mut realization_states: Vec<RealizationState> = input
        .bodies
        .iter()
        .flat_map(|body| {
            body.realizations.iter().map(|realization| {
                let clip = look_map
                    .get(&realization.target.family)
                    .map_or(0.03, |look| look.clip.k);
                RealizationState::initial(realization, clip)
            })
        })
        .collect();
    let mut sigma = initial_sigma(&contributors, &options.shared_sigma);
    let mut firmware: BTreeMap<FamilyFirmwareId, [f64; FIRMWARE_PARAMETERS]> =
        options.firmware.iter().cloned().collect();
    let mut family_theta = BTreeMap::new();
    let mut family_lambdas = BTreeMap::new();
    let mut completed_outer = 0usize;
    let mut last_body_decrement = None;
    if let Some(state) = &options.resume {
        completed_outer = state.outer_iterations;
        last_body_decrement = state.body_decrement;
        resume_states(
            input,
            &state_indices,
            state,
            ResumeTargets {
                body_states: &mut body_states,
                realization_states: &mut realization_states,
                sigma: &mut sigma,
                firmware: &mut firmware,
                family_theta: &mut family_theta,
                family_lambdas: &mut family_lambdas,
            },
        );
    }
    apply_body_pin(input, options, &mut body_states)?;
    let mut outer_iterations = completed_outer;
    let mut converged = false;
    let mut exhausted = false;
    let mut evaluations = 0usize;
    let mut remaining = options.fit.sweep_budget;
    let invocation_end = completed_outer + options.outer_iterations.max(1);
    for iteration in completed_outer..invocation_end {
        outer_iterations = iteration + 1;
        let previous_bodies = body_states.clone();
        let previous_realizations = realization_states.clone();
        let mut body_decrement: Option<f64> = Some(0.0);
        let mut body_plateau = true;
        for (component, members) in groups.iter().enumerate() {
            let gauge = &gauges[component];
            for index in members {
                let outcome = fit_body(
                    &input.bodies[*index],
                    &state_indices[*index],
                    &mut realization_states,
                    &mut body_states[*index],
                    &look_map,
                    &firmware,
                    gauge,
                    designated[*index],
                    &sigma,
                    &contributors,
                    options,
                    false,
                    &mut remaining,
                )?;
                evaluations += outcome.evaluations;
                body_plateau &= outcome.plateau;
                if outcome.exhausted {
                    exhausted = true;
                    break;
                }
                body_decrement = match (body_decrement, outcome.decrement) {
                    (Some(current), Some(value)) => Some(current.max(value)),
                    _ => None,
                };
            }
            if exhausted {
                break;
            }
        }
        last_body_decrement = body_decrement;
        if exhausted {
            break;
        }
        if remaining <= 0.0 {
            exhausted = true;
            break;
        }
        let mut change = body_states
            .iter()
            .zip(previous_bodies.iter())
            .map(|(current, previous)| body_distance(current, previous))
            .fold(0.0_f64, f64::max);
        change = change.max(
            realization_states
                .iter()
                .zip(previous_realizations.iter())
                .map(|(current, previous)| realization_distance(current, previous))
                .fold(0.0_f64, f64::max),
        );
        if change < 1e-5 && body_plateau {
            converged = true;
            break;
        }
    }
    if outer_iterations == completed_outer {
        converged = options.resume.as_ref().is_some_and(|state| state.converged);
    }
    if !options.body_pin.fixed() {
        apply_component_gauges(
            &mut body_states,
            &groups,
            &gauges,
            &input.bodies,
            false,
            false,
        );
    }
    let native_body_certificate = Some(certify_bodies(
        input,
        &state_indices,
        &body_states,
        &realization_states,
        &look_map,
        &firmware,
        &groups,
        &gauges,
        &designated,
        &sigma,
        &contributors,
        options,
        false,
        &mut remaining,
    ));
    let native_body_certified =
        native_body_certificate.is_some_and(|report| report.certificate.is_certified_stationary());
    let native_body_states = body_states.clone();
    let native_realization_states = realization_states.clone();
    let recipe = has_recipe(&input.bodies);
    if recipe {
        let mut recipe_decrement: Option<f64> = Some(0.0);
        'recipe: for (component, members) in groups.iter().enumerate() {
            let gauge = &gauges[component];
            for index in members {
                let outcome = fit_body(
                    &input.bodies[*index],
                    &state_indices[*index],
                    &mut realization_states,
                    &mut body_states[*index],
                    &look_map,
                    &firmware,
                    gauge,
                    designated[*index],
                    &sigma,
                    &contributors,
                    options,
                    true,
                    &mut remaining,
                )?;
                evaluations += outcome.evaluations;
                if outcome.exhausted {
                    exhausted = true;
                    break 'recipe;
                }
                recipe_decrement = match (recipe_decrement, outcome.decrement) {
                    (Some(current), Some(value)) => Some(current.max(value)),
                    _ => None,
                };
            }
        }
        last_body_decrement = recipe_decrement;
    }
    if exhausted {
        converged = false;
    }
    if !options.body_pin.fixed() {
        apply_component_gauges(
            &mut body_states,
            &groups,
            &gauges,
            &input.bodies,
            recipe,
            false,
        );
    }
    let body_certificate = Some(certify_bodies(
        input,
        &state_indices,
        &body_states,
        &realization_states,
        &look_map,
        &firmware,
        &groups,
        &gauges,
        &designated,
        &sigma,
        &contributors,
        options,
        recipe,
        &mut remaining,
    ));
    let published_body_certified =
        body_certificate.is_some_and(|report| report.certificate.is_certified_stationary());
    if let Some(bounds) = body_certificate.and_then(|report| report.bounds) {
        last_body_decrement = Some(bounds.upper.get());
    }
    let certified_body = native_body_certified && published_body_certified;
    let numerically_converged = converged && certified_body;
    let bodies_out = input
        .bodies
        .iter()
        .zip(body_states.iter())
        .map(|(body, state)| body_parameters(body, state))
        .collect();
    let mut realizations_out = Vec::new();
    let mut state_index = 0usize;
    for body in &input.bodies {
        for realization in &body.realizations {
            realizations_out.push(realization_parameters(
                realization,
                &realization_states[state_index],
                has_recipe(&input.bodies),
            ));
            state_index += 1;
        }
    }
    let diagnostics = diagnostics(
        evaluations,
        outer_iterations,
        &contributors,
        &sigma,
        numerically_converged,
        Certificates {
            family: None,
            body: body_certificate,
            native_body: native_body_certificate,
            body_decrement: last_body_decrement,
        },
        false,
    );
    let state = build_state(
        fingerprint,
        outer_iterations,
        input,
        &native_body_states,
        &native_realization_states,
        &body_states,
        &realization_states,
        recipe,
        &BTreeMap::new(),
        &firmware,
        &contributors,
        &sigma,
        last_body_decrement,
        true,
        numerically_converged,
    );
    Ok(RealizationFit {
        families: pinned,
        bodies: bodies_out,
        realizations: realizations_out,
        gauges: bindings,
        diagnostics,
        state,
    })
}

#[cfg(test)]
mod review_tests {
    use super::*;

    #[test]
    fn local_steps_write_to_original_image_indices() {
        let layout = BodyLayout {
            delta: [0, 1, 2, 3, 4, 5],
            xi: Vec::new(),
            omega: Vec::new(),
            mode_ids: Vec::new(),
            wb_ids: Vec::new(),
            fixed: false,
            clip: 6,
            image: vec![[7, 8, 9]],
            active: vec![1],
            response: Vec::new(),
            total: 10,
            offset: 0,
        };
        let mut state = RealizationState {
            clip: 0.03,
            nuisances: vec![[0.0; 3]; 2],
            response: None,
            coefficients: Vec::new(),
            damping: 1e-3,
        };
        let mut step = vec![0.0_f64; 4];
        step[1] = 1.0;
        apply_local_step(&layout, &mut state, &step, 1.0);
        assert!((state.nuisances[1][0] - 1.0).abs() < 1e-12);
        assert_eq!(state.nuisances[0][0], 0.0);
    }

    #[test]
    fn a_nonzero_body_gradient_is_not_certified_stationary() {
        let layout = BodyLayout {
            delta: [0, 1, 2, 3, 4, 5],
            xi: Vec::new(),
            omega: Vec::new(),
            mode_ids: Vec::new(),
            wb_ids: Vec::new(),
            fixed: false,
            clip: 6,
            image: Vec::new(),
            active: Vec::new(),
            response: Vec::new(),
            total: 6,
            offset: 0,
        };
        let mut pass = BodyPass {
            gradient: vec![0.0_f64; 6],
            hessian: vec![0.0_f64; 36],
            gradient_error: vec![0.0_f64; 6],
            hessian_abs: vec![1.0_f64; 6],
            hessian_terms: 1,
            cost: 0.0,
            pairs: Vec::new(),
            infeasible: false,
        };
        for index in 0..6 {
            pass.hessian[index * 6 + index] = 1.0;
        }
        pass.gradient[0] = 1.0;
        let report = certify_body_pass(&[pass], &[layout], &[1.0; 6], &[]);
        assert!(
            matches!(
                report.certificate,
                StationarityCertificate::CertifiedNonstationary { .. }
            ),
            "nonzero gradient certified stationary: {:?}",
            report.certificate
        );
    }

    #[test]
    fn energy_enclosure_includes_first_order_hessian_error() {
        let layout = BodyLayout {
            delta: [0, 1, 2, 3, 4, 5],
            xi: Vec::new(),
            omega: Vec::new(),
            mode_ids: Vec::new(),
            wb_ids: Vec::new(),
            fixed: false,
            clip: 6,
            image: Vec::new(),
            active: Vec::new(),
            response: Vec::new(),
            total: 6,
            offset: 0,
        };
        let mut pass = BodyPass {
            gradient: vec![0.0_f64; 6],
            hessian: vec![0.0_f64; 36],
            gradient_error: vec![0.0_f64; 6],
            hessian_abs: vec![0.0_f64; 6],
            hessian_terms: 1,
            cost: 0.0,
            pairs: Vec::new(),
            infeasible: false,
        };
        for index in 0..6 {
            pass.hessian[index * 6 + index] = 1.0;
        }
        pass.gradient[0] = -1.0;
        pass.hessian_abs[0] = 1.0e12;
        let report = certify_body_pass(&[pass], &[layout], &[1.0; 6], &[]);
        let bounds = report.bounds.expect("energy bounds computed");
        assert!(
            bounds.upper.get() >= 0.5 + 1.0e-4,
            "first-order Hessian accumulation error missing from upper bound: {}",
            bounds.upper.get()
        );
    }
}
