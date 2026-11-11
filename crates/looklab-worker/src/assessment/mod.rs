// SPDX-License-Identifier: AGPL-3.0-or-later
//! Versioned qualification protocols (docs/02 §5.1, docs/05 §8.1).

mod controls;
mod engine;
mod native;
mod pooling;

pub use controls::{
    ControlClaim, ControlsAssessmentInput, ObservedJpeg, RecoveryObservation, SettingObservation,
    assess_controls, matching_scope_covers, observed_rerender,
};
pub(crate) use engine::engine_prerequisite;
pub use engine::{EngineAssessment, EngineTrial, EngineVersion, qualify_engine};
pub use native::{
    NativeAssessmentInput, NativeObservation, NativeProbeSupport, assess_native, support_from_fit,
};
pub use pooling::{
    CrossBodyAssessmentInput, CrossBodyEvidence, GaugeBindingId, ProbePrediction,
    assess_cross_body, predictions_from_fit,
};

use looklab_color::RgbSpace;
use looklab_schema::{
    AssessedPoint, AssessmentOutcome, CellId, CohortAdmissionRequest, CombinationBasis,
    DependencyKind, DependencyRef, DependencyTarget, EvidenceRequest, EvidenceRequestKind,
    FamilyTargetId, FirmwareId, Limitation, MetricKey, ModelReport, ModelReportId, ModelSubject,
    ModelVersionId, OutputEncoding, QualificationCapability, RealizationRevisionId, ReportMetrics,
    ReportProvenance, ReportScope, RuleRevision, SettingCombination, SettingValue, SupportedMask,
    WbModeKey,
};
use looklab_server::model_store::{
    EvidenceGroup as StoredEvidenceGroup, EvidenceMembership, EvidenceRole,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub use looklab_server::model_store::EvidenceGroupId as EvidenceGroup;

/// P2.2 observed-JPEG re-render median bound (docs/09).
pub const P22_OBSERVED_MEDIAN_DE: f64 = 1.5;
/// P2.3 settings recovery step bound per control (docs/09).
pub const P23_RECOVERY_STEPS: i64 = 1;
/// P2.3 alternative predicted-error bound (docs/09).
pub const P23_ERROR_TOLERANCE_DE: f64 = 0.5;
/// P1.9 tentative cross-body median bound (docs/09).
pub const P19_DISAGREEMENT_MEDIAN_DE: f64 = 2.0;
/// P4.3 known-reference coverage bound (docs/09).
pub const P43_BAND_COVERAGE: f64 = 0.85;

pub const NATIVE_PREDICTION_RULE: &str = "native-prediction/1";
pub const CONTROLS_MATCHING_RULE: &str = "controls-matching/1";
pub const CROSS_BODY_RULE: &str = "cross-body-admission/1";
pub const ENGINE_COVERAGE_RULE: &str = "engine-coverage/1";

/// # Errors
/// The image reference is not a valid evidence group token.
pub fn evidence_group(
    image: &looklab_fit::FitImage,
    image_ref: &str,
) -> Result<EvidenceGroup, String> {
    let token = match image.samples.header().rerender_group {
        Some(group) => {
            let hex: String = group
                .as_bytes()
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect();
            format!("rerender-{hex}")
        }
        None => image_ref.to_owned(),
    };
    EvidenceGroup::new(token).map_err(|error| error.to_string())
}

pub(crate) fn evidence_membership(
    training: Vec<EvidenceGroup>,
    held_out: Vec<EvidenceGroup>,
) -> EvidenceMembership {
    let mut groups = Vec::with_capacity(training.len() + held_out.len());
    for group in training {
        groups.push(StoredEvidenceGroup {
            group,
            role: EvidenceRole::Training,
        });
    }
    for group in held_out {
        groups.push(StoredEvidenceGroup {
            group,
            role: EvidenceRole::Assessment,
        });
    }
    groups.sort_by(|left, right| left.group.as_str().cmp(right.group.as_str()));
    EvidenceMembership {
        blobs: Vec::new(),
        groups,
        roles: Vec::new(),
        evaluated_images: Vec::new(),
        evaluated_contributions: Vec::new(),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceSource {
    Observed,
    Synthetic,
}

#[derive(Clone, Debug)]
pub struct AssessmentBinding {
    pub subject: ModelSubject,
    pub candidate: ModelVersionId,
    pub scope: ReportScope,
    pub provenance: ReportProvenance,
    pub dependencies: Vec<DependencyRef>,
}

impl AssessmentBinding {
    /// A subject/candidate mismatch is an execution failure.
    pub fn validate(&self) -> Result<(), String> {
        if self.scope.points.is_empty() {
            return Err("assessment scope has no assessed points".to_owned());
        }
        if self.scope.assessed_bodies.is_empty() {
            return Err("assessment scope has no bodies".to_owned());
        }
        if self.scope.combination_basis != CombinationBasis::Enumerated {
            return Err("assessment settings must be enumerated, not a product".to_owned());
        }
        if self
            .scope
            .points
            .iter()
            .any(|point| point.settings.is_empty())
            && !self.scope.defaults_assessed
        {
            return Err("an empty settings combination is defaults-only and unassessed".to_owned());
        }
        match (&self.subject, &self.candidate) {
            (
                ModelSubject::SharedFamily { family: subject },
                ModelVersionId::SharedFamily { target },
            ) if *subject == target.family => Ok(()),
            (
                ModelSubject::BodyRealization { realization },
                ModelVersionId::BodyRealization {
                    realization: candidate,
                },
            ) if *realization == candidate.realization => Ok(()),
            (ModelSubject::SharedFamily { .. }, ModelVersionId::BodyRealization { .. })
            | (ModelSubject::BodyRealization { .. }, ModelVersionId::SharedFamily { .. }) => {
                Err("candidate kind does not match the report subject".to_owned())
            }
            _ => Err("candidate does not name the report subject".to_owned()),
        }
    }
}

#[derive(Clone, Debug)]
pub struct ProtocolAssessment {
    pub report: ModelReport,
    pub report_id: ModelReportId,
    pub dependencies: Vec<DependencyRef>,
    pub membership: EvidenceMembership,
    pub cohort: Option<CohortAdmissionRequest>,
}

pub type NativeAssessment = ProtocolAssessment;
pub type ControlsAssessment = ProtocolAssessment;
pub type CrossBodyAssessment = ProtocolAssessment;

pub enum AssessmentRequest {
    Native(NativeAssessmentInput),
    Controls(ControlsAssessmentInput),
    CrossBody(CrossBodyAssessmentInput),
}

impl AssessmentRequest {
    pub fn candidate(&self) -> &ModelVersionId {
        match self {
            Self::Native(input) => &input.binding.candidate,
            Self::Controls(input) => &input.binding.candidate,
            Self::CrossBody(input) => &input.binding.candidate,
        }
    }
}

pub fn assess(request: AssessmentRequest) -> ProtocolAssessment {
    match request {
        AssessmentRequest::Native(input) => assess_native(input),
        AssessmentRequest::Controls(input) => assess_controls(input),
        AssessmentRequest::CrossBody(input) => assess_cross_body(input),
    }
}

/// # Errors
/// The request names a different candidate from the selected model.
pub fn assess_selected(
    selected: &ModelVersionId,
    request: AssessmentRequest,
) -> Result<ProtocolAssessment, String> {
    if request.candidate() != selected {
        return Err("assessment request does not name the selected model".to_owned());
    }
    Ok(assess(request))
}

pub(crate) struct ReportParts {
    pub capability: QualificationCapability,
    pub rule: RuleRevision,
    pub outcome: AssessmentOutcome,
    pub metrics: ReportMetrics,
    pub supported_mask: SupportedMask,
    pub limitations: Vec<Limitation>,
    pub evidence_requests: Vec<EvidenceRequest>,
    pub extra_dependencies: Vec<DependencyRef>,
    pub membership: EvidenceMembership,
    pub cohort: Option<CohortAdmissionRequest>,
}

pub(crate) fn build_report(binding: &AssessmentBinding, parts: ReportParts) -> ProtocolAssessment {
    let ReportParts {
        capability,
        rule,
        outcome,
        metrics,
        supported_mask,
        limitations,
        evidence_requests,
        extra_dependencies,
        membership,
        cohort,
    } = parts;
    let mut provenance = binding.provenance.clone();
    provenance.rule_revision = rule.clone();
    let mut dependencies = binding.dependencies.clone();
    dependencies.extend(extra_dependencies);
    dependencies.push(rule_dependency(&rule));
    dependencies.extend(target_dependencies(&binding.candidate));
    dependencies.sort();
    dependencies.dedup();
    let report = ModelReport {
        subject: binding.subject.clone(),
        candidate: binding.candidate.clone(),
        capability,
        scope: binding.scope.clone(),
        provenance,
        outcome,
        metrics,
        supported_mask,
        limitations,
        evidence_requests,
    };
    let public_id = report.semantic_id(&dependencies);
    let report_id = stored_report_id(public_id, &membership);
    ProtocolAssessment {
        report,
        report_id,
        dependencies,
        membership,
        cohort,
    }
}

fn stored_report_id(public: ModelReportId, membership: &EvidenceMembership) -> ModelReportId {
    let mut hasher = Sha256::new();
    hasher.update(public.digest().as_bytes());
    hasher.update(membership.digest().as_bytes());
    ModelReportId::from_digest(hasher.finalize().into())
}

/// Call after filling blob/contribution membership.
pub fn refresh_report_identity(assessment: &mut ProtocolAssessment) {
    let public = assessment.report.semantic_id(&assessment.dependencies);
    assessment.report_id = stored_report_id(public, &assessment.membership);
    if let Some(cohort) = &mut assessment.cohort {
        cohort.report = assessment.report_id;
    }
}

pub(crate) fn rule_dependency(rule: &RuleRevision) -> DependencyRef {
    DependencyRef::required(DependencyKind::RuleVersion, DependencyTarget::rule(rule))
}

pub(crate) fn engine_dependency(
    prerequisite: &looklab_schema::EnginePrerequisite,
) -> DependencyRef {
    DependencyRef::required(
        DependencyKind::EngineQualification,
        DependencyTarget::engine_qualification(&prerequisite.id()),
    )
}

fn target_dependencies(candidate: &ModelVersionId) -> Vec<DependencyRef> {
    let target: &FamilyTargetId = match candidate {
        ModelVersionId::SharedFamily { target } => target,
        ModelVersionId::BodyRealization { realization } => &realization.target,
    };
    let mut dependencies = vec![DependencyRef::required(
        DependencyKind::FamilyVersion,
        DependencyTarget::family_version(target),
    )];
    if let ModelVersionId::BodyRealization { realization } = candidate {
        dependencies.push(DependencyRef::required(
            DependencyKind::RealizationRevision,
            DependencyTarget::realization_revision(&RealizationRevisionId::for_version(
                realization,
            )),
        ));
    }
    dependencies
}

pub(crate) fn limitation(text: impl Into<String>) -> Limitation {
    Limitation::new(text).expect("assessment limitations are non-empty")
}

pub(crate) fn evidence_request(
    kind: EvidenceRequestKind,
    detail: impl Into<String>,
) -> EvidenceRequest {
    EvidenceRequest {
        kind,
        detail: detail.into(),
    }
}

pub(crate) fn metric(metrics: &mut ReportMetrics, key: MetricKey, value: f64) {
    metrics.insert(key, value);
}

pub(crate) fn median(values: &[f64]) -> Option<f64> {
    let mut finite: Vec<f64> = values
        .iter()
        .copied()
        .filter(|value| value.is_finite())
        .collect();
    if finite.is_empty() {
        return None;
    }
    finite.sort_by(f64::total_cmp);
    let middle = finite.len() / 2;
    if finite.len().is_multiple_of(2) {
        Some(0.5 * (finite[middle - 1] + finite[middle]))
    } else {
        Some(finite[middle])
    }
}

pub(crate) fn outcome(execution: bool, failed: bool, insufficient: bool) -> AssessmentOutcome {
    if execution {
        AssessmentOutcome::ExecutionFailure
    } else if failed {
        AssessmentOutcome::Fail
    } else if insufficient {
        AssessmentOutcome::InsufficientEvidence
    } else {
        AssessmentOutcome::Pass
    }
}

pub fn cell_for_probe(probe: &looklab_model::ProbeId) -> Option<CellId> {
    let probes = looklab_model::probes::v1();
    let probe = probes.probes.iter().find(|entry| entry.id == *probe)?;
    let input = looklab_model::LookInput::new(probe.xyz);
    let index = looklab_fit::model_error::cell(&input);
    CellId::new(format!("cell-{index}")).ok()
}

pub fn rgb_space(encoding: OutputEncoding) -> RgbSpace {
    match encoding {
        OutputEncoding::Srgb => RgbSpace::Srgb,
        OutputEncoding::AdobeRgb => RgbSpace::AdobeRgb,
        OutputEncoding::Rec2020 => RgbSpace::Rec2020,
        OutputEncoding::DisplayP3 => RgbSpace::DisplayP3,
        OutputEncoding::ProPhoto => RgbSpace::ProPhoto,
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PinnedModes {
    pub firmware: Vec<FirmwareId>,
    pub wb_modes: Vec<WbModeKey>,
}

pub fn resolve_point(
    point: &AssessedPoint,
    pinned: &PinnedModes,
) -> Option<(FirmwareId, WbModeKey)> {
    let firmware = match &point.firmware {
        Some(firmware) => firmware.clone(),
        None if pinned.firmware.len() == 1 => pinned.firmware[0].clone(),
        None => return None,
    };
    let wb_mode = match &point.wb_mode {
        Some(wb_mode) => wb_mode.clone(),
        None if pinned.wb_modes.len() == 1 => pinned.wb_modes[0].clone(),
        None => return None,
    };
    Some((firmware, wb_mode))
}

pub fn point_matches(
    point: &AssessedPoint,
    pinned: &PinnedModes,
    settings: &SettingCombination,
    firmware: Option<&FirmwareId>,
    wb_mode: Option<&WbModeKey>,
) -> bool {
    let Some((point_firmware, point_wb_mode)) = resolve_point(point, pinned) else {
        return false;
    };
    firmware == Some(&point_firmware)
        && wb_mode == Some(&point_wb_mode)
        && point
            .settings
            .iter()
            .all(|(key, value)| settings.get(key) == Some(value))
}

pub(crate) fn setting_combination(
    settings: &looklab_model::realization::Settings,
    epoch: &looklab_schema::Epoch,
) -> SettingCombination {
    let mut combination = SettingCombination::new();
    for control in &epoch.controls {
        if !control.look_affecting {
            continue;
        }
        let value = settings
            .get(&control.key)
            .cloned()
            .unwrap_or_else(|| default_value(control));
        combination.insert(control.key.clone(), setting_value(&value));
    }
    combination
}

pub(crate) fn setting_value(value: &looklab_model::realization::ControlValue) -> SettingValue {
    match value {
        looklab_model::realization::ControlValue::Step(step) => SettingValue::Step(*step),
        looklab_model::realization::ControlValue::Enum(value) => SettingValue::Enum(value.clone()),
        looklab_model::realization::ControlValue::Bool(value) => SettingValue::Bool(*value),
    }
}

pub(crate) fn default_value(
    control: &looklab_schema::Control,
) -> looklab_model::realization::ControlValue {
    use looklab_model::realization::ControlValue;
    use looklab_schema::ControlDefault;
    match &control.default {
        ControlDefault::Number(number) => {
            let index = control
                .steps()
                .into_iter()
                .find(|index| {
                    control
                        .value_at(*index)
                        .is_some_and(|value| value == *number)
                })
                .unwrap_or(0);
            ControlValue::Step(index)
        }
        ControlDefault::Enum(value) => ControlValue::Enum(value.clone()),
        ControlDefault::Bool(value) => ControlValue::Bool(*value),
    }
}

pub(crate) fn supported_mask(cells: impl IntoIterator<Item = CellId>) -> SupportedMask {
    let mut mask = SupportedMask::new();
    for cell in cells {
        mask.insert(cell);
    }
    mask
}
