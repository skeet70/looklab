// SPDX-License-Identifier: AGPL-3.0-or-later
//! Release gates against the previous version, the anchor and the root observations.

use looklab_model::ProbeId;
use looklab_trust::{
    GateInput, GateReference, ProbeEstimate, RootObservation, TrustParams, VersionProbes,
    Violation, convert_exposure_gauge, exposure_gauge_excluded, gate_release,
};

/// The fit's default `FitOptions::support_sigma`.
const SUPPORT_SIGMA: f64 = 1.5;

fn probe(id: usize, jab: [f64; 3], sigma: f64, density_ok: bool) -> ProbeEstimate {
    ProbeEstimate::new(
        ProbeId::new(format!("p{id}")),
        jab,
        sigma,
        density_ok,
        SUPPORT_SIGMA,
    )
}

/// The same probe with a different σ, re-deriving its support.
fn with_sigma(probe: &ProbeEstimate, sigma: f64) -> ProbeEstimate {
    ProbeEstimate::new(
        probe.id.clone(),
        probe.jab,
        sigma,
        probe.density_ok,
        SUPPORT_SIGMA,
    )
}

fn observation(id: usize, jab: [f64; 3]) -> RootObservation {
    RootObservation {
        id: ProbeId::new(format!("p{id}")),
        jab,
    }
}

/// 100 probes; the first ten carry root observations at `[50, 0, 0]`.
fn baseline() -> GateInput {
    let probes = (0..100)
        .map(|index| probe(index, [50.0, 0.0, 0.0], 0.2, true))
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
    }
}

fn with_shift(mut input: GateInput, indices: &[usize], shift: f64) -> GateInput {
    for index in indices {
        input.candidate.probes[*index].jab = [50.0 + shift, 0.0, 0.0];
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
            probe.sigma = 0.3;
        }
    }
    for probe in &mut input.previous.probes {
        probe.jab = [50.4, 0.0, 0.0];
    }
    for probe in &mut input.anchor.probes {
        probe.jab = [50.0, 0.0, 0.0];
    }
    for probe in &mut input.candidate.probes {
        probe.jab = [49.6, 0.0, 0.0];
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
        probe.jab = [50.4, 0.0, 0.0];
    }
    for probe in &mut input.anchor.probes {
        probe.jab = [50.0, 0.0, 0.0];
    }
    for probe in &mut input.candidate.probes {
        probe.jab = [51.0, 0.0, 0.0];
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
fn sigma_jitter_at_the_support_threshold_passes() {
    let mut input = baseline();
    input.candidate.probes[50] = with_sigma(&input.candidate.probes[50], 1.52);
    for version in [&mut input.previous, &mut input.anchor] {
        version.probes[50] = with_sigma(&version.probes[50], 1.47);
    }
    let report = gate_release(&TrustParams::default(), &input);

    assert!(!input.candidate.probes[50].supported);
    assert!(report.supported_regressions.is_empty());
    assert!(!violations_contain(&report, |violation| matches!(
        violation,
        Violation::SupportedToUnsupported { .. }
    )));
    assert!(report.passed, "{:?}", report.violations);
}

#[test]
fn a_density_only_support_loss_is_a_coverage_change() {
    let mut input = baseline();
    let probe = input.candidate.probes[50].clone();
    input.candidate.probes[50] =
        ProbeEstimate::new(probe.id, probe.jab, probe.sigma, false, SUPPORT_SIGMA);
    let report = gate_release(&TrustParams::default(), &input);

    assert!(!input.candidate.probes[50].supported);
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
fn a_probe_supported_only_in_the_reference_is_gated() {
    let mut input = baseline();
    input.candidate.probes[60] = ProbeEstimate {
        jab: [52.5, 0.0, 0.0],
        ..with_sigma(&input.candidate.probes[60], 2.0)
    };
    let report = gate_release(&TrustParams::default(), &input);

    assert_eq!(
        report.drift_previous.over_ceiling[0].probe,
        ProbeId::new("p60")
    );
    assert!(!report.drift_previous.pass);
}

#[test]
fn a_probe_unsupported_in_both_versions_is_outside_the_gate() {
    let mut input = baseline();
    input.candidate.probes[70] = with_sigma(&input.candidate.probes[70], 2.0);
    for version in [&mut input.previous, &mut input.anchor] {
        version.probes[70] = with_sigma(&version.probes[70], 2.0);
    }
    let report = gate_release(&TrustParams::default(), &input);
    assert_eq!(report.drift_previous.gated, 99);
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
