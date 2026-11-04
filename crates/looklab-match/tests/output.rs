// SPDX-License-Identifier: AGPL-3.0-or-later

use looklab_match::synth;
use looklab_match::{
    ControlCovariance, Emphasis, MatchOptions, Permalink, PermalinkSources, PreparedTarget,
    TargetSpec, optimize, predicted_trace, settings_card,
};
use looklab_model::ProbeSetVersion;

#[test]
fn permalink_round_trips_through_its_canonical_form() {
    let permalink = Permalink {
        target_slug: "classic-chrome".to_string(),
        target_version: "3.0.0".to_string(),
        base_slug: "provia".to_string(),
        base_version: "1.2.0".to_string(),
        body: "fujifilm/x-t5".to_string(),
        epoch: 1,
        sim: "classic_chrome".to_string(),
        crm_version: 4,
        input_transform_version: 2,
        realization_params_version: 5,
        epoch_map_version: 3,
        probe_set: ProbeSetVersion::V1,
        optimizer: "looklab-match/1".to_string(),
        emphasis: Emphasis::Skin,
        sources: PermalinkSources {
            target_look: Some("looks/classic-chrome.looklab".to_string()),
            base_look: Some("looks/provia.looklab".to_string()),
            body: Some("data/bodies/fujifilm-x-t5.yaml".to_string()),
            crm: Some("models/classic-chrome.crm.json".to_string()),
            transform: Some("models/transform.yaml".to_string()),
            covariance: Some("models/classic-chrome.cov.json".to_string()),
        },
    };
    let text = permalink.canonical();
    assert!(text.starts_with("looklab-match/1|"));
    let parsed = Permalink::parse(&text).expect("parse");
    assert_eq!(parsed, permalink);
    assert!(Permalink::parse("looklab-match/1|look=x").is_err());
    assert!(Permalink::parse("other/1").is_err());
}

#[test]
fn settings_card_lists_every_control_and_a_summary() {
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
    })
    .expect("target");
    let options = MatchOptions::default();
    let outcome =
        optimize(std::slice::from_ref(&body.model), &target, &plan, &options).expect("optimize");
    let best = outcome.results.first().expect("suggestion");
    let card = settings_card(&target, &plan, &body.model, best).expect("card");
    assert_eq!(card.entries.len(), plan.controls.len());
    assert!(!card.residual_summary.is_empty());
    assert!(card.confidence.reason.contains("controls"));
    let text = card.render_text();
    assert!(text.contains("base sim"));
    assert!(text.contains("predicted ΔE"));
    assert!(text.contains("confidence"));
}

#[test]
fn covariance_round_trips_and_predicts_a_band() {
    let probes = synth::probes();
    let body = synth::body(6).expect("synthetic body");
    let ids = body.model.crm.parameter_ids();
    let n = ids.len();
    let mut covariance = vec![0.0_f64; n * n];
    for index in 0..n {
        covariance[index * n + index] = 0.01;
    }
    let covariance = ControlCovariance {
        parameter_ids: ids,
        covariance,
    };
    let bytes = covariance.to_json().expect("encode");
    let decoded = ControlCovariance::from_json(&bytes).expect("decode");
    assert_eq!(decoded, covariance);

    let model = looklab_match::MatchModel::new(
        body.model.base_look.clone(),
        body.model.crm.clone(),
        body.model.transform,
        Some(decoded),
        1,
        1,
    )
    .expect("model with covariance");
    assert!(!model.groups.is_empty());
    let trace = predicted_trace(&model, &body.settings, probes.probes[0].xyz).expect("trace");
    assert!(trace > 0.0, "trace {trace}");
}

#[test]
fn covariance_rejects_wrong_dimensions() {
    let covariance = ControlCovariance {
        parameter_ids: Vec::new(),
        covariance: vec![1.0],
    };
    assert!(covariance.validate().is_err());
}
