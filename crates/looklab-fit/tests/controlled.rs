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
    println!(
        "held-out rerenders: n {} median {:.3} p95 {:.3} fit {:?}",
        errors.len(),
        median(errors.clone()),
        {
            let mut sorted = errors.clone();
            sorted.sort_by(f64::total_cmp);
            sorted[sorted.len() * 95 / 100]
        },
        elapsed
    );
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
    assert!(error_median <= 1.5);
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
    let base =
        fit_controlled(defaults, &dataset.schema, 1, &sim(), &options()).expect("default fit");
    let extended = fit_controlled(with_non_default, &dataset.schema, 1, &sim(), &options())
        .expect("joint fit");
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
