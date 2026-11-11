// SPDX-License-Identifier: AGPL-3.0-or-later

use looklab_match::synth;
use looklab_match::{
    AssessedDomain, Emphasis, MatchOptions, OutputPin, PreparedTarget, TargetAppearance,
    TargetSpec, optimize,
};
use looklab_model::realization::ControlValue;
use looklab_schema::{
    AssessedPoint, AssessmentOutcome, BodyId, BodyRealizationId, CombinationBasis, ControlEpochId,
    DomainId, FamilyTargetId, FirmwareId, LookId, ModelReport, ModelSubject, ModelVersionId,
    OutputEncoding, ProbeSetId, QualificationCapability, RealizationVersion, ReportMetrics,
    ReportProvenance, ReportScope, RuleRevision, SettingCombination, SettingValue, Sha256Hex,
    SimKey, SourceRef, SupportedMask, TargetVersion, WbModeKey,
};

fn body() -> BodyId {
    BodyId::new("fujifilm/x-t5").expect("body")
}

fn realization() -> BodyRealizationId {
    BodyRealizationId {
        body: body(),
        epoch: ControlEpochId::new(1),
        native_sim: SimKey::new(synth::SYNTH_SIM).expect("sim"),
    }
}

fn dummy_firmware() -> FirmwareId {
    FirmwareId {
        body: body(),
        epoch: ControlEpochId::new(1),
        version: "1.00".parse().expect("firmware version"),
    }
}

fn dummy_wb() -> WbModeKey {
    WbModeKey::new("daylight").expect("wb mode")
}

fn assessed(
    plan: &looklab_match::Plan,
    report: &ModelReport,
) -> Result<AssessedDomain, looklab_match::MatchError> {
    AssessedDomain::from_parts(
        plan,
        report.capability,
        report.outcome,
        &report.scope,
        &report.supported_mask,
        None,
        None,
        &[dummy_firmware()],
        &[dummy_wb()],
    )
}

fn report(
    settings: Vec<SettingCombination>,
    defaults_assessed: bool,
    mask: SupportedMask,
) -> ModelReport {
    let realization = realization();
    ModelReport {
        subject: ModelSubject::BodyRealization {
            realization: realization.clone(),
        },
        candidate: ModelVersionId::BodyRealization {
            realization: looklab_schema::BodyRealizationVersionId {
                realization,
                version: RealizationVersion::new(2).expect("version"),
                target: FamilyTargetId {
                    family: LookId::new("classic-chrome").expect("look"),
                    version: TargetVersion::new(1).expect("version"),
                },
            },
        },
        capability: QualificationCapability::ControlsMatching,
        scope: ReportScope {
            points: settings
                .into_iter()
                .map(|settings| AssessedPoint {
                    settings,
                    firmware: Some(dummy_firmware()),
                    wb_mode: Some(dummy_wb()),
                })
                .collect(),
            combination_basis: CombinationBasis::Enumerated,
            defaults_assessed,
            output_encoding: OutputEncoding::Srgb,
            colour_cells: Vec::new(),
            exposure_cells: Vec::new(),
            assessed_bodies: vec![body()],
            assessed_domain: DomainId::new("supported-probes").expect("domain"),
        },
        provenance: ReportProvenance {
            rule_revision: RuleRevision::new("controls-matching/1").expect("rule"),
            fitter: SourceRef::new("looklab-fit@dev").expect("source"),
            evaluator: SourceRef::new("looklab-model@dev").expect("source"),
            model: SourceRef::new("looklab-model@dev").expect("source"),
            probe_set: ProbeSetId::new("v1").expect("probes"),
            effective_options: Sha256Hex::from_bytes([1; 32]),
            partition_policy: Sha256Hex::from_bytes([2; 32]),
            assessment_seeds: Vec::new(),
        },
        outcome: AssessmentOutcome::Pass,
        metrics: ReportMetrics::new(),
        supported_mask: mask,
        limitations: Vec::new(),
        evidence_requests: Vec::new(),
    }
}

fn mask_for(probes: &looklab_model::ProbeSet) -> SupportedMask {
    let mut mask = SupportedMask::new();
    for probe in &probes.probes {
        if let Some(cell) = AssessedDomain::cell_of(probe) {
            mask.insert(cell);
        }
    }
    mask
}

fn setting_value(value: &ControlValue) -> SettingValue {
    match value {
        ControlValue::Step(step) => SettingValue::Step(*step),
        ControlValue::Enum(value) => SettingValue::Enum(value.clone()),
        ControlValue::Bool(value) => SettingValue::Bool(*value),
    }
}

fn target_for<'a>(
    probes: &'a looklab_model::ProbeSet,
    function: &'a synth::RealizationTarget,
) -> PreparedTarget<'a> {
    PreparedTarget::new(TargetSpec {
        probes,
        function,
        uncertainty: None,
        emphasis: Emphasis::Balanced,
        tau: 1.0,
        support_sigma: 1.5,
        density_floor: None,
        appearance: TargetAppearance::Rendered(
            OutputPin::new(OutputEncoding::Srgb, function.model.base_look.clip.k).expect("clip"),
        ),
    })
    .expect("target")
}

#[test]
fn reachability_intersects_the_assessed_cells() {
    let plan = synth::plan();
    let probes = synth::probes();
    let body = synth::body(4).expect("synthetic body");
    let target_function = body.target();
    let target = PreparedTarget::new(TargetSpec {
        probes: &probes,
        function: &target_function,
        uncertainty: None,
        emphasis: Emphasis::Balanced,
        tau: 1.0,
        support_sigma: 1.5,
        density_floor: None,
        appearance: body.appearance(),
    })
    .expect("target");
    let cell = AssessedDomain::cell_of(&probes.probes[0]).expect("cell");
    let mut mask = SupportedMask::new();
    mask.insert(cell.clone());
    let report = report(vec![SettingCombination::new()], true, mask);
    let domain = assessed(&plan, &report).expect("domain");
    let settings = plan.default.clone();
    let output = target
        .optional_output_context(&body.model, &settings)
        .expect("context");
    let suggestion = looklab_match::Suggestion {
        sim: body.model.sim().clone(),
        settings,
        objective: 0.0,
        median_delta_e: 0.0,
        p95_delta_e: 0.0,
        max_delta_e: 0.0,
        sigma_band: None,
        metric: target.metric(),
        output,
    };
    let card = looklab_match::settings_card_with_support(
        &target,
        &plan,
        &body.model,
        &suggestion,
        Some(&domain),
    )
    .expect("card");
    let expected = target
        .supported
        .iter()
        .filter(|index| {
            AssessedDomain::cell_of(&probes.probes[**index]).is_some_and(|value| value == cell)
        })
        .count();
    assert_eq!(card.reachability.declared, probes.probes.len());
    assert_eq!(card.reachability.assessed, expected);
    assert!(card.reachability.assessed < probes.probes.len());
    assert_ne!(
        card.reachability.status,
        looklab_match::ReachabilityStatus::Reachable
    );
}

#[test]
fn assessed_domain_scores_only_listed_combinations() {
    let plan = synth::plan();
    let body = synth::body(4).expect("synthetic body");
    let probes = synth::probes();
    let function = body.target();
    let target = target_for(&probes, &function);

    let control = plan.controls[0].key.clone();
    let domain = &plan.domains[0];
    let chosen = (0..domain.values.len())
        .find(|index| domain.values[*index] != *domain.default_value())
        .expect("a non-default value");
    let chosen_value = domain.values[chosen].clone();
    let mut combination = SettingCombination::new();
    combination.insert(control.clone(), setting_value(&chosen_value));

    let report = report(vec![combination], false, mask_for(&probes));
    let assessed = assessed(&plan, &report).expect("domain");
    let assessed_plan = plan.with_assessed(assessed);
    let outcome = optimize(
        std::slice::from_ref(&body.model),
        &target,
        &assessed_plan,
        &MatchOptions::default(),
    )
    .expect("match");

    assert_eq!(outcome.results.len(), 1);
    let winner = &outcome.results[0];
    assert_eq!(winner.settings.get(&control), Some(&chosen_value));
    assert_ne!(winner.settings, assessed_plan.default);
    assert_eq!(outcome.stats.exact_evaluations, 1);
}

#[test]
fn an_unassessed_default_is_refused() {
    let plan = synth::plan();
    let probes = synth::probes();
    let mask = mask_for(&probes);
    assert!(
        assessed(
            &plan,
            &report(vec![SettingCombination::new()], false, mask.clone())
        )
        .is_err()
    );
    let domain = assessed(&plan, &report(vec![SettingCombination::new()], true, mask))
        .expect("assessed default");
    assert_eq!(domain.combinations().len(), 1);
}

#[test]
fn probe_filter_keeps_only_supported_cells() {
    let probes = synth::probes();
    let cell = AssessedDomain::cell_of(&probes.probes[0]).expect("cell");
    let mut mask = SupportedMask::new();
    mask.insert(cell.clone());
    let plan = synth::plan();
    let domain =
        assessed(&plan, &report(vec![SettingCombination::new()], true, mask)).expect("domain");
    let filtered = domain.filter_probes(&probes).expect("probes");
    assert!(!filtered.probes.is_empty());
    for probe in &filtered.probes {
        assert_eq!(AssessedDomain::cell_of(probe), Some(cell.clone()));
    }
}

#[test]
fn a_wide_assessed_product_is_refused() {
    let plan = synth::plan();
    let probes = synth::probes();
    let domain = assessed(
        &plan,
        &report(vec![SettingCombination::new()], true, mask_for(&probes)),
    )
    .expect("domain");
    assert!(
        domain
            .check_budget(looklab_match::MAX_ASSESSED_EVALUATIONS + 1)
            .is_err()
    );
    assert!(domain.check_budget(1).is_ok());
}
