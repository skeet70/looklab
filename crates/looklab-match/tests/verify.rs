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
            truth.appearance().metric(),
            &observed,
            &FitObservedOptions::default(),
        )
        .expect("fit");
    let mut calibrated = truth.model.clone();
    calibrated.crm = fitted;
    let before = truth
        .model
        .realize(&truth.settings)
        .expect("original render");
    let after = calibrated
        .realize(&truth.settings)
        .expect("calibrated render");
    let metric = truth.appearance().metric();
    for probe in &probes.probes {
        let expected = truth
            .model
            .scored_ucs(metric, &before, probe.xyz)
            .to_array();
        let actual = calibrated.scored_ucs(metric, &after, probe.xyz).to_array();
        for (expected, actual) in expected.into_iter().zip(actual) {
            assert!((actual - expected).abs() <= 1e-3, "{}", probe.id.as_str());
        }
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
        appearance: truth.appearance(),
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

#[test]
fn uninformative_zero_response_observations_do_not_invent_a_correction() {
    let plan = synth::plan();
    let probes = synth::probes();
    let truth = synth::body(3).expect("synthetic body");
    let color = plan
        .by_key
        .get(&looklab_schema::ControlKey::new("color").expect("key"))
        .copied()
        .expect("color control");
    let domain = &plan.domains[color];
    let zero_index = (0..domain.values.len())
        .find(|index| {
            *index != domain.default
                && truth
                    .model
                    .value_row(plan.key(color), &domain.values[*index])
                    .expect("row")
                    .0
                    .is_empty()
        })
        .expect("a zero-response color value");
    let nonzero_index = (0..domain.values.len())
        .find(|index| {
            !truth
                .model
                .value_row(plan.key(color), &domain.values[*index])
                .expect("row")
                .0
                .is_empty()
        })
        .expect("a nonzero-response color value");

    let mut observed_settings = plan.default.clone();
    plan.set(color, zero_index, &mut observed_settings);
    let samples =
        synth::observed(&truth.model, &observed_settings, &probes, 200).expect("observed");
    let observed = vec![ObservedRender { samples }];
    let (fitted, scales) = ScaleFitter
        .fit_with_scales(
            &truth.model,
            &plan,
            &observed_settings,
            truth.appearance().metric(),
            &observed,
            &FitObservedOptions::default(),
        )
        .expect("fit");
    assert!(
        scales.iter().all(|(_, factor)| *factor == 1.0),
        "unobserved zero-response value invented a correction: {scales:?}"
    );

    let mut probe_settings = plan.default.clone();
    plan.set(color, nonzero_index, &mut probe_settings);
    let probe = &probes.probes[0];
    let before = truth
        .model
        .render(&probe_settings, probe.xyz)
        .expect("before");
    let after = truth
        .model
        .with_crm(fitted)
        .expect("fitted model")
        .render(&probe_settings, probe.xyz)
        .expect("after");
    assert_eq!(before, after);
}

#[test]
fn one_loop_iteration_reduces_measured_error_by_at_least_30_percent() {
    let seed = 3;
    let plan = synth::plan();
    let probes = synth::probes();
    let truth = synth::identifiable_body(seed).expect("synthetic body");
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
        appearance: truth.appearance(),
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
