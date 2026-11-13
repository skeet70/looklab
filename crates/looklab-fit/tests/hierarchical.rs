// SPDX-License-Identifier: AGPL-3.0-or-later
//! Acceptance tests for the hierarchical fit, bootstrap uncertainty and the
//! firmware split path (docs/09 P4.2-P4.4).

#![cfg(feature = "synth")]
#![allow(clippy::disallowed_types)]
// The long recovery cases are compiled only for the pre-release tier.
#![cfg_attr(not(feature = "long-tests"), allow(dead_code, unused_imports))]

use looklab_body::Delta6;
use looklab_color::RgbSpace;
use looklab_color::ucs::delta_e_ucs;
use looklab_fit::bootstrap::{BootstrapOptions, published_uncertainty};
use looklab_fit::hierarchical::{
    BodyData, BodyPin, BodyResume, BodyWarmStart, FamilyData, FamilyMode, FirmwareId, HierInput,
    HierOptions, RealizationData, firmware_images, fit_hierarchical, fit_realizations,
};
use looklab_fit::synth::{ControlSynthConfig, MultiBodyConfig, SynthConfig, multi_body};
use looklab_fit::{ComponentGauge, ExposureOffset, ImageIntent, WbModeClass};
use looklab_fit::{
    FitError, FitOptions, ImageNuisances, SplitOptions, detect_split, project_images,
};
#[cfg(feature = "full-scale")]
use looklab_fit::{PROJECTION_DIM, SampleProjection};
use looklab_model::probes::v1 as probes_v1;
use looklab_model::{Look, Tone};
use looklab_schema::{
    BodyId, BodyRealizationId, ControlEpochId, FamilyTargetId, LookId, SimKey, TargetVersion,
};

fn fit_options() -> FitOptions {
    FitOptions {
        seed: 11,
        max_samples_per_image: 1200,
        hessian_samples: 6000,
        max_sweeps: 8,
        sweep_budget: 2_000_000.0,
        exposure_prior_sd: 0.04,
        exposure_mode_prior_sd: 0.2,
        wb_prior_sd: 0.008,
        ..FitOptions::default()
    }
}

fn hier_options() -> HierOptions {
    HierOptions {
        fit: fit_options(),
        outer_iterations: 4,
        body_iterations: 6,
        ..HierOptions::default()
    }
}

#[cfg(feature = "full-scale")]
fn config() -> MultiBodyConfig {
    MultiBodyConfig {
        seed: 0x51e2_0001,
        bodies: 5,
        images_per_body: 8,
        samples_per_image: 1200,
        look_families: 1,
        firmware: 1,
        exposure_modes: 1,
        wb_modes: 1,
        delta_sd: 0.0015,
        exposure_sd: 0.06,
        wb_sd: 0.004,
        ..MultiBodyConfig::default()
    }
}

fn body<'a>(data: &'a HierInput, name: &str) -> &'a BodyData {
    data.bodies
        .iter()
        .find(|body| body.id.as_str() == name)
        .expect("synthetic body exists")
}

fn family(index: u32) -> LookId {
    LookId::new(&format!("family-{index}")).expect("synthetic family id")
}

fn target(index: u32) -> FamilyTargetId {
    FamilyTargetId {
        family: family(index),
        version: TargetVersion::new(1).expect("one is a positive target version"),
    }
}

fn firmware(body: &BodyId, version: &str) -> FirmwareId {
    FirmwareId::new(body.clone(), ControlEpochId::new(1), version).expect("firmware identity")
}

fn free_input(bodies: Vec<BodyData>) -> HierInput {
    HierInput {
        families: vec![FamilyData {
            target: target(0),
            mode: FamilyMode::Fit { initial: None },
            previous_support: None,
        }],
        bodies,
    }
}

fn truth_exposure(body: &looklab_fit::synth::SynthBody, mode: looklab_fit::ExposureModeId) -> f64 {
    body.truth_exposure
        .iter()
        .find(|(candidate, _)| *candidate == mode)
        .map_or(0.0, |(_, value)| *value)
}

fn truth_wb(body: &looklab_fit::synth::SynthBody, mode: looklab_fit::WbModeClass) -> [f64; 2] {
    body.truth_wb
        .iter()
        .find(|(candidate, _)| *candidate == mode)
        .map_or([0.0, 0.0], |(_, value)| *value)
}

fn median(mut values: Vec<f64>) -> f64 {
    values.sort_by(|a, b| a.total_cmp(b));
    values[values.len() / 2]
}

#[cfg(feature = "full-scale")]
#[test]
#[ignore = "release-mode timing measurement"]
fn multi_body_fit_timing() {
    let data = multi_body(&config());
    let input = data.body_data();
    let start = std::time::Instant::now();
    let fit = fit_hierarchical(&input, &hier_options()).expect("hierarchical fit");
    let elapsed = start.elapsed();
    eprintln!(
        "hierarchical fit: {} bodies, {} images, {} evaluations, {:.1} s",
        input.bodies.len(),
        input
            .bodies
            .iter()
            .flat_map(|body| &body.realizations)
            .map(|realization| realization.images.len())
            .sum::<usize>(),
        fit.diagnostics.evaluations,
        elapsed.as_secs_f64()
    );
}

#[cfg(feature = "full-scale")]
#[test]
fn synthetic_bodies_recover_delta_exposure_wb_and_look() {
    let data = multi_body(&config());
    let input = data.body_data();
    let fit = fit_hierarchical(&input, &hier_options()).expect("hierarchical fit");

    let prior_sd = looklab_body::DNG_PRIOR_SD;
    for (synth, fitted) in data.bodies.iter().zip(fit.bodies.iter()) {
        let expected = synth.truth_delta.params();
        let actual = fitted.transform.delta().params();
        for (index, (expected, actual)) in expected.iter().zip(actual.iter()).enumerate() {
            assert!(
                (actual - expected).abs() <= prior_sd,
                "{} delta {index} outside the prior: {actual} vs {expected}",
                synth.id.as_str()
            );
        }
        for (mode, expected) in &synth.truth_exposure {
            let actual = fitted
                .exposure
                .iter()
                .find(|offset| offset.mode == *mode)
                .map_or(0.0, |offset| offset.offset);
            assert!(
                (actual - expected).abs() <= 0.3,
                "{} exposure mode {} outside the prior: {actual} vs {expected}",
                synth.id.as_str(),
                mode.0
            );
        }
        for (mode, expected) in &synth.truth_wb {
            let actual = fitted
                .wb
                .iter()
                .find(|(candidate, _)| candidate == mode)
                .map_or([0.0, 0.0], |(_, value)| *value);
            for channel in 0..2 {
                assert!(
                    (actual[channel] - expected[channel]).abs() <= 0.03,
                    "{} wb mode {} channel {channel} outside the prior: {} vs {}",
                    synth.id.as_str(),
                    mode.0,
                    actual[channel],
                    expected[channel]
                );
            }
        }
    }

    for (family, truth) in &data.truths {
        let fitted = fit.family_of(family).expect("fitted family");
        let errors: Vec<f64> = probes_v1()
            .probes
            .iter()
            .map(|probe| delta_e_ucs(fitted.look.eval(probe.xyz), truth.eval(probe.xyz)))
            .collect();
        let value = median(errors);
        eprintln!("look {} median dE: {value:.3}", family.as_str());
        assert!(value <= 0.5, "look {} median {value}", family.as_str());
    }
}

#[test]
fn one_body_update_refits_exactly_its_dependent_looks() {
    let data = multi_body(&MultiBodyConfig {
        seed: 0x51e2_0003,
        bodies: 3,
        images_per_body: 4,
        samples_per_image: 120,
        look_families: 2,
        exposure_modes: 1,
        wb_modes: 1,
        ..MultiBodyConfig::default()
    });
    let input = data.body_data();
    let options = HierOptions {
        fit: FitOptions {
            max_samples_per_image: 120,
            hessian_samples: 800,
            max_sweeps: 3,
            sweep_budget: 200_000.0,
            ..fit_options()
        },
        outer_iterations: 1,
        body_iterations: 1,
        ..hier_options()
    };
    let fit = fit_hierarchical(&input, &options).expect("hierarchical fit");
    let first = body(&input, "test/body-0").id.clone();
    let last = body(&input, "test/body-2").id.clone();
    let middle = body(&input, "test/body-1").id.clone();

    assert_eq!(fit.refit_cascade(&[first]), vec![target(0)]);
    assert_eq!(fit.refit_cascade(&[last]), vec![target(1)]);
    assert_eq!(fit.refit_cascade(&[middle]), vec![target(0), target(1)]);
}

#[cfg(feature = "full-scale")]
fn coverage_config() -> MultiBodyConfig {
    MultiBodyConfig {
        seed: 0x51e2_0009,
        bodies: 3,
        images_per_body: 3,
        samples_per_image: 300,
        look_families: 1,
        firmware: 1,
        exposure_modes: 1,
        wb_modes: 1,
        delta_sd: 0.0015,
        exposure_sd: 0.06,
        wb_sd: 0.004,
        ..MultiBodyConfig::default()
    }
}

#[cfg(feature = "full-scale")]
fn coverage_options() -> HierOptions {
    HierOptions {
        fit: FitOptions {
            max_samples_per_image: 300,
            hessian_samples: 1200,
            max_sweeps: 5,
            sweep_budget: 400_000.0,
            exposure_prior_sd: 0.04,
            exposure_mode_prior_sd: 0.2,
            wb_prior_sd: 0.008,
            ..FitOptions::default()
        },
        outer_iterations: 2,
        body_iterations: 4,
        ..HierOptions::default()
    }
}

#[cfg(feature = "full-scale")]
fn held_out_regions(
    data: &looklab_fit::synth::MultiBodyDataset,
    index: usize,
    look: &looklab_fit::Look,
    options: &FitOptions,
) -> (Vec<f64>, Vec<f64>) {
    let body = &data.bodies[index];
    let transform = body.a0.mul(&body.truth_delta.transform());
    let bins = looklab_fit::model_error::MODEL_HUE_BINS * looklab_fit::model_error::MODEL_S_BINS;
    let mut sums = vec![[0.0_f64; 3]; bins];
    let mut counts = vec![0.0_f64; bins];
    for (image_index, tag) in body.images.iter().enumerate() {
        let image = &tag.image;
        let mode = image.exposure_mode;
        let xi = truth_exposure(body, mode);
        let omega = truth_wb(body, tag.wb_mode);
        let nuisance = body.image_nuisances[image_index];
        let gain = looklab_fit::Mat3::diag([
            libm::exp(omega[0] + nuisance[1]),
            1.0,
            libm::exp(omega[1] + nuisance[2]),
        ]);
        let matrix = transform.mul(&gain);
        let exposure = libm::pow(2.0, xi + nuisance[0]);
        let space = match image.samples.header().output_space {
            looklab_samples::OutputSpace::Srgb => RgbSpace::Srgb,
            looklab_samples::OutputSpace::AdobeRgb => RgbSpace::AdobeRgb,
        };
        let input_rgb = image.samples.in_rgb();
        let output_rgb = image.samples.out_rgb();
        for (sample, rgb) in input_rgb.iter().enumerate() {
            let scale = exposure;
            let xyz = looklab_color::Xyz::from_array(matrix.mul_vec([
                rgb[0] as f64 * scale,
                rgb[1] as f64 * scale,
                rgb[2] as f64 * scale,
            ]));
            let input = looklab_model::LookInput::new(xyz);
            let target = looklab_model::solve::ucs_of_xyz(space.rgb_linear_to_xyz(
                looklab_color::Rgb::from_array(output_rgb[sample].map(|value| value as f64)),
            ));
            let prediction = look.eval(xyz);
            let bin = looklab_fit::model_error::region(input.s, input.h_in);
            sums[bin][0] += prediction.j - target.j;
            sums[bin][1] += prediction.a - target.a;
            sums[bin][2] += prediction.b - target.b;
            counts[bin] += 1.0;
        }
    }
    let _ = options;
    let error = sums
        .iter()
        .zip(counts.iter())
        .map(|(sum, count)| {
            if *count > 0.0 {
                let mean = [sum[0] / count, sum[1] / count, sum[2] / count];
                (mean[0] * mean[0] + mean[1] * mean[1] + mean[2] * mean[2]).sqrt()
            } else {
                0.0
            }
        })
        .collect();
    (error, counts)
}

#[cfg(feature = "full-scale")]
#[test]
fn published_uncertainty_covers_held_out_bodies() {
    let data = multi_body(&coverage_config());
    let bins = looklab_fit::model_error::MODEL_HUE_BINS * looklab_fit::model_error::MODEL_S_BINS;
    let mut before = 0usize;
    let mut after = 0usize;
    let mut regions = 0usize;
    for outer in 0..data.bodies.len() {
        let mut training = data.body_data();
        training.bodies.remove(outer);
        let fit = fit_hierarchical(&training, &coverage_options()).expect("hierarchical fit");
        let parameters = fit.family(&target(0)).expect("family 0 is fitted");
        let published = published_uncertainty(
            &parameters.look,
            &parameters.pool,
            &parameters.groups,
            &parameters.probes,
            &parameters.density,
            &coverage_options().fit,
            &BootstrapOptions {
                resamples: 32,
                seed: 0x0b00_7e57 ^ outer as u64,
                leave_one_body_out: true,
            },
            parameters.clip_k,
            None,
            None,
        )
        .expect("published uncertainty");
        let probes = probes_v1();
        let mut before_sums = vec![0.0_f64; bins];
        let mut after_sums = vec![0.0_f64; bins];
        let mut probe_counts = vec![0.0_f64; bins];
        for (index, probe) in probes.probes.iter().enumerate() {
            let input = looklab_model::LookInput::new(probe.xyz);
            let bin = looklab_fit::model_error::region(input.s, input.h_in);
            let fit_covariance = published
                .fit_covariance
                .get(index)
                .copied()
                .unwrap_or([[0.0; 3]; 3]);
            let disagreement = published
                .disagreement
                .get(index)
                .copied()
                .unwrap_or([[0.0; 3]; 3]);
            let before_variance: f64 = (0..3)
                .map(|channel| fit_covariance[channel][channel] + disagreement[channel][channel])
                .sum();
            before_sums[bin] += before_variance;
            after_sums[bin] += published.probes.probes[index].covariance[0][0]
                + published.probes.probes[index].covariance[1][1]
                + published.probes.probes[index].covariance[2][2];
            probe_counts[bin] += 1.0;
        }
        let (held, held_counts) =
            held_out_regions(&data, outer, &parameters.look, &coverage_options().fit);
        for bin in 0..bins {
            if held_counts[bin] <= 0.0 || probe_counts[bin] <= 0.0 {
                continue;
            }
            regions += 1;
            let before_band = 2.5002 * (before_sums[bin] / probe_counts[bin]).max(0.0).sqrt();
            let after_band = 2.5002 * (after_sums[bin] / probe_counts[bin]).max(0.0).sqrt();
            if held[bin] <= before_band {
                before += 1;
            }
            if held[bin] <= after_band {
                after += 1;
            }
        }
    }
    let before_fraction = before as f64 / regions.max(1) as f64;
    let after_fraction = after as f64 / regions.max(1) as f64;
    eprintln!(
        "outer-body region coverage before {before}/{regions} = {before_fraction:.3}, after {after}/{regions} = {after_fraction:.3}"
    );
    assert!(regions >= 20, "only {regions} held-out regions");
    assert!(after_fraction >= 0.85, "coverage {after_fraction}");
}

#[cfg(feature = "full-scale")]
#[test]
fn write_known_reference_workload() {
    let Some(out) = std::env::var_os("LOOKLAB_ENGINE_REFERENCE_OUT") else {
        return;
    };
    let fitter = std::env::var("LOOKLAB_ENGINE_FITTER").expect("LOOKLAB_ENGINE_FITTER");
    let evaluator = std::env::var("LOOKLAB_ENGINE_EVALUATOR").expect("LOOKLAB_ENGINE_EVALUATOR");
    let model = std::env::var("LOOKLAB_ENGINE_MODEL").expect("LOOKLAB_ENGINE_MODEL");
    let data = multi_body(&coverage_config());
    let truth = data
        .truths
        .iter()
        .find(|(family, _)| *family == target(0).family)
        .map(|(_, look)| look)
        .expect("family truth");
    let bodies: Vec<String> = data
        .bodies
        .iter()
        .map(|body| body.id.as_str().to_owned())
        .collect();
    let mut trials = Vec::new();
    for outer in 0..data.bodies.len() {
        let held_body = data.bodies[outer].id.clone();
        let training: Vec<String> = data
            .bodies
            .iter()
            .enumerate()
            .filter(|(index, _)| *index != outer)
            .map(|(_, body)| body.id.as_str().to_owned())
            .collect();
        let mut training_input = data.body_data();
        training_input.bodies.remove(outer);
        let fit = fit_hierarchical(&training_input, &coverage_options()).expect("hierarchical fit");
        let parameters = fit.family(&target(0)).expect("family 0 is fitted");
        let published = published_uncertainty(
            &parameters.look,
            &parameters.pool,
            &parameters.groups,
            &parameters.probes,
            &parameters.density,
            &coverage_options().fit,
            &BootstrapOptions {
                resamples: 32,
                seed: 0x0b00_7e57 ^ outer as u64,
                leave_one_body_out: true,
            },
            parameters.clip_k,
            None,
            None,
        )
        .expect("published uncertainty");
        let bins =
            looklab_fit::model_error::MODEL_HUE_BINS * looklab_fit::model_error::MODEL_S_BINS;
        let probes = probes_v1();
        let mut after_sums = vec![0.0_f64; bins];
        let mut probe_counts = vec![0.0_f64; bins];
        for (index, probe) in probes.probes.iter().enumerate() {
            let input = looklab_model::LookInput::new(probe.xyz);
            let bin = looklab_fit::model_error::region(input.s, input.h_in);
            after_sums[bin] += published.probes.probes[index].covariance[0][0]
                + published.probes.probes[index].covariance[1][1]
                + published.probes.probes[index].covariance[2][2];
            probe_counts[bin] += 1.0;
        }
        let (held, held_counts) =
            held_out_regions(&data, outer, &parameters.look, &coverage_options().fit);
        for bin in 0..bins {
            if held_counts[bin] <= 0.0 || probe_counts[bin] <= 0.0 {
                continue;
            }
            let band = 2.5002 * (after_sums[bin] / probe_counts[bin]).max(0.0).sqrt();
            trials.push(serde_json::json!({
                "body": held_body.as_str(),
                "cell": format!("cell-{bin}"),
                "true_error_de": held[bin],
                "band_90_de": band,
                "recovered": held[bin] <= band,
                "training_bodies": &training,
                "held_out_body": true,
                "out_of_gamut": false,
                "marked_unsupported": false,
            }));
        }
        for (index, probe) in probes.probes.iter().enumerate() {
            if probe.id.as_str().split('-').nth(2) != Some("80") {
                continue;
            }
            let estimate = &published.probes.probes[index];
            if estimate.supported {
                continue;
            }
            let predicted = looklab_fit::post_clip(
                &parameters.look,
                probe.xyz,
                parameters.output_space,
                parameters.clip_k,
            );
            let target_ucs = looklab_fit::post_clip(
                truth,
                probe.xyz,
                parameters.output_space,
                parameters.clip_k,
            );
            trials.push(serde_json::json!({
                "body": held_body.as_str(),
                "cell": format!("cell-{}", looklab_fit::model_error::cell(&looklab_model::LookInput::new(probe.xyz))),
                "true_error_de": delta_e_ucs(predicted, target_ucs),
                "band_90_de": 2.5002 * estimate.post_sigma_de,
                "recovered": true,
                "training_bodies": &training,
                "held_out_body": true,
                "out_of_gamut": true,
                "marked_unsupported": true,
            }));
        }
    }
    let reference = serde_json::json!({
        "format": "looklab-engine-reference/1",
        "engine": format!("{fitter}|{evaluator}|{model}"),
        "rule_revision": "engine-coverage/1",
        "bodies": bodies,
        "trials": trials,
    });
    std::fs::write(
        out,
        serde_json::to_vec_pretty(&reference).expect("workload json"),
    )
    .expect("write workload");
}

fn projected_body_images(
    data: &looklab_fit::synth::MultiBodyDataset,
) -> (
    Vec<looklab_fit::FitImage>,
    Vec<ImageNuisances>,
    Vec<FirmwareId>,
) {
    let mut images = Vec::new();
    let mut nuisances = Vec::new();
    let mut firmware = Vec::new();
    for (body_index, body) in data.bodies.iter().enumerate() {
        let transform = body.a0.mul(&body.truth_delta.transform());
        for (image_index, tag) in body.images.iter().enumerate() {
            let mut image = tag.image.clone();
            image.transform = transform;
            let mode = image.exposure_mode;
            let xi = truth_exposure(body, mode);
            let omega = truth_wb(body, tag.wb_mode);
            let nuisance = body.image_nuisances[image_index];
            nuisances.push(ImageNuisances {
                exposure: xi + nuisance[0],
                wb_log_r: omega[0] + nuisance[1],
                wb_log_b: omega[1] + nuisance[2],
                exposure_mode: mode,
                contributor: looklab_fit::ContributorId(body_index as u32),
                weight: 1.0,
            });
            firmware.push(tag.firmware.clone());
            images.push(image);
        }
    }
    (images, nuisances, firmware)
}

#[test]
fn firmware_projection_reports_a_retained_row_outside_the_inverse_domain() {
    let data = multi_body(&MultiBodyConfig {
        seed: 0x51e2_0004,
        bodies: 1,
        images_per_body: 1,
        samples_per_image: 32,
        look_families: 1,
        firmware: 1,
        exposure_modes: 1,
        wb_modes: 1,
        ..MultiBodyConfig::default()
    });
    let mut base = data.truth(&family(0)).cloned().expect("truth look");
    base.color.g.plane.fill(1.0e6);
    base.color.g.volume.fill(1.0e6);
    base.color.g.core.fill(1.0e6);
    let (images, nuisances, firmware) = projected_body_images(&data);
    let error = firmware_images(
        &base,
        &images,
        &nuisances,
        &firmware,
        RgbSpace::Srgb,
        &vec![0.03; images.len()],
        &fit_options(),
    )
    .expect_err("a retained row outside the inverse domain must fail the projection");
    assert!(matches!(error, FitError::Hierarchy(_)));
}

#[test]
fn a_silent_firmware_change_is_detected_through_the_projection_path() {
    let data = multi_body(&MultiBodyConfig {
        seed: 0x51e2_0002,
        bodies: 2,
        images_per_body: 12,
        samples_per_image: 300,
        look_families: 1,
        firmware: 2,
        firmware_shift: [2.0, 0.08],
        ..MultiBodyConfig::default()
    });
    let base = data.truth(&family(0)).expect("truth look");
    let (images, nuisances, firmware) = projected_body_images(&data);
    let projections = firmware_images(
        base,
        &images,
        &nuisances,
        &firmware,
        RgbSpace::Srgb,
        &vec![0.03; images.len()],
        &fit_options(),
    )
    .expect("projections");
    let samples = project_images(&projections);
    for sample in &samples {
        eprintln!(
            "group {} projection [{:+.3},{:+.3}] effect {:.3}",
            sample.group, sample.projection[0], sample.projection[1], sample.effect_de
        );
    }
    let decision = detect_split(
        &samples,
        &SplitOptions {
            permutations: 4000,
            seed: 3,
            ..SplitOptions::default()
        },
    );
    let decision = decision.expect("firmware change detected");
    eprintln!(
        "split groups {:?} p {:.5} effect {:.3}",
        decision.groups, decision.p_value, decision.effect_de
    );
    assert_eq!(decision.groups, (0, 1));
    assert!(decision.p_value < 0.001);
}

#[cfg(feature = "full-scale")]
fn deterministic_noise(state: &mut u64) -> f64 {
    *state ^= *state << 13;
    *state ^= *state >> 7;
    *state ^= *state << 17;
    ((*state >> 11) as f64 / (1u64 << 53) as f64) - 0.5
}

#[cfg(feature = "full-scale")]
#[test]
fn a_million_same_version_samples_do_not_split() {
    let mut state = 0x5eed_0f0f_1234_5678_u64;
    let mut images = Vec::new();
    for image in 0..200u32 {
        let mut samples = Vec::with_capacity(5000);
        for _ in 0..5000 {
            let mut basis = [[0.0_f64; 3]; PROJECTION_DIM];
            for vector in basis.iter_mut() {
                for channel in vector.iter_mut() {
                    *channel = deterministic_noise(&mut state);
                }
            }
            samples.push(SampleProjection {
                residual: [
                    deterministic_noise(&mut state),
                    deterministic_noise(&mut state),
                    deterministic_noise(&mut state),
                ],
                basis,
                weight: 1.0,
            });
        }
        images.push(looklab_fit::ImageProjection {
            group: image % 2,
            samples,
        });
    }
    let samples = project_images(&images);
    let decision = detect_split(
        &samples,
        &SplitOptions {
            permutations: 4000,
            seed: 5,
            ..SplitOptions::default()
        },
    );
    assert!(decision.is_none(), "false split: {decision:?}");
}

#[test]
fn firmware_offsets_have_the_observed_sign() {
    let data = multi_body(&MultiBodyConfig {
        seed: 0x51e2_0010,
        bodies: 3,
        images_per_body: 4,
        samples_per_image: 250,
        look_families: 1,
        firmware: 2,
        firmware_shift: [2.0, 0.08],
        exposure_modes: 1,
        wb_modes: 1,
        ..MultiBodyConfig::default()
    });
    let input = data.body_data();
    let fit = fit_hierarchical(
        &input,
        &HierOptions {
            fit: FitOptions {
                max_samples_per_image: 250,
                hessian_samples: 1200,
                max_sweeps: 5,
                sweep_budget: 400_000.0,
                ..fit_options()
            },
            outer_iterations: 2,
            body_iterations: 3,
            ..HierOptions::default()
        },
    )
    .expect("hierarchical fit");
    let parameters = fit.family(&target(0)).expect("family 0");
    let offset = parameters
        .firmware
        .iter()
        .find(|(id, _)| id.version.as_str() == "1.1")
        .map(|(_, offset)| *offset)
        .expect("firmware 1 offset");
    assert!(offset[0] > 0.05, "tone offset {offset:?}");
    assert!(offset[1] > 0.002, "log G offset {offset:?}");
}

#[test]
fn persisted_gauge_body_is_kept() {
    let data = multi_body(&MultiBodyConfig {
        seed: 0x51e2_0011,
        bodies: 2,
        images_per_body: 2,
        samples_per_image: 150,
        look_families: 1,
        exposure_modes: 1,
        wb_modes: 1,
        ..MultiBodyConfig::default()
    });
    let mut input = data.body_data();
    let extra = input.bodies[1].realizations[0].images[0].clone();
    input.bodies[1].realizations[0].images.push(extra);
    let gauge = ComponentGauge {
        designated: looklab_body::BodyId::new(input.bodies[0].id.as_str()).expect("gauge body"),
        exposure_mode: looklab_fit::ExposureModeId(0),
        wb_mode: WbModeClass(0),
        anchored_delta: true,
    };
    let options = HierOptions {
        fit: FitOptions {
            max_samples_per_image: 150,
            hessian_samples: 800,
            max_sweeps: 3,
            sweep_budget: 200_000.0,
            ..fit_options()
        },
        outer_iterations: 1,
        body_iterations: 2,
        gauges: vec![gauge],
        ..HierOptions::default()
    };
    let fit = fit_hierarchical(&input, &options).expect("hierarchical fit");
    assert_eq!(fit.components[0].designated.as_str(), "test/body-0");
}

#[test]
fn recipe_only_relationships_do_not_enter_the_cascade() {
    let data = multi_body(&MultiBodyConfig {
        seed: 0x51e2_0012,
        bodies: 3,
        images_per_body: 2,
        samples_per_image: 150,
        look_families: 2,
        exposure_modes: 1,
        wb_modes: 1,
        ..MultiBodyConfig::default()
    });
    let mut input = data.body_data();
    let last = input.bodies.len() - 1;
    let mut recipe = input.bodies[last].realizations[0].images[0].clone();
    recipe.image.intent = ImageIntent::Recipe;
    let last_body = input.bodies[last].id.clone();
    input.bodies[last].realizations.push(RealizationData {
        id: BodyRealizationId {
            body: last_body.clone(),
            epoch: ControlEpochId::new(2),
            native_sim: SimKey::new("recipe").expect("recipe sim"),
        },
        target: target(0),
        model: None,
        images: vec![recipe],
        warm: None,
    });
    let options = HierOptions {
        fit: FitOptions {
            max_samples_per_image: 150,
            hessian_samples: 800,
            max_sweeps: 3,
            sweep_budget: 200_000.0,
            ..fit_options()
        },
        outer_iterations: 1,
        body_iterations: 2,
        ..HierOptions::default()
    };
    let fit = fit_hierarchical(&input, &options).expect("hierarchical fit");
    let cascade = fit.refit_cascade(std::slice::from_ref(&last_body));
    assert!(cascade.contains(&target(1)));
    assert!(!cascade.contains(&target(0)));
    let family = fit.family(&target(0)).expect("family 0");
    assert!(
        !family
            .provenance
            .iter()
            .any(|provenance| provenance.body == last_body),
        "recipe-only body in Look 0 provenance"
    );
}

#[test]
fn recipe_images_do_not_move_the_published_look() {
    let base = MultiBodyConfig {
        seed: 0x51e2_0013,
        bodies: 2,
        images_per_body: 2,
        samples_per_image: 150,
        look_families: 1,
        exposure_modes: 1,
        wb_modes: 1,
        ..MultiBodyConfig::default()
    };
    let data = multi_body(&base);
    let options = HierOptions {
        fit: FitOptions {
            max_samples_per_image: 150,
            hessian_samples: 800,
            max_sweeps: 3,
            sweep_budget: 200_000.0,
            ..fit_options()
        },
        outer_iterations: 1,
        body_iterations: 2,
        ..HierOptions::default()
    };
    let plain = fit_hierarchical(&data.body_data(), &options).expect("plain fit");
    let mut with_recipe = data.body_data();
    let mut recipe = with_recipe.bodies[1].realizations[0].images[0].clone();
    recipe.image.intent = ImageIntent::Recipe;
    let recipe_body = with_recipe.bodies[1].id.clone();
    with_recipe.bodies[1].realizations.push(RealizationData {
        id: BodyRealizationId {
            body: recipe_body,
            epoch: ControlEpochId::new(2),
            native_sim: SimKey::new("recipe").expect("recipe sim"),
        },
        target: target(0),
        model: None,
        images: vec![recipe],
        warm: None,
    });
    let treated = fit_hierarchical(&with_recipe, &options).expect("recipe fit");
    let left = plain.family(&target(0)).expect("plain").look.coef_vector();
    let right = treated
        .family(&target(0))
        .expect("recipe")
        .look
        .coef_vector();
    let difference = left
        .iter()
        .zip(right.iter())
        .map(|(a, b)| (a - b).abs())
        .fold(0.0_f64, f64::max);
    assert!(difference < 1e-9, "recipe moved the Look by {difference}");
}

#[cfg(feature = "long-tests")]
#[test]
fn single_body_bootstrap_draws_images() {
    let data = looklab_fit::synth::dataset(&looklab_fit::synth::SynthConfig {
        seed: 0x51e2_0014,
        images: 8,
        samples_per_image: 300,
        ..looklab_fit::synth::SynthConfig::default()
    });
    let result = looklab_fit::fit(&data.images, &fit_options()).expect("fit");
    let group = looklab_fit::BodyGroup {
        body: BodyId::new("test/one").expect("id"),
        images: (0..data.images.len()).collect(),
    };
    let published = published_uncertainty(
        &result.look,
        &data.images,
        &[group],
        &result.probes,
        &result.density,
        &fit_options(),
        &BootstrapOptions {
            resamples: 8,
            seed: 0x5eed,
            leave_one_body_out: false,
        },
        result.realization.clip_k,
        None,
        None,
    )
    .expect("published");
    assert_eq!(
        published.fit_method,
        looklab_fit::bootstrap::UncertaintyMethod::Bootstrap
    );
    let trace: f64 = published
        .fit_covariance
        .iter()
        .map(|covariance| covariance[0][0] + covariance[1][1] + covariance[2][2])
        .sum();
    assert!(trace > 0.0);
}

fn control_body(
    dataset: &looklab_fit::synth::ControlledDataset,
    model: Option<looklab_fit::ControlResponseModel>,
) -> BodyData {
    let id = BodyId::new("test/controlled").expect("body id");
    let body_id = looklab_body::BodyId::new("test/controlled").expect("body id");
    let a0 = dataset.images[0].transform;
    let prior = looklab_body::BodyPrior {
        source: looklab_body::A0Source::Dng,
        a0_a: a0,
        a0_d65: a0,
    };
    let images: Vec<looklab_fit::HierImage> = dataset
        .images
        .iter()
        .map(|image| looklab_fit::HierImage {
            image: image.clone(),
            firmware: firmware(&id, "1.0"),
            wb_mode: WbModeClass(0),
        })
        .collect();
    let realization = RealizationData {
        id: BodyRealizationId {
            body: id.clone(),
            epoch: ControlEpochId::new(1),
            native_sim: SimKey::new("native").expect("native sim"),
        },
        target: target(0),
        model,
        images,
        warm: None,
    };
    BodyData {
        id,
        transform: looklab_body::InputTransform::prior(
            body_id,
            looklab_body::TransformVersion::new(1),
            prior,
        ),
        default_mode: looklab_fit::ExposureModeId(0),
        default_wb_mode: WbModeClass(0),
        charts: Vec::new(),
        warm: None,
        realizations: vec![realization],
    }
}

#[cfg(feature = "long-tests")]
#[test]
fn hierarchical_body_block_fits_control_responses() {
    let schema = looklab_schema::validate_control_schema(include_str!(
        "../../../data/bodies/fujifilm-x-t5.yaml"
    ))
    .expect("schema validates");
    let mut settings = vec![looklab_fit::Settings::new(); 2];
    for (name, value) in [
        ("color", 4),
        ("color", 2),
        ("highlight_tone", 3),
        ("shadow_tone", 3),
        ("color", -2),
        ("shadow_tone", -3),
    ] {
        let mut setting = looklab_fit::Settings::new();
        setting.set(
            looklab_schema::ControlKey::new(name).expect("control key"),
            looklab_fit::ControlValue::Step(value),
        );
        settings.push(setting);
    }
    let count = settings.len();
    let dataset = looklab_fit::synth::controlled_dataset(&ControlSynthConfig {
        base: SynthConfig {
            seed: 0x51e2_0015,
            images: count,
            samples_per_image: 100,
            noise_ucs: 0.3,
            ..SynthConfig::default()
        },
        schema,
        epoch: 1,
        base_sim: looklab_schema::SimKey::new("classic_chrome").expect("sim"),
        settings,
        groups: vec![Some([7; 16]); count],
        response_scale: 0.08,
        truth_seed: 0x51e2_0c0d,
    });
    let options = HierOptions {
        fit: FitOptions {
            max_samples_per_image: 100,
            hessian_samples: 800,
            max_sweeps: 6,
            sweep_budget: 400_000.0,
            ..fit_options()
        },
        outer_iterations: 3,
        body_iterations: 4,
        ..HierOptions::default()
    };
    let with_model = fit_hierarchical(
        &free_input(vec![control_body(
            &dataset,
            Some(dataset.truth_model.clone()),
        )]),
        &options,
    )
    .expect("controlled hierarchical fit");
    let without_model = fit_hierarchical(&free_input(vec![control_body(&dataset, None)]), &options)
        .expect("uncontrolled hierarchical fit");
    assert!(
        with_model.realizations[0].model.is_some(),
        "body block produced no control response"
    );
    let error = |fit: &looklab_fit::hierarchical::HierarchicalFit| {
        let fitted = &fit.family(&target(0)).expect("family 0").look;
        median(
            probes_v1()
                .probes
                .iter()
                .map(|probe| {
                    delta_e_ucs(fitted.eval(probe.xyz), dataset.truth_look.eval(probe.xyz))
                })
                .collect(),
        )
    };
    let fitted = error(&with_model);
    let bare = error(&without_model);
    eprintln!("controlled look median dE: with model {fitted:.3}, without {bare:.3}");
    let model = with_model.realizations[0]
        .model
        .as_ref()
        .expect("fitted model");
    assert!(
        model.controls.iter().any(|control| control
            .values
            .iter()
            .any(|value| !value.delta.layers.is_empty())),
        "fitted control response is empty"
    );
    let difference = with_model
        .family(&target(0))
        .expect("family 0")
        .look
        .coef_vector()
        .iter()
        .zip(
            without_model
                .family(&target(0))
                .expect("family 0")
                .look
                .coef_vector()
                .iter(),
        )
        .map(|(left, right)| (left - right).abs())
        .fold(0.0_f64, f64::max);
    assert!(
        difference > 1e-6,
        "control model did not change the Look fit"
    );
    assert!(fitted <= 0.75, "controlled look median {fitted}");
}

fn pinned_input(data: &looklab_fit::synth::MultiBodyDataset) -> HierInput {
    let mut input = data.body_data();
    input.families = data
        .truths
        .iter()
        .map(|(family, look)| {
            FamilyData::pinned(
                FamilyTargetId {
                    family: family.clone(),
                    version: TargetVersion::new(1).expect("one is a positive target version"),
                },
                look.clone(),
            )
        })
        .collect();
    input
}

fn with_realization(input: &HierInput, index: usize) -> HierInput {
    let mut out = input.clone();
    for body in &mut out.bodies {
        body.realizations = vec![body.realizations[index].clone()];
    }
    out
}

fn shifted(look: &Look) -> Look {
    let mut out = look.clone();
    if let Tone::Spline { c0, beta } = &out.tone {
        out.tone = Tone::spline(*c0 + 0.25, *beta).expect("shifted tone spline");
    }
    out
}

fn exposure_of(
    fit: &looklab_fit::hierarchical::RealizationFit,
    body: &BodyId,
    mode: looklab_fit::ExposureModeId,
) -> f64 {
    fit.body(body)
        .expect("fitted body")
        .exposure
        .iter()
        .find(|offset| offset.mode == mode)
        .map_or(0.0, |offset| offset.offset)
}

#[test]
fn pinned_realizations_keep_the_exact_target() {
    let data = multi_body(&MultiBodyConfig {
        seed: 0x51e2_0022,
        bodies: 1,
        images_per_body: 4,
        samples_per_image: 200,
        look_families: 2,
        exposure_modes: 1,
        wb_modes: 1,
        ..MultiBodyConfig::default()
    });
    let input = pinned_input(&data);
    let options = HierOptions {
        fit: fit_options(),
        outer_iterations: 1,
        body_iterations: 2,
        ..HierOptions::default()
    };
    let fit = fit_realizations(&input, &options).expect("pinned realization fit");
    for (family, look) in &data.truths {
        let pinned = fit
            .families
            .iter()
            .find(|pinned| &pinned.target.family == family)
            .expect("pinned family");
        assert_eq!(
            pinned.look.coef_vector(),
            look.coef_vector(),
            "pinned target moved"
        );
    }
    assert!(
        fit.state.families.is_empty(),
        "pinned fit stored family coordinates"
    );
}

#[test]
fn shared_body_calibration_balances_every_realization() {
    let data = multi_body(&MultiBodyConfig {
        seed: 0x51e2_0021,
        bodies: 2,
        images_per_body: 6,
        samples_per_image: 300,
        look_families: 2,
        exposure_modes: 1,
        wb_modes: 1,
        ..MultiBodyConfig::default()
    });
    let mut input = pinned_input(&data);
    let (family_one, look_one) = &data.truths[1];
    let corrupted = shifted(look_one);
    for family in &mut input.families {
        if &family.target.family == family_one {
            family.mode = FamilyMode::Pinned(corrupted.clone());
        }
    }
    let options = HierOptions {
        fit: fit_options(),
        outer_iterations: 3,
        body_iterations: 6,
        ..HierOptions::default()
    };
    let joint = fit_realizations(&input, &options).expect("joint pinned fit");
    let first =
        fit_realizations(&with_realization(&input, 0), &options).expect("first realization fit");
    let second =
        fit_realizations(&with_realization(&input, 1), &options).expect("second realization fit");
    let body = input.bodies[1].id.clone();
    let mode = input.bodies[1].default_mode;
    let joint = exposure_of(&joint, &body, mode);
    let first = exposure_of(&first, &body, mode);
    let second = exposure_of(&second, &body, mode);
    assert!(
        (first - second).abs() > 1e-4,
        "realizations do not discriminate: {first} vs {second}"
    );
    let low = first.min(second);
    let high = first.max(second);
    assert!(
        joint >= low - 1e-3 && joint <= high + 1e-3,
        "shared exposure {joint} outside [{low}, {high}]"
    );
}

#[test]
fn a_nonstationary_body_certificate_blocks_numerical_publication() {
    let data = multi_body(&MultiBodyConfig {
        seed: 0x51e2_0023,
        bodies: 1,
        images_per_body: 4,
        samples_per_image: 200,
        look_families: 2,
        exposure_modes: 1,
        wb_modes: 1,
        ..MultiBodyConfig::default()
    });
    let mut input = pinned_input(&data);
    let corrupted = shifted(&data.truths[0].1);
    for family in &mut input.families {
        family.mode = FamilyMode::Pinned(corrupted.clone());
    }
    let options = HierOptions {
        fit: FitOptions {
            max_samples_per_image: 200,
            hessian_samples: 1200,
            max_sweeps: 2,
            sweep_budget: 200_000.0,
            ..fit_options()
        },
        outer_iterations: 1,
        body_iterations: 1,
        ..HierOptions::default()
    };
    let fit = fit_realizations(&input, &options).expect("pinned realization fit");
    let report = fit
        .diagnostics
        .body_certificate
        .expect("body certificate computed");
    assert!(
        !report.certificate.is_certified_stationary(),
        "a single body step certified stationarity"
    );
    assert!(
        !fit.numerically_publishable(),
        "a nonstationary body block was numerically publishable"
    );
}

#[test]
fn fixed_body_pin_keeps_shared_calibration() {
    let data = multi_body(&MultiBodyConfig {
        seed: 0x51e2_0024,
        bodies: 1,
        images_per_body: 4,
        samples_per_image: 200,
        look_families: 2,
        exposure_modes: 1,
        wb_modes: 1,
        ..MultiBodyConfig::default()
    });
    let input = pinned_input(&data);
    let body = &input.bodies[0];
    let mut params = [0.0_f64; 6];
    params[0] = 0.002;
    let delta = Delta6::new(params).expect("valid delta");
    let state = BodyWarmStart {
        delta,
        exposure: vec![ExposureOffset {
            mode: body.default_mode,
            offset: 0.25,
        }],
        wb: vec![(body.default_wb_mode, [0.1, -0.1])],
    };
    let resumes = vec![BodyResume {
        id: body.id.clone(),
        state: state.clone(),
    }];
    for pin in [
        BodyPin::Native(resumes.clone()),
        BodyPin::Published(resumes.clone()),
    ] {
        let options = HierOptions {
            fit: fit_options(),
            outer_iterations: 2,
            body_iterations: 4,
            body_pin: pin,
            ..HierOptions::default()
        };
        let fit = fit_realizations(&input, &options).expect("fixed body fit");
        let fitted = fit.body(&body.id).expect("fitted body");
        assert_eq!(fitted.transform.delta().params(), delta.params());
        let exposure = fitted
            .exposure
            .iter()
            .find(|offset| offset.mode == body.default_mode)
            .map_or(0.0, |offset| offset.offset);
        assert!((exposure - 0.25).abs() < 1e-12, "fixed exposure moved");
        let wb = fitted
            .wb
            .iter()
            .find(|(mode, _)| *mode == body.default_wb_mode)
            .map_or([0.0; 2], |(_, value)| *value);
        assert!(
            (wb[0] - 0.1).abs() < 1e-12 && (wb[1] + 0.1).abs() < 1e-12,
            "fixed WB moved"
        );
        assert!(
            fit.realizations
                .iter()
                .flat_map(|realization| realization.images.iter())
                .any(|nuisance| nuisance.exposure.abs() > 1e-6
                    || nuisance.wb_log_r.abs() > 1e-6
                    || nuisance.wb_log_b.abs() > 1e-6),
            "conditional realization locals were not fitted"
        );
    }
}
