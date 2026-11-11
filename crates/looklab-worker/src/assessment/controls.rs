// SPDX-License-Identifier: AGPL-3.0-or-later
//! Observed-JPEG controls and settings matching (docs/02 §5.1, docs/04 §2).

use std::collections::BTreeSet;

use looklab_color::ucs::{Ucs, delta_e_ucs};
use looklab_model::realization::{ControlValue, Settings};
use looklab_schema::{
    AssessedPoint, CellId, ControlKey, ControlSchema, ControlType, Epoch, EvidenceRequestKind,
    FirmwareId, MetricKey, ModelSubject, QualificationCapability, ReportMetrics, RuleRevision,
    SettingCombination, SupportedMask, WbModeKey,
};

use crate::assessment::{
    AssessmentBinding, CONTROLS_MATCHING_RULE, EvidenceGroup, EvidenceSource,
    P22_OBSERVED_MEDIAN_DE, P23_ERROR_TOLERANCE_DE, P23_RECOVERY_STEPS, PinnedModes,
    ProtocolAssessment, ReportParts, build_report, default_value, evidence_membership,
    evidence_request, limitation, median, metric, outcome, point_matches, resolve_point,
    setting_combination, setting_value, supported_mask,
};

#[derive(Clone, Debug)]
pub struct SettingObservation {
    pub group: EvidenceGroup,
    pub settings: Settings,
    pub firmware: Option<FirmwareId>,
    pub wb_mode: Option<WbModeKey>,
    pub source: EvidenceSource,
}

#[derive(Clone, Debug)]
pub struct ObservedJpeg {
    pub group: EvidenceGroup,
    pub settings: Settings,
    pub firmware: Option<FirmwareId>,
    pub wb_mode: Option<WbModeKey>,
    pub cell: CellId,
    pub predicted: Ucs,
    pub observed: Ucs,
    pub source: EvidenceSource,
}

#[derive(Clone, Debug)]
pub struct RecoveryObservation {
    pub group: EvidenceGroup,
    pub true_settings: Settings,
    pub recovered_settings: Settings,
    pub firmware: Option<FirmwareId>,
    pub wb_mode: Option<WbModeKey>,
    pub predicted_true: Ucs,
    pub observed_true: Ucs,
    pub predicted_recovered: Ucs,
    pub source: EvidenceSource,
}

#[derive(Clone, Debug)]
pub struct ControlClaim {
    pub key: ControlKey,
    pub values: Vec<ControlValue>,
    pub interval: Option<(i64, i64)>,
}

pub struct ControlsAssessmentInput {
    pub binding: AssessmentBinding,
    pub schema: ControlSchema,
    pub epoch: u32,
    pub pinned: PinnedModes,
    pub training: Vec<SettingObservation>,
    pub held_out: Vec<ObservedJpeg>,
    pub recoveries: Vec<RecoveryObservation>,
    pub claims: Vec<ControlClaim>,
}

/// # Errors
/// The realized model cannot evaluate the settings or scene.
#[allow(clippy::too_many_arguments)]
pub fn observed_rerender(
    group: EvidenceGroup,
    settings: Settings,
    firmware: Option<FirmwareId>,
    wb_mode: Option<WbModeKey>,
    cell: CellId,
    model: &looklab_match::MatchModel,
    scene: looklab_color::Xyz,
    observed: Ucs,
    source: EvidenceSource,
) -> Result<ObservedJpeg, looklab_match::MatchError> {
    Ok(ObservedJpeg {
        group,
        predicted: model.render(&settings, scene)?,
        settings,
        firmware,
        wb_mode,
        cell,
        observed,
        source,
    })
}

pub fn assess_controls(input: ControlsAssessmentInput) -> ProtocolAssessment {
    let ControlsAssessmentInput {
        binding,
        schema,
        epoch,
        pinned,
        training,
        held_out,
        recoveries,
        claims,
    } = input;
    let rule = RuleRevision::new(CONTROLS_MATCHING_RULE).expect("controls rule revision");
    let mut metrics = ReportMetrics::new();
    let mut limitations = Vec::new();
    let mut requests = Vec::new();
    let mut execution = false;
    let mut failed = false;
    let mut insufficient = false;

    if let Err(detail) = binding.validate() {
        execution = true;
        limitations.push(limitation(detail));
    }
    if !matches!(&binding.subject, ModelSubject::BodyRealization { .. }) {
        execution = true;
        limitations.push(limitation(
            "controls and matching are a claim about a named BodyRealization",
        ));
    }
    if let ModelSubject::BodyRealization { realization } = &binding.subject
        && !binding.scope.assessed_bodies.contains(&realization.body)
    {
        execution = true;
        limitations.push(limitation(
            "the report scope does not name the realization body",
        ));
    }
    let Some(epoch_schema) = schema.epochs.iter().find(|entry| entry.id == epoch) else {
        execution = true;
        limitations.push(limitation(format!("epoch {epoch} is not declared")));
        return finish(
            binding,
            rule,
            execution,
            failed,
            insufficient,
            metrics,
            SupportedMask::new(),
            limitations,
            requests,
            training,
            held_out,
            recoveries,
        );
    };

    let training_combinations: Vec<SettingCombination> = training
        .iter()
        .map(|entry| setting_combination(&entry.settings, epoch_schema))
        .collect();
    let training_groups: BTreeSet<EvidenceGroup> =
        training.iter().map(|entry| entry.group.clone()).collect();
    let scope_points: Vec<&AssessedPoint> = binding.scope.points.iter().collect();
    if scope_points.is_empty() {
        insufficient = true;
        requests.push(evidence_request(
            EvidenceRequestKind::MissingSettings,
            "the report claims no assessed point",
        ));
    }
    for point in &scope_points {
        if resolve_point(point, &pinned).is_none() {
            insufficient = true;
            requests.push(evidence_request(
                EvidenceRequestKind::MissingControlValues,
                "an assessed point does not resolve to one pinned firmware and WB mode",
            ));
        }
    }

    let mut leaked = false;
    let mut synthetic = false;
    let mut seen = false;
    let mut eligible = Vec::new();
    let mut assessed_cells: BTreeSet<CellId> = BTreeSet::new();
    for observation in &held_out {
        if training_groups.contains(&observation.group) {
            leaked = true;
            continue;
        }
        if observation.source == EvidenceSource::Synthetic {
            synthetic = true;
            continue;
        }
        if !point_in_scope(
            &observation.settings,
            observation.firmware.as_ref(),
            observation.wb_mode.as_ref(),
            epoch_schema,
            &scope_points,
            &pinned,
        ) {
            continue;
        }
        seen = true;
        assessed_cells.insert(observation.cell.clone());
        let combination = setting_combination(&observation.settings, epoch_schema);
        if training_combinations.contains(&combination) {
            continue;
        }
        if !finite_ucs(observation.predicted) || !finite_ucs(observation.observed) {
            execution = true;
            continue;
        }
        eligible.push((
            observation.cell.clone(),
            observation.group.clone(),
            delta_e_ucs(observation.predicted, observation.observed),
        ));
    }
    if leaked {
        insufficient = true;
        requests.push(evidence_request(
            EvidenceRequestKind::MissingIndependentCaptures,
            "same-RAW re-render groups cross the training/assessment partition",
        ));
    }
    if synthetic {
        limitations.push(limitation(
            "synthetic re-renders are engine validation, not observed camera evidence",
        ));
    }
    if !seen {
        insufficient = true;
        requests.push(evidence_request(
            EvidenceRequestKind::MissingSettings,
            "no held-out observed JPEG covers a claimed settings combination",
        ));
    }
    if eligible.is_empty() {
        insufficient = true;
        requests.push(evidence_request(
            EvidenceRequestKind::MissingSettings,
            "no held-out observed JPEG is at an unseen assessed setting",
        ));
    }

    let observed_median = median(
        &eligible
            .iter()
            .map(|(_, _, error)| *error)
            .collect::<Vec<_>>(),
    );
    if let Some(value) = observed_median {
        metric(&mut metrics, MetricKey::MedianDePostClip, value);
        if value > P22_OBSERVED_MEDIAN_DE {
            failed = true;
            limitations.push(limitation(format!(
                "observed-JPEG median {value:.3} ΔE exceeds the {P22_OBSERVED_MEDIAN_DE:.3} ΔE bound"
            )));
        }
    }

    for point in &scope_points {
        if !held_out.iter().any(|observation| {
            observation.source == EvidenceSource::Observed
                && point_matches(
                    point,
                    &pinned,
                    &setting_combination(&observation.settings, epoch_schema),
                    observation.firmware.as_ref(),
                    observation.wb_mode.as_ref(),
                )
        }) {
            insufficient = true;
            requests.push(evidence_request(
                EvidenceRequestKind::MissingSettings,
                format!("missing held-out observed JPEG for assessed point {point:?}"),
            ));
        }
    }

    for claim in &claims {
        let Some(control) = epoch_schema
            .controls
            .iter()
            .find(|control| control.key == claim.key)
        else {
            execution = true;
            limitations.push(limitation(format!(
                "claimed control {} is not declared in epoch {epoch}",
                claim.key
            )));
            continue;
        };
        let mut values: Vec<ControlValue> = Vec::new();
        if claim.interval.is_some()
            && !matches!(control.kind, ControlType::Int | ControlType::Decimal)
        {
            execution = true;
            limitations.push(limitation(format!(
                "control {} is not numeric and cannot carry an interval claim",
                claim.key
            )));
            continue;
        }
        for value in claim.values.iter().cloned().chain(
            claim
                .interval
                .into_iter()
                .flat_map(|(low, high)| (low..=high).map(ControlValue::Step)),
        ) {
            if !values.contains(&value) {
                values.push(value);
            }
        }
        if let Some((low, high)) = claim.interval
            && low > high
        {
            execution = true;
            limitations.push(limitation(format!(
                "claimed interval {low}..={high} for {} is empty",
                claim.key
            )));
            continue;
        }
        for value in values {
            if !claim_value_in_scope(claim, &value, &scope_points) {
                insufficient = true;
                requests.push(evidence_request(
                    EvidenceRequestKind::MissingSettings,
                    format!(
                        "claimed {}={value:?} is absent from the report scope",
                        claim.key
                    ),
                ));
            }
            let matched = held_out.iter().any(|observation| {
                observation.source == EvidenceSource::Observed
                    && isolated_value_matches(&observation.settings, epoch_schema, control, &value)
            });
            if !matched {
                insufficient = true;
                requests.push(evidence_request(
                    EvidenceRequestKind::MissingControlValues,
                    format!(
                        "missing isolated held-out observation for {}={value:?}",
                        claim.key
                    ),
                ));
            }
        }
    }

    let mut recovery_passed = 0usize;
    let mut recovery_total = 0usize;
    let mut recovery_failed = false;
    for recovery in &recoveries {
        if training_groups.contains(&recovery.group) {
            leaked = true;
            continue;
        }
        if recovery.source == EvidenceSource::Synthetic {
            limitations.push(limitation(
                "synthetic recovery is engine validation, not observed camera evidence",
            ));
            continue;
        }
        let true_combination = setting_combination(&recovery.true_settings, epoch_schema);
        if !point_in_scope(
            &recovery.true_settings,
            recovery.firmware.as_ref(),
            recovery.wb_mode.as_ref(),
            epoch_schema,
            &scope_points,
            &pinned,
        ) {
            limitations.push(limitation(
                "a recovery target lies outside the assessed physical points",
            ));
            continue;
        }
        if true_combination == setting_combination(&Settings::new(), epoch_schema) {
            limitations.push(limitation(
                "a default-setting re-render is not a P2.3 non-default recovery target",
            ));
            continue;
        }
        if !finite_ucs(recovery.predicted_true)
            || !finite_ucs(recovery.predicted_recovered)
            || !finite_ucs(recovery.observed_true)
        {
            execution = true;
            continue;
        }
        recovery_total += 1;
        let true_error = delta_e_ucs(recovery.predicted_true, recovery.observed_true);
        let recovered_error = delta_e_ucs(recovery.predicted_recovered, recovery.observed_true);
        let within_steps = epoch_schema.controls.iter().all(|control| {
            if !control.look_affecting {
                return true;
            }
            let Some(true_value) = recovery.true_settings.get(&control.key) else {
                return false;
            };
            let Some(recovered_value) = recovery.recovered_settings.get(&control.key) else {
                return false;
            };
            recovery_control_within(true_value, recovered_value)
        });
        if within_steps || (recovered_error - true_error).abs() <= P23_ERROR_TOLERANCE_DE {
            recovery_passed += 1;
        } else {
            recovery_failed = true;
        }
    }
    if recovery_total == 0 {
        insufficient = true;
        requests.push(evidence_request(
            EvidenceRequestKind::MissingSettings,
            "no held-out non-default settings recovery evidence",
        ));
    }
    if recovery_total > 0 {
        metric(
            &mut metrics,
            MetricKey::RecoveryRate,
            recovery_passed as f64 / recovery_total as f64,
        );
    }
    if recovery_failed {
        failed = true;
        limitations.push(limitation(
            "a held-out target missed the one-step and 0.5 ΔE recovery alternatives",
        ));
    }
    if leaked {
        insufficient = true;
    }

    let supported_cells: BTreeSet<CellId> =
        eligible.iter().map(|(cell, _, _)| cell.clone()).collect();
    let mask: SupportedMask = supported_mask(supported_cells.iter().cloned());
    let supported_count = mask.len();
    let assessed_count = assessed_cells.len().max(supported_count);
    let coverage = if assessed_count == 0 {
        0.0
    } else {
        supported_count as f64 / assessed_count as f64
    };
    metric(
        &mut metrics,
        MetricKey::SupportedCells,
        supported_count as f64,
    );
    metric(
        &mut metrics,
        MetricKey::AssessedCells,
        assessed_count as f64,
    );
    metric(&mut metrics, MetricKey::Coverage, coverage);
    if !eligible.is_empty() {
        limitations.push(limitation(
            "observed-JPEG re-render error, not latent mean-response uncertainty",
        ));
    }

    finish(
        binding,
        rule,
        execution,
        failed,
        insufficient,
        metrics,
        mask,
        limitations,
        requests,
        training,
        held_out,
        recoveries,
    )
}

#[allow(clippy::too_many_arguments)]
fn finish(
    binding: AssessmentBinding,
    rule: RuleRevision,
    execution: bool,
    failed: bool,
    insufficient: bool,
    metrics: ReportMetrics,
    mask: SupportedMask,
    mut limitations: Vec<looklab_schema::Limitation>,
    requests: Vec<looklab_schema::EvidenceRequest>,
    training: Vec<SettingObservation>,
    held_out: Vec<ObservedJpeg>,
    recoveries: Vec<RecoveryObservation>,
) -> ProtocolAssessment {
    limitations.push(limitation(
        "controls absent from supplied settings are treated as unknown, not defaults",
    ));
    let membership = evidence_membership(
        training.iter().map(|entry| entry.group.clone()).collect(),
        held_out
            .iter()
            .map(|entry| entry.group.clone())
            .chain(recoveries.iter().map(|entry| entry.group.clone()))
            .collect(),
    );
    build_report(
        &binding,
        ReportParts {
            capability: QualificationCapability::ControlsMatching,
            rule,
            outcome: outcome(execution, failed, insufficient),
            metrics,
            supported_mask: mask,
            limitations,
            evidence_requests: requests,
            extra_dependencies: Vec::new(),
            membership,
            cohort: None,
        },
    )
}

fn finite_ucs(value: Ucs) -> bool {
    value.j.is_finite() && value.a.is_finite() && value.b.is_finite()
}

/// Whether a passing controls report covers the requested physical point and cell.
pub fn matching_scope_covers(
    report: &looklab_schema::ModelReport,
    epoch: &Epoch,
    settings: &Settings,
    firmware: Option<&FirmwareId>,
    wb_mode: Option<&WbModeKey>,
    pinned: &PinnedModes,
    cell: &CellId,
) -> bool {
    let combination = setting_combination(settings, epoch);
    report.outcome == looklab_schema::AssessmentOutcome::Pass
        && report.supported_mask.contains(cell)
        && report
            .scope
            .points
            .iter()
            .any(|point| point_matches(point, pinned, &combination, firmware, wb_mode))
}

fn point_in_scope(
    settings: &Settings,
    firmware: Option<&FirmwareId>,
    wb_mode: Option<&WbModeKey>,
    epoch: &Epoch,
    scope: &[&AssessedPoint],
    pinned: &PinnedModes,
) -> bool {
    let combination = setting_combination(settings, epoch);
    scope
        .iter()
        .any(|point| point_matches(point, pinned, &combination, firmware, wb_mode))
}

fn isolated_value_matches(
    settings: &Settings,
    epoch: &Epoch,
    control: &looklab_schema::Control,
    value: &ControlValue,
) -> bool {
    for other in &epoch.controls {
        if !other.look_affecting {
            continue;
        }
        let Some(actual) = settings.get(&other.key) else {
            return false;
        };
        if other.key == control.key {
            if *actual != *value {
                return false;
            }
        } else if *actual != default_value(other) {
            return false;
        }
    }
    true
}

fn recovery_control_within(true_value: &ControlValue, recovered: &ControlValue) -> bool {
    match (true_value, recovered) {
        (ControlValue::Step(true_step), ControlValue::Step(recovered_step)) => {
            (*true_step - *recovered_step).abs() <= P23_RECOVERY_STEPS
        }
        (true_value, recovered) => true_value == recovered,
    }
}

fn claim_value_in_scope(
    claim: &ControlClaim,
    value: &ControlValue,
    scope: &[&AssessedPoint],
) -> bool {
    let value = setting_value(value);
    scope
        .iter()
        .any(|point| point.settings.get(&claim.key) == Some(&value))
}
