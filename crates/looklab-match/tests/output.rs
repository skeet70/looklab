// SPDX-License-Identifier: AGPL-3.0-or-later

use looklab_match::synth;
use looklab_match::{
    Calibration, ControlCovariance, Emphasis, MatchOptions, Permalink, PermalinkSources,
    PreparedTarget, TargetSpec, optimize, predicted_trace, settings_card,
};
use looklab_model::ProbeSetVersion;
use looklab_model::file::{ProbeCovariance, UncertaintyData};
use looklab_schema::{ControlKey, SimKey};

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
        seed: 42,
        sources: PermalinkSources {
            target_look: Some("looks/classic-chrome.looklab".to_string()),
            base_look: Some("looks/provia.looklab".to_string()),
            body: Some("data/bodies/fujifilm-x-t5.yaml".to_string()),
            crm: Some("models/classic-chrome.crm.json".to_string()),
            transform: Some("models/transform.yaml".to_string()),
            covariance: Some("models/classic-chrome.cov.json".to_string()),
        },
        calibration: Calibration::default(),
    };
    let text = permalink.canonical();
    assert!(text.starts_with("looklab-match/1|"));
    let parsed = Permalink::parse(&text).expect("parse");
    assert_eq!(parsed, permalink);
    assert!(Permalink::parse("looklab-match/1|look=x").is_err());
    assert!(Permalink::parse("other/1").is_err());
}

#[test]
fn calibrated_permalink_round_trips_and_rejects_bad_overlays() {
    let plan = synth::plan();
    let sim = SimKey::new("classic_chrome").expect("sim");
    let control = plan.controls[0].key.clone();
    let calibration = Calibration::default()
        .merged(&sim, &[(control.clone(), 1.25)])
        .expect("calibration");
    let permalink = Permalink {
        target_slug: "classic-chrome".to_string(),
        target_version: "3.0.0".to_string(),
        base_slug: "provia".to_string(),
        base_version: "1.2.0".to_string(),
        body: "fujifilm/x-t5".to_string(),
        epoch: 1,
        sim: sim.to_string(),
        crm_version: 4,
        input_transform_version: 2,
        realization_params_version: 5,
        epoch_map_version: 3,
        probe_set: ProbeSetVersion::V1,
        optimizer: "looklab-match/1".to_string(),
        emphasis: Emphasis::Skin,
        seed: 42,
        sources: PermalinkSources::default(),
        calibration: calibration.clone(),
    };
    let text = permalink.canonical();
    assert!(text.starts_with("looklab-match/2|"));
    assert_eq!(Permalink::parse(&text).expect("parse"), permalink);
    calibration
        .validate(std::slice::from_ref(&sim), &plan)
        .expect("known sim and control");

    let unknown_sim = Calibration::default()
        .merged(
            &SimKey::new("other").expect("sim"),
            &[(control.clone(), 1.1)],
        )
        .expect("calibration");
    assert!(
        unknown_sim
            .validate(std::slice::from_ref(&sim), &plan)
            .is_err()
    );
    let unknown_control = Calibration::default()
        .merged(
            &sim,
            &[(ControlKey::new("other_control").expect("control"), 1.1)],
        )
        .expect("calibration");
    assert!(
        unknown_control
            .validate(std::slice::from_ref(&sim), &plan)
            .is_err()
    );
    assert!(
        Calibration::default()
            .merged(&sim, &[(control.clone(), 1e9)])
            .is_err()
    );
    assert!(
        Calibration::default()
            .merged(&sim, &[(control.clone(), f64::NAN)])
            .is_err()
    );

    let v1 = text.replacen("looklab-match/2", "looklab-match/1", 1);
    assert!(Permalink::parse(&v1).is_err());
    let base = text.split("|calibration=").next().expect("base");
    let duplicate = format!("{base}|calibration={sim}@{control}=1.2;{sim}@{control}=1.3");
    assert!(Permalink::parse(&duplicate).is_err());
    let unknown_field = format!("{base}|calibration={sim}@{control}=1.2|bogus=1");
    assert!(Permalink::parse(&unknown_field).is_err());
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
fn target_confidence_scales_error_but_not_predicted_covariance() {
    let probes = synth::probes();
    let body = synth::body(8).expect("synthetic body");
    let mut values = vec![0.0_f64; probes.probes.len() * 9];
    values[0] = 1.0;
    let uncertainty = UncertaintyData {
        probe_cov: Some(ProbeCovariance::new(probes.probes.len(), values).expect("covariance")),
        ..UncertaintyData::default()
    };
    let target_function = body.target();
    let target = PreparedTarget::new(TargetSpec {
        probes: &probes,
        function: &target_function,
        uncertainty: Some(&uncertainty),
        emphasis: Emphasis::Balanced,
        tau: 1.0,
        support_sigma: 1.5,
        density_floor: None,
    })
    .expect("target");
    assert!(target.covariance_weight[0] > 0.0);
    let ratio = target.weight[0] / target.covariance_weight[0];
    assert!((ratio - 0.5).abs() < 1e-12, "c_p ratio {ratio}");
    let unaffected = (1..probes.probes.len())
        .find(|index| target.covariance_weight[*index] > 0.0)
        .expect("supported probe");
    assert!((target.weight[unaffected] - target.covariance_weight[unaffected]).abs() < 1e-12);
}

#[test]
fn residual_summary_includes_input_effects() {
    let plan = synth::plan();
    let probes = synth::probes();
    let schema = synth::schema();
    let body = synth::body(10).expect("synthetic body");
    let sim = looklab_schema::SimKey::new(synth::SYNTH_SIM).expect("sim");
    let mut crm = looklab_model::realization::ControlResponseModel::zeroed(&schema, 1, &sim, 1)
        .expect("zeroed model");
    for step in &mut crm.exposure.steps {
        if step.key.as_str() == "exposure_compensation" {
            step.ev_per_step = 1.0;
        }
    }
    let model = looklab_match::MatchModel::new(
        body.model.base_look.clone(),
        crm,
        body.model.transform,
        None,
        1,
        1,
    )
    .expect("model");
    let target_function = &body.model.base_look;
    let target = PreparedTarget::new(TargetSpec {
        probes: &probes,
        function: target_function,
        uncertainty: None,
        emphasis: Emphasis::Balanced,
        tau: 1.0,
        support_sigma: 1.5,
        density_floor: None,
    })
    .expect("target");
    let ec = plan
        .by_key
        .get(&looklab_schema::ControlKey::new("exposure_compensation").expect("key"))
        .copied()
        .expect("exposure control");
    let mut settings = plan.default.clone();
    settings.set(
        plan.key(ec).clone(),
        looklab_model::realization::ControlValue::Step(6),
    );
    let suggestion = looklab_match::Suggestion {
        sim: model.sim().clone(),
        settings,
        objective: 0.0,
        median_delta_e: 0.0,
        p95_delta_e: 0.0,
        max_delta_e: 0.0,
        sigma_band: 0.0,
    };
    let card = settings_card(&target, &plan, &model, &suggestion).expect("card");
    assert!(card.median_delta_e > 0.1, "median {}", card.median_delta_e);
    assert!(
        !card.residual_summary.contains("indistinguishable"),
        "summary {}",
        card.residual_summary
    );
}

#[test]
fn confidence_rejects_support_that_only_covers_defaults() {
    let plan = synth::plan();
    let probes = synth::probes();
    let body = synth::body(11).expect("synthetic body");
    let mut crm = body.model.crm.clone();
    crm.support.controls = plan
        .domains
        .iter()
        .map(|domain| looklab_model::realization::ControlSupport {
            key: domain.key.clone(),
            default: domain.default_value().clone(),
            observed: vec![domain.default_value().clone()],
        })
        .collect();
    let model = looklab_match::MatchModel::new(
        body.model.base_look.clone(),
        crm,
        body.model.transform,
        None,
        1,
        1,
    )
    .expect("model");
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
    let suggestion = looklab_match::Suggestion {
        sim: model.sim().clone(),
        settings: plan.default.clone(),
        objective: 0.0,
        median_delta_e: 0.0,
        p95_delta_e: 0.0,
        max_delta_e: 0.0,
        sigma_band: 0.0,
    };
    let card = settings_card(&target, &plan, &model, &suggestion).expect("card");
    assert_eq!(
        card.confidence.level,
        looklab_match::ConfidenceLevel::Low,
        "{}",
        card.confidence.reason
    );
}

#[test]
fn covariance_rejects_wrong_dimensions() {
    let covariance = ControlCovariance {
        parameter_ids: Vec::new(),
        covariance: vec![1.0],
    };
    assert!(covariance.validate().is_err());
}
