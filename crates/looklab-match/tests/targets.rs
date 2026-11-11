// SPDX-License-Identifier: AGPL-3.0-or-later

use looklab_match::synth;
use looklab_match::{
    ControlDomain, Emphasis, MatchOptions, OutputPin, Plan, PreparedTarget, Settings,
    TargetAppearance, TargetSpec, optimize,
};
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
