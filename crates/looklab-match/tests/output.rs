// SPDX-License-Identifier: AGPL-3.0-or-later

use looklab_color::Xyz;
use looklab_color::ucs::{Ucs, delta_e_ucs};
use looklab_match::synth;
use looklab_match::{
    AssessmentStatus, Calibration, ControlCovariance, Emphasis, LinearCache, MatchOptions,
    OPTIMIZER_VERSION, OutputContext, OutputPin, PERMALINK_FORMAT, Permalink, PermalinkSources,
    PreparedTarget, QualificationRef, REACHABILITY_TOLERANCE_DE, ReachabilityStatus, ReportPin,
    ReportSubject, ScoringMetric, SelectionContext, SelectionMode, TargetAppearance,
    TargetFunction, TargetSpec, optimize, predicted_trace, settings_card,
};
use looklab_model::ProbeSetVersion;
use looklab_model::file::{ProbeCovariance, ProbeScalars, UncertaintyData};
use looklab_schema::{
    ControlKey, LookId, OutputEncoding, PublishedTargetId, RealizationRevisionId, Sha256Hex,
    SimKey, TargetVersion,
};

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
        metric: ScoringMetric::LegacyPreClipV1,
        output: None,
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
        selection: None,
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
        metric: ScoringMetric::LegacyPreClipV1,
        output: None,
        emphasis: Emphasis::Skin,
        seed: 42,
        sources: PermalinkSources::default(),
        calibration: calibration.clone(),
        selection: None,
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
        appearance: body.appearance(),
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
    assert_eq!(card.metric, ScoringMetric::RenderedPostClipV1);
    assert_eq!(&card.reachability.witness, &best.settings);
    assert_eq!(card.reachability.evaluated, target.supported.len());
    assert!(
        text.contains("metric: looklab-scoring-metric/rendered-post-clip/1"),
        "{text}"
    );
    assert!(text.contains("reachability:"), "{text}");
    assert_eq!(card.joint_uncertainty, AssessmentStatus::Unassessed);
    assert!(text.contains("joint uncertainty: unassessed"), "{text}");
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
        body.model.output_encoding,
    )
    .expect("model with covariance");
    assert!(!model.groups.is_empty());
    let trace = predicted_trace(
        &model,
        &body.settings,
        body.appearance().metric(),
        probes.probes[0].xyz,
    )
    .expect("trace")
    .expect("assessed trace");
    assert!(trace > 0.0, "trace {trace}");
}

#[test]
fn target_confidence_scales_error_but_not_predicted_covariance() {
    let probes = synth::probes();
    let body = synth::body(8).expect("synthetic body");
    let mut values = vec![0.0_f64; probes.probes.len() * 9];
    values[0] = 1.0;
    let uncertainty = UncertaintyData {
        probe_cov_post: Some(
            ProbeCovariance::new(probes.probes.len(), values).expect("covariance"),
        ),
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
        appearance: body.appearance(),
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
        body.model.output_encoding,
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
        appearance: body.appearance(),
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
    let output = target
        .optional_output_context(&model, &settings)
        .expect("context");
    let suggestion = looklab_match::Suggestion {
        sim: model.sim().clone(),
        settings,
        objective: 0.0,
        median_delta_e: 0.0,
        p95_delta_e: 0.0,
        max_delta_e: 0.0,
        sigma_band: None,
        metric: target.metric(),
        output,
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
        body.model.output_encoding,
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
        appearance: body.appearance(),
    })
    .expect("target");
    let settings = plan.default.clone();
    let output = target
        .optional_output_context(&model, &settings)
        .expect("context");
    let suggestion = looklab_match::Suggestion {
        sim: model.sim().clone(),
        settings,
        objective: 0.0,
        median_delta_e: 0.0,
        p95_delta_e: 0.0,
        max_delta_e: 0.0,
        sigma_band: None,
        metric: target.metric(),
        output,
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

fn scoped_permalink(selection: Option<SelectionContext>) -> Permalink {
    Permalink {
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
        metric: ScoringMetric::LegacyPreClipV1,
        output: None,
        emphasis: Emphasis::Balanced,
        seed: 42,
        sources: PermalinkSources::default(),
        calibration: Calibration::default(),
        selection,
    }
}

fn report_pin() -> ReportPin {
    ReportPin {
        subject: ReportSubject::Target(PublishedTargetId {
            look: LookId::new("classic-chrome").expect("look"),
            version: TargetVersion::new(3).expect("version"),
        }),
        reference: QualificationRef::Report(looklab_schema::ModelReportId::from_digest([7; 32])),
    }
}

fn realization_pin() -> ReportPin {
    ReportPin {
        subject: ReportSubject::Realization(
            RealizationRevisionId::new("body_realization:fujifilm/x-t5/1/classic_chrome@2")
                .expect("revision"),
        ),
        reference: QualificationRef::Eligibility(
            looklab_schema::EligibilitySnapshotId::from_digest([9; 32]),
        ),
    }
}

#[test]
fn selection_permalink_round_trips_with_typed_subjects() {
    let selection = SelectionContext {
        bundle: Sha256Hex::from_bytes([5; 32]),
        mode: SelectionMode::Qualified,
        reports: vec![realization_pin(), report_pin(), report_pin()],
        firmware: None,
        wb_mode: None,
    };
    let permalink = scoped_permalink(Some(selection));
    let text = permalink.canonical();
    assert!(text.starts_with("looklab-match/3|"));
    assert!(text.contains("|selection=qualified"));
    let parsed = Permalink::parse(&text).expect("parse");
    let selection = parsed.selection.as_ref().expect("selection");
    assert_eq!(selection.mode, SelectionMode::Qualified);
    assert_eq!(selection.reports.len(), 2);
    assert_eq!(parsed.canonical(), text);
}

#[test]
fn qualified_selection_requires_a_pinned_report() {
    let selection = SelectionContext {
        bundle: Sha256Hex::from_bytes([5; 32]),
        mode: SelectionMode::Qualified,
        reports: Vec::new(),
        firmware: None,
        wb_mode: None,
    };
    assert!(Permalink::parse(&scoped_permalink(Some(selection)).canonical()).is_err());
}

#[test]
fn experimental_selection_may_pin_no_report() {
    let selection = SelectionContext {
        bundle: Sha256Hex::from_bytes([5; 32]),
        mode: SelectionMode::Experimental,
        reports: Vec::new(),
        firmware: None,
        wb_mode: None,
    };
    let text = scoped_permalink(Some(selection)).canonical();
    assert!(text.starts_with("looklab-match/3|"));
    let parsed = Permalink::parse(&text).expect("parse");
    let selection = parsed.selection.expect("selection");
    assert_eq!(selection.mode, SelectionMode::Experimental);
    assert!(selection.reports.is_empty());
}

#[test]
fn legacy_formats_reject_selection_fields() {
    let scoped = scoped_permalink(Some(SelectionContext {
        bundle: Sha256Hex::from_bytes([5; 32]),
        mode: SelectionMode::Experimental,
        reports: Vec::new(),
        firmware: None,
        wb_mode: None,
    }))
    .canonical();
    for legacy in ["looklab-match/1", "looklab-match/2"] {
        let downgraded = scoped.replacen("looklab-match/3", legacy, 1);
        assert!(Permalink::parse(&downgraded).is_err(), "{legacy}");
    }
    let malformed = scoped.replacen(&Sha256Hex::from_bytes([5; 32]).to_hex(), "not-a-digest", 1);
    assert!(Permalink::parse(&malformed).is_err());
}

struct FixedTarget(Ucs);

impl TargetFunction for FixedTarget {
    fn predict(&self, _xyz: Xyz) -> Ucs {
        self.0
    }
}

fn prepared<'a>(
    probes: &'a looklab_model::ProbeSet,
    function: &'a dyn TargetFunction,
    appearance: TargetAppearance,
) -> PreparedTarget<'a> {
    PreparedTarget::new(TargetSpec {
        probes,
        function,
        uncertainty: None,
        emphasis: Emphasis::Balanced,
        tau: 1.0,
        support_sigma: 1.5,
        density_floor: None,
        appearance,
    })
    .expect("target")
}

fn pin(encoding: OutputEncoding, clip_k: f64) -> OutputPin {
    OutputPin::new(encoding, clip_k).expect("pin")
}

#[test]
fn rendered_metric_clips_gamut_limited_targets() {
    let probes = synth::probes();
    let srgb = pin(OutputEncoding::Srgb, 0.03);
    let adobe = pin(OutputEncoding::AdobeRgb, 0.03);
    let candidates = [
        Ucs::new(70.0, 60.0, -40.0),
        Ucs::new(60.0, 80.0, 20.0),
        Ucs::new(50.0, 90.0, -60.0),
        Ucs::new(80.0, 40.0, 70.0),
        Ucs::new(60.0, 50.0, 0.0),
        Ucs::new(70.0, 40.0, 40.0),
        Ucs::new(55.0, 70.0, 40.0),
        Ucs::new(65.0, 70.0, -60.0),
    ];
    let clipped = candidates.iter().any(|value| {
        looklab_model::try_output_encode_clip(*value, looklab_color::RgbSpace::Srgb, 0.03)
            .is_some_and(|rendered| delta_e_ucs(*value, rendered) > 0.5)
    });
    let gamut = candidates.iter().any(|value| {
        match (
            looklab_model::try_output_encode_clip(*value, looklab_color::RgbSpace::Srgb, 0.03),
            looklab_model::try_output_encode_clip(*value, looklab_color::RgbSpace::AdobeRgb, 0.03),
        ) {
            (Some(srgb), Some(adobe)) => delta_e_ucs(srgb, adobe) > 0.5,
            _ => false,
        }
    });
    assert!(clipped, "a candidate renders through the soft clip");
    assert!(gamut, "different gamut limitations remain in the result");

    let fixed = FixedTarget(candidates[0]);
    let pre = prepared(&probes, &fixed, TargetAppearance::PreClip);
    let rendered = prepared(&probes, &fixed, TargetAppearance::Rendered(srgb));
    let rendered_adobe = prepared(&probes, &fixed, TargetAppearance::Rendered(adobe));
    assert_eq!(pre.metric(), ScoringMetric::LegacyPreClipV1);
    assert_eq!(rendered.metric(), ScoringMetric::RenderedPostClipV1);
    assert_eq!(rendered.target[0], srgb.render(pre.target[0]));
    assert_eq!(rendered.supported, pre.supported);
    assert_eq!(rendered.supported, rendered_adobe.supported);
}

#[test]
fn exposure_and_wb_move_prediction_not_the_fixed_target() {
    let plan = synth::plan();
    let probes = synth::probes();
    let body = synth::body(4).expect("synthetic body");
    let target_function = body.target();
    let target = prepared(&probes, &target_function, body.appearance());
    let before_target = target.target.clone();
    let before_supported = target.supported.clone();

    let ec = plan
        .by_key
        .get(&ControlKey::new("exposure_compensation").expect("key"))
        .copied()
        .expect("exposure control");
    let mut settings = body.settings.clone();
    let current = plan.value_index(ec, &settings);
    let other = (current + 1).min(plan.domains[ec].values.len() - 1);
    plan.set(ec, other, &mut settings);

    let first = body.model.realize(&body.settings).expect("first");
    let second = body.model.realize(&settings).expect("second");
    let xyz = probes.probes[target.supported[0]].xyz;
    assert!(
        delta_e_ucs(
            body.model.scored_ucs(target.metric(), &first, xyz),
            body.model.scored_ucs(target.metric(), &second, xyz),
        ) > 0.0
    );
    assert_eq!(target.target, before_target);
    assert_eq!(target.supported, before_supported);
}

#[test]
fn additive_linearization_matches_forward_differences_on_smooth_points() {
    let plan = synth::plan();
    let probes = synth::probes();
    let body = synth::body(4).expect("synthetic body");
    let target_function = body.target();
    let target = prepared(&probes, &target_function, body.appearance());
    let cache = LinearCache::new(
        &body.model,
        &plan,
        &target,
        &plan.default,
        &target.full_subset(),
        0.0,
    )
    .expect("cache");
    let metric = target.metric();
    let base = cache.realization(&plan, &plan.default).expect("base");
    let mut errors = Vec::new();
    for probe in 0..cache.indices.len().min(24) {
        let xyz = probes.probes[cache.indices[probe]].xyz;
        let base_prediction = body.model.scored_ucs(metric, &base, xyz).to_array();
        for control in plan.plan_controls.iter().copied() {
            let current = plan.value_index(control, &plan.default);
            for value in 0..plan.domains[control].values.len() {
                if value.abs_diff(current) != 1 {
                    continue;
                }
                let candidate = plan.candidate(&plan.default, control, value);
                let realization = cache.realization(&plan, &candidate).expect("candidate");
                let forward = body.model.scored_ucs(metric, &realization, xyz).to_array();
                let contribution = cache.contributions[probe][control][value];
                let mut squared = 0.0_f64;
                for channel in 0..3 {
                    let difference =
                        forward[channel] - (base_prediction[channel] + contribution[channel]);
                    squared += difference * difference;
                }
                errors.push(squared.sqrt());
            }
        }
    }
    errors.sort_by(f64::total_cmp);
    let median = errors[errors.len() / 2];
    assert!(median < 0.25, "median linearization error {median}");
}

#[test]
fn rendered_permalink_records_metric_and_output_context() {
    let permalink = rendered_permalink();
    let text = permalink.canonical();
    assert!(text.starts_with("looklab-match/4|"), "{text}");
    assert!(
        text.contains("metric=looklab-scoring-metric/rendered-post-clip/1"),
        "{text}"
    );
    let parsed = Permalink::parse(&text).expect("parse");
    assert_eq!(parsed, permalink);
    assert_eq!(parsed.metric, ScoringMetric::RenderedPostClipV1);
    assert_eq!(parsed.output, permalink.output);

    let legacy = Permalink::parse(&scoped_permalink(None).canonical()).expect("legacy");
    assert!(legacy.canonical().starts_with(PERMALINK_FORMAT));
    assert_eq!(legacy.metric, ScoringMetric::LegacyPreClipV1);
    assert!(legacy.output.is_none());
}

#[test]
fn rendered_permalink_requires_metric_output_and_optimizer() {
    let text = rendered_permalink().canonical();
    let without_output = text
        .split('|')
        .filter(|part| !part.starts_with("output="))
        .collect::<Vec<_>>()
        .join("|");
    assert!(Permalink::parse(&without_output).is_err());

    let legacy_metric = text.replace(
        "looklab-scoring-metric/rendered-post-clip/1",
        "looklab-scoring-metric/legacy-pre-clip/1",
    );
    assert!(Permalink::parse(&legacy_metric).is_err());

    let legacy_optimizer = text.replacen(
        &format!("optimizer={OPTIMIZER_VERSION}"),
        "optimizer=looklab-match/1",
        1,
    );
    assert!(Permalink::parse(&legacy_optimizer).is_err());

    let legacy_with_metric = format!(
        "{}|metric=looklab-scoring-metric/rendered-post-clip/1",
        scoped_permalink(None).canonical()
    );
    assert!(Permalink::parse(&legacy_with_metric).is_err());
}

fn rendered_permalink() -> Permalink {
    let mut permalink = scoped_permalink(None);
    permalink.optimizer = OPTIMIZER_VERSION.to_string();
    permalink.metric = ScoringMetric::RenderedPostClipV1;
    permalink.output = Some(OutputContext {
        target: pin(OutputEncoding::Srgb, 0.03),
        camera: pin(OutputEncoding::AdobeRgb, 0.02),
    });
    permalink
}

fn half_support(probes: &looklab_model::ProbeSet) -> UncertaintyData {
    let values = probes
        .probes
        .iter()
        .enumerate()
        .map(|(index, _)| if index % 2 == 0 { 1.0 } else { 0.0 })
        .collect();
    UncertaintyData {
        support: Some(ProbeScalars::new(values).expect("support mask")),
        ..UncertaintyData::default()
    }
}

fn card_for(
    target: &PreparedTarget<'_>,
    plan: &looklab_match::Plan,
    model: &looklab_match::MatchModel,
    settings: &looklab_model::realization::Settings,
) -> looklab_match::SettingsCard {
    let output = target
        .optional_output_context(model, settings)
        .expect("context");
    let suggestion = looklab_match::Suggestion {
        sim: model.sim().clone(),
        settings: settings.clone(),
        objective: 0.0,
        median_delta_e: 0.0,
        p95_delta_e: 0.0,
        max_delta_e: 0.0,
        sigma_band: None,
        metric: target.metric(),
        output,
    };
    settings_card(target, plan, model, &suggestion).expect("card")
}

#[test]
fn missing_domain_keeps_declared_counts_and_assesses_zero() {
    let plan = synth::plan();
    let probes = synth::probes();
    let body = synth::body(6).expect("synthetic body");
    let function = synth::RealizationTarget {
        model: body.model.clone(),
        settings: plan.default.clone(),
    };
    let uncertainty = half_support(&probes);
    let target = PreparedTarget::new(TargetSpec {
        probes: &probes,
        function: &function,
        uncertainty: Some(&uncertainty),
        emphasis: Emphasis::Balanced,
        tau: 1.0,
        support_sigma: 1.5,
        density_floor: None,
        appearance: body.appearance(),
    })
    .expect("target");
    let requested = probes.probes.len() + 7;
    let target = target
        .with_declared_points(requested)
        .expect("declared points");
    let settings = plan.default.clone();
    let card = card_for(&target, &plan, &body.model, &settings);
    assert_eq!(card.reachability.witness, settings);
    assert_eq!(card.reachability.tolerance, REACHABILITY_TOLERANCE_DE);
    assert_eq!(card.reachability.declared, requested);
    assert_eq!(card.reachability.evaluated, target.supported.len());
    assert_eq!(card.reachability.assessed, 0);
    assert_eq!(card.reachability.within_assessed, 0);
    assert!(card.reachability.within_tolerance <= card.reachability.evaluated);
    assert_eq!(card.reachability.status, ReachabilityStatus::Unassessed);
    assert!(card.missed.is_empty());
}

#[test]
fn fixed_target_without_domain_is_unassessed_not_reachable() {
    let plan = synth::plan();
    let probes = synth::probes();
    let body = synth::body(6).expect("synthetic body");
    let fixed = FixedTarget(Ucs::new(90.0, 120.0, -120.0));
    let target = prepared(&probes, &fixed, body.appearance());
    let card = card_for(&target, &plan, &body.model, &plan.default);
    assert_eq!(card.reachability.assessed, 0);
    assert!(card.reachability.evaluated > 0);
    assert!(
        card.reachability.within_tolerance < card.reachability.evaluated,
        "a fixed target far from the prediction must miss descriptively"
    );
    assert_eq!(card.reachability.status, ReachabilityStatus::NoRecipeFound);
    assert!(card.missed.is_empty());
    assert!(!card.render_text().contains("unreachable"));
}

#[test]
fn settings_text_names_the_declared_base_it_approximates() {
    let plan = synth::plan();
    let probes = synth::probes();
    let body = synth::body(6).expect("synthetic body");
    let fixed = FixedTarget(Ucs::new(50.0, 0.0, 0.0));
    let target = prepared(&probes, &fixed, body.appearance());
    let mut card = card_for(&target, &plan, &body.model, &plan.default);
    card.approximated_base = Some("classic-chrome@2.0.0".to_owned());
    card.destination_scope = Some("the camera realization is experimental".to_owned());
    let text = card.render_text();
    assert!(
        text.contains("the settings approximate the declared base classic-chrome@2.0.0"),
        "{text}"
    );
    assert!(
        text.contains("destination realization: the camera realization is experimental"),
        "{text}"
    );
}

#[test]
fn settings_text_omits_approximation_without_a_declared_base() {
    let plan = synth::plan();
    let probes = synth::probes();
    let body = synth::body(6).expect("synthetic body");
    let fixed = FixedTarget(Ucs::new(50.0, 0.0, 0.0));
    let target = prepared(&probes, &fixed, body.appearance());
    let text = card_for(&target, &plan, &body.model, &plan.default).render_text();
    assert!(!text.contains("approximation:"), "{text}");
    assert!(!text.contains("destination realization"), "{text}");
}
