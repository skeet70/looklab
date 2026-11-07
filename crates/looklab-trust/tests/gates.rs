// SPDX-License-Identifier: AGPL-3.0-or-later
//! Release gates against the previous version, the anchor and the root observations.

use looklab_model::ProbeId;
use looklab_trust::{
    GateInput, GateReference, GatedUnion, ProbeEstimate, ProbePrediction, RootObservation,
    SupportThresholds, TrustParams, VersionProbes, Violation, convert_exposure_gauge,
    exposure_gauge_excluded, gate_release,
};

const THRESHOLDS: SupportThresholds = SupportThresholds {
    sigma: 1.5,
    sigma_exit: 1.65,
    bias_fraction: 0.33,
    bias_exit_factor: 1.1,
    density_floor: 0.5,
    min_clusters: 3,
};

const PREVIOUS_FLOOR: f64 = 0.75;
const PREVIOUS_CEILING: f64 = 2.0;
const ANCHOR_FLOOR: f64 = 1.0;
const ANCHOR_CEILING: f64 = 3.0;

#[derive(Clone, Copy, Debug)]
enum Cause {
    Sigma,
    Density,
    Clusters,
    Bias,
}

fn probe(id: usize, jab: [f64; 3], sigma: f64) -> ProbeEstimate {
    let prediction = ProbePrediction {
        id: ProbeId::new(format!("p{id}")),
        pre_clip: jab,
        sigma_pre_clip: sigma,
        post_clip: jab,
        sigma_post_clip: sigma,
        bias_de: 0.0,
        density: 10.0,
        clusters: 5,
        pre_clip_identified: true,
    };
    ProbeEstimate::new(prediction, None, &THRESHOLDS)
}

fn with_sigma(probe: &ProbeEstimate, sigma: f64) -> ProbeEstimate {
    let prediction = ProbePrediction {
        id: probe.id.clone(),
        pre_clip: probe.pre_clip,
        sigma_pre_clip: probe.sigma_pre_clip,
        post_clip: probe.post_clip,
        sigma_post_clip: sigma,
        bias_de: probe.bias_de,
        density: 10.0,
        clusters: 5,
        pre_clip_identified: probe.pre_clip_identified,
    };
    ProbeEstimate::new(prediction, None, &THRESHOLDS)
}

fn unsupported(probe: &ProbeEstimate, cause: Cause) -> ProbeEstimate {
    let mut prediction = ProbePrediction {
        id: probe.id.clone(),
        pre_clip: probe.pre_clip,
        sigma_pre_clip: probe.sigma_pre_clip,
        post_clip: probe.post_clip,
        sigma_post_clip: probe.sigma_post_clip,
        bias_de: probe.bias_de,
        density: 10.0,
        clusters: 5,
        pre_clip_identified: true,
    };
    match cause {
        Cause::Sigma => prediction.sigma_post_clip = 2.0,
        Cause::Density => prediction.density = 0.0,
        Cause::Clusters => prediction.clusters = 0,
        Cause::Bias => prediction.bias_de = 1.0,
    }
    ProbeEstimate::new(prediction, None, &THRESHOLDS)
}

fn observation(id: usize, jab: [f64; 3]) -> RootObservation {
    RootObservation {
        id: ProbeId::new(format!("p{id}")),
        jab,
    }
}

fn gate_params() -> TrustParams {
    TrustParams {
        gate_bound_scale: 2.0,
        gate_previous_floor: PREVIOUS_FLOOR,
        gate_previous_ceiling: PREVIOUS_CEILING,
        gate_anchor_floor: ANCHOR_FLOOR,
        gate_anchor_ceiling: ANCHOR_CEILING,
        gate_over_bound_fraction: 0.02,
        gate_p95_ratio: 1.0,
        gate_support_margin: 0.1,
        support_sigma_exit: THRESHOLDS.sigma_exit,
        ..TrustParams::default()
    }
}

/// 100 probes; the first ten carry root observations at `[50, 0, 0]`.
fn baseline() -> GateInput {
    let probes = (0..100)
        .map(|index| probe(index, [50.0, 0.0, 0.0], 0.2))
        .collect::<Vec<_>>();
    GateInput {
        candidate: VersionProbes {
            probes: probes.clone(),
        },
        previous: VersionProbes {
            probes: probes.clone(),
        },
        anchor: VersionProbes { probes },
        root_observations: (0..10)
            .map(|index| observation(index, [50.0, 0.0, 0.0]))
            .collect(),
        gated_since_anchor: GatedUnion::default(),
    }
}

fn with_shift(mut input: GateInput, indices: &[usize], shift: f64) -> GateInput {
    for index in indices {
        input.candidate.probes[*index].pre_clip = [50.0 + shift, 0.0, 0.0];
        input.candidate.probes[*index].post_clip = [50.0 + shift, 0.0, 0.0];
    }
    input
}

fn violations_contain(
    report: &looklab_trust::GateReport,
    matcher: impl Fn(&Violation) -> bool,
) -> bool {
    report.violations.iter().any(matcher)
}

#[test]
fn unchanged_versions_pass_every_gate() {
    let report = gate_release(&TrustParams::default(), &baseline());
    assert!(report.passed, "{:?}", report.violations);
    assert_eq!(report.root_previous.probes, 10);
    assert_eq!(report.drift_previous.gated, 100);
}

#[test]
fn root_median_rise_fails_the_previous_comparison() {
    let input = with_shift(baseline(), &[0, 1, 2, 3, 4, 5, 6, 7, 8, 9], 0.4);
    let report = gate_release(&TrustParams::default(), &input);

    assert!((report.root_previous.median - 0.4).abs() < 1e-12);
    assert!(!report.root_previous.pass);
    assert!(report.root_anchor.pass);
    assert!(report.drift_previous.pass);
    assert!(violations_contain(&report, |violation| matches!(
        violation,
        Violation::RootMedian {
            reference: GateReference::Previous,
            ..
        }
    )));
}

#[test]
fn root_p95_rise_fails_even_with_a_flat_median() {
    let input = with_shift(baseline(), &[0, 1], 2.0);
    let report = gate_release(&TrustParams::default(), &input);

    assert!((report.root_previous.median).abs() < 1e-12);
    assert!((report.root_previous.p95 - 2.0).abs() < 1e-12);
    assert!(!report.root_previous.pass);
    assert!(!report.root_anchor.pass);
    assert!(report.drift_previous.pass);
}

#[test]
fn root_error_is_measured_against_the_observations() {
    let mut input = baseline();
    input.root_observations = (0..10)
        .map(|index| observation(index, [51.0, 0.0, 0.0]))
        .collect();
    for version in [&mut input.candidate, &mut input.previous, &mut input.anchor] {
        for probe in &mut version.probes {
            probe.sigma_post_clip = 0.3;
        }
    }
    for probe in &mut input.previous.probes {
        probe.post_clip = [50.4, 0.0, 0.0];
    }
    for probe in &mut input.anchor.probes {
        probe.post_clip = [50.0, 0.0, 0.0];
    }
    for probe in &mut input.candidate.probes {
        probe.post_clip = [49.6, 0.0, 0.0];
    }

    let report = gate_release(&TrustParams::default(), &input);
    assert!((report.root_previous.median - 1.4).abs() < 1e-12);
    assert!((report.root_previous.median_reference - 0.6).abs() < 1e-12);
    assert!(!report.root_previous.pass);
    assert!(report.drift_previous.pass);
}

#[test]
fn a_root_improvement_passes_even_though_the_anchor_is_farther() {
    let mut input = baseline();
    input.root_observations = (0..10)
        .map(|index| observation(index, [51.0, 0.0, 0.0]))
        .collect();
    for probe in &mut input.previous.probes {
        probe.post_clip = [50.4, 0.0, 0.0];
    }
    for probe in &mut input.anchor.probes {
        probe.post_clip = [50.0, 0.0, 0.0];
    }
    for probe in &mut input.candidate.probes {
        probe.post_clip = [51.0, 0.0, 0.0];
    }

    let report = gate_release(&TrustParams::default(), &input);
    assert!(report.root_previous.pass);
    assert!(report.root_anchor.pass);
    assert!(report.passed, "{:?}", report.violations);
}

#[test]
fn drift_above_the_previous_ceiling_fails() {
    let input = with_shift(baseline(), &[50], 2.5);
    let report = gate_release(&TrustParams::default(), &input);

    assert!(!report.drift_previous.pass);
    assert_eq!(report.drift_previous.over_ceiling.len(), 1);
    assert_eq!(
        report.drift_previous.over_ceiling[0].probe,
        ProbeId::new("p50")
    );
    assert!(report.root_previous.pass);
    assert!(violations_contain(&report, |violation| matches!(
        violation,
        Violation::OverCeiling {
            reference: GateReference::Previous,
            ..
        }
    )));
}

#[test]
fn cumulative_drift_within_each_previous_bound_fails_at_the_anchor_ceiling() {
    let base = baseline();
    let anchor = base.anchor;
    let mut previous = base.previous;
    let step = 0.5;

    for release in 0..10 {
        let mut candidate = previous.clone();
        candidate.probes[50].post_clip[0] += step;
        let report = gate_release(
            &gate_params(),
            &GateInput {
                candidate: candidate.clone(),
                previous: previous.clone(),
                anchor: anchor.clone(),
                root_observations: Vec::new(),
                gated_since_anchor: GatedUnion::default(),
            },
        );
        let cumulative = step * (release + 1) as f64;
        assert!(
            report.drift_previous.pass,
            "release {release}: previous comparison failed: {:?}",
            report.drift_previous
        );
        if cumulative <= ANCHOR_CEILING {
            assert!(
                report.passed,
                "release {release}: cumulative {cumulative} should pass: {:?}",
                report.violations
            );
            assert!(report.drift_anchor.over_ceiling.is_empty());
        } else {
            assert!(
                !report.passed,
                "release {release}: cumulative {cumulative} should fail"
            );
            assert_eq!(report.drift_anchor.over_ceiling.len(), 1);
            let gated = &report.drift_anchor.over_ceiling[0];
            assert_eq!(gated.probe, ProbeId::new("p50"));
            assert!((gated.shift - cumulative).abs() < 1e-12);
            assert!(violations_contain(&report, |violation| matches!(
                violation,
                Violation::OverCeiling {
                    reference: GateReference::Anchor,
                    probe,
                    shift,
                    ceiling,
                } if probe == &ProbeId::new("p50")
                    && (*shift - cumulative).abs() < 1e-12
                    && (*ceiling - ANCHOR_CEILING).abs() < 1e-12
            )));
        }
        previous = candidate;
    }
}

#[test]
fn a_concentrated_shift_inside_the_previous_allowance_fails_the_anchor_ceiling() {
    let mut input = baseline();
    input.root_observations.clear();
    input.previous.probes[50].post_clip = [52.0, 0.0, 0.0];
    input.candidate.probes[50].post_clip = [53.5, 0.0, 0.0];

    let report = gate_release(&gate_params(), &input);

    let expected_shift = ANCHOR_CEILING + 0.5;
    assert!(report.drift_previous.pass, "{:?}", report.drift_previous);
    assert!(!report.drift_anchor.pass);
    assert_eq!(report.drift_anchor.over_ceiling.len(), 1);
    let gated = &report.drift_anchor.over_ceiling[0];
    assert_eq!(gated.probe, ProbeId::new("p50"));
    assert!((gated.shift - expected_shift).abs() < 1e-12);
    assert!((gated.bound - ANCHOR_CEILING).abs() < 1e-12);
    assert!(violations_contain(&report, |violation| matches!(
        violation,
        Violation::OverCeiling {
            reference: GateReference::Anchor,
            probe,
            shift,
            ceiling,
        } if probe == &ProbeId::new("p50")
            && (*shift - expected_shift).abs() < 1e-12
            && (*ceiling - ANCHOR_CEILING).abs() < 1e-12
    )));
}

#[test]
fn two_percent_of_gated_probes_may_exceed_their_bound() {
    let input = with_shift(baseline(), &[10, 11], 1.0);
    let report = gate_release(&TrustParams::default(), &input);
    assert_eq!(report.drift_previous.allowed_over_bound, 2);
    assert_eq!(report.drift_previous.over_bound.len(), 2);
    assert!(report.drift_previous.pass);

    let input = with_shift(baseline(), &[10, 11, 12], 1.0);
    let report = gate_release(&TrustParams::default(), &input);
    assert_eq!(report.drift_previous.over_bound.len(), 3);
    assert!(!report.drift_previous.pass);
    assert!(violations_contain(&report, |violation| matches!(
        violation,
        Violation::TooManyOverBound {
            reference: GateReference::Previous,
            ..
        }
    )));
}

#[test]
fn p95_ratio_above_one_fails() {
    let input = with_shift(baseline(), &[10, 11, 12, 13, 14, 15, 16, 17, 18, 19], 1.0);
    let report = gate_release(&TrustParams::default(), &input);

    assert!(report.drift_previous.p95_ratio > 1.0);
    assert!(!report.drift_previous.pass);
    assert!(violations_contain(&report, |violation| matches!(
        violation,
        Violation::P95Ratio {
            reference: GateReference::Previous,
            ..
        }
    )));
}

#[test]
fn a_sigma_jump_that_loses_support_fails() {
    let mut input = baseline();
    input.candidate.probes[50] = with_sigma(&input.candidate.probes[50], 2.8);
    for version in [&mut input.previous, &mut input.anchor] {
        version.probes[50] = with_sigma(&version.probes[50], 0.7);
    }
    let report = gate_release(&TrustParams::default(), &input);

    assert!(!report.passed);
    assert_eq!(report.supported_regressions, vec![ProbeId::new("p50")]);
    assert!(report.coverage_changes.is_empty());
    assert!(report.drift_previous.pass);
    assert!(violations_contain(&report, |violation| matches!(
        violation,
        Violation::SupportedToUnsupported { probe, .. } if probe == &ProbeId::new("p50")
    )));
}

#[test]
fn a_probe_supported_only_in_the_anchor_still_fails_when_it_loses_support() {
    let mut input = baseline();
    input.previous.probes[50] = with_sigma(&input.previous.probes[50], 2.0);
    input.anchor.probes[50] = with_sigma(&input.anchor.probes[50], 0.7);
    input.candidate.probes[50] = with_sigma(&input.candidate.probes[50], 2.8);

    let report = gate_release(&gate_params(), &input);

    assert_eq!(report.supported_regressions, vec![ProbeId::new("p50")]);
    assert!(!report.passed);
    assert!(report.coverage_changes.is_empty());
    assert!(report.drift_previous.pass);
    assert!(report.drift_anchor.pass);
    assert!(violations_contain(&report, |violation| matches!(
        violation,
        Violation::SupportedToUnsupported { probe, .. } if probe == &ProbeId::new("p50")
    )));
}

#[test]
fn a_sigma_rise_to_the_exit_threshold_is_a_coverage_change() {
    let mut input = baseline();
    input.candidate.probes[50] = with_sigma(&input.candidate.probes[50], 1.65);
    for version in [&mut input.previous, &mut input.anchor] {
        version.probes[50] = with_sigma(&version.probes[50], 0.7);
    }
    let report = gate_release(&TrustParams::default(), &input);

    assert!(!input.candidate.probes[50].supported());
    assert_eq!(report.coverage_changes, vec![ProbeId::new("p50")]);
    assert!(report.supported_regressions.is_empty());
    assert!(!violations_contain(&report, |violation| matches!(
        violation,
        Violation::SupportedToUnsupported { .. }
    )));
    assert!(report.passed, "{:?}", report.violations);
}

#[test]
fn sigma_jitter_at_the_support_threshold_passes() {
    let mut input = baseline();
    input.candidate.probes[50] = with_sigma(&input.candidate.probes[50], 1.52);
    for version in [&mut input.previous, &mut input.anchor] {
        version.probes[50] = with_sigma(&version.probes[50], 1.47);
    }
    let report = gate_release(&TrustParams::default(), &input);

    assert!(!input.candidate.probes[50].supported());
    assert!(report.supported_regressions.is_empty());
    assert!(!violations_contain(&report, |violation| matches!(
        violation,
        Violation::SupportedToUnsupported { .. }
    )));
    assert!(report.passed, "{:?}", report.violations);
}

#[test]
fn support_hysteresis_keeps_a_probe_through_the_exit_threshold() {
    let previous = probe(50, [50.0, 0.0, 0.0], 1.4);
    let prediction = ProbePrediction {
        id: ProbeId::new("p50"),
        pre_clip: [50.0, 0.0, 0.0],
        sigma_pre_clip: 1.6,
        post_clip: [50.0, 0.0, 0.0],
        sigma_post_clip: 1.6,
        bias_de: 0.0,
        density: 10.0,
        clusters: 5,
        pre_clip_identified: true,
    };

    let held = ProbeEstimate::new(prediction.clone(), Some(&previous), &THRESHOLDS);
    let fresh = ProbeEstimate::new(prediction, None, &THRESHOLDS);
    assert!(held.supported());
    assert!(!fresh.supported());
    assert!(!fresh.support.sigma);
    assert!(fresh.support.density);
}

#[test]
fn bias_hysteresis_holds_a_supported_probe_past_the_enter_threshold() {
    let previous = probe(50, [50.0, 0.0, 0.0], 0.2);
    let prediction = ProbePrediction {
        id: ProbeId::new("p50"),
        pre_clip: [50.0, 0.0, 0.0],
        sigma_pre_clip: 0.2,
        post_clip: [50.0, 0.0, 0.0],
        sigma_post_clip: 0.2,
        bias_de: 0.52,
        density: 10.0,
        clusters: 5,
        pre_clip_identified: true,
    };

    let held = ProbeEstimate::new(prediction.clone(), Some(&previous), &THRESHOLDS);
    let fresh = ProbeEstimate::new(prediction, None, &THRESHOLDS);
    assert!(held.supported());
    assert!(!fresh.supported());
    assert!(fresh.support.sigma);
}

#[test]
fn a_density_only_support_loss_is_a_coverage_change() {
    let mut input = baseline();
    input.candidate.probes[50] = unsupported(&input.candidate.probes[50], Cause::Density);
    let report = gate_release(&TrustParams::default(), &input);

    assert!(!input.candidate.probes[50].supported());
    assert_eq!(report.coverage_changes, vec![ProbeId::new("p50")]);
    assert!(report.supported_regressions.is_empty());
    assert!(!violations_contain(&report, |violation| matches!(
        violation,
        Violation::SupportedToUnsupported { .. }
    )));
    assert!(report.drift_previous.pass);
    assert!(report.passed, "{:?}", report.violations);
}

#[test]
fn a_bias_only_support_loss_is_a_coverage_change() {
    let mut input = baseline();
    input.candidate.probes[50] = unsupported(&input.candidate.probes[50], Cause::Bias);
    let report = gate_release(&TrustParams::default(), &input);

    assert!(!input.candidate.probes[50].supported());
    assert_eq!(report.coverage_changes, vec![ProbeId::new("p50")]);
    assert!(report.supported_regressions.is_empty());
    assert!(report.passed, "{:?}", report.violations);
}

#[test]
fn the_sticky_union_keeps_every_lost_probe_gated_until_re_anchor() {
    for cause in [Cause::Sigma, Cause::Density, Cause::Clusters, Cause::Bias] {
        let mut input = baseline();
        let supported = input.previous.probes[50].clone();
        let mut history = VersionProbes::default();
        history.probes.push(supported.clone());

        input.previous.probes[50] = unsupported(&supported, cause);
        input.anchor.probes[50] = unsupported(&supported, cause);
        input.candidate.probes[50] = unsupported(&supported, cause);
        input.candidate.probes[50].post_clip = [51.0, 0.0, 0.0];

        let report = gate_release(&TrustParams::default(), &input);
        assert_eq!(report.drift_previous.gated, 99, "{cause:?}");
        assert!(report.drift_previous.over_bound.is_empty(), "{cause:?}");
        assert!(report.supported_regressions.is_empty(), "{cause:?}");

        input.gated_since_anchor.include(&history);
        let report = gate_release(&TrustParams::default(), &input);
        assert_eq!(report.drift_previous.gated, 100, "{cause:?}");
        assert_eq!(report.drift_previous.carried, 1, "{cause:?}");
        // A σ at the support ceiling puts b_p at C, so only the ceiling bounds it.
        if !matches!(cause, Cause::Sigma) {
            assert_eq!(
                report.drift_previous.over_bound[0].probe,
                ProbeId::new("p50"),
                "{cause:?}"
            );
        }
        assert!(report.supported_regressions.is_empty(), "{cause:?}");

        input.gated_since_anchor = GatedUnion::at_anchor(&input.anchor);
        let report = gate_release(&TrustParams::default(), &input);
        assert_eq!(report.drift_previous.gated, 99, "{cause:?}");
        assert!(report.drift_previous.over_bound.is_empty(), "{cause:?}");
    }
}

#[test]
fn a_union_carried_probe_that_loses_support_is_reported_as_a_coverage_change() {
    let mut input = baseline();
    let supported = input.previous.probes[50].clone();
    let mut history = VersionProbes::default();
    history.probes.push(supported.clone());

    input.previous.probes[50] = unsupported(&supported, Cause::Density);
    input.anchor.probes[50] = unsupported(&supported, Cause::Density);
    input.candidate.probes[50] = unsupported(&supported, Cause::Density);
    input.gated_since_anchor.include(&history);

    let report = gate_release(&TrustParams::default(), &input);
    assert_eq!(report.coverage_changes, vec![ProbeId::new("p50")]);
    assert!(report.supported_regressions.is_empty());
    assert_eq!(report.drift_previous.carried, 1);
    assert!(report.passed, "{:?}", report.violations);
}

#[test]
fn the_ceilings_apply_to_probes_outside_the_union() {
    let mut input = baseline();
    for index in [50, 51, 52] {
        let probe = unsupported(&input.previous.probes[index], Cause::Density);
        input.previous.probes[index] = probe.clone();
        input.anchor.probes[index] = probe.clone();
        input.candidate.probes[index] = probe;
    }
    input.candidate.probes[50].post_clip = [52.5, 0.0, 0.0];
    input.candidate.probes[51].pre_clip = [52.5, 0.0, 0.0];
    for version in [&mut input.previous, &mut input.anchor] {
        version.probes[52].pre_clip = [50.0, 40.0, 0.0];
    }
    input.candidate.probes[52].pre_clip = [50.0, 34.641_016_151_377_54, 20.0];

    let report = gate_release(&TrustParams::default(), &input);
    assert_eq!(report.drift_previous.gated, 97);
    assert!(report.drift_previous.over_bound.is_empty());
    assert_eq!(
        report.drift_previous.over_ceiling[0].probe,
        ProbeId::new("p50")
    );
    assert_eq!(
        report.drift_previous.pre_clip_over_ceiling[0].probe,
        ProbeId::new("p51")
    );
    assert_eq!(
        report.drift_previous.hue_over_ceiling[0].probe,
        ProbeId::new("p52")
    );
    assert!(report.coverage_changes.is_empty());
    assert!(!report.passed);
    assert!(violations_contain(&report, |violation| matches!(
        violation,
        Violation::OverCeiling {
            reference: GateReference::Previous,
            probe,
            ..
        } if probe == &ProbeId::new("p50")
    )));
}

#[test]
fn a_candidate_cannot_exempt_itself_from_the_pre_clip_ceilings() {
    let mut input = baseline();
    input.candidate.probes[50].pre_clip = [52.5, 0.0, 0.0];
    input.candidate.probes[50].pre_clip_identified = false;
    let report = gate_release(&TrustParams::default(), &input);

    assert_eq!(
        report.drift_previous.pre_clip_over_ceiling[0].probe,
        ProbeId::new("p50")
    );
    assert!(!report.drift_previous.pass);

    let mut input = baseline();
    input.previous.probes[50].pre_clip_identified = false;
    input.anchor.probes[50].pre_clip_identified = false;
    input.candidate.probes[50].pre_clip = [52.5, 0.0, 0.0];
    let report = gate_release(&TrustParams::default(), &input);
    assert!(report.drift_previous.pre_clip_over_ceiling.is_empty());
    assert!(report.drift_anchor.pre_clip_over_ceiling.is_empty());
}

#[test]
fn an_unidentified_pre_clip_skips_the_pre_clip_ceilings() {
    let mut input = baseline();
    for index in [50, 51, 52] {
        for version in [&mut input.candidate, &mut input.previous, &mut input.anchor] {
            version.probes[index].pre_clip_identified = false;
        }
    }
    input.candidate.probes[50].post_clip = [52.5, 0.0, 0.0];
    input.candidate.probes[51].pre_clip = [52.5, 0.0, 0.0];
    for version in [&mut input.previous, &mut input.anchor] {
        version.probes[52].pre_clip = [50.0, 40.0, 0.0];
    }
    input.candidate.probes[52].pre_clip = [50.0, 34.641_016_151_377_54, 20.0];

    let report = gate_release(&TrustParams::default(), &input);
    assert_eq!(
        report.drift_previous.over_ceiling[0].probe,
        ProbeId::new("p50")
    );
    assert!(report.drift_previous.pre_clip_over_ceiling.is_empty());
    assert!(report.drift_previous.hue_over_ceiling.is_empty());
    assert!(!report.drift_previous.pass);
}

#[test]
fn pre_clip_drift_past_the_ceiling_fails_with_a_small_post_clip_shift() {
    let mut input = baseline();
    input.candidate.probes[50].pre_clip = [52.5, 0.0, 0.0];
    let report = gate_release(&TrustParams::default(), &input);

    assert!(report.drift_previous.over_ceiling.is_empty());
    assert_eq!(
        report.drift_previous.pre_clip_over_ceiling[0].probe,
        ProbeId::new("p50")
    );
    assert!(!report.drift_previous.pass);
    assert!(violations_contain(&report, |violation| matches!(
        violation,
        Violation::PreClipOverCeiling {
            reference: GateReference::Previous,
            ..
        }
    )));
}

#[test]
fn hue_drift_past_the_ceiling_fails_within_the_post_clip_bound() {
    let mut input = baseline();
    for version in [&mut input.previous, &mut input.anchor] {
        version.probes[50].pre_clip = [50.0, 40.0, 0.0];
    }
    input.candidate.probes[50].pre_clip = [50.0, 40.0, 0.0];
    // 30° of hue rotation at chroma 40: `ΔH_ab ≈ 20.7`, far past C = 2.
    input.candidate.probes[50].pre_clip = [50.0, 34.641_016_151_377_54, 20.0];
    let report = gate_release(&TrustParams::default(), &input);

    assert!(report.drift_previous.over_ceiling.is_empty());
    assert!(report.drift_previous.pre_clip_over_ceiling.is_empty());
    assert_eq!(
        report.drift_previous.hue_over_ceiling[0].probe,
        ProbeId::new("p50")
    );
    assert!(!report.drift_previous.pass);
    assert!(violations_contain(&report, |violation| matches!(
        violation,
        Violation::HueOverCeiling {
            reference: GateReference::Previous,
            ..
        }
    )));
}

#[test]
fn a_small_hue_rotation_within_the_ceiling_passes() {
    let mut input = baseline();
    for version in [&mut input.previous, &mut input.anchor] {
        version.probes[50].pre_clip = [50.0, 40.0, 0.0];
    }
    // 2° at chroma 40: `ΔH_ab ≈ 1.40`.
    input.candidate.probes[50].pre_clip = [50.0, 39.975_633_080_764_31, 1.395_979_869_701_459];
    let report = gate_release(&TrustParams::default(), &input);

    assert!(report.drift_previous.hue_over_ceiling.is_empty());
    assert!(report.passed, "{:?}", report.violations);
}

#[test]
fn a_probe_supported_only_in_the_reference_is_gated() {
    let mut input = baseline();
    input.candidate.probes[60] = with_sigma(&input.candidate.probes[60], 2.0);
    input.candidate.probes[60].post_clip = [52.5, 0.0, 0.0];
    let report = gate_release(&TrustParams::default(), &input);

    assert_eq!(
        report.drift_previous.over_ceiling[0].probe,
        ProbeId::new("p60")
    );
    assert!(!report.drift_previous.pass);
}

#[test]
fn a_probe_unsupported_in_both_versions_is_outside_the_bounded_set() {
    let mut input = baseline();
    input.candidate.probes[70] = with_sigma(&input.candidate.probes[70], 2.0);
    for version in [&mut input.previous, &mut input.anchor] {
        version.probes[70] = with_sigma(&version.probes[70], 2.0);
    }
    let report = gate_release(&TrustParams::default(), &input);
    assert_eq!(report.drift_previous.gated, 99);
    assert!(report.drift_previous.over_bound.is_empty());
    assert!(report.drift_previous.over_ceiling.is_empty());
    assert!(report.supported_regressions.is_empty());
    assert!(report.coverage_changes.is_empty());
}

#[test]
fn no_root_observations_means_no_root_gate() {
    let mut input = baseline();
    input.root_observations.clear();
    let report = gate_release(&TrustParams::default(), &input);
    assert_eq!(report.root_previous.probes, 0);
    assert!(report.root_previous.pass);
    assert!(report.root_anchor.pass);
}

#[test]
fn an_empty_candidate_fails_closed() {
    let mut input = baseline();
    input.candidate.probes.clear();
    let report = gate_release(&TrustParams::default(), &input);

    assert!(!report.passed);
    assert!(report.violations.iter().any(|violation| matches!(
        violation,
        Violation::MissingProbe {
            role: looklab_trust::VersionRole::Candidate,
            ..
        }
    )));
    assert!(!report.drift_previous.pass);
}

#[test]
fn a_duplicate_probe_fails_closed() {
    let mut input = baseline();
    let duplicate = input.candidate.probes[0].clone();
    input.candidate.probes.push(duplicate);
    let report = gate_release(&TrustParams::default(), &input);

    assert!(!report.passed);
    assert!(report.violations.iter().any(|violation| matches!(
        violation,
        Violation::DuplicateProbe {
            role: looklab_trust::VersionRole::Candidate,
            ..
        }
    )));
}

#[test]
fn a_missing_reference_probe_fails_closed() {
    let mut input = baseline();
    input.previous.probes.remove(99);
    let report = gate_release(&TrustParams::default(), &input);

    assert!(!report.passed);
    assert!(report.violations.iter().any(|violation| matches!(
        violation,
        Violation::MissingProbe {
            role: looklab_trust::VersionRole::Previous,
            probe,
        } if probe == &ProbeId::new("p99")
    )));
}

#[test]
fn exposure_gauge_conversion_rescales_the_input() {
    let evaluate = |xyz: [f64; 3]| [xyz[0] * 2.0, xyz[1] * 3.0, xyz[2] * 4.0];
    let converted = convert_exposure_gauge(evaluate, 1.0);
    let output = converted([1.0, 2.0, 4.0]);
    assert!((output[0] - 4.0).abs() < 1e-12);
    assert!((output[1] - 12.0).abs() < 1e-12);
    assert!((output[2] - 32.0).abs() < 1e-12);
}

#[test]
fn gauge_exclusion_only_touches_the_clamped_ends() {
    assert!(exposure_gauge_excluded(5.5, 1.0));
    assert!(!exposure_gauge_excluded(5.5, 0.5));
    assert!(exposure_gauge_excluded(-11.5, -1.0));
    assert!(!exposure_gauge_excluded(0.0, 3.0));
}
