// SPDX-License-Identifier: AGPL-3.0-or-later
//! Behaviour of the structured held-out mean-response model error.

#![cfg(feature = "synth")]
#![allow(clippy::disallowed_types)]

use looklab_body::BodyId;
use looklab_color::ucs::{Ucs, UcsSpace, ucs_to_jmh};
use looklab_color::{Rgb, RgbSpace, Xyz};
use looklab_fit::hierarchical::BodyGroup;
use looklab_fit::model_error::{
    MODEL_C_BINS, MODEL_HUE_BINS, MODEL_S_BINS, cell, post_index, region,
};
use looklab_fit::synth::{
    self, ControlSynthConfig, LookSelector, SynthConfig, localized_hue_model,
};
use looklab_fit::{
    ControlResponseModel, ControlValue, Density, FitError, FitImage, FitOptions, FitResult, Look,
    ModelErrorField, ProbeEstimate, ProbeUncertainty, Settings, apply_model_error,
    estimate_model_error, fit,
};
use looklab_model::basis::C_MAX;
use looklab_model::probes::v1 as probes_v1;
use looklab_model::solve::{reference_model, ucs_of_xyz};
use looklab_model::{LookInput, soft_clip};
use looklab_samples::{RerenderGroupId, SampleSet};
use looklab_schema::{ControlKey, EnumValue, SimKey, validate_control_schema};

const SAMPLES: usize = 1600;

fn options(seed: u64) -> FitOptions {
    FitOptions {
        seed,
        max_samples_per_image: SAMPLES,
        hessian_samples: 5000,
        max_sweeps: 64,
        sweep_budget: 4_000_000.0,
        early_samples_per_image: 200,
        ..FitOptions::default()
    }
}

fn light_options(seed: u64) -> FitOptions {
    FitOptions {
        seed,
        max_samples_per_image: 400,
        hessian_samples: 1600,
        max_sweeps: 48,
        sweep_budget: 1_500_000.0,
        early_samples_per_image: 80,
        ..FitOptions::default()
    }
}

#[allow(clippy::too_many_arguments)]
fn add_body(
    images: &mut Vec<FitImage>,
    groups: &mut Vec<BodyGroup>,
    body_of: &mut Vec<usize>,
    truth: &Look,
    seed: u64,
    count: usize,
    noise_ucs: f64,
    exposure_sd: f64,
    wb_sd: f64,
    label: usize,
) {
    let config = SynthConfig {
        seed,
        images: count,
        samples_per_image: SAMPLES,
        noise_ucs,
        exposure_sd,
        wb_sd,
        ..SynthConfig::default()
    };
    let data = synth::dataset_with(&config, truth, &LookSelector::Single(truth));
    let start = images.len();
    for image in data.images {
        body_of.push(label);
        images.push(image);
    }
    groups.push(BodyGroup {
        body: BodyId::new(format!("body-{label}")).expect("body id"),
        images: (start..images.len()).collect(),
    });
}

fn same_body(truth: &Look, seed: u64, count: usize, noise_ucs: f64) -> Vec<FitImage> {
    let config = SynthConfig {
        seed,
        images: count,
        samples_per_image: SAMPLES,
        noise_ucs,
        exposure_sd: 0.25,
        wb_sd: 0.015,
        ..SynthConfig::default()
    };
    synth::dataset_with(&config, truth, &LookSelector::Single(truth)).images
}

fn model_error(images: &[FitImage], groups: &[BodyGroup], options: &FitOptions) -> ModelErrorField {
    estimate_model_error(images, groups, None, options, 0.03).expect("model error")
}

fn neutral_density() -> Density {
    Density {
        s_bins: 1,
        hue_bins: 1,
        c_bins: 1,
        counts: vec![1.0],
        total: 1.0,
        floor: 0.0,
    }
}

fn neutral_probes() -> ProbeUncertainty {
    let v1 = probes_v1();
    let probes = v1
        .probes
        .iter()
        .enumerate()
        .map(|(index, _)| ProbeEstimate {
            id_hint: index,
            ucs: Ucs::new(50.0, 0.0, 0.0),
            covariance: [[0.0; 3]; 3],
            sigma_de: 0.0,
            post_ucs: Ucs::new(50.0, 0.0, 0.0),
            post_covariance: [[0.0; 3]; 3],
            post_sigma_de: 0.0,
            bias_de: 0.0,
            pre_clip_reliable: false,
            gamut_edge: false,
            density: 1.0,
            supported_sigma: false,
            supported_density: false,
            supported_clusters: false,
            supported_bias: false,
            supported_evidence: false,
            supported: false,
            clusters: 10,
        })
        .collect();
    ProbeUncertainty {
        probes,
        ids: v1.probes.iter().map(|probe| probe.id.clone()).collect(),
        supported_fraction: 0.0,
    }
}

fn support_options() -> FitOptions {
    FitOptions {
        density_floor: 0.0,
        ..FitOptions::default()
    }
}

fn moment_trace(moment: [[f64; 3]; 3]) -> f64 {
    moment[0][0] + moment[1][1] + moment[2][2]
}

fn populated_average(field: &ModelErrorField) -> f64 {
    let mut sum = 0.0;
    let mut count = 0usize;
    for (moment, rms) in field.post.iter().zip(field.held_out_mean_rms.iter()) {
        if *rms > 0.0 {
            sum += moment_trace(*moment);
            count += 1;
        }
    }
    sum / count.max(1) as f64
}

fn peak_variance(field: &ModelErrorField) -> f64 {
    field
        .post
        .iter()
        .zip(field.repeats.iter())
        .filter(|(_, repeats)| **repeats > 1)
        .map(|(moment, _)| moment_trace(*moment))
        .fold(0.0_f64, f64::max)
}

fn fixed_average(
    small: &ModelErrorField,
    large: &ModelErrorField,
    min_bodies: usize,
    min_repeats: usize,
) -> (f64, f64, usize) {
    let mut small_sum = 0.0;
    let mut large_sum = 0.0;
    let mut count = 0usize;
    for probe in probes_v1().probes {
        let input = LookInput::new(probe.xyz);
        let index = post_index(&input, RgbSpace::Srgb);
        if small.bodies[index] < min_bodies || small.repeats[index] < min_repeats {
            continue;
        }
        small_sum += small.radial(&input, RgbSpace::Srgb);
        large_sum += large.radial(&input, RgbSpace::Srgb);
        count += 1;
    }
    (
        small_sum / count.max(1) as f64,
        large_sum / count.max(1) as f64,
        count,
    )
}

#[test]
fn more_independent_bodies_reduce_mean_response_uncertainty() {
    let truth = synth::truth_look(7);
    let mut small_images = Vec::new();
    let mut small_groups = Vec::new();
    let mut small_body = Vec::new();
    for body in 0..4 {
        add_body(
            &mut small_images,
            &mut small_groups,
            &mut small_body,
            &truth,
            200 + body as u64,
            3,
            0.8,
            0.25,
            0.015,
            body,
        );
    }
    let mut large_images = Vec::new();
    let mut large_groups = Vec::new();
    let mut large_body = Vec::new();
    for body in 0..8 {
        add_body(
            &mut large_images,
            &mut large_groups,
            &mut large_body,
            &truth,
            200 + body as u64,
            3,
            0.8,
            0.25,
            0.015,
            body,
        );
    }
    let small = model_error(&small_images, &small_groups, &options(3));
    let large = model_error(&large_images, &large_groups, &options(3));
    let (small_average, large_average, count) = fixed_average(&small, &large, 2, 3);
    assert!(count >= 20, "only {count} adequately measured probe cells");
    assert!(
        large_average < small_average,
        "mean-response uncertainty did not fall: 4 bodies {small_average}, 8 bodies {large_average}"
    );
    assert!(
        large_average < 1.0,
        "model error sits on the observation-noise floor: {large_average}"
    );
}

#[test]
fn same_body_independent_images_reduce_mean_response_uncertainty() {
    let truth = synth::truth_look(17);
    let images = same_body(&truth, 250, 14, 0.8);
    let small = model_error(&images[..6], &[], &options(7));
    let large = model_error(&images, &[], &options(7));
    let (small_average, large_average, count) = fixed_average(&small, &large, 1, 3);
    assert!(count >= 20, "only {count} adequately measured probe cells");
    assert!(
        large_average < small_average,
        "same-body image growth did not reduce uncertainty: 6 images {small_average}, 14 images {large_average}"
    );
    assert!(
        large_average < 1.0,
        "same-body model error sits on the noise floor: {large_average}"
    );
}

fn amplified(seed: u64) -> Look {
    let mut look = synth::truth_look(seed);
    for value in &mut look.color.g.plane {
        *value *= 5.0;
    }
    for value in &mut look.color.g.volume {
        *value *= 5.0;
    }
    for value in &mut look.color.r.plane {
        *value *= 4.0;
    }
    for value in &mut look.color.r.volume {
        *value *= 4.0;
    }
    for value in &mut look.color.lambda.plane {
        *value *= 5.0;
    }
    for value in &mut look.color.lambda.volume {
        *value *= 5.0;
    }
    look
}

fn max_sigma_v(result: &FitResult) -> f64 {
    result
        .diagnostics
        .sigma_v
        .iter()
        .map(|(_, value)| *value)
        .fold(0.0_f64, f64::max)
}

#[test]
fn coherent_underfit_survives_growing_sigma_v() {
    let truth = amplified(31);
    let mut quiet_images = Vec::new();
    let mut quiet_groups = Vec::new();
    let mut quiet_body = Vec::new();
    let mut loud_images = Vec::new();
    let mut loud_groups = Vec::new();
    let mut loud_body = Vec::new();
    for index in 0..5 {
        add_body(
            &mut quiet_images,
            &mut quiet_groups,
            &mut quiet_body,
            &truth,
            300 + index as u64,
            2,
            0.2,
            0.25,
            0.015,
            index,
        );
        add_body(
            &mut loud_images,
            &mut loud_groups,
            &mut loud_body,
            &truth,
            300 + index as u64,
            2,
            1.6,
            0.25,
            0.015,
            index,
        );
    }
    let underfit = FitOptions {
        identity_scale: 20.0,
        ..options(11)
    };
    let diagnostic = FitOptions {
        identity_scale: 0.0,
        ..options(11)
    };
    let quiet_sigma = max_sigma_v(&fit(&quiet_images, &diagnostic).expect("quiet fit"));
    let loud_sigma = max_sigma_v(&fit(&loud_images, &diagnostic).expect("loud fit"));
    assert!(
        loud_sigma > quiet_sigma,
        "sigma_v did not grow: quiet {quiet_sigma}, loud {loud_sigma}"
    );
    let quiet = model_error(&quiet_images, &quiet_groups, &underfit);
    let loud = model_error(&loud_images, &loud_groups, &underfit);
    let quiet_peak = peak_variance(&quiet);
    let loud_peak = peak_variance(&loud);
    assert!(
        quiet_peak > 0.05,
        "coherent underfit not detected: {quiet_peak}"
    );
    assert!(
        loud_peak > 0.05,
        "sigma_v growth erased coherent underfit: {loud_peak}"
    );
    assert!(
        loud_peak > 0.25 * quiet_peak,
        "sigma_v growth collapsed the discrepancy: {quiet_peak} -> {loud_peak}"
    );
}

fn rotated(seed: u64, degrees: f64) -> Look {
    let mut look = synth::truth_look(seed);
    for value in &mut look.color.r.plane {
        *value += degrees;
    }
    for value in &mut look.color.r.volume {
        *value += 0.5 * degrees;
    }
    look
}

#[test]
fn opposite_body_bias_does_not_dilute_with_more_bodies() {
    let plus = rotated(41, 9.0);
    let minus = rotated(41, -9.0);
    let small = alternating(&plus, &minus, 8);
    let large = alternating(&plus, &minus, 12);
    let small = peak_variance(&model_error(&small.0, &small.1, &light_options(13)));
    let large = peak_variance(&model_error(&large.0, &large.1, &light_options(15)));
    assert!(
        large > 0.05,
        "opposite body bias vanished: small {small}, large {large}"
    );
    assert!(
        large > 0.4 * small,
        "opposite body bias diluted with more bodies: small {small}, large {large}"
    );
}

fn alternating(plus: &Look, minus: &Look, count: usize) -> (Vec<FitImage>, Vec<BodyGroup>) {
    let mut images = Vec::new();
    let mut groups = Vec::new();
    let mut body = Vec::new();
    for index in 0..count {
        let truth = if index % 2 == 0 { plus } else { minus };
        add_body(
            &mut images,
            &mut groups,
            &mut body,
            truth,
            900 + index as u64,
            2,
            0.4,
            0.2,
            0.01,
            index,
        );
    }
    (images, groups)
}

fn retain_regions(image: &mut FitImage, keep: impl Fn(usize) -> bool) {
    let kept: Vec<usize> = (0..image.samples.n())
        .filter(|index| {
            let input = sample_input(image, *index);
            keep(region(input.s, input.h_in))
        })
        .collect();
    if kept.is_empty() {
        return;
    }
    let mut header = image.samples.header().clone();
    header.n = kept.len() as u32;
    let in_rgb = kept
        .iter()
        .map(|index| image.samples.in_rgb()[*index])
        .collect();
    let out_rgb = kept
        .iter()
        .map(|index| image.samples.out_rgb()[*index])
        .collect();
    let density = kept
        .iter()
        .map(|index| image.samples.density_w()[*index])
        .collect();
    let flags = kept
        .iter()
        .map(|index| image.samples.flags()[*index])
        .collect();
    image.samples = SampleSet::new(header, in_rgb, out_rgb, density, flags).expect("samples");
}

fn retain_single_cell(image: &mut FitImage, target: usize) -> Option<usize> {
    let mut cell = None;
    let mut keep = Vec::new();
    for index in 0..image.samples.n() {
        let input = sample_input(image, index);
        if region(input.s, input.h_in) != target {
            keep.push(index);
            continue;
        }
        let candidate = chroma_cell(input.c_in);
        if cell.is_none() {
            cell = Some(candidate);
            keep.push(index);
        }
    }
    let cell = cell?;
    let mut header = image.samples.header().clone();
    header.n = keep.len() as u32;
    let in_rgb = keep
        .iter()
        .map(|index| image.samples.in_rgb()[*index])
        .collect();
    let out_rgb = keep
        .iter()
        .map(|index| image.samples.out_rgb()[*index])
        .collect();
    let density = keep
        .iter()
        .map(|index| image.samples.density_w()[*index])
        .collect();
    let flags = keep
        .iter()
        .map(|index| image.samples.flags()[*index])
        .collect();
    image.samples = SampleSet::new(header, in_rgb, out_rgb, density, flags).expect("samples");
    Some(cell)
}

fn sample_input(image: &FitImage, index: usize) -> LookInput {
    let rgb = image.samples.in_rgb()[index];
    let scale = libm::pow(2.0, image.exposure_hint.clamp(-8.0, 8.0));
    let xyz = image.transform.mul_vec([
        rgb[0] as f64 * scale,
        rgb[1] as f64 * scale,
        rgb[2] as f64 * scale,
    ]);
    LookInput::new(Xyz::from_array(xyz))
}

fn output_space(image: &FitImage) -> RgbSpace {
    match image.samples.header().output_space {
        looklab_samples::OutputSpace::AdobeRgb => RgbSpace::AdobeRgb,
        looklab_samples::OutputSpace::Srgb => RgbSpace::Srgb,
    }
}

fn chroma_cell(c: f64) -> usize {
    (((c / C_MAX) * 4.0).floor() as isize).clamp(0, 3) as usize
}

fn inject_balanced_chroma(images: &mut [FitImage], value: f64) -> Option<usize> {
    let bins = MODEL_HUE_BINS * MODEL_S_BINS;
    let mut best: Option<(usize, usize)> = None;
    for target in 0..bins {
        let mut total = 0usize;
        let mut usable = true;
        for image in images.iter() {
            let mut low = 0usize;
            let mut high = 0usize;
            for index in 0..image.samples.n() {
                let input = sample_input(image, index);
                if region(input.s, input.h_in) != target {
                    continue;
                }
                match chroma_cell(input.c_in) {
                    0 => low += 1,
                    1 => high += 1,
                    _ => {}
                }
            }
            let pairs = low.min(high);
            if pairs == 0 {
                usable = false;
                break;
            }
            total += pairs;
        }
        if usable && total > best.map_or(0, |(_, count)| count) {
            best = Some((target, total));
        }
    }
    let (target, _) = best?;
    for image in images.iter_mut() {
        let mut low = Vec::new();
        let mut high = Vec::new();
        for index in 0..image.samples.n() {
            let input = sample_input(image, index);
            if region(input.s, input.h_in) != target {
                continue;
            }
            match chroma_cell(input.c_in) {
                0 => low.push(index),
                1 => high.push(index),
                _ => {}
            }
        }
        let pairs = low.len().min(high.len());
        if pairs == 0 {
            continue;
        }
        let space = output_space(image);
        let mut out_rgb = image.samples.out_rgb().to_vec();
        for (indices, sign) in [(&low, 1.0), (&high, -1.0)] {
            for index in indices.iter().take(pairs) {
                let base = ucs_of_xyz(space.rgb_linear_to_xyz(Rgb::from_array(
                    out_rgb[*index].map(|channel| channel as f64),
                )));
                let shifted = Ucs::new(base.j, base.a + sign * value, base.b + sign * value);
                let xyz = reference_model().jmh_to_xyz(ucs_to_jmh(shifted, UcsSpace::Ucs));
                let linear = space.xyz_to_rgb_linear(xyz);
                out_rgb[*index] =
                    [linear.r, linear.g, linear.b].map(|channel| soft_clip(channel, 0.03) as f32);
            }
        }
        let header = image.samples.header().clone();
        let in_rgb = image.samples.in_rgb().to_vec();
        let density = image.samples.density_w().to_vec();
        let flags = image.samples.flags().to_vec();
        image.samples = SampleSet::new(header, in_rgb, out_rgb, density, flags).expect("samples");
    }
    Some(target)
}

#[test]
fn opposing_chroma_errors_inside_a_region_are_retained() {
    let truth = synth::truth_look(81);
    let data = synth::dataset_with(
        &SynthConfig {
            seed: 820,
            images: 4,
            samples_per_image: 1600,
            noise_ucs: 0.2,
            ..SynthConfig::default()
        },
        &truth,
        &LookSelector::Single(&truth),
    );
    let mut images = data.images;
    let target = inject_balanced_chroma(&mut images, 2.0).expect("balanced chroma cells");
    let underfit = FitOptions {
        identity_scale: 20.0,
        max_samples_per_image: 1600,
        hessian_samples: 4000,
        sweep_budget: 3_000_000.0,
        ..options(31)
    };
    let field = model_error(&images, &[], &underfit);
    let start = target * MODEL_C_BINS;
    let rms = (start..start + MODEL_C_BINS)
        .filter(|cell| field.repeats[*cell] > 0)
        .map(|cell| field.held_out_mean_rms[cell])
        .fold(0.0_f64, f64::max);
    let variance = (start..start + MODEL_C_BINS)
        .filter(|cell| field.repeats[*cell] > 0)
        .map(|cell| moment_trace(field.post[cell]))
        .fold(0.0_f64, f64::max);
    assert!(rms > 0.05, "local chroma error collapsed: {rms}");
    assert!(
        variance > 0.01,
        "local chroma variance collapsed: {variance}"
    );
}

#[test]
fn sparse_and_missing_regions_stay_conservative() {
    let truth = synth::truth_look(51);
    let mut images = Vec::new();
    let mut groups = Vec::new();
    let mut body = Vec::new();
    add_body(
        &mut images,
        &mut groups,
        &mut body,
        &truth,
        600,
        3,
        0.6,
        0.2,
        0.01,
        0,
    );
    for index in 1..4 {
        add_body(
            &mut images,
            &mut groups,
            &mut body,
            &truth,
            600 + index as u64,
            2,
            0.6,
            0.2,
            0.01,
            index,
        );
    }
    for (index, image) in images.iter_mut().enumerate() {
        let body = body[index];
        if body == 0 {
            retain_regions(image, |bin| bin / MODEL_S_BINS == 0);
        } else {
            retain_regions(image, |bin| (1..=5).contains(&(bin / MODEL_S_BINS)));
        }
    }
    let field = model_error(&images, &groups, &options(17));
    let single = region(0.0, 15.0) * MODEL_C_BINS;
    assert!(
        field.held_out_mean_rms[single..single + MODEL_C_BINS]
            .iter()
            .any(|value| *value > 0.0),
        "thin region produced no estimate"
    );
    assert!(
        field.post[single..single + MODEL_C_BINS]
            .iter()
            .any(|moment| moment_trace(*moment) > 0.0),
        "thin region produced zero variance"
    );
    let missing = (0..field.post.len())
        .find(|cell| field.held_out_mean_rms[*cell] == 0.0 && !field.evidence[*cell])
        .expect("an unobserved cell");
    assert!(
        moment_trace(field.post[missing]) > 0.0,
        "missing cell fell back to zero model error"
    );
}

#[test]
fn single_sample_cell_uses_global_fallback() {
    let truth = synth::truth_look(91);
    let mut body0 = synth::dataset_with(
        &SynthConfig {
            seed: 900,
            images: 1,
            samples_per_image: SAMPLES,
            noise_ucs: 0.5,
            ..SynthConfig::default()
        },
        &truth,
        &LookSelector::Single(&truth),
    )
    .images;
    let input = sample_input(&body0[0], 0);
    let target = region(input.s, input.h_in);
    retain_single_cell(&mut body0[0], target).expect("target cell");
    let mut body1 = synth::dataset_with(
        &SynthConfig {
            seed: 901,
            images: 3,
            samples_per_image: SAMPLES,
            noise_ucs: 0.5,
            ..SynthConfig::default()
        },
        &truth,
        &LookSelector::Single(&truth),
    )
    .images;
    for image in &mut body1 {
        retain_regions(image, |bin| bin != target);
    }
    let start = body0.len();
    let mut images = body0;
    images.extend(body1);
    let groups = vec![
        BodyGroup {
            body: BodyId::new("sparse").expect("body id"),
            images: (0..start).collect(),
        },
        BodyGroup {
            body: BodyId::new("rest").expect("body id"),
            images: (start..images.len()).collect(),
        },
    ];
    let field = model_error(&images, &groups, &options(33));
    let maximum = field
        .post
        .iter()
        .map(|moment| moment_trace(*moment))
        .fold(0.0_f64, f64::max);
    assert!(maximum > 0.0, "fixture produced no discrepancy");
    let start = target * MODEL_C_BINS;
    let thin = (start..start + MODEL_C_BINS)
        .find(|cell| field.held_out_mean_rms[*cell] > 0.0)
        .expect("target cell has a sample");
    assert!(
        !field.evidence[thin],
        "single-sample cell claimed independent evidence"
    );
    assert!(
        moment_trace(field.post[thin]) > 0.0,
        "single-sample cell lost the global fallback"
    );
}

fn retain_one_sample_in_cell(images: &mut [FitImage], target: usize) -> bool {
    let mut kept = false;
    for image in images {
        let mut keep = Vec::new();
        for index in 0..image.samples.n() {
            let input = sample_input(image, index);
            if cell(&input) == target {
                if !kept {
                    kept = true;
                    keep.push(index);
                }
            } else {
                keep.push(index);
            }
        }
        if keep.is_empty() {
            continue;
        }
        let mut header = image.samples.header().clone();
        header.n = keep.len() as u32;
        let in_rgb = keep
            .iter()
            .map(|index| image.samples.in_rgb()[*index])
            .collect();
        let out_rgb = keep
            .iter()
            .map(|index| image.samples.out_rgb()[*index])
            .collect();
        let density = keep
            .iter()
            .map(|index| image.samples.density_w()[*index])
            .collect();
        let flags = keep
            .iter()
            .map(|index| image.samples.flags()[*index])
            .collect();
        image.samples = SampleSet::new(header, in_rgb, out_rgb, density, flags).expect("samples");
    }
    kept
}

#[test]
fn estimated_field_marks_thin_cell_without_evidence_and_keeps_encodings_separate() {
    let seed = 121;
    let truth = Look::colorimetric_spline().expect("look");
    let mut images = synth::dataset_with(
        &SynthConfig {
            seed: 1210,
            images: 6,
            samples_per_image: SAMPLES,
            noise_ucs: 0.0,
            exposure_sd: 0.0,
            wb_sd: 0.0,
            hint_sd: 0.0,
            ..SynthConfig::default()
        },
        &truth,
        &LookSelector::Single(&truth),
    )
    .images;
    let target_probe = probes_v1()
        .probes
        .iter()
        .enumerate()
        .find(|(_, probe)| {
            let target = cell(&LookInput::new(probe.xyz));
            images.iter().any(|image| {
                (0..image.samples.n()).any(|index| cell(&sample_input(image, index)) == target)
            })
        })
        .map(|(index, _)| index)
        .expect("a probe cell with samples");
    let target_input = LookInput::new(probes_v1().probes[target_probe].xyz);
    let target_cell = cell(&target_input);
    assert!(
        retain_one_sample_in_cell(&mut images, target_cell),
        "target probe cell has a sample"
    );
    let field = model_error(&images, &[], &options(seed));
    assert!(
        !field.evidence_at(&target_input, RgbSpace::Srgb),
        "thin cell claimed independent evidence"
    );
    assert!(
        field.radial(&target_input, RgbSpace::Srgb) < 0.05,
        "thin cell discrepancy is not near zero"
    );
    let supported = probes_v1()
        .probes
        .iter()
        .enumerate()
        .find(|(index, probe)| {
            *index != target_probe && field.evidence_at(&LookInput::new(probe.xyz), RgbSpace::Srgb)
        })
        .map(|(index, _)| index)
        .expect("another sRGB cell has evidence");
    let options = support_options();
    let mut measured = neutral_probes();
    apply_model_error(
        &mut measured,
        &field,
        &truth,
        &neutral_density(),
        RgbSpace::Srgb,
        &options,
        0.03,
        None,
    );
    let target = &measured.probes[target_probe];
    assert!(target.post_sigma_de <= options.support_sigma);
    assert!(!target.supported_evidence);
    assert!(!target.supported);
    assert!(measured.probes[supported].supported_evidence);
    assert!(measured.probes[supported].supported);

    for space in [
        RgbSpace::AdobeRgb,
        RgbSpace::Rec2020,
        RgbSpace::DisplayP3,
        RgbSpace::ProPhoto,
    ] {
        let mut unmeasured = neutral_probes();
        apply_model_error(
            &mut unmeasured,
            &field,
            &truth,
            &neutral_density(),
            space,
            &options,
            0.03,
            None,
        );
        assert!(
            unmeasured
                .probes
                .iter()
                .all(|probe| !probe.supported_evidence),
            "{} inherited sRGB model evidence",
            space.name()
        );
        assert!(
            unmeasured.probes.iter().all(|probe| !probe.supported),
            "{} probes passed support without model evidence",
            space.name()
        );
        assert!(
            unmeasured
                .probes
                .iter()
                .all(|probe| probe.post_sigma_de <= options.support_sigma),
            "{} did not exercise the 1.5 sigma threshold",
            space.name()
        );
    }
}

#[test]
fn insufficient_evidence_is_a_typed_failure() {
    let data = synth::dataset(&SynthConfig {
        seed: 1,
        images: 1,
        samples_per_image: SAMPLES,
        ..SynthConfig::default()
    });
    let error = estimate_model_error(&data.images, &[], None, &options(19), 0.03)
        .expect_err("one image has no held-out fit");
    assert!(matches!(error, FitError::NoModelErrorEvidence));
    let error = estimate_model_error(&[], &[], None, &options(19), 0.03).expect_err("no images");
    assert!(matches!(error, FitError::NoImages));
}

fn copy_with_group(image: &FitImage, group: RerenderGroupId) -> FitImage {
    let mut header = image.samples.header().clone();
    header.rerender_group = Some(group);
    let samples = SampleSet::new(
        header,
        image.samples.in_rgb().to_vec(),
        image.samples.out_rgb().to_vec(),
        image.samples.density_w().to_vec(),
        image.samples.flags().to_vec(),
    )
    .expect("samples");
    let mut copy = image.clone();
    copy.samples = samples;
    copy
}

#[test]
fn copies_without_independent_units_are_a_typed_failure() {
    let truth = synth::truth_look(73);
    let data = synth::dataset_with(
        &SynthConfig {
            seed: 810,
            images: 1,
            samples_per_image: SAMPLES,
            ..SynthConfig::default()
        },
        &truth,
        &LookSelector::Single(&truth),
    );
    let group = RerenderGroupId::new([0; 16]);
    let images: Vec<FitImage> = (0..4)
        .map(|_| copy_with_group(&data.images[0], group))
        .collect();
    let error = estimate_model_error(&images, &[], None, &options(29), 0.03)
        .expect_err("rerender copies are one independent unit");
    assert!(matches!(error, FitError::NoModelErrorEvidence));
}

#[test]
fn image_copies_do_not_add_confidence() {
    let truth = synth::truth_look(71);
    let mut independent_images = Vec::new();
    let mut independent_groups = Vec::new();
    let mut copy_images = Vec::new();
    let mut copy_groups = Vec::new();
    for index in 0..8 {
        let config = SynthConfig {
            seed: 800 + index as u64,
            images: 1,
            samples_per_image: SAMPLES,
            noise_ucs: 0.6,
            exposure_sd: 0.2,
            wb_sd: 0.01,
            ..SynthConfig::default()
        };
        let data = synth::dataset_with(&config, &truth, &LookSelector::Single(&truth));
        let image = &data.images[0];
        let start = independent_images.len();
        independent_images.push(image.clone());
        independent_groups.push(BodyGroup {
            body: BodyId::new(format!("image-{index}")).expect("body id"),
            images: (start..independent_images.len()).collect(),
        });
        let group =
            RerenderGroupId::new([index as u8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        let start = copy_images.len();
        for _ in 0..4 {
            copy_images.push(copy_with_group(image, group));
        }
        copy_groups.push(BodyGroup {
            body: BodyId::new(format!("copy-{index}")).expect("body id"),
            images: (start..copy_images.len()).collect(),
        });
    }
    let independent = populated_average(&model_error(
        &independent_images,
        &independent_groups,
        &options(23),
    ));
    let copies = populated_average(&model_error(&copy_images, &copy_groups, &options(23)));
    assert!(
        copies >= 0.6 * independent,
        "image copies added pseudo-evidence: independent {independent}, copies {copies}"
    );
}

#[test]
fn distinct_same_raw_renders_are_preserved() {
    let truth = synth::truth_look(101);
    let variant = synth::perturbed_look(&truth);
    let config = SynthConfig {
        seed: 1000,
        images: 1,
        samples_per_image: SAMPLES,
        noise_ucs: 0.4,
        ..SynthConfig::default()
    };
    let a = synth::dataset_with(&config, &truth, &LookSelector::Single(&truth))
        .images
        .remove(0);
    let b = synth::dataset_with(&config, &variant, &LookSelector::Single(&variant))
        .images
        .remove(0);
    let group = RerenderGroupId::new([7; 16]);
    let a = copy_with_group(&a, group);
    let b = copy_with_group(&b, group);
    let other = synth::dataset_with(
        &SynthConfig {
            seed: 1001,
            images: 1,
            samples_per_image: SAMPLES,
            noise_ucs: 0.4,
            ..SynthConfig::default()
        },
        &truth,
        &LookSelector::Single(&truth),
    )
    .images
    .remove(0);
    let groups = || {
        vec![
            BodyGroup {
                body: BodyId::new("same-raw").expect("body id"),
                images: vec![0, 1],
            },
            BodyGroup {
                body: BodyId::new("other").expect("body id"),
                images: vec![2],
            },
        ]
    };
    let first = model_error(
        &[a.clone(), b.clone(), other.clone()],
        &groups(),
        &options(35),
    );
    let second = model_error(&[b, a, other], &groups(), &options(35));
    let difference = first
        .post
        .iter()
        .zip(second.post.iter())
        .flat_map(|(left, right)| {
            (0..3).flat_map(move |i| (0..3).map(move |j| (left[i][j] - right[i][j]).abs()))
        })
        .fold(0.0_f64, f64::max);
    assert!(
        difference < 1e-6,
        "same-RAW renders are order-dependent: {difference}"
    );
}

#[test]
fn controlled_responses_do_not_become_model_error() {
    let schema = validate_control_schema(include_str!("../../../data/bodies/fujifilm-x-t5.yaml"))
        .expect("schema");
    let sim = SimKey::new("classic_chrome").expect("sim");
    let key = ControlKey::new("color_chrome_fx_blue").expect("key");
    let setting = |name: &str| {
        let mut settings = Settings::new();
        settings.set(
            key.clone(),
            ControlValue::Enum(EnumValue::new(name).expect("enum value")),
        );
        settings
    };
    let weak = setting("weak");
    let strong = setting("strong");
    let settings = vec![
        Settings::new(),
        weak.clone(),
        strong.clone(),
        Settings::new(),
        weak.clone(),
        strong.clone(),
        weak,
        strong,
    ];
    let groups = vec![
        None,
        Some([1u8; 16]),
        Some([1u8; 16]),
        None,
        None,
        None,
        None,
        None,
    ];
    let config = ControlSynthConfig {
        base: SynthConfig {
            seed: 1100,
            images: settings.len(),
            samples_per_image: SAMPLES,
            noise_ucs: 0.4,
            ..SynthConfig::default()
        },
        schema: schema.clone(),
        epoch: 1,
        base_sim: sim.clone(),
        settings,
        groups,
        response_scale: 1.0,
        truth_seed: 1101,
    };
    let data =
        synth::controlled_dataset_with(&config, localized_hue_model(&schema, 1, &sim, 0, 2.0));
    let template = ControlResponseModel::zeroed(&schema, 1, &sim, 1).expect("template");
    let uncontrolled = peak_variance(
        &estimate_model_error(&data.images, &[], None, &options(39), data.clip_k)
            .expect("uncontrolled model error"),
    );
    let controlled = peak_variance(
        &estimate_model_error(
            &data.images,
            &[],
            Some(&template),
            &options(39),
            data.clip_k,
        )
        .expect("controlled model error"),
    );
    assert!(uncontrolled > 0.05, "fixture produced no discrepancy");
    assert!(
        controlled < 0.5 * uncontrolled,
        "known controlled response became model error: controlled {controlled}, uncontrolled {uncontrolled}"
    );
    let mut reordered = data.images.clone();
    reordered.swap(1, 2);
    let reordered = peak_variance(
        &estimate_model_error(&reordered, &[], Some(&template), &options(39), data.clip_k)
            .expect("reordered controlled model error"),
    );
    assert!(
        (reordered - controlled).abs() < 1e-4,
        "same-RAW settings changed with render order: {controlled} -> {reordered}"
    );
}

#[test]
fn held_nuisance_offsets_do_not_become_model_error() {
    let truth = synth::truth_look(61);
    let mut base_images = Vec::new();
    let mut base_groups = Vec::new();
    let mut base_body = Vec::new();
    let mut shifted_images = Vec::new();
    let mut shifted_groups = Vec::new();
    let mut shifted_body = Vec::new();
    for index in 0..6 {
        add_body(
            &mut base_images,
            &mut base_groups,
            &mut base_body,
            &truth,
            700 + index as u64,
            2,
            0.5,
            0.02,
            0.002,
            index,
        );
        add_body(
            &mut shifted_images,
            &mut shifted_groups,
            &mut shifted_body,
            &truth,
            700 + index as u64,
            2,
            0.5,
            0.35,
            0.05,
            index,
        );
    }
    let tolerant = FitOptions {
        exposure_prior_sd: 0.5,
        wb_prior_sd: 0.08,
        ..options(27)
    };
    let base = peak_variance(&model_error(&base_images, &base_groups, &tolerant));
    let shifted = peak_variance(&model_error(&shifted_images, &shifted_groups, &tolerant));
    assert!(
        shifted < 4.0 * base + 0.1,
        "held nuisance offsets became model error: base {base}, shifted {shifted}"
    );
}
