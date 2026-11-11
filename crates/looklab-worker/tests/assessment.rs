// SPDX-License-Identifier: AGPL-3.0-or-later
//! Consumer-facing behaviour of the versioned qualification protocols.

use looklab_color::ucs::Ucs;
use looklab_model::realization::{ControlValue, Settings};
use looklab_schema::{
    AssessedPoint, AssessmentOutcome, BodyId, BodyRealizationId, BodyRealizationVersionId, CellId,
    ControlEpochId, DependencyKind, DependencyRef, DependencyTarget, DomainId, EvidenceRequestKind,
    FamilyTargetId, FirmwareId, LookId, ModelReportId, ModelSubject, ModelVersionId,
    OutputEncoding, ProbeSetId, RealizationVersion, ReportProvenance, ReportScope, RuleRevision,
    SettingCombination, SettingValue, Sha256Hex, SimKey, SourceRef, TargetVersion, WbModeKey,
};
use looklab_worker::assessment::{
    AssessmentBinding, AssessmentRequest, ControlClaim, ControlsAssessmentInput,
    CrossBodyAssessmentInput, CrossBodyEvidence, ENGINE_COVERAGE_RULE, EngineTrial, EngineVersion,
    EvidenceGroup, EvidenceSource, GaugeBindingId, NativeAssessmentInput, NativeObservation,
    NativeProbeSupport, ObservedJpeg, P19_DISAGREEMENT_MEDIAN_DE, P22_OBSERVED_MEDIAN_DE,
    PinnedModes, ProbePrediction, RecoveryObservation, SettingObservation, assess, assess_controls,
    assess_cross_body, assess_native, assess_selected, matching_scope_covers, qualify_engine,
};

fn body(name: &str) -> BodyId {
    BodyId::new(name).expect("body")
}

fn look(name: &str) -> LookId {
    LookId::new(name).expect("look")
}

fn sim(name: &str) -> SimKey {
    SimKey::new(name).expect("sim")
}

fn source(name: &str) -> SourceRef {
    SourceRef::new(name).expect("source")
}

fn cell(name: &str) -> CellId {
    CellId::new(name).expect("cell")
}

fn ucs(j: f64, a: f64, b: f64) -> Ucs {
    Ucs::new(j, a, b)
}

fn group(name: &str) -> EvidenceGroup {
    EvidenceGroup::new(name).expect("group")
}

fn target() -> FamilyTargetId {
    FamilyTargetId {
        family: look("classic-chrome"),
        version: TargetVersion::new(1).expect("version"),
    }
}

fn realization(body: &BodyId) -> BodyRealizationId {
    BodyRealizationId {
        body: body.clone(),
        epoch: ControlEpochId::new(1),
        native_sim: sim("classic_chrome"),
    }
}

fn provenance() -> ReportProvenance {
    ReportProvenance {
        rule_revision: RuleRevision::new("test/1").expect("rule"),
        fitter: source("looklab-fit@test"),
        evaluator: source("looklab-model@test"),
        model: source("looklab-model@test"),
        probe_set: ProbeSetId::new("v1").expect("probes"),
        effective_options: Sha256Hex::from_bytes([0; 32]),
        partition_policy: Sha256Hex::from_bytes([1; 32]),
        assessment_seeds: vec![1],
    }
}

fn firmware() -> FirmwareId {
    FirmwareId {
        body: body("fujifilm/x-t5"),
        epoch: ControlEpochId::new(1),
        version: "1.0".parse().expect("firmware version"),
    }
}

fn wb_mode() -> WbModeKey {
    WbModeKey::new("auto").expect("wb mode")
}

fn pinned() -> PinnedModes {
    PinnedModes {
        firmware: vec![firmware()],
        wb_modes: vec![wb_mode()],
    }
}

fn point(settings: SettingCombination) -> AssessedPoint {
    AssessedPoint {
        settings,
        firmware: Some(firmware()),
        wb_mode: Some(wb_mode()),
    }
}

fn scope(settings: Vec<SettingCombination>, bodies: Vec<BodyId>) -> ReportScope {
    ReportScope {
        points: settings.into_iter().map(point).collect(),
        combination_basis: looklab_schema::CombinationBasis::Enumerated,
        defaults_assessed: true,
        output_encoding: OutputEncoding::Srgb,
        colour_cells: vec![cell("cell-0")],
        exposure_cells: vec![cell("ev0")],
        assessed_bodies: bodies,
        assessed_domain: DomainId::new("supported-probes").expect("domain"),
    }
}

fn binding(body: &BodyId, scope: ReportScope) -> AssessmentBinding {
    let realization = realization(body);
    AssessmentBinding {
        subject: ModelSubject::BodyRealization {
            realization: realization.clone(),
        },
        candidate: ModelVersionId::BodyRealization {
            realization: BodyRealizationVersionId {
                realization,
                version: RealizationVersion::new(1).expect("version"),
                target: target(),
            },
        },
        scope,
        provenance: provenance(),
        dependencies: vec![DependencyRef::required(
            DependencyKind::Transform,
            DependencyTarget::transform(body, 1),
        )],
    }
}

fn family_binding(bodies: Vec<BodyId>, scope: ReportScope) -> AssessmentBinding {
    AssessmentBinding {
        subject: ModelSubject::SharedFamily {
            family: look("classic-chrome"),
        },
        candidate: ModelVersionId::SharedFamily { target: target() },
        scope,
        provenance: provenance(),
        dependencies: bodies
            .iter()
            .map(|body| {
                DependencyRef::required(
                    DependencyKind::Transform,
                    DependencyTarget::transform(body, 1),
                )
            })
            .collect(),
    }
}

fn setting_value(value: &ControlValue) -> SettingValue {
    match value {
        ControlValue::Step(step) => SettingValue::Step(*step),
        ControlValue::Enum(value) => SettingValue::Enum(value.clone()),
        ControlValue::Bool(value) => SettingValue::Bool(*value),
    }
}

fn combination(entries: &[(&looklab_schema::ControlKey, ControlValue)]) -> SettingCombination {
    let mut combination = SettingCombination::new();
    for (key, value) in entries {
        combination.insert((*key).clone(), setting_value(value));
    }
    combination
}

fn step_domains() -> Vec<(looklab_schema::ControlKey, i64, Vec<i64>)> {
    let plan = looklab_match::synth::plan();
    plan.domains
        .iter()
        .filter_map(|domain| {
            let ControlValue::Step(default) = domain.default_value() else {
                return None;
            };
            let values: Vec<i64> = domain
                .values
                .iter()
                .filter_map(|value| match value {
                    ControlValue::Step(step) => Some(*step),
                    _ => None,
                })
                .collect();
            (values.len() >= 3).then(|| (domain.key.clone(), *default, values))
        })
        .collect()
}

fn settings_with(entries: &[(&looklab_schema::ControlKey, ControlValue)]) -> Settings {
    let mut settings = looklab_match::synth::plan().default.clone();
    for (key, value) in entries {
        settings.set((*key).clone(), value.clone());
    }
    settings
}

fn native_support() -> Vec<NativeProbeSupport> {
    vec![NativeProbeSupport {
        probe: looklab_model::probes::v1().probes[0].id.clone(),
        cell: cell("cell-0"),
        supported: true,
        held_out_evidence: true,
    }]
}

#[test]
fn native_group_leakage_is_insufficient() {
    let input = NativeAssessmentInput {
        binding: binding(
            &body("fujifilm/x-t5"),
            scope(vec![combination(&[])], vec![body("fujifilm/x-t5")]),
        ),
        encoding: OutputEncoding::Srgb,
        pinned: pinned(),
        min_independent_groups: 1,
        support: native_support(),
        training_groups: vec![group("same-raw")],
        observations: vec![NativeObservation {
            group: group("same-raw"),
            settings: combination(&[]),
            firmware: Some(firmware()),
            wb_mode: Some(wb_mode()),
            cell: cell("cell-0"),
            predicted: ucs(50.0, 0.0, 0.0),
            observed: ucs(50.0, 0.0, 0.0),
            source: EvidenceSource::Observed,
        }],
        support_sigma: 1.5,
    };
    let assessment = assess_native(input);
    assert_eq!(
        assessment.report.outcome,
        AssessmentOutcome::InsufficientEvidence
    );
    assert!(
        assessment
            .report
            .evidence_requests
            .iter()
            .any(|request| { request.kind == EvidenceRequestKind::MissingIndependentCaptures })
    );
}

#[test]
fn native_contrary_held_out_error_fails() {
    let input = NativeAssessmentInput {
        binding: binding(
            &body("fujifilm/x-t5"),
            scope(vec![combination(&[])], vec![body("fujifilm/x-t5")]),
        ),
        encoding: OutputEncoding::Srgb,
        pinned: pinned(),
        min_independent_groups: 1,
        support: native_support(),
        training_groups: vec![group("train")],
        observations: vec![NativeObservation {
            group: group("held-out"),
            settings: combination(&[]),
            firmware: Some(firmware()),
            wb_mode: Some(wb_mode()),
            cell: cell("cell-0"),
            predicted: ucs(50.0, 0.0, 0.0),
            observed: ucs(53.0, 0.0, 0.0),
            source: EvidenceSource::Observed,
        }],
        support_sigma: 1.5,
    };
    let assessment = assess_native(input);
    assert_eq!(assessment.report.outcome, AssessmentOutcome::Fail);
}

#[test]
fn native_accepts_adobe_rgb_scope() {
    let mut report_scope = scope(vec![combination(&[])], vec![body("fujifilm/x-t5")]);
    report_scope.output_encoding = OutputEncoding::AdobeRgb;
    let input = NativeAssessmentInput {
        binding: binding(&body("fujifilm/x-t5"), report_scope),
        encoding: OutputEncoding::AdobeRgb,
        pinned: pinned(),
        min_independent_groups: 1,
        support: native_support(),
        training_groups: Vec::new(),
        observations: vec![NativeObservation {
            group: group("held-out"),
            settings: combination(&[]),
            firmware: Some(firmware()),
            wb_mode: Some(wb_mode()),
            cell: cell("cell-0"),
            predicted: ucs(50.0, 0.0, 0.0),
            observed: ucs(50.0, 0.0, 0.0),
            source: EvidenceSource::Observed,
        }],
        support_sigma: 1.5,
    };
    let assessment = assess_native(input);
    assert_eq!(assessment.report.outcome, AssessmentOutcome::Pass);
}

#[test]
fn native_uses_group_means_not_observation_median() {
    let observation = |group_name: &str, residual: f64| NativeObservation {
        group: group(group_name),
        settings: combination(&[]),
        firmware: Some(firmware()),
        wb_mode: Some(wb_mode()),
        cell: cell("cell-0"),
        predicted: ucs(50.0 + residual, 0.0, 0.0),
        observed: ucs(50.0, 0.0, 0.0),
        source: EvidenceSource::Observed,
    };
    let mut observations = vec![observation("bad", 10.0)];
    for _ in 0..10 {
        observations.push(observation("good-a", 0.0));
        observations.push(observation("good-b", 0.0));
    }
    let input = NativeAssessmentInput {
        binding: binding(
            &body("fujifilm/x-t5"),
            scope(vec![combination(&[])], vec![body("fujifilm/x-t5")]),
        ),
        encoding: OutputEncoding::Srgb,
        pinned: pinned(),
        min_independent_groups: 3,
        support: native_support(),
        training_groups: Vec::new(),
        observations,
        support_sigma: 1.5,
    };
    let assessment = assess_native(input);
    assert_eq!(assessment.report.outcome, AssessmentOutcome::Fail);
}

#[test]
fn native_does_not_cancel_residuals_across_physical_points() {
    let domains = step_domains();
    let (key, default, _) = domains[0].clone();
    let first = combination(&[(&key, ControlValue::Step(default + 1))]);
    let second = combination(&[(&key, ControlValue::Step(default + 2))]);
    let observation =
        |group_name: &str, settings: SettingCombination, predicted: f64| NativeObservation {
            group: group(group_name),
            settings,
            firmware: Some(firmware()),
            wb_mode: Some(wb_mode()),
            cell: cell("cell-0"),
            predicted: ucs(predicted, 0.0, 0.0),
            observed: ucs(50.0, 0.0, 0.0),
            source: EvidenceSource::Observed,
        };
    let input = NativeAssessmentInput {
        binding: binding(
            &body("fujifilm/x-t5"),
            scope(
                vec![first.clone(), second.clone()],
                vec![body("fujifilm/x-t5")],
            ),
        ),
        encoding: OutputEncoding::Srgb,
        pinned: pinned(),
        min_independent_groups: 1,
        support: native_support(),
        training_groups: Vec::new(),
        observations: vec![
            observation("positive", first, 60.0),
            observation("negative", second, 40.0),
        ],
        support_sigma: 1.5,
    };
    let assessment = assess_native(input);
    assert_eq!(assessment.report.outcome, AssessmentOutcome::Fail);
}

#[test]
fn native_repeated_rows_in_one_group_do_not_create_independent_support() {
    let observations: Vec<NativeObservation> = (0..10)
        .map(|_| NativeObservation {
            group: group("one-group"),
            settings: combination(&[]),
            firmware: Some(firmware()),
            wb_mode: Some(wb_mode()),
            cell: cell("cell-0"),
            predicted: ucs(50.0, 0.0, 0.0),
            observed: ucs(50.0, 0.0, 0.0),
            source: EvidenceSource::Observed,
        })
        .collect();
    let input = NativeAssessmentInput {
        binding: binding(
            &body("fujifilm/x-t5"),
            scope(vec![combination(&[])], vec![body("fujifilm/x-t5")]),
        ),
        encoding: OutputEncoding::Srgb,
        pinned: pinned(),
        min_independent_groups: 3,
        support: native_support(),
        training_groups: Vec::new(),
        observations,
        support_sigma: 1.5,
    };
    let assessment = assess_native(input);
    assert_eq!(
        assessment.report.outcome,
        AssessmentOutcome::InsufficientEvidence
    );
    assert!(
        assessment
            .report
            .evidence_requests
            .iter()
            .any(|request| { request.kind == EvidenceRequestKind::MissingIndependentCaptures })
    );
}

#[test]
fn native_out_of_domain_rows_do_not_poison_a_supported_pass() {
    let mut report_scope = scope(vec![combination(&[])], vec![body("fujifilm/x-t5")]);
    report_scope.colour_cells = vec![cell("cell-0"), cell("cell-999")];
    let observation = |group_name: &str, cell_name: &str, residual: f64| NativeObservation {
        group: group(group_name),
        settings: combination(&[]),
        firmware: Some(firmware()),
        wb_mode: Some(wb_mode()),
        cell: cell(cell_name),
        predicted: ucs(50.0 + residual, 0.0, 0.0),
        observed: ucs(50.0, 0.0, 0.0),
        source: EvidenceSource::Observed,
    };
    let input = NativeAssessmentInput {
        binding: binding(&body("fujifilm/x-t5"), report_scope),
        encoding: OutputEncoding::Srgb,
        pinned: pinned(),
        min_independent_groups: 1,
        support: native_support(),
        training_groups: Vec::new(),
        observations: vec![
            observation("supported", "cell-0", 0.0),
            observation("outside", "cell-999", 40.0),
        ],
        support_sigma: 1.5,
    };
    let assessment = assess_native(input);
    assert_eq!(assessment.report.outcome, AssessmentOutcome::Pass);
    assert_eq!(assessment.report.scope.colour_cells, vec![cell("cell-0")]);
    assert!(assessment.report.supported_mask.contains(&cell("cell-0")));
    assert!(!assessment.report.supported_mask.contains(&cell("cell-999")));
}

#[test]
fn native_out_of_domain_rows_do_not_poison_a_supported_fail() {
    let observation = |group_name: &str, cell_name: &str, residual: f64| NativeObservation {
        group: group(group_name),
        settings: combination(&[]),
        firmware: Some(firmware()),
        wb_mode: Some(wb_mode()),
        cell: cell(cell_name),
        predicted: ucs(50.0 + residual, 0.0, 0.0),
        observed: ucs(50.0, 0.0, 0.0),
        source: EvidenceSource::Observed,
    };
    let input = NativeAssessmentInput {
        binding: binding(
            &body("fujifilm/x-t5"),
            scope(vec![combination(&[])], vec![body("fujifilm/x-t5")]),
        ),
        encoding: OutputEncoding::Srgb,
        pinned: pinned(),
        min_independent_groups: 1,
        support: native_support(),
        training_groups: Vec::new(),
        observations: vec![
            observation("supported", "cell-0", 10.0),
            observation("outside", "cell-999", 0.0),
        ],
        support_sigma: 1.5,
    };
    let assessment = assess_native(input);
    assert_eq!(assessment.report.outcome, AssessmentOutcome::Fail);
}

#[test]
fn native_out_of_domain_rows_alone_are_insufficient() {
    let input = NativeAssessmentInput {
        binding: binding(
            &body("fujifilm/x-t5"),
            scope(vec![combination(&[])], vec![body("fujifilm/x-t5")]),
        ),
        encoding: OutputEncoding::Srgb,
        pinned: pinned(),
        min_independent_groups: 1,
        support: native_support(),
        training_groups: Vec::new(),
        observations: vec![NativeObservation {
            group: group("outside"),
            settings: combination(&[]),
            firmware: Some(firmware()),
            wb_mode: Some(wb_mode()),
            cell: cell("cell-999"),
            predicted: ucs(50.0, 0.0, 0.0),
            observed: ucs(50.0, 0.0, 0.0),
            source: EvidenceSource::Observed,
        }],
        support_sigma: 1.5,
    };
    let assessment = assess_native(input);
    assert_eq!(
        assessment.report.outcome,
        AssessmentOutcome::InsufficientEvidence
    );
    assert!(
        assessment
            .report
            .evidence_requests
            .iter()
            .any(|request| { request.kind == EvidenceRequestKind::MissingIndependentCaptures })
    );
}

#[test]
fn native_without_supported_cells_is_insufficient() {
    let mut support = native_support();
    support[0].supported = false;
    let input = NativeAssessmentInput {
        binding: binding(
            &body("fujifilm/x-t5"),
            scope(vec![combination(&[])], vec![body("fujifilm/x-t5")]),
        ),
        encoding: OutputEncoding::Srgb,
        pinned: pinned(),
        min_independent_groups: 1,
        support,
        training_groups: Vec::new(),
        observations: vec![NativeObservation {
            group: group("held-out"),
            settings: combination(&[]),
            firmware: Some(firmware()),
            wb_mode: Some(wb_mode()),
            cell: cell("cell-0"),
            predicted: ucs(50.0, 0.0, 0.0),
            observed: ucs(50.0, 0.0, 0.0),
            source: EvidenceSource::Observed,
        }],
        support_sigma: 1.5,
    };
    let assessment = assess_native(input);
    assert_eq!(
        assessment.report.outcome,
        AssessmentOutcome::InsufficientEvidence
    );
}

#[test]
fn controls_default_only_data_does_not_qualify_a_nondefault_claim() {
    let domains = step_domains();
    let (key, default, _) = domains[0].clone();
    let nondefault = ControlValue::Step(default + 1);
    let default_observation = ObservedJpeg {
        group: group("default"),
        settings: settings_with(&[(&key, ControlValue::Step(default))]),
        firmware: Some(firmware()),
        wb_mode: Some(wb_mode()),
        cell: cell("cell-0"),
        predicted: ucs(50.0, 0.0, 0.0),
        observed: ucs(50.0, 0.0, 0.0),
        source: EvidenceSource::Observed,
    };
    let input = ControlsAssessmentInput {
        binding: binding(
            &body("fujifilm/x-t5"),
            scope(
                vec![combination(&[(&key, ControlValue::Step(default))])],
                vec![body("fujifilm/x-t5")],
            ),
        ),
        schema: looklab_match::synth::schema(),
        epoch: 1,
        pinned: pinned(),
        training: Vec::new(),
        held_out: vec![default_observation],
        recoveries: Vec::new(),
        claims: vec![ControlClaim {
            key,
            values: vec![nondefault],
            interval: None,
        }],
    };
    let assessment = assess_controls(input);
    assert_eq!(
        assessment.report.outcome,
        AssessmentOutcome::InsufficientEvidence
    );
    assert!(
        assessment
            .report
            .evidence_requests
            .iter()
            .any(|request| { request.kind == EvidenceRequestKind::MissingControlValues })
    );
}

#[test]
fn controls_endpoints_do_not_cover_an_interval() {
    let domains = step_domains();
    let (key, default, values) = domains[0].clone();
    let low = values.iter().copied().min().expect("low");
    let high = values.iter().copied().max().expect("high");
    let observation = |step: i64| ObservedJpeg {
        group: group(&format!("g{step}")),
        settings: settings_with(&[(&key, ControlValue::Step(step))]),
        firmware: Some(firmware()),
        wb_mode: Some(wb_mode()),
        cell: cell("cell-0"),
        predicted: ucs(50.0, 0.0, 0.0),
        observed: ucs(50.0, 0.0, 0.0),
        source: EvidenceSource::Observed,
    };
    let recovery = RecoveryObservation {
        group: group("recovery"),
        firmware: Some(firmware()),
        wb_mode: Some(wb_mode()),
        true_settings: settings_with(&[(&key, ControlValue::Step(high))]),
        recovered_settings: settings_with(&[(&key, ControlValue::Step(high))]),
        predicted_true: ucs(50.0, 0.0, 0.0),
        observed_true: ucs(50.0, 0.0, 0.0),
        predicted_recovered: ucs(50.0, 0.0, 0.0),
        source: EvidenceSource::Observed,
    };
    let input = ControlsAssessmentInput {
        binding: binding(
            &body("fujifilm/x-t5"),
            scope(
                vec![
                    combination(&[(&key, ControlValue::Step(low))]),
                    combination(&[(&key, ControlValue::Step(default))]),
                    combination(&[(&key, ControlValue::Step(high))]),
                ],
                vec![body("fujifilm/x-t5")],
            ),
        ),
        schema: looklab_match::synth::schema(),
        epoch: 1,
        pinned: pinned(),
        training: Vec::new(),
        held_out: vec![observation(default), observation(low), observation(high)],
        recoveries: vec![recovery],
        claims: vec![ControlClaim {
            key,
            values: Vec::new(),
            interval: Some((low, high)),
        }],
    };
    let assessment = assess_controls(input);
    assert_eq!(
        assessment.report.outcome,
        AssessmentOutcome::InsufficientEvidence
    );
    assert!(
        assessment
            .report
            .evidence_requests
            .iter()
            .any(|request| { request.kind == EvidenceRequestKind::MissingControlValues })
    );
}

#[test]
fn controls_separate_controls_do_not_cover_their_product() {
    let domains = step_domains();
    let (first, first_default, _) = domains[0].clone();
    let (second, second_default, _) = domains[1].clone();
    let first_nondefault = ControlValue::Step(first_default + 1);
    let second_nondefault = ControlValue::Step(second_default + 1);
    let observation =
        |entries: &[(&looklab_schema::ControlKey, ControlValue)], name: &str| ObservedJpeg {
            group: group(name),
            settings: settings_with(entries),
            firmware: Some(firmware()),
            wb_mode: Some(wb_mode()),
            cell: cell("cell-0"),
            predicted: ucs(50.0, 0.0, 0.0),
            observed: ucs(50.0, 0.0, 0.0),
            source: EvidenceSource::Observed,
        };
    let recovery = RecoveryObservation {
        group: group("recovery"),
        firmware: Some(firmware()),
        wb_mode: Some(wb_mode()),
        true_settings: settings_with(&[(&first, first_nondefault.clone())]),
        recovered_settings: settings_with(&[(&first, first_nondefault.clone())]),
        predicted_true: ucs(50.0, 0.0, 0.0),
        observed_true: ucs(50.0, 0.0, 0.0),
        predicted_recovered: ucs(50.0, 0.0, 0.0),
        source: EvidenceSource::Observed,
    };
    let input = ControlsAssessmentInput {
        binding: binding(
            &body("fujifilm/x-t5"),
            scope(
                vec![combination(&[
                    (&first, first_nondefault.clone()),
                    (&second, second_nondefault.clone()),
                ])],
                vec![body("fujifilm/x-t5")],
            ),
        ),
        schema: looklab_match::synth::schema(),
        epoch: 1,
        pinned: pinned(),
        training: Vec::new(),
        held_out: vec![
            observation(
                &[
                    (&first, first_nondefault),
                    (&second, ControlValue::Step(second_default)),
                ],
                "first",
            ),
            observation(
                &[
                    (&first, ControlValue::Step(first_default)),
                    (&second, second_nondefault),
                ],
                "second",
            ),
        ],
        recoveries: vec![recovery],
        claims: Vec::new(),
    };
    let assessment = assess_controls(input);
    assert_eq!(
        assessment.report.outcome,
        AssessmentOutcome::InsufficientEvidence
    );
    assert!(
        assessment
            .report
            .evidence_requests
            .iter()
            .any(|request| { request.kind == EvidenceRequestKind::MissingSettings })
    );
}

#[test]
fn controls_group_leakage_is_insufficient() {
    let domains = step_domains();
    let (key, default, _) = domains[0].clone();
    let nondefault = ControlValue::Step(default + 1);
    let input = ControlsAssessmentInput {
        binding: binding(
            &body("fujifilm/x-t5"),
            scope(
                vec![combination(&[(&key, nondefault.clone())])],
                vec![body("fujifilm/x-t5")],
            ),
        ),
        schema: looklab_match::synth::schema(),
        epoch: 1,
        pinned: pinned(),
        training: vec![SettingObservation {
            group: group("same-raw"),
            settings: settings_with(&[(&key, nondefault.clone())]),
            firmware: Some(firmware()),
            wb_mode: Some(wb_mode()),
            source: EvidenceSource::Observed,
        }],
        held_out: vec![ObservedJpeg {
            group: group("same-raw"),
            settings: settings_with(&[(&key, nondefault.clone())]),
            firmware: Some(firmware()),
            wb_mode: Some(wb_mode()),
            cell: cell("cell-0"),
            predicted: ucs(50.0, 0.0, 0.0),
            observed: ucs(50.0, 0.0, 0.0),
            source: EvidenceSource::Observed,
        }],
        recoveries: vec![RecoveryObservation {
            group: group("recovery"),
            firmware: Some(firmware()),
            wb_mode: Some(wb_mode()),
            true_settings: settings_with(&[(&key, nondefault.clone())]),
            recovered_settings: settings_with(&[(&key, nondefault)]),
            predicted_true: ucs(50.0, 0.0, 0.0),
            observed_true: ucs(50.0, 0.0, 0.0),
            predicted_recovered: ucs(50.0, 0.0, 0.0),
            source: EvidenceSource::Observed,
        }],
        claims: Vec::new(),
    };
    let assessment = assess_controls(input);
    assert_eq!(
        assessment.report.outcome,
        AssessmentOutcome::InsufficientEvidence
    );
    assert!(
        assessment
            .report
            .evidence_requests
            .iter()
            .any(|request| { request.kind == EvidenceRequestKind::MissingIndependentCaptures })
    );
}

#[test]
fn controls_observed_median_above_bound_fails() {
    let domains = step_domains();
    let (key, default, _) = domains[0].clone();
    let nondefault = ControlValue::Step(default + 1);
    let input = ControlsAssessmentInput {
        binding: binding(
            &body("fujifilm/x-t5"),
            scope(
                vec![combination(&[(&key, nondefault.clone())])],
                vec![body("fujifilm/x-t5")],
            ),
        ),
        schema: looklab_match::synth::schema(),
        epoch: 1,
        pinned: pinned(),
        training: Vec::new(),
        held_out: vec![ObservedJpeg {
            group: group("held-out"),
            settings: settings_with(&[(&key, nondefault.clone())]),
            firmware: Some(firmware()),
            wb_mode: Some(wb_mode()),
            cell: cell("cell-0"),
            predicted: ucs(50.0, 0.0, 0.0),
            observed: ucs(54.0, 0.0, 0.0),
            source: EvidenceSource::Observed,
        }],
        recoveries: vec![RecoveryObservation {
            group: group("recovery"),
            firmware: Some(firmware()),
            wb_mode: Some(wb_mode()),
            true_settings: settings_with(&[(&key, nondefault.clone())]),
            recovered_settings: settings_with(&[(&key, nondefault)]),
            predicted_true: ucs(50.0, 0.0, 0.0),
            observed_true: ucs(50.0, 0.0, 0.0),
            predicted_recovered: ucs(50.0, 0.0, 0.0),
            source: EvidenceSource::Observed,
        }],
        claims: Vec::new(),
    };
    let assessment = assess_controls(input);
    assert_eq!(assessment.report.outcome, AssessmentOutcome::Fail);
    assert!(
        assessment
            .report
            .metrics
            .get(looklab_schema::MetricKey::MedianDePostClip)
            .expect("median")
            > P22_OBSERVED_MEDIAN_DE
    );
}

#[test]
fn controls_one_step_recovery_passes_and_report_validates() {
    let domains = step_domains();
    let (key, default, _) = domains[0].clone();
    let nondefault = ControlValue::Step(default + 1);
    let recovered = ControlValue::Step(default + 2);
    let input = ControlsAssessmentInput {
        binding: binding(
            &body("fujifilm/x-t5"),
            scope(
                vec![combination(&[(&key, nondefault.clone())])],
                vec![body("fujifilm/x-t5")],
            ),
        ),
        schema: looklab_match::synth::schema(),
        epoch: 1,
        pinned: pinned(),
        training: Vec::new(),
        held_out: vec![ObservedJpeg {
            group: group("held-out"),
            settings: settings_with(&[(&key, nondefault.clone())]),
            firmware: Some(firmware()),
            wb_mode: Some(wb_mode()),
            cell: cell("cell-0"),
            predicted: ucs(50.0, 0.0, 0.0),
            observed: ucs(50.0, 0.0, 0.0),
            source: EvidenceSource::Observed,
        }],
        recoveries: vec![RecoveryObservation {
            group: group("recovery"),
            firmware: Some(firmware()),
            wb_mode: Some(wb_mode()),
            true_settings: settings_with(&[(&key, nondefault.clone())]),
            recovered_settings: settings_with(&[(&key, recovered)]),
            predicted_true: ucs(50.0, 0.0, 0.0),
            observed_true: ucs(50.0, 0.0, 0.0),
            predicted_recovered: ucs(50.0, 0.0, 0.0),
            source: EvidenceSource::Observed,
        }],
        claims: Vec::new(),
    };
    let assessment = assess_controls(input);
    assert_eq!(assessment.report.outcome, AssessmentOutcome::Pass);
    assert_eq!(
        assessment
            .report
            .metrics
            .get(looklab_schema::MetricKey::RecoveryRate),
        Some(1.0)
    );
    assessment
        .report
        .validate(&assessment.dependencies)
        .expect("passing controls report");
    let epoch = looklab_match::synth::schema()
        .epochs
        .into_iter()
        .find(|epoch| epoch.id == 1)
        .expect("epoch");
    assert!(matching_scope_covers(
        &assessment.report,
        &epoch,
        &settings_with(&[(&key, nondefault.clone())]),
        Some(&firmware()),
        Some(&wb_mode()),
        &pinned(),
        &cell("cell-0"),
    ));
    assert!(!matching_scope_covers(
        &assessment.report,
        &epoch,
        &settings_with(&[(&key, ControlValue::Step(default + 3))]),
        Some(&firmware()),
        Some(&wb_mode()),
        &pinned(),
        &cell("cell-0"),
    ));
    assert!(!matching_scope_covers(
        &assessment.report,
        &epoch,
        &settings_with(&[(&key, nondefault)]),
        Some(&firmware()),
        Some(&wb_mode()),
        &pinned(),
        &cell("cell-1"),
    ));
}

#[test]
fn controls_recovery_alternative_accepts_close_predicted_error() {
    let domains = step_domains();
    let (key, default, _) = domains[0].clone();
    let nondefault = ControlValue::Step(default + 1);
    let recovered = ControlValue::Step(default + 4);
    let input = ControlsAssessmentInput {
        binding: binding(
            &body("fujifilm/x-t5"),
            scope(
                vec![combination(&[(&key, nondefault.clone())])],
                vec![body("fujifilm/x-t5")],
            ),
        ),
        schema: looklab_match::synth::schema(),
        epoch: 1,
        pinned: pinned(),
        training: Vec::new(),
        held_out: vec![ObservedJpeg {
            group: group("held-out"),
            settings: settings_with(&[(&key, nondefault.clone())]),
            firmware: Some(firmware()),
            wb_mode: Some(wb_mode()),
            cell: cell("cell-0"),
            predicted: ucs(50.0, 0.0, 0.0),
            observed: ucs(50.0, 0.0, 0.0),
            source: EvidenceSource::Observed,
        }],
        recoveries: vec![RecoveryObservation {
            group: group("recovery"),
            firmware: Some(firmware()),
            wb_mode: Some(wb_mode()),
            true_settings: settings_with(&[(&key, nondefault.clone())]),
            recovered_settings: settings_with(&[(&key, recovered)]),
            predicted_true: ucs(51.0, 0.0, 0.0),
            observed_true: ucs(50.0, 0.0, 0.0),
            predicted_recovered: ucs(51.2, 0.0, 0.0),
            source: EvidenceSource::Observed,
        }],
        claims: Vec::new(),
    };
    let assessment = assess_controls(input);
    assert_eq!(assessment.report.outcome, AssessmentOutcome::Pass);
}

#[test]
fn controls_recovery_far_from_target_fails() {
    let domains = step_domains();
    let (key, default, _) = domains[0].clone();
    let nondefault = ControlValue::Step(default + 1);
    let recovered = ControlValue::Step(default + 5);
    let input = ControlsAssessmentInput {
        binding: binding(
            &body("fujifilm/x-t5"),
            scope(
                vec![combination(&[(&key, nondefault.clone())])],
                vec![body("fujifilm/x-t5")],
            ),
        ),
        schema: looklab_match::synth::schema(),
        epoch: 1,
        pinned: pinned(),
        training: Vec::new(),
        held_out: vec![ObservedJpeg {
            group: group("held-out"),
            settings: settings_with(&[(&key, nondefault.clone())]),
            firmware: Some(firmware()),
            wb_mode: Some(wb_mode()),
            cell: cell("cell-0"),
            predicted: ucs(50.0, 0.0, 0.0),
            observed: ucs(50.0, 0.0, 0.0),
            source: EvidenceSource::Observed,
        }],
        recoveries: vec![RecoveryObservation {
            group: group("recovery"),
            firmware: Some(firmware()),
            wb_mode: Some(wb_mode()),
            true_settings: settings_with(&[(&key, nondefault.clone())]),
            recovered_settings: settings_with(&[(&key, recovered)]),
            predicted_true: ucs(50.0, 0.0, 0.0),
            observed_true: ucs(50.0, 0.0, 0.0),
            predicted_recovered: ucs(53.0, 0.0, 0.0),
            source: EvidenceSource::Observed,
        }],
        claims: Vec::new(),
    };
    let assessment = assess_controls(input);
    assert_eq!(assessment.report.outcome, AssessmentOutcome::Fail);
    assert_eq!(
        assessment
            .report
            .metrics
            .get(looklab_schema::MetricKey::RecoveryRate),
        Some(0.0)
    );
}

#[test]
fn synthetic_recovery_is_not_observed_camera_evidence() {
    let domains = step_domains();
    let (key, default, _) = domains[0].clone();
    let nondefault = ControlValue::Step(default + 1);
    let input = ControlsAssessmentInput {
        binding: binding(
            &body("fujifilm/x-t5"),
            scope(
                vec![combination(&[(&key, nondefault.clone())])],
                vec![body("fujifilm/x-t5")],
            ),
        ),
        schema: looklab_match::synth::schema(),
        epoch: 1,
        pinned: pinned(),
        training: Vec::new(),
        held_out: vec![ObservedJpeg {
            group: group("held-out"),
            settings: settings_with(&[(&key, nondefault.clone())]),
            firmware: Some(firmware()),
            wb_mode: Some(wb_mode()),
            cell: cell("cell-0"),
            predicted: ucs(50.0, 0.0, 0.0),
            observed: ucs(50.0, 0.0, 0.0),
            source: EvidenceSource::Observed,
        }],
        recoveries: vec![RecoveryObservation {
            group: group("synthetic"),
            firmware: Some(firmware()),
            wb_mode: Some(wb_mode()),
            true_settings: settings_with(&[(&key, nondefault.clone())]),
            recovered_settings: settings_with(&[(&key, nondefault)]),
            predicted_true: ucs(50.0, 0.0, 0.0),
            observed_true: ucs(50.0, 0.0, 0.0),
            predicted_recovered: ucs(50.0, 0.0, 0.0),
            source: EvidenceSource::Synthetic,
        }],
        claims: Vec::new(),
    };
    let assessment = assess_controls(input);
    assert_eq!(
        assessment.report.outcome,
        AssessmentOutcome::InsufficientEvidence
    );
    assert!(
        assessment
            .report
            .limitations
            .iter()
            .any(|limitation| limitation.as_str().contains("synthetic"))
    );
}

fn engine_fixture() -> (EngineVersion, looklab_schema::EnginePrerequisite) {
    let version = EngineVersion::new(
        source("looklab-fit@test"),
        source("looklab-model@test"),
        source("looklab-model@test"),
        RuleRevision::new(ENGINE_COVERAGE_RULE).expect("engine rule"),
    )
    .expect("engine version");
    let trials = vec![
        EngineTrial {
            body: body("fujifilm/x-t5"),
            cell: cell("cell-0"),
            true_error_de: 0.5,
            band_90_de: 1.0,
            recovered: true,
            held_out_body: true,
            out_of_gamut: true,
            marked_unsupported: true,
        },
        EngineTrial {
            body: body("sony/a7"),
            cell: cell("cell-1"),
            true_error_de: 0.5,
            band_90_de: 1.0,
            recovered: true,
            held_out_body: true,
            out_of_gamut: true,
            marked_unsupported: true,
        },
    ];
    let assessment = qualify_engine(&version, &trials);
    (version, assessment.prerequisite.expect("engine pass"))
}

fn cross_body(
    disagreement: f64,
    provided_engine: Option<looklab_schema::EnginePrerequisite>,
) -> CrossBodyAssessmentInput {
    let (version, pass) = engine_fixture();
    let engine = provided_engine.or(Some(pass));
    let probe = looklab_model::probes::v1().probes[0].id.clone();
    let first = body("fujifilm/x-t5");
    let second = body("sony/a7");
    let prediction = |body: &BodyId, ucs| CrossBodyEvidence {
        body: body.clone(),
        epoch: ControlEpochId::new(1),
        native_sim: sim("classic_chrome"),
        groups: vec![group(body.as_str())],
        chart_anchored: true,
        gauge: GaugeBindingId::new("gauge@1").expect("gauge"),
        predictions: vec![ProbePrediction {
            probe: probe.clone(),
            ucs,
            supported: true,
        }],
        source: EvidenceSource::Observed,
        calibration: DependencyRef::required(
            DependencyKind::Transform,
            DependencyTarget::transform(body, 1),
        ),
        realization: BodyRealizationVersionId {
            realization: realization(body),
            version: RealizationVersion::new(1).expect("version"),
            target: target(),
        },
        member_scope: Some(scope(vec![combination(&[])], vec![body.clone()])),
        member_mask: Some({
            let mut mask = looklab_schema::SupportedMask::new();
            mask.insert(cell("cell-0"));
            mask.insert(cell("ev0"));
            mask
        }),
        report: Some(ModelReportId::from_digest([1; 32])),
    };
    let scope = scope(vec![combination(&[])], vec![first.clone(), second.clone()]);
    CrossBodyAssessmentInput {
        binding: family_binding(vec![first.clone(), second.clone()], scope),
        family_target: target(),
        engine,
        engine_version: version,
        bodies: vec![
            prediction(&first, ucs(50.0, 0.0, 0.0)),
            prediction(&second, ucs(50.0 + disagreement, 0.0, 0.0)),
        ],
    }
}

#[test]
fn cross_body_disagreement_above_bound_fails_without_admission() {
    let assessment = assess_cross_body(cross_body(4.0, None));
    assert_eq!(assessment.report.outcome, AssessmentOutcome::Fail);
    assert!(assessment.cohort.is_none());
    assert!(
        assessment
            .report
            .metrics
            .get(looklab_schema::MetricKey::CrossBodyMedianDisagreementDe)
            .expect("disagreement")
            > P19_DISAGREEMENT_MEDIAN_DE
    );
}

#[test]
fn cross_body_agreement_passes_with_a_cohort_request() {
    let assessment = assess_cross_body(cross_body(0.5, None));
    assert_eq!(assessment.report.outcome, AssessmentOutcome::Pass);
    let cohort = assessment.cohort.as_ref().expect("cohort request");
    assert_eq!(cohort.members.len(), 2);
    assert!(
        cohort
            .members
            .iter()
            .all(|member| matches!(member, looklab_schema::CohortMember::Admitted { .. }))
    );
    assert_eq!(cohort.report, assessment.report_id);
    assessment
        .report
        .validate(&assessment.dependencies)
        .expect("passing cross-body report");
}

#[test]
fn cross_body_shared_group_is_insufficient() {
    let mut input = cross_body(0.5, None);
    let groups = input.bodies[0].groups.clone();
    input.bodies[1].groups = groups;
    let assessment = assess_cross_body(input);
    assert_eq!(
        assessment.report.outcome,
        AssessmentOutcome::InsufficientEvidence
    );
    assert!(
        assessment
            .report
            .evidence_requests
            .iter()
            .any(|request| { request.kind == EvidenceRequestKind::MissingIndependentCaptures })
    );
}

#[test]
fn cross_body_gauge_mismatch_is_insufficient() {
    let mut input = cross_body(0.5, None);
    input.bodies[1].gauge = GaugeBindingId::new("gauge@2").expect("gauge");
    let assessment = assess_cross_body(input);
    assert_eq!(
        assessment.report.outcome,
        AssessmentOutcome::InsufficientEvidence
    );
    assert!(
        assessment
            .report
            .evidence_requests
            .iter()
            .any(|request| { request.kind == EvidenceRequestKind::MissingCrossBodyEvidence })
    );
}

#[test]
fn cross_body_missing_engine_is_insufficient_not_waived() {
    let assessment = assess_cross_body(cross_body(
        0.5,
        Some(looklab_schema::EnginePrerequisite {
            engine: source("different-engine"),
            rule_revision: RuleRevision::new(ENGINE_COVERAGE_RULE).expect("rule"),
            outcome: AssessmentOutcome::Pass,
            metrics: looklab_schema::ReportMetrics::new(),
            supported_mask: looklab_schema::SupportedMask::new(),
            limitations: Vec::new(),
        }),
    ));
    assert_eq!(
        assessment.report.outcome,
        AssessmentOutcome::InsufficientEvidence
    );
    assert!(
        assessment
            .report
            .evidence_requests
            .iter()
            .any(|request| { request.kind == EvidenceRequestKind::MissingEngineQualification })
    );
}

#[test]
fn engine_coverage_boundary_is_exact() {
    let version = EngineVersion::new(
        source("looklab-fit@test"),
        source("looklab-model@test"),
        source("looklab-model@test"),
        RuleRevision::new(ENGINE_COVERAGE_RULE).expect("rule"),
    )
    .expect("version");
    let trials = |covered: usize| {
        (0..20)
            .map(|index| EngineTrial {
                body: body(&format!("test/body-{index}")),
                cell: cell(&format!("cell-{index}")),
                true_error_de: if index < covered { 0.5 } else { 2.0 },
                band_90_de: 1.0,
                recovered: true,
                held_out_body: true,
                out_of_gamut: true,
                marked_unsupported: true,
            })
            .collect::<Vec<_>>()
    };
    let pass = qualify_engine(&version, &trials(17));
    let pass_prerequisite = pass.prerequisite.as_ref().expect("engine pass");
    assert_eq!(pass.outcome, AssessmentOutcome::Pass);
    assert!((pass.coverage - 0.85).abs() < 1e-12);
    pass_prerequisite.validate().expect("engine pass");
    let fail = qualify_engine(&version, &trials(16));
    let fail_prerequisite = fail.prerequisite.as_ref().expect("engine fail");
    assert_eq!(fail.outcome, AssessmentOutcome::Fail);
    assert_eq!(fail_prerequisite.outcome, AssessmentOutcome::Fail);
}

#[test]
fn selected_model_reassessment_uses_the_exact_candidate() {
    let domains = step_domains();
    let (key, default, _) = domains[0].clone();
    let nondefault = ControlValue::Step(default + 1);
    let input = ControlsAssessmentInput {
        binding: binding(
            &body("fujifilm/x-t5"),
            scope(
                vec![combination(&[(&key, nondefault.clone())])],
                vec![body("fujifilm/x-t5")],
            ),
        ),
        schema: looklab_match::synth::schema(),
        epoch: 1,
        pinned: pinned(),
        training: Vec::new(),
        held_out: vec![ObservedJpeg {
            group: group("contrary"),
            settings: settings_with(&[(&key, nondefault.clone())]),
            firmware: Some(firmware()),
            wb_mode: Some(wb_mode()),
            cell: cell("cell-0"),
            predicted: ucs(50.0, 0.0, 0.0),
            observed: ucs(54.0, 0.0, 0.0),
            source: EvidenceSource::Observed,
        }],
        recoveries: vec![RecoveryObservation {
            group: group("recovery"),
            firmware: Some(firmware()),
            wb_mode: Some(wb_mode()),
            true_settings: settings_with(&[(&key, nondefault.clone())]),
            recovered_settings: settings_with(&[(&key, nondefault.clone())]),
            predicted_true: ucs(50.0, 0.0, 0.0),
            observed_true: ucs(50.0, 0.0, 0.0),
            predicted_recovered: ucs(50.0, 0.0, 0.0),
            source: EvidenceSource::Observed,
        }],
        claims: Vec::new(),
    };
    let selected = input.binding.candidate.clone();
    let assessment = assess_selected(&selected, AssessmentRequest::Controls(input))
        .expect("selected assessment");
    assert_eq!(assessment.report.outcome, AssessmentOutcome::Fail);
    assert_eq!(assessment.report.candidate, selected);
}

#[test]
fn dispatch_routes_each_capability() {
    let native = NativeAssessmentInput {
        binding: binding(
            &body("fujifilm/x-t5"),
            scope(vec![combination(&[])], vec![body("fujifilm/x-t5")]),
        ),
        encoding: OutputEncoding::Srgb,
        pinned: pinned(),
        min_independent_groups: 1,
        support: native_support(),
        training_groups: Vec::new(),
        observations: vec![NativeObservation {
            group: group("held-out"),
            settings: combination(&[]),
            firmware: Some(firmware()),
            wb_mode: Some(wb_mode()),
            cell: cell("cell-0"),
            predicted: ucs(50.0, 0.0, 0.0),
            observed: ucs(50.0, 0.0, 0.0),
            source: EvidenceSource::Observed,
        }],
        support_sigma: 1.5,
    };
    assert_eq!(
        assess(AssessmentRequest::Native(native)).report.capability,
        looklab_schema::QualificationCapability::NativePrediction
    );
    assert_eq!(
        assess(AssessmentRequest::CrossBody(cross_body(0.5, None)))
            .report
            .capability,
        looklab_schema::QualificationCapability::CrossBodyAdmission
    );
}
