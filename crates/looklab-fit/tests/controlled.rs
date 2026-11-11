// SPDX-License-Identifier: AGPL-3.0-or-later
#![cfg(feature = "synth")]
#![allow(clippy::disallowed_types)]

use looklab_color::{Rgb, RgbSpace};
use looklab_fit::synth::{
    ControlSynthConfig, SynthConfig, controlled_dataset, controlled_dataset_with,
    localized_hue_model, random_control_model, random_settings,
};
use looklab_fit::{ControlValue, FitImage, FitOptions, ImageIntent, Settings, fit_controlled};
use looklab_model::output_encode_clip;
use looklab_model::realization::realize;
use looklab_model::solve::ucs_of_xyz;
use looklab_schema::{ControlKey, SimKey, validate_control_schema};
use rand_chacha::rand_core::SeedableRng;

const SCHEMA: &str = include_str!("../../../data/bodies/fujifilm-x-t5.yaml");

fn schema() -> looklab_schema::ControlSchema {
    validate_control_schema(SCHEMA).expect("body schema validates")
}

fn sim() -> SimKey {
    SimKey::new("classic_chrome").expect("sim key")
}

fn key(name: &str) -> ControlKey {
    ControlKey::new(name).expect("control key")
}

fn step(name: &str, value: i64, settings: &mut Settings) {
    settings.set(key(name), ControlValue::Step(value));
}

fn name(name: &str, value: &str, settings: &mut Settings) {
    settings.set(
        key(name),
        ControlValue::Enum(looklab_schema::EnumValue::new(value).expect("enum value")),
    );
}

fn library_settings() -> Vec<Settings> {
    let mut all = Vec::new();
    for _ in 0..6 {
        all.push(Settings::new());
    }
    for (control, value) in [
        ("color", 2),
        ("color", -2),
        ("color", 4),
        ("color", -4),
        ("highlight_tone", 2),
        ("highlight_tone", -2),
        ("highlight_tone", 4),
        ("highlight_tone", -4),
        ("shadow_tone", 3),
        ("shadow_tone", -3),
        ("shadow_tone", 4),
        ("shadow_tone", -4),
        ("wb_shift_r", 4),
        ("wb_shift_r", 6),
        ("wb_shift_b", -4),
        ("wb_shift_b", -6),
    ] {
        let mut settings = Settings::new();
        step(control, value, &mut settings);
        all.push(settings);
    }
    for mode in ["dr200", "dr400"] {
        let mut settings = Settings::new();
        name("dynamic_range", mode, &mut settings);
        all.push(settings);
    }
    for effect in ["weak", "strong"] {
        let mut settings = Settings::new();
        name("color_chrome_effect", effect, &mut settings);
        all.push(settings);
        let mut settings = Settings::new();
        name("color_chrome_fx_blue", effect, &mut settings);
        all.push(settings);
    }
    all
}

fn group_settings() -> Vec<Settings> {
    let mut all = Vec::new();
    let mut settings = Settings::new();
    step("color", 3, &mut settings);
    step("highlight_tone", 4, &mut settings);
    all.push(settings);
    let mut settings = Settings::new();
    step("color", -3, &mut settings);
    step("shadow_tone", 4, &mut settings);
    step("wb_shift_r", 6, &mut settings);
    all.push(settings);
    let mut settings = Settings::new();
    name("dynamic_range", "dr400", &mut settings);
    name("color_chrome_effect", "strong", &mut settings);
    step("exposure_compensation", 3, &mut settings);
    all.push(settings);
    let mut settings = Settings::new();
    step("color", 4, &mut settings);
    name("color_chrome_fx_blue", "weak", &mut settings);
    step("wb_shift_b", -6, &mut settings);
    all.push(settings);
    all
}

fn config() -> ControlSynthConfig {
    let schema = schema();
    let mut settings = library_settings();
    settings.extend(group_settings());
    let mut groups = vec![None; library_settings().len()];
    groups.extend(vec![Some([7; 16]); group_settings().len()]);
    ControlSynthConfig {
        base: SynthConfig {
            seed: 0x2211,
            images: settings.len(),
            samples_per_image: 120,
            noise_ucs: 0.4,
            clip_k: 0.03,
            ..SynthConfig::default()
        },
        schema,
        epoch: 1,
        base_sim: sim(),
        settings,
        groups,
        response_scale: 0.08,
        truth_seed: 0x3311,
    }
}

fn options() -> FitOptions {
    FitOptions {
        seed: 0x5511,
        fit_residual: false,
        max_sweeps: 24,
        ..FitOptions::default()
    }
}

fn prediction_error(
    look: &looklab_model::Look,
    model: &looklab_model::realization::ControlResponseModel,
    image: &FitImage,
    settings: &Settings,
    exposure: f64,
    wb: [f64; 2],
) -> Vec<f64> {
    let realization = realize(look, model, settings).expect("realizes");
    let space = match image.samples.header().output_space {
        looklab_samples::OutputSpace::Srgb => RgbSpace::Srgb,
        looklab_samples::OutputSpace::AdobeRgb => RgbSpace::AdobeRgb,
    };
    let scale = libm::pow(2.0, exposure + realization.exposure_ev);
    let gain = [
        realization.wb_gain[0] * libm::exp(wb[0]),
        realization.wb_gain[1],
        realization.wb_gain[2] * libm::exp(wb[1]),
    ];
    let mut errors = Vec::new();
    for (in_rgb, out_rgb) in image.samples.in_rgb().iter().zip(image.samples.out_rgb()) {
        let cam = [
            f64::from(in_rgb[0]) * gain[0] * scale,
            f64::from(in_rgb[1]) * gain[1] * scale,
            f64::from(in_rgb[2]) * gain[2] * scale,
        ];
        let xyz = looklab_color::Xyz::from_array(image.transform.mul_vec(cam));
        let predicted =
            output_encode_clip(realization.look.eval(xyz), space, realization.look.clip.k);
        let target = ucs_of_xyz(space.rgb_linear_to_xyz(Rgb::from_array(out_rgb.map(f64::from))));
        errors.push(looklab_color::ucs::delta_e_ucs(predicted, target));
    }
    errors
}

fn median(mut values: Vec<f64>) -> f64 {
    values.sort_by(f64::total_cmp);
    values[values.len() / 2]
}

fn trace(probe: &looklab_fit::ProbeUncertainty) -> f64 {
    probe
        .probes
        .iter()
        .map(|probe| probe.covariance[0][0] + probe.covariance[1][1] + probe.covariance[2][2])
        .sum()
}

#[cfg(feature = "full-scale")]
#[test]
#[ignore = "held-out vs sweep diagnostic; run with --ignored --nocapture"]
fn held_out_vs_sweeps() {
    let config = config();
    let held_out = group_settings();
    let library = library_settings();
    let dataset = controlled_dataset(&config);
    let train = &dataset.images[..library.len()];
    for max_sweeps in [1usize, 2, 3, 4] {
        let fit_options = FitOptions {
            max_sweeps,
            ..options()
        };
        let result = fit_controlled(train, &dataset.schema, 1, &sim(), &fit_options).expect("fit");
        let model = result
            .control_response
            .as_ref()
            .expect("model")
            .model
            .clone();
        let mut held = Vec::new();
        for (index, settings) in held_out.iter().enumerate() {
            let image = &dataset.images[library.len() + index];
            held.extend(prediction_error(
                &result.look,
                &model,
                image,
                settings,
                image.exposure_hint,
                [0.0, 0.0],
            ));
        }
        let mut trained = Vec::new();
        for (index, settings) in library.iter().enumerate() {
            let image = &dataset.images[index];
            trained.extend(prediction_error(
                &result.look,
                &model,
                image,
                settings,
                image.exposure_hint,
                [0.0, 0.0],
            ));
        }
        eprintln!(
            "max_sweeps {max_sweeps}: held {:.3} train {:.3} converged {} evaluations {} lambda grl {:.2} resp {:.1}",
            median(held),
            median(trained),
            result.diagnostics.converged,
            result.diagnostics.evaluations,
            result.lambdas.grl,
            result.lambdas.response_tone,
        );
    }
    for response in [1.0_f64, 10.0, 100.0] {
        let fit_options = FitOptions {
            max_sweeps: 8,
            run_gcv: false,
            initial_lambdas: looklab_fit::StageLambdas {
                tone: 0.1,
                tint: 2.87,
                grl: 11.4,
                response_tone: response,
                response_tint: response,
                response_chroma: response,
                response_hue: response,
                response_lightness: response,
                response_wb: response,
                ..looklab_fit::StageLambdas::default()
            },
            ..options()
        };
        let result = fit_controlled(train, &dataset.schema, 1, &sim(), &fit_options).expect("fit");
        let model = result
            .control_response
            .as_ref()
            .expect("model")
            .model
            .clone();
        let mut held = Vec::new();
        for (index, settings) in held_out.iter().enumerate() {
            let image = &dataset.images[library.len() + index];
            held.extend(prediction_error(
                &result.look,
                &model,
                image,
                settings,
                image.exposure_hint,
                [0.0, 0.0],
            ));
        }
        let mut trained = Vec::new();
        for (index, settings) in library.iter().enumerate() {
            let image = &dataset.images[index];
            trained.extend(prediction_error(
                &result.look,
                &model,
                image,
                settings,
                image.exposure_hint,
                [0.0, 0.0],
            ));
        }
        eprintln!(
            "response lambda {response}: held {:.3} train {:.3}",
            median(held),
            median(trained)
        );
    }
}

#[test]
fn predicts_held_out_rerenders_at_unseen_settings() {
    let config = config();
    let held_out = group_settings();
    let library = library_settings();
    let dataset = controlled_dataset(&config);
    let train = &dataset.images[..library.len()];
    let started = std::time::Instant::now();
    let result =
        fit_controlled(train, &dataset.schema, 1, &sim(), &options()).expect("controlled fit");
    let elapsed = started.elapsed();
    let model = result
        .control_response
        .as_ref()
        .expect("control model")
        .model
        .clone();
    let mut errors = Vec::new();
    for (index, settings) in held_out.iter().enumerate() {
        let image = &dataset.images[library.len() + index];
        errors.extend(prediction_error(
            &result.look,
            &model,
            image,
            settings,
            image.exposure_hint,
            [0.0, 0.0],
        ));
    }
    let mut train_errors = Vec::new();
    for (index, settings) in library.iter().enumerate() {
        let image = &dataset.images[index];
        train_errors.extend(prediction_error(
            &result.look,
            &model,
            image,
            settings,
            image.exposure_hint,
            [0.0, 0.0],
        ));
    }
    println!(
        "held-out rerenders: n {} median {:.3} p95 {:.3}; train median {:.3}; converged {}; lambdas {:?}; fit {:?}",
        errors.len(),
        median(errors.clone()),
        {
            let mut sorted = errors.clone();
            sorted.sort_by(f64::total_cmp);
            sorted[sorted.len() * 95 / 100]
        },
        median(train_errors),
        result.diagnostics.converged,
        result.lambdas,
        elapsed
    );
    // GCV response smoothness with a floor of 1.0 (not 100) keeps held-out
    // prediction at 1.15 ΔE on this fixture.
    assert!(median(errors) <= 1.5);
}

#[test]
fn hue_localized_blue_response_is_predicted() {
    use looklab_model::realization::GRL_FEATURES;

    let config = config();
    let truth = localized_hue_model(&config.schema, 1, &sim(), GRL_FEATURES - 2, 0.12);
    let dataset = controlled_dataset_with(&config, truth);
    let library = library_settings();
    let result = fit_controlled(
        &dataset.images[..library.len()],
        &dataset.schema,
        1,
        &sim(),
        &options(),
    )
    .expect("localized fit");
    let model = result
        .control_response
        .as_ref()
        .expect("model")
        .model
        .clone();
    let mut errors = Vec::new();
    for (index, settings) in group_settings().iter().enumerate() {
        let image = &dataset.images[library.len() + index];
        errors.extend(prediction_error(
            &result.look,
            &model,
            image,
            settings,
            image.exposure_hint,
            [0.0, 0.0],
        ));
    }
    let error_median = median(errors);
    println!("hue-localized blue median {error_median:.3}");
    assert!(error_median <= 4.0);
}

#[test]
fn refit_recovers_responses_with_theta_fixed() {
    let dataset = controlled_dataset(&config());
    let library = library_settings();
    let result = looklab_fit::refit_responses(
        &dataset.truth_look,
        &dataset.images[..library.len()],
        &dataset.schema,
        1,
        &sim(),
        &dataset.truth_model,
        &options(),
    )
    .expect("response refit");
    let fitted = &result.model;
    let fitted_delta = fitted
        .controls
        .iter()
        .find(|control| control.key.as_str() == "color")
        .and_then(|control| {
            control
                .values
                .iter()
                .find(|value| value.value == ControlValue::Step(2))
        })
        .expect("fitted color response");
    let truth_delta = dataset
        .truth_model
        .controls
        .iter()
        .find(|control| control.key.as_str() == "color")
        .and_then(|control| {
            control
                .values
                .iter()
                .find(|value| value.value == ControlValue::Step(2))
        })
        .expect("truth color response");
    let fitted_layer = &fitted_delta.delta.layers[0];
    let truth_layer = &truth_delta.delta.layers[0];
    for (feature, truth) in &truth_layer.entries {
        let fitted = fitted_layer
            .entries
            .iter()
            .find(|(candidate, _)| candidate == feature)
            .map(|(_, value)| *value)
            .unwrap_or(0.0);
        assert!((fitted - truth).abs() < 0.01, "feature {feature}");
    }
    let mut errors = Vec::new();
    for (index, settings) in library.iter().enumerate() {
        let image = &dataset.images[index];
        errors.extend(prediction_error(
            &dataset.truth_look,
            fitted,
            image,
            settings,
            image.exposure_hint,
            [0.0, 0.0],
        ));
    }
    let error_median = median(errors);
    println!("refit training median {error_median:.3}");
    assert!(error_median < 2.5);
}

#[test]
fn non_default_images_reduce_theta_uncertainty() {
    let dataset = controlled_dataset(&config());
    let library = library_settings();
    let defaults = &dataset.images[..6];
    let with_non_default = &dataset.images[..library.len()];
    let production = FitOptions {
        max_sweeps: FitOptions::default().max_sweeps,
        ..options()
    };
    let base =
        fit_controlled(defaults, &dataset.schema, 1, &sim(), &production).expect("default fit");
    let extended = fit_controlled(with_non_default, &dataset.schema, 1, &sim(), &production)
        .expect("joint fit");
    assert!(base.diagnostics.converged, "default fit did not converge");
    assert!(extended.diagnostics.converged, "joint fit did not converge");
    let base_trace = trace(&base.probes);
    let extended_trace = trace(&extended.probes);
    println!("theta probe covariance trace: {base_trace:.4} -> {extended_trace:.4}");
    assert!(
        extended_trace < 0.9 * base_trace,
        "trace {base_trace} -> {extended_trace}"
    );
}

#[test]
fn recipe_images_do_not_move_theta() {
    let config = config();
    let native_count = library_settings().len();
    let native = controlled_dataset(&config);
    let recipe_count = config.settings.len() - native_count;
    let mut recipe_images: Vec<FitImage> = native.images[native_count..].to_vec();
    for image in &mut recipe_images {
        image.intent = ImageIntent::Recipe;
    }
    let mut all = native.images[..native_count].to_vec();
    all.extend(recipe_images);
    let without = fit_controlled(
        &native.images[..native_count],
        &native.schema,
        1,
        &sim(),
        &options(),
    )
    .expect("native fit");
    let with = fit_controlled(&all, &native.schema, 1, &sim(), &options()).expect("recipe fit");
    println!("recipe images: {recipe_count}");
    assert!(
        without
            .look
            .coef_vector()
            .iter()
            .zip(with.look.coef_vector().iter())
            .all(|(left, right)| left.to_bits() == right.to_bits())
    );
}

#[test]
fn rerender_groups_share_input_pixels_and_support_inflates() {
    let dataset = controlled_dataset(&config());
    let library = library_settings().len();
    let group: Vec<&FitImage> = dataset.images[library..].iter().collect();
    let reference = group[0].samples.in_rgb();
    for image in &group[1..] {
        assert_eq!(reference, image.samples.in_rgb());
    }
    let model = random_control_model(&dataset.schema, 1, &sim(), 9, 0.05);
    let mut observed = Settings::new();
    step("color", 2, &mut observed);
    let mut far = Settings::new();
    step("color", 4, &mut far);
    let support = looklab_model::realization::Support {
        controls: vec![looklab_model::realization::ControlSupport {
            key: key("color"),
            default: ControlValue::Step(0),
            observed: vec![ControlValue::Step(0), ControlValue::Step(2)],
        }],
    };
    assert_eq!(support.inflation(&observed), 1.0);
    assert!(support.inflation(&far) > 1.0);
    let _ = model;
}

#[test]
fn exposure_mode_prior_resolves_the_gauge() {
    let dataset = controlled_dataset(&config());
    let library = library_settings();
    let train = &dataset.images[..library.len()];
    let tight = FitOptions {
        exposure_mode_prior_sd: 0.01,
        max_sweeps: 16,
        ..options()
    };
    let loose = FitOptions {
        exposure_mode_prior_sd: 3.0,
        max_sweeps: 16,
        ..options()
    };
    let tight_fit =
        fit_controlled(train, &dataset.schema, 1, &sim(), &tight).expect("tight mode fit");
    let loose_fit =
        fit_controlled(train, &dataset.schema, 1, &sim(), &loose).expect("loose mode fit");
    let mode_offset = |fit: &looklab_fit::FitResult, mode: looklab_fit::ExposureModeId| {
        fit.realization
            .exposure_offsets
            .iter()
            .find(|entry| entry.mode == mode)
            .map_or(0.0, |entry| entry.offset)
    };
    let mut tight_largest = 0.0_f64;
    let mut loose_largest = 0.0_f64;
    let mut total_gap = 0.0_f64;
    let mut non_default = 0usize;
    for (index, image) in train.iter().enumerate() {
        if image.exposure_mode == looklab_fit::ExposureModeId(0) {
            continue;
        }
        non_default += 1;
        let tight_mode = mode_offset(&tight_fit, image.exposure_mode);
        let loose_mode = mode_offset(&loose_fit, image.exposure_mode);
        tight_largest = tight_largest.max(tight_mode.abs());
        loose_largest = loose_largest.max(loose_mode.abs());
        let tight_total = tight_fit.images[index].exposure + tight_mode;
        let loose_total = loose_fit.images[index].exposure + loose_mode;
        total_gap = total_gap.max((tight_total - loose_total).abs());
    }
    assert!(
        non_default >= 2,
        "fixture must carry non-default exposure modes"
    );
    assert!(loose_largest > 0.001, "loose mode offsets {loose_largest}");
    assert!(
        tight_largest < loose_largest,
        "{tight_largest} vs {loose_largest}"
    );
    assert!(
        total_gap < 0.15,
        "data-identified exposure totals differ by {total_gap}"
    );
}

#[test]
fn capped_hessian_rows_converge_on_a_noisy_fit() {
    let dataset = controlled_dataset(&config());
    let library = library_settings();
    let train = &dataset.images[..library.len()];
    let capped = FitOptions {
        hessian_samples: 997,
        max_sweeps: FitOptions::default().max_sweeps,
        ..options()
    };
    let result =
        fit_controlled(train, &dataset.schema, 1, &sim(), &capped).expect("capped-row fit");
    assert_eq!(result.diagnostics.hessian_samples, capped.hessian_samples);
    assert!(
        result.diagnostics.converged,
        "capped noisy fit did not converge after {} evaluations",
        result.diagnostics.evaluations
    );
    let lambdas = result.lambdas;
    for value in [
        lambdas.tone,
        lambdas.tint,
        lambdas.grl,
        lambdas.residual,
        lambdas.response_tone,
        lambdas.response_tint,
        lambdas.response_chroma,
        lambdas.response_hue,
        lambdas.response_lightness,
        lambdas.response_wb,
    ] {
        assert!(value.is_finite() && value >= 0.0, "lambda {value}");
    }
    assert!(result.realization.clip_k.is_finite());
    for image in &result.images {
        assert!(image.exposure.is_finite());
        assert!(image.wb_log_r.is_finite());
        assert!(image.wb_log_b.is_finite());
    }
    let model = result
        .control_response
        .as_ref()
        .expect("control model")
        .model
        .clone();
    let mut errors = Vec::new();
    for (index, settings) in library.iter().enumerate() {
        let image = &dataset.images[index];
        errors.extend(prediction_error(
            &result.look,
            &model,
            image,
            settings,
            image.exposure_hint,
            [0.0, 0.0],
        ));
    }
    assert!(median(errors) <= 2.5);
}

#[test]
fn capped_hessian_rows_still_use_the_full_operator_row_set() {
    let dataset = controlled_dataset(&config());
    let library = library_settings();
    let train = &dataset.images[..library.len()];
    let capped = FitOptions {
        hessian_samples: 997,
        ..options()
    };
    let result =
        fit_controlled(train, &dataset.schema, 1, &sim(), &capped).expect("capped-row fit");
    assert_eq!(result.diagnostics.hessian_samples, capped.hessian_samples);
    assert_eq!(
        result.diagnostics.operator_rows, result.diagnostics.objective_samples,
        "final-stage curvature operator did not use the full active row set"
    );
}

#[test]
fn an_exhausted_budget_never_reports_convergence() {
    let dataset = controlled_dataset(&config());
    let library = library_settings();
    let train = &dataset.images[..library.len()];
    let exhausted = FitOptions {
        sweep_budget: 0.0,
        ..options()
    };
    let result =
        fit_controlled(train, &dataset.schema, 1, &sim(), &exhausted).expect("budget-limited fit");
    assert!(
        !result.diagnostics.converged,
        "a fit that spent its whole budget certified stationarity after {} evaluations",
        result.diagnostics.evaluations
    );
}

#[test]
fn resumed_controlled_fit_keeps_checkpoint_theta() {
    let dataset = controlled_dataset(&config());
    let library = library_settings();
    let train = &dataset.images[..library.len()];
    let first_options = FitOptions {
        max_sweeps: 1,
        ..options()
    };
    let (first, mut state) = looklab_fit::fit_controlled_with_history_resumable(
        train,
        &dataset.schema,
        1,
        &sim(),
        &first_options,
        None,
        None,
    )
    .expect("first attempt");
    assert!(!first.diagnostics.converged);
    let terminal = state.take().expect("non-converged state");
    let revised = FitOptions {
        seed: first_options.seed ^ 0x1234,
        sweep_budget: 0.0,
        ..options()
    };
    let (resumed, _) = looklab_fit::fit_controlled_with_history_resumable(
        train,
        &dataset.schema,
        1,
        &sim(),
        &revised,
        None,
        Some(&terminal),
    )
    .expect("resume");
    assert!(
        first
            .look
            .coef_vector()
            .iter()
            .zip(resumed.look.coef_vector().iter())
            .all(|(before, after)| before.to_bits() == after.to_bits()),
        "resume ran fresh initialization over the restored checkpoint"
    );
}

#[test]
fn random_settings_round_trip_through_realize() {
    let schema = schema();
    let model = random_control_model(&schema, 1, &sim(), 3, 0.05);
    let mut rng = rand_chacha::ChaCha12Rng::seed_from_u64(11);
    for _ in 0..8 {
        let settings = random_settings(&model, &mut rng, 0.5);
        let realization = realize(
            &looklab_model::Look::colorimetric_spline().expect("spline"),
            &model,
            &settings,
        )
        .expect("realizes");
        assert!(realization.wb_gain.iter().all(|value| value.is_finite()));
        assert!(realization.exposure_ev.is_finite());
    }
}
