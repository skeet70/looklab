// SPDX-License-Identifier: AGPL-3.0-or-later

use looklab_match::synth;
use looklab_match::{
    Emphasis, FitObservedOptions, MatchOptions, ObservedFitter, ObservedRender, PreparedTarget,
    ScaleFitter, TargetSpec, VerifyRequest, measure_target_error, optimize, verify_once,
};

#[test]
fn scale_fitter_leaves_a_correct_model_unchanged() {
    let seed = 3;
    let plan = synth::plan();
    let probes = synth::probes();
    let truth = synth::body(seed).expect("synthetic body");
    let samples = synth::observed(&truth.model, &truth.settings, &probes, 400).expect("observed");
    let observed = vec![ObservedRender { samples }];
    let fitted = ScaleFitter
        .fit(
            &truth.model,
            &plan,
            &truth.settings,
            &observed,
            &FitObservedOptions::default(),
        )
        .expect("fit");
    for control in &truth.model.crm.controls {
        let before = response_norm(&truth.model.crm, &control.key);
        let after = response_norm(&fitted, &control.key);
        assert!(
            (after - before).abs() <= 1e-3 * before.max(1e-9),
            "{} changed from {before} to {after}",
            control.key
        );
    }
}

#[test]
fn measured_error_is_independent_of_the_settings_argument() {
    let seed = 3;
    let probes = synth::probes();
    let plan = synth::plan();
    let truth = synth::body(seed).expect("synthetic body");
    let target_function = truth.target();
    let target = PreparedTarget::new(TargetSpec {
        probes: &probes,
        function: &target_function,
        uncertainty: None,
        emphasis: Emphasis::Balanced,
        tau: 1.0,
        support_sigma: 1.5,
        density_floor: None,
    })
    .expect("target");
    let samples = synth::observed(&truth.model, &truth.settings, &probes, 300).expect("observed");
    let observed = vec![ObservedRender { samples }];
    let first =
        measure_target_error(&target, &truth.model, &truth.settings, &observed).expect("first");
    let second =
        measure_target_error(&target, &truth.model, &plan.default, &observed).expect("second");
    assert_eq!(first, second);
}

fn response_norm(
    crm: &looklab_model::realization::ControlResponseModel,
    key: &looklab_schema::ControlKey,
) -> f64 {
    crm.controls
        .iter()
        .filter(|control| &control.key == key)
        .flat_map(|control| control.values.iter())
        .flat_map(|value| value.delta.layers.iter())
        .flat_map(|layer| layer.entries.iter())
        .map(|(_, value)| value * value)
        .sum::<f64>()
        .sqrt()
}

#[test]
fn one_loop_iteration_reduces_measured_error_by_at_least_30_percent() {
    let seed = 3;
    let plan = synth::plan();
    let probes = synth::probes();
    let truth = synth::body(seed).expect("synthetic body");
    let degraded = truth.degraded(1.6);
    let target_function = truth.target();
    let target = PreparedTarget::new(TargetSpec {
        probes: &probes,
        function: &target_function,
        uncertainty: None,
        emphasis: Emphasis::Balanced,
        tau: 1.0,
        support_sigma: 1.5,
        density_floor: None,
    })
    .expect("target");
    let options = MatchOptions {
        seed: 11,
        ..MatchOptions::default()
    };

    let first = optimize(
        std::slice::from_ref(&degraded.model),
        &target,
        &plan,
        &options,
    )
    .expect("first suggestion")
    .results
    .into_iter()
    .next()
    .expect("a suggestion");
    let samples =
        synth::observed(&truth.model, &first.settings, &probes, 400).expect("observed render");
    let observed = vec![ObservedRender { samples }];
    let request = VerifyRequest {
        target: &target,
        plan: &plan,
        model: &degraded.model,
        observed: &observed,
        options: &options,
        fit_options: FitObservedOptions::default(),
        truth: Some(&truth.model),
    };
    let outcome = verify_once(&request, &ScaleFitter).expect("verify");
    let second = outcome.second_measured.expect("truth renderer present");
    assert!(
        outcome.first_measured.median > 0.0,
        "degraded first suggestion should have measurable error"
    );
    assert!(
        second.median <= 0.7 * outcome.first_measured.median,
        "first {:.3}, second {:.3}",
        outcome.first_measured.median,
        second.median
    );
    let reduction = outcome.reduction.expect("reduction");
    assert!(reduction >= 0.3, "reduction {reduction:.3}");
}
