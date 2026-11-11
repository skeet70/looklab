// SPDX-License-Identifier: AGPL-3.0-or-later

use looklab_match::synth;
use looklab_match::{
    ControlDomain, Emphasis, ImageCount, MatchOptions, OutputPin, Plan, PreparedTarget, Settings,
    TargetAppearance, TargetSpec, TargetSupport, optimize,
};
use looklab_model::file::{Density, ProbeCovariance, ProbeScalars, UncertaintyData};
use looklab_model::realization::ControlValue;
use looklab_schema::OutputEncoding;

fn within_one_step(plan: &Plan, a: &Settings, b: &Settings) -> bool {
    plan.plan_controls.iter().all(|&control| {
        let left = plan.value(control, a);
        let right = plan.value(control, b);
        match (left, right) {
            (ControlValue::Step(x), ControlValue::Step(y)) => (x - y).abs() <= 1,
            (left, right) => left == right,
        }
    })
}

fn label(plan: &Plan, settings: &Settings) -> String {
    plan.plan_controls
        .iter()
        .map(|&control| {
            format!(
                "{}={}",
                plan.key(control),
                plan.current_label(control, settings)
            )
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[test]
fn recovers_held_out_targets_within_one_step_or_05_delta_e() {
    let plan = synth::plan();
    let probes = synth::probes();
    let options = MatchOptions {
        seed: 7,
        ..MatchOptions::default()
    };
    let cases = 12;
    let mut passed = 0;
    for seed in 0..cases {
        let body = synth::body(seed).expect("synthetic body");
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
        let outcome = optimize(std::slice::from_ref(&body.model), &target, &plan, &options)
            .expect("optimize");
        let best = outcome.results.first().expect("a suggestion");
        let close = within_one_step(&plan, &best.settings, &body.settings);
        let accurate = best.median_delta_e <= 0.5;
        assert!(
            close || accurate,
            "seed {seed}: median ΔE {:.3}, settings {} vs true {}",
            best.median_delta_e,
            label(&plan, &best.settings),
            label(&plan, &body.settings)
        );
        passed += 1;
    }
    assert_eq!(passed, cases);
}

#[test]
fn every_control_has_a_non_empty_domain() {
    let plan = synth::plan();
    for domain in &plan.domains {
        assert!(!domain.values.is_empty(), "{}", domain.key);
        assert!(domain.default < domain.values.len());
    }
}

#[test]
fn emphasis_changes_group_weights() {
    let plan = synth::plan();
    let probes = synth::probes();
    let body = synth::body(1).expect("synthetic body");
    let target_function = body.target();
    let balanced = PreparedTarget::new(TargetSpec {
        probes: &probes,
        function: &target_function,
        uncertainty: None,
        emphasis: Emphasis::Balanced,
        tau: 1.0,
        support_sigma: 1.5,
        density_floor: None,
        appearance: body.appearance(),
    })
    .expect("balanced");
    let skin = PreparedTarget::new(TargetSpec {
        probes: &probes,
        function: &target_function,
        uncertainty: None,
        emphasis: Emphasis::Skin,
        tau: 1.0,
        support_sigma: 1.5,
        density_floor: None,
        appearance: body.appearance(),
    })
    .expect("skin");
    let skin_probe = probes
        .probes
        .iter()
        .position(|probe| probe.group == looklab_model::ProbeGroup::Skin)
        .expect("skin probe");
    assert!(skin.weight[skin_probe] > balanced.weight[skin_probe]);
    let hue_probe = probes
        .probes
        .iter()
        .position(|probe| probe.group == looklab_model::ProbeGroup::HueGrid)
        .expect("hue probe");
    assert!(skin.weight[hue_probe] < balanced.weight[hue_probe]);
    assert_eq!(plan.epoch, 1);
}

#[test]
fn probe_subset_is_deterministic_and_bounded() {
    let probes = synth::probes();
    let body = synth::body(2).expect("synthetic body");
    let target_function = body.target();
    let target = PreparedTarget::new(TargetSpec {
        probes: &probes,
        function: &target_function,
        uncertainty: None,
        emphasis: Emphasis::Landscape,
        tau: 1.0,
        support_sigma: 1.5,
        density_floor: None,
        appearance: body.appearance(),
    })
    .expect("target");
    let first = target.subset(200);
    let second = target.subset(200);
    assert_eq!(first.indices, second.indices);
    assert_eq!(first.weights, second.weights);
    assert!(first.indices.len() <= 200);
    assert!(!first.indices.is_empty());
    assert_eq!(first.indices.len(), first.weights.len());
}

#[test]
fn control_domain_steps_match_the_schema() {
    let plan = synth::plan();
    let color = plan
        .domains
        .iter()
        .position(|domain| domain.key.as_str() == "color")
        .expect("color control");
    assert_eq!(plan.domains[color].values.len(), 9);
    let ec = plan
        .domains
        .iter()
        .position(|domain| domain.key.as_str() == "exposure_compensation")
        .expect("exposure control");
    assert_eq!(plan.domains[ec].values.len(), 19);
    let domain: &ControlDomain = &plan.domains[ec];
    assert_eq!(domain.default, 9);
}

#[test]
fn an_infeasible_published_render_has_no_supported_probes() {
    let mut look = looklab_model::Look::colorimetric_spline().expect("look");
    look.color.lambda.plane.fill(400.0);
    look.color.lambda.volume.fill(400.0);
    let probes = looklab_model::probes::v1();
    let error = PreparedTarget::new(TargetSpec {
        probes: &probes,
        function: &look,
        uncertainty: None,
        emphasis: Emphasis::Balanced,
        tau: 1.0,
        support_sigma: f64::MAX,
        density_floor: None,
        appearance: TargetAppearance::Rendered(
            OutputPin::new(OutputEncoding::Srgb, look.clip.k).expect("clip"),
        ),
    })
    .err()
    .expect("no feasible probe");
    assert!(matches!(
        error,
        looklab_match::MatchError::NoSupportedProbes
    ));
}

fn probe_uncertainty(count: usize, support: Vec<f64>) -> UncertaintyData {
    let mut covariance = vec![0.0_f64; count * 9];
    for (index, chunk) in covariance.as_chunks_mut::<9>().0.iter_mut().enumerate() {
        let variance = if index % 2 == 0 { 0.01 } else { 9.0 };
        chunk[0] = variance;
        chunk[4] = variance;
        chunk[8] = variance;
    }
    UncertaintyData {
        probe_cov: Some(ProbeCovariance::new(count, covariance.clone()).expect("covariance")),
        probe_cov_post: Some(ProbeCovariance::new(count, covariance).expect("covariance")),
        support: Some(ProbeScalars::new(support).expect("support")),
        density: Some(Density::new(vec![1.0; count]).expect("density")),
        ..UncertaintyData::default()
    }
}

fn spec<'a>(
    body: &'a synth::SyntheticBody,
    probes: &'a looklab_model::ProbeSet,
    uncertainty: &'a UncertaintyData,
    function: &'a synth::RealizationTarget,
) -> TargetSpec<'a> {
    TargetSpec {
        probes,
        function,
        uncertainty: Some(uncertainty),
        emphasis: Emphasis::Balanced,
        tau: 1.0,
        support_sigma: 1.5,
        density_floor: None,
        appearance: body.appearance(),
    }
}

#[test]
fn a_target_with_no_supported_probe_refuses_an_ordinary_selection() {
    let body = synth::body(3).expect("synthetic body");
    let probes = synth::probes();
    let count = probes.probes.len();
    let uncertainty = probe_uncertainty(count, vec![0.0; count]);
    let function = body.target();
    let error = PreparedTarget::new(spec(&body, &probes, &uncertainty, &function))
        .err()
        .expect("no supported probe");
    assert!(matches!(
        error,
        looklab_match::MatchError::NoSupportedProbes
    ));
}

#[test]
fn experimental_selection_falls_back_to_the_sigma_and_density_rule() {
    let body = synth::body(3).expect("synthetic body");
    let probes = synth::probes();
    let count = probes.probes.len();
    let uncertainty = probe_uncertainty(count, vec![0.0; count]);
    let unmasked = probe_uncertainty(count, vec![0.0; count]);
    let unmasked = UncertaintyData {
        support: None,
        ..unmasked
    };
    let function = body.target();
    let images = ImageCount::new(2);
    let target =
        PreparedTarget::experimental(spec(&body, &probes, &uncertainty, &function), images)
            .expect("experimental target");
    let reference =
        PreparedTarget::new(spec(&body, &probes, &unmasked, &function)).expect("predicate target");
    assert_eq!(target.support, TargetSupport::Experimental(images));
    assert_eq!(target.supported, reference.supported);
    assert!(
        target.supported.len() < count,
        "the sigma predicate must exclude the high-uncertainty probes"
    );
    assert!(
        target.supported.iter().all(|index| index % 2 == 0),
        "only the low-uncertainty probes pass the predicate"
    );
}

#[test]
fn experimental_selection_applies_the_density_floor() {
    let body = synth::body(3).expect("synthetic body");
    let probes = synth::probes();
    let count = probes.probes.len();
    let mut uncertainty = probe_uncertainty(count, vec![0.0; count]);
    let mut density = vec![1.0_f64; count];
    density[0] = 0.0;
    density[2] = 0.0;
    uncertainty.density = Some(Density::new(density).expect("density"));
    let function = body.target();
    let target = PreparedTarget::experimental(
        spec(&body, &probes, &uncertainty, &function),
        ImageCount::new(1),
    )
    .expect("experimental target");
    assert!(
        !target.supported.contains(&0) && !target.supported.contains(&2),
        "the fallback applies the density floor"
    );
    assert!(
        target.supported.contains(&4),
        "a dense low-uncertainty probe stays supported"
    );
}

#[test]
fn experimental_selection_keeps_render_infeasible_probes_out() {
    let mut look = looklab_model::Look::colorimetric_spline().expect("look");
    look.color.lambda.plane.fill(400.0);
    look.color.lambda.volume.fill(400.0);
    let probes = looklab_model::probes::v1();
    let count = probes.probes.len();
    let uncertainty = probe_uncertainty(count, vec![0.0; count]);
    let error = PreparedTarget::experimental(
        TargetSpec {
            probes: &probes,
            function: &look,
            uncertainty: Some(&uncertainty),
            emphasis: Emphasis::Balanced,
            tau: 1.0,
            support_sigma: 1.5,
            density_floor: None,
            appearance: TargetAppearance::Rendered(
                OutputPin::new(OutputEncoding::Srgb, look.clip.k).expect("clip"),
            ),
        },
        ImageCount::new(1),
    )
    .err()
    .expect("no render-feasible probe");
    assert!(matches!(
        error,
        looklab_match::MatchError::NoSupportedProbes
    ));
}

#[test]
fn experimental_selection_uses_a_non_empty_support_mask() {
    let body = synth::body(3).expect("synthetic body");
    let probes = synth::probes();
    let count = probes.probes.len();
    let mut mask = vec![1.0; count];
    mask[1] = 0.0;
    let uncertainty = probe_uncertainty(count, mask);
    let function = body.target();
    let target = PreparedTarget::experimental(
        spec(&body, &probes, &uncertainty, &function),
        ImageCount::new(2),
    )
    .expect("experimental target");
    assert_eq!(target.support, TargetSupport::Published);
    assert!(
        !target.supported.contains(&1),
        "the published mask excludes the probe"
    );
    assert!(
        target.supported.contains(&3),
        "the published mask keeps a probe the sigma predicate would reject"
    );
}
