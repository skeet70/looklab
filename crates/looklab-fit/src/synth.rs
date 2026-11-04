// SPDX-License-Identifier: AGPL-3.0-or-later
//! Synthetic single-body fixtures: a known Look rendered through the forward model with nuisances and Student-t noise.

use looklab_color::ucs::{UcsSpace, ucs_to_jmh};
use looklab_color::{Mat3, Rgb, RgbSpace, Xyz};
use looklab_model::basis::{
    C_MAX, HUE_COEFS, RESIDUAL_COEFS, S_COEFS, S_KNOTS, TINT_COEFS, TINT_KNOTS, TONE_COEFS,
};
use looklab_model::solve::reference_model;
use looklab_model::{ClipSettings, ColorLayer, Look, Residual, Tint, Tone, soft_clip};
use looklab_samples::{
    InputSpace, OutputSpace, PrepParamsHash, PrepVersion, PriorTransformVersion, SampleFlags,
    SampleHeader, SampleSet,
};
use rand_chacha::ChaCha12Rng;
use rand_chacha::rand_core::{RngCore, SeedableRng};

use crate::types::{ExposureModeId, FitImage};

#[derive(Clone, Copy, Debug)]
pub struct SynthConfig {
    pub seed: u64,
    pub images: usize,
    pub samples_per_image: usize,
    pub noise_ucs: f64,
    pub nu: f64,
    pub clip_k: f64,
    pub space: RgbSpace,
    pub exposure_sd: f64,
    pub wb_sd: f64,
    pub hint_sd: f64,
}

impl Default for SynthConfig {
    fn default() -> Self {
        Self {
            seed: 0x5eed_1234,
            images: 8,
            samples_per_image: 500,
            noise_ucs: 1.0,
            nu: 4.0,
            clip_k: 0.03,
            space: RgbSpace::Srgb,
            exposure_sd: 0.25,
            wb_sd: 0.015,
            hint_sd: 0.08,
        }
    }
}

pub struct SyntheticDataset {
    pub images: Vec<FitImage>,
    pub truth: Look,
    pub clip_k: f64,
    pub nuisances: Vec<Nuisance>,
    pub transforms: Vec<Mat3>,
    /// Noise-free scene colours per image, for held-out evaluation.
    pub points: Vec<Vec<Xyz>>,
}

#[derive(Clone, Copy, Debug)]
pub struct Nuisance {
    pub exposure: f64,
    pub wb_log_r: f64,
    pub wb_log_b: f64,
    pub hint: f64,
}

#[derive(Clone)]
struct Scene {
    xyz: Vec<Xyz>,
    rgb: Vec<[f64; 3]>,
    transform: Mat3,
    nuisance: Nuisance,
}

fn gaussian(rng: &mut ChaCha12Rng) -> f64 {
    let u1 = (rng.next_u32() as f64 + 0.5) / (u32::MAX as f64 + 1.0);
    let u2 = (rng.next_u32() as f64 + 0.5) / (u32::MAX as f64 + 1.0);
    (-2.0 * libm::log(u1)).sqrt() * libm::cos(2.0 * core::f64::consts::PI * u2)
}

fn student_t(rng: &mut ChaCha12Rng, nu: f64) -> f64 {
    let z = gaussian(rng);
    let u1 = (rng.next_u32() as f64 + 0.5) / (u32::MAX as f64 + 1.0);
    let u2 = (rng.next_u32() as f64 + 0.5) / (u32::MAX as f64 + 1.0);
    let chi_square = 2.0 * (-libm::log(u1) - libm::log(u2));
    z * libm::pow(nu / chi_square.max(1e-12), 0.5)
}

/// A ground-truth Look with smooth, non-trivial T/N/G/R/Λ and zero residual.
pub fn truth_look(seed: u64) -> Look {
    let mut rng = ChaCha12Rng::seed_from_u64(seed ^ 0x7a11_0c0f_fe11_0d00);
    let base = Tone::colorimetric(&looklab_model::basis::TONE_KNOTS).unwrap_or(Tone::Colorimetric);
    let (c0, beta) = match base {
        Tone::Spline { c0, beta } => (c0, beta),
        Tone::Colorimetric => (0.0, [1.0; TONE_COEFS]),
    };
    let mut shaped = beta;
    for (k, value) in shaped.iter_mut().enumerate() {
        let phase = k as f64 / TONE_COEFS as f64;
        *value = (*value * (1.0 + 0.15 * libm::sin(2.0 * core::f64::consts::PI * phase))).max(1e-6);
    }
    let tone = Tone::spline(c0 + 0.5 * gaussian(&mut rng), shaped).unwrap_or(base);

    let mut tint = Tint::zeros();
    for m in 0..TINT_COEFS {
        let j = tint_knot(m);
        tint.a[m] = 0.6 * libm::sin(core::f64::consts::PI * j / 100.0);
        tint.b[m] = -0.5 * libm::cos(core::f64::consts::PI * j / 100.0 + 0.4);
    }

    let mut color = ColorLayer::zeros();
    for h in 0..HUE_COEFS {
        let hue = 15.0 * h as f64;
        for s_index in 0..S_COEFS {
            let s = s_knot(s_index);
            for c in 0..4 {
                let chroma = c_knot(c);
                let phase = 2.0 * core::f64::consts::PI * hue / 360.0;
                let ramp = 0.25 + 0.75 * libm::exp(-(s + 1.0) * (s + 1.0) * 0.08);
                let gain = 0.12 * libm::cos(2.0 * phase + 0.7) * ramp;
                let rotation = 3.0 * libm::sin(phase + 1.1);
                let lightness = 1.4 * libm::cos(phase) * (0.4 + 0.6 * chroma / C_MAX);
                if c == 0 {
                    color.g.plane[s_index] = 0.0;
                    color.r.plane[s_index] = 0.0;
                    color.lambda.plane[s_index] = 0.0;
                } else {
                    let node = (h * S_COEFS + s_index) * 3 + (c - 1);
                    color.g.volume[node] = gain * (chroma / C_MAX).sqrt();
                    color.r.volume[node] = rotation;
                    color.lambda.volume[node] = lightness;
                }
            }
        }
    }
    for s_index in 0..S_COEFS {
        let s = s_knot(s_index);
        color.g.plane[s_index] = -0.05 + 0.01 * s;
        color.lambda.plane[s_index] = 0.4 * libm::sin(s * 0.4);
    }

    Look {
        tone,
        tint,
        color,
        residual: Residual {
            values: vec![0.0; RESIDUAL_COEFS],
        },
        clip: ClipSettings {
            function: looklab_model::ClipFunction::TanhSoftV1,
            k: 0.03,
        },
    }
}

fn tint_knot(index: usize) -> f64 {
    let knots = &TINT_KNOTS;
    (knots[index + 1] + knots[index + 2] + knots[index + 3]) / 3.0
}

fn s_knot(index: usize) -> f64 {
    let knots = &S_KNOTS;
    (knots[index + 1] + knots[index + 2] + knots[index + 3]) / 3.0
}

fn c_knot(index: usize) -> f64 {
    C_MAX * (index as f64 / 3.0) * (index as f64 / 3.0)
}

/// Varies only the tone layer, for the cross-layer leakage test.
pub fn tone_only_variant(look: &Look) -> Look {
    let mut out = look.clone();
    if let Tone::Spline { c0, beta } = look.tone {
        let mut shaped = beta;
        for (k, value) in shaped.iter_mut().enumerate() {
            *value *= 1.0 + 0.04 * libm::sin(k as f64 * 0.7);
        }
        if let Ok(tone) = Tone::spline(c0 + 1.2, shaped) {
            out.tone = tone;
        }
    }
    out
}

/// Moves every layer a little, for the mislabelled-image test.
pub fn perturbed_look(look: &Look) -> Look {
    let mut out = look.clone();
    for value in &mut out.tint.a {
        *value += 2.5;
    }
    for value in &mut out.color.g.volume {
        *value += 0.12;
    }
    for value in &mut out.color.r.volume {
        *value += 6.0;
    }
    out
}

fn d65() -> Xyz {
    Xyz::new(0.950_455_927_051_671_6, 1.0, 1.089_057_750_759_878_4)
}

fn camera_matrix(rng: &mut ChaCha12Rng) -> Mat3 {
    let base = RgbSpace::Srgb.to_xyz_matrix();
    let mut perturbed = [[0.0_f64; 3]; 3];
    for row in 0..3 {
        for column in 0..3 {
            perturbed[row][column] = base.0[row][column] * (1.0 + 0.01 * gaussian(rng));
        }
    }
    let mut matrix = Mat3(perturbed);
    let ones = matrix.mul_vec([1.0, 1.0, 1.0]);
    let white = d65();
    let scale = [white.x / ones[0], white.y / ones[1], white.z / ones[2]];
    for row in 0..3 {
        for column in 0..3 {
            matrix.0[row][column] *= scale[row];
        }
    }
    matrix
}

fn scene_color(rng: &mut ChaCha12Rng, neutral: bool) -> Xyz {
    let white = d65();
    if neutral {
        let lift = 1.0 + 0.02 * gaussian(rng);
        let y = 0.18 * libm::pow(2.0, -4.0 + 6.0 * rng.next_u32() as f64 / u32::MAX as f64);
        return Xyz::new(white.x * lift, 1.0, white.z * lift).scale(y);
    }
    let rgb = Rgb::new(
        rng.next_u32() as f64 / u32::MAX as f64,
        rng.next_u32() as f64 / u32::MAX as f64,
        rng.next_u32() as f64 / u32::MAX as f64,
    );
    let xyz = RgbSpace::Srgb.rgb_linear_to_xyz(rgb);
    let s = -6.0 + 9.0 * rng.next_u32() as f64 / u32::MAX as f64;
    let target = 0.18 * libm::pow(2.0, s);
    let scale = if xyz.y > 1e-9 { target / xyz.y } else { 1.0 };
    xyz.scale(scale)
}

fn header(n: usize, space: RgbSpace) -> SampleHeader {
    SampleHeader {
        n: n as u32,
        input_space: InputSpace::CameraRgbWbLinear,
        output_space: match space {
            RgbSpace::AdobeRgb => OutputSpace::AdobeRgb,
            _ => OutputSpace::Srgb,
        },
        wb_multipliers: [1.0, 1.0, 1.0],
        cct_hint_k: 0.0,
        exposure_hint_ev: 0.0,
        rerender_group: None,
        prep_version: PrepVersion::new(0),
        prior_transform_version: PriorTransformVersion::new(0),
        prep_params_hash: PrepParamsHash::new([0; 32]),
    }
}

fn build_scenes(config: &SynthConfig) -> Vec<Scene> {
    let mut rng = ChaCha12Rng::seed_from_u64(config.seed);
    let mut scenes = Vec::with_capacity(config.images);
    for _ in 0..config.images {
        let transform = camera_matrix(&mut rng);
        let inverse = transform.inverse().unwrap_or(Mat3::IDENTITY);
        let exposure = config.exposure_sd * gaussian(&mut rng);
        let wb_log_r = config.wb_sd * gaussian(&mut rng);
        let wb_log_b = config.wb_sd * gaussian(&mut rng);
        let hint = exposure + config.hint_sd * gaussian(&mut rng);
        let mut xyz = Vec::with_capacity(config.samples_per_image);
        let mut rgb = Vec::with_capacity(config.samples_per_image);
        let gain = libm::pow(2.0, exposure);
        for index in 0..config.samples_per_image {
            let neutral = index % 4 == 0;
            let colour = scene_color(&mut rng, neutral);
            let camera_xyz = inverse.mul_vec([colour.x, colour.y, colour.z]);
            let camera = [
                camera_xyz[0] / (gain * libm::exp(wb_log_r)),
                camera_xyz[1] / gain,
                camera_xyz[2] / (gain * libm::exp(wb_log_b)),
            ];
            xyz.push(colour);
            rgb.push(camera.map(|value| value.clamp(-0.4, 32.0)));
        }
        scenes.push(Scene {
            xyz,
            rgb,
            transform,
            nuisance: Nuisance {
                exposure,
                wb_log_r,
                wb_log_b,
                hint,
            },
        });
    }
    scenes
}

pub enum LookSelector<'a> {
    Single(&'a Look),
    Replace { image: usize, look: &'a Look },
}

fn render(
    config: &SynthConfig,
    scenes: &[Scene],
    truth: &Look,
    selector: &LookSelector<'_>,
) -> SyntheticDataset {
    let mut rng = ChaCha12Rng::seed_from_u64(config.seed ^ 0x0bad_c0de_1234_5678);
    let mut images = Vec::with_capacity(config.images);
    let mut nuisances = Vec::with_capacity(config.images);
    let mut transforms = Vec::with_capacity(config.images);
    for (index, scene) in scenes.iter().enumerate() {
        let look = match selector {
            LookSelector::Single(look) => *look,
            LookSelector::Replace {
                image,
                look: replacement,
            } => {
                if *image == index {
                    *replacement
                } else {
                    truth
                }
            }
        };
        let n = scene.xyz.len();
        let mut in_rgb = Vec::with_capacity(n);
        let mut out_rgb = Vec::with_capacity(n);
        let mut density = Vec::with_capacity(n);
        let mut flags = Vec::with_capacity(n);
        for (position, colour) in scene.xyz.iter().enumerate() {
            let clean = look.eval(*colour);
            let jitter = [
                student_t(&mut rng, config.nu) * config.noise_ucs,
                student_t(&mut rng, config.nu) * config.noise_ucs,
                student_t(&mut rng, config.nu) * config.noise_ucs,
            ];
            let noisy = looklab_color::ucs::Ucs::new(
                clean.j + jitter[0],
                clean.a + jitter[1],
                clean.b + jitter[2],
            );
            let jmh = ucs_to_jmh(noisy, UcsSpace::Ucs);
            let xyz = reference_model().jmh_to_xyz(jmh);
            let linear = config.space.xyz_to_rgb_linear(xyz);
            let clipped = Rgb::new(
                soft_clip(linear.r, config.clip_k),
                soft_clip(linear.g, config.clip_k),
                soft_clip(linear.b, config.clip_k),
            );
            in_rgb.push(scene.rgb[position].map(|value| value as f32));
            out_rgb.push(clipped.to_array().map(|value| value as f32));
            density.push(1.0_f32);
            flags.push(SampleFlags::NONE);
        }
        let samples = SampleSet::new(header(n, config.space), in_rgb, out_rgb, density, flags)
            .unwrap_or_else(|_| unreachable!("generated samples are in range"));
        let mut image = FitImage::new(samples, scene.transform);
        image.exposure_hint = scene.nuisance.hint;
        image.exposure_mode = ExposureModeId(0);
        images.push(image);
        nuisances.push(scene.nuisance);
        transforms.push(scene.transform);
    }
    SyntheticDataset {
        images,
        truth: truth.clone(),
        clip_k: config.clip_k,
        nuisances,
        transforms,
        points: scenes.iter().map(|scene| scene.xyz.clone()).collect(),
    }
}

pub fn dataset(config: &SynthConfig) -> SyntheticDataset {
    let scenes = build_scenes(config);
    let truth = truth_look(config.seed);
    render(config, &scenes, &truth, &LookSelector::Single(&truth))
}

pub fn dataset_with(
    config: &SynthConfig,
    truth: &Look,
    selector: &LookSelector<'_>,
) -> SyntheticDataset {
    let scenes = build_scenes(config);
    render(config, &scenes, truth, selector)
}

use looklab_model::realization::{ControlResponseModel, ControlValue, Delta, LayerKind, Settings};
use looklab_schema::{ControlSchema, SimKey};

#[derive(Clone, Debug)]
pub struct ControlSynthConfig {
    pub base: SynthConfig,
    pub schema: ControlSchema,
    pub epoch: u32,
    pub base_sim: SimKey,
    pub settings: Vec<Settings>,
    /// `None` gives the image its own scene.
    pub groups: Vec<Option<[u8; 16]>>,
    pub response_scale: f64,
    pub truth_seed: u64,
}

pub struct ControlledDataset {
    pub images: Vec<FitImage>,
    pub truth_look: Look,
    pub truth_model: ControlResponseModel,
    pub schema: ControlSchema,
    pub settings: Vec<Settings>,
    pub groups: Vec<Option<[u8; 16]>>,
    pub clip_k: f64,
}

/// Per-control coefficients as a smooth function of the value index.
pub fn random_control_model(
    schema: &ControlSchema,
    epoch: u32,
    base_sim: &SimKey,
    seed: u64,
    scale: f64,
) -> ControlResponseModel {
    let mut rng = ChaCha12Rng::seed_from_u64(seed ^ 0xc0_11_ec_7e_d0_0d);
    let mut model = ControlResponseModel::zeroed(schema, epoch, base_sim, 1)
        .expect("synthetic schemas are validated before generation");
    for control in &mut model.controls {
        let count = control.values.len().max(1);
        let layers = control.layers();
        for (vi, value) in control.values.iter_mut().enumerate() {
            let t = if count > 1 {
                vi as f64 / (count - 1) as f64 - 0.5
            } else {
                0.0
            };
            for layer in &layers {
                let layer_scale = match layer {
                    LayerKind::Tone => scale,
                    LayerKind::Tint => 1.5 * scale,
                    _ => 0.4 * scale,
                };
                let a = gaussian(&mut rng) / 3.0;
                let b = gaussian(&mut rng) / 3.0;
                let c = gaussian(&mut rng) / 6.0;
                let mut delta = Delta::default();
                let features = match layer {
                    LayerKind::Tone | LayerKind::Tint => layer.feature_count(),
                    _ => 6,
                };
                for feature in 0..features {
                    let jitter = 0.2 * gaussian(&mut rng);
                    delta.add(
                        *layer,
                        feature,
                        layer_scale * (a + b * t + c * t * t + jitter),
                    );
                }
                value
                    .delta
                    .layers
                    .push(match delta.layers.into_iter().next() {
                        Some(entry) => entry,
                        None => continue,
                    });
            }
        }
    }
    for control in &mut model.wb.controls {
        for value in &mut control.values {
            let offset = match value.value {
                ControlValue::Step(step) => step as f64,
                _ => 0.0,
            };
            value.log_gain = [
                0.008 * offset + 0.002 * gaussian(&mut rng),
                0.008 * offset + 0.002 * gaussian(&mut rng),
            ];
        }
    }
    for (index, mode) in model.exposure.modes.iter_mut().enumerate() {
        mode.ev = if index == 0 {
            0.0
        } else {
            0.05 * gaussian(&mut rng)
        };
    }
    model
}

pub fn localized_hue_model(
    schema: &ControlSchema,
    epoch: u32,
    base_sim: &SimKey,
    feature: u32,
    strength: f64,
) -> ControlResponseModel {
    let mut model = ControlResponseModel::zeroed(schema, epoch, base_sim, 1)
        .expect("synthetic schemas are validated before generation");
    for control in &mut model.controls {
        if control.key.as_str() != "color_chrome_fx_blue" {
            continue;
        }
        for value in &mut control.values {
            let amount = match &value.value {
                ControlValue::Enum(name) if name.as_str() == "strong" => strength,
                ControlValue::Enum(name) if name.as_str() == "weak" => 0.5 * strength,
                _ => 0.0,
            };
            let mut delta = Delta::default();
            delta.add(LayerKind::Chroma, feature, amount);
            value.delta.layers = delta.layers;
        }
    }
    model
}

/// Random settings over a model's declared non-default values.
pub fn random_settings(
    model: &ControlResponseModel,
    rng: &mut ChaCha12Rng,
    non_default_fraction: f64,
) -> Settings {
    let mut settings = Settings::new();
    for control in &model.controls {
        let draw = rng.next_u32() as f64 / (u32::MAX as f64);
        if control.values.is_empty() || draw > non_default_fraction {
            continue;
        }
        let index = (rng.next_u32() as usize) % control.values.len();
        settings.set(control.key.clone(), control.values[index].value.clone());
    }
    for control in &model.wb.controls {
        if control.values.is_empty() {
            continue;
        }
        let index = (rng.next_u32() as usize) % control.values.len();
        settings.set(control.key.clone(), control.values[index].value.clone());
    }
    for step in &model.exposure.steps {
        let draw = rng.next_u32() as f64 / (u32::MAX as f64);
        if draw > non_default_fraction {
            continue;
        }
        let shift = (rng.next_u32() as i64 % 7) - 3;
        settings.set(step.key.clone(), ControlValue::Step(step.default + shift));
    }
    settings
}

pub fn controlled_dataset(config: &ControlSynthConfig) -> ControlledDataset {
    let truth_model = random_control_model(
        &config.schema,
        config.epoch,
        &config.base_sim,
        config.truth_seed,
        config.response_scale,
    );
    controlled_dataset_with(config, truth_model)
}

pub fn controlled_dataset_with(
    config: &ControlSynthConfig,
    truth_model: ControlResponseModel,
) -> ControlledDataset {
    let n = config.settings.len();
    let mut base = config.base;
    base.images = n;
    let mut scenes = build_scenes(&base);
    for index in 0..n {
        let Some(group) = config.groups.get(index).copied().flatten() else {
            continue;
        };
        if let Some(first) =
            (0..index).find(|prior| config.groups.get(*prior).copied().flatten() == Some(group))
        {
            scenes[index] = scenes[first].clone();
        }
    }
    let truth_look = truth_look(config.truth_seed);
    let mut rng = ChaCha12Rng::seed_from_u64(config.truth_seed ^ 0x5a_11_ed_00_00_00_00_01);
    let mut images = Vec::with_capacity(n);
    for (index, scene) in scenes.iter().enumerate() {
        let settings = &config.settings[index];
        let realization = looklab_model::realization::realize(&truth_look, &truth_model, settings)
            .expect("generated model realizes every declared setting");
        let group = config.groups.get(index).copied().flatten();
        let mode = mode_id(&truth_model, settings);
        let exposure_scale = libm::pow(2.0, realization.exposure_ev);
        let gain = realization.wb_gain;
        let mut in_rgb = Vec::with_capacity(scene.xyz.len());
        let mut out_rgb = Vec::with_capacity(scene.xyz.len());
        let mut density = Vec::with_capacity(scene.xyz.len());
        let mut flags = Vec::with_capacity(scene.xyz.len());
        for rgb in &scene.rgb {
            let cam = [
                rgb[0] * gain[0] * exposure_scale,
                rgb[1] * gain[1] * exposure_scale,
                rgb[2] * gain[2] * exposure_scale,
            ];
            let xyz = Xyz::from_array(scene.transform.mul_vec(cam));
            let clean = realization.look.eval(xyz);
            let jitter = [
                student_t(&mut rng, config.base.nu) * config.base.noise_ucs,
                student_t(&mut rng, config.base.nu) * config.base.noise_ucs,
                student_t(&mut rng, config.base.nu) * config.base.noise_ucs,
            ];
            let noisy = looklab_color::ucs::Ucs::new(
                clean.j + jitter[0],
                clean.a + jitter[1],
                clean.b + jitter[2],
            );
            let xyz = reference_model().jmh_to_xyz(ucs_to_jmh(noisy, UcsSpace::Ucs));
            let linear = config.base.space.xyz_to_rgb_linear(xyz);
            let clipped = Rgb::new(
                soft_clip(linear.r, config.base.clip_k),
                soft_clip(linear.g, config.base.clip_k),
                soft_clip(linear.b, config.base.clip_k),
            );
            in_rgb.push(rgb.map(|value| value as f32));
            out_rgb.push(clipped.to_array().map(|value| value as f32));
            density.push(1.0_f32);
            flags.push(SampleFlags::NONE);
        }
        let mut header = header(scene.xyz.len(), config.base.space);
        header.rerender_group = group.map(looklab_samples::RerenderGroupId::new);
        let samples = SampleSet::new(header, in_rgb, out_rgb, density, flags)
            .unwrap_or_else(|_| unreachable!("generated samples are in range"));
        let mut image = FitImage::new(samples, scene.transform);
        image.exposure_hint = scene.nuisance.hint;
        image.exposure_mode = mode;
        image.settings = settings.clone();
        images.push(image);
    }
    ControlledDataset {
        images,
        truth_look,
        truth_model,
        schema: config.schema.clone(),
        settings: config.settings.clone(),
        groups: config.groups.clone(),
        clip_k: config.base.clip_k,
    }
}

pub fn mode_id(model: &ControlResponseModel, settings: &Settings) -> ExposureModeId {
    model
        .exposure
        .modes
        .iter()
        .position(|mode| settings.get(&mode.control) == Some(&mode.value))
        .map(|index| ExposureModeId(index as u32))
        .unwrap_or_default()
}
