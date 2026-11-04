// SPDX-License-Identifier: AGPL-3.0-or-later

use looklab_match::synth;
use looklab_match::{
    Emphasis, FitObservedOptions, MatchOptions, ObservedRender, PreparedTarget, ScaleFitter,
    TargetSpec, VerifyRequest, optimize, verify_once,
};

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
