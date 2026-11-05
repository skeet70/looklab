// SPDX-License-Identifier: AGPL-3.0-or-later
//! Acceptance tests for the hierarchical fit, bootstrap uncertainty and the
//! firmware split path (docs/09 P4.2-P4.4).

#![cfg(feature = "synth")]
#![allow(clippy::disallowed_types)]

use looklab_color::RgbSpace;
use looklab_color::ucs::delta_e_ucs;
use looklab_fit::bootstrap::{BootstrapOptions, UncertaintyMethod, published_uncertainty};
use looklab_fit::hierarchical::{
    BodyData, FirmwareId, HierOptions, LookFamilyId, firmware_images, fit_hierarchical,
};
use looklab_fit::synth::{MultiBodyConfig, multi_body};
use looklab_fit::{
    FitOptions, ImageNuisances, PROJECTION_DIM, SampleProjection, SplitOptions, detect_split,
    project_images,
};
use looklab_model::probes::v1 as probes_v1;

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

fn body<'a>(data: &'a [BodyData], name: &str) -> &'a BodyData {
    data.iter()
        .find(|body| body.id.as_str() == name)
        .expect("synthetic body exists")
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
        input.len(),
        input.iter().map(|body| body.images.len()).sum::<usize>(),
        fit.diagnostics.evaluations,
        elapsed.as_secs_f64()
    );
}

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
        let fitted = fit.look(*family).expect("fitted family");
        let errors: Vec<f64> = probes_v1()
            .probes
            .iter()
            .map(|probe| delta_e_ucs(fitted.look.eval(probe.xyz), truth.eval(probe.xyz)))
            .collect();
        let value = median(errors);
        eprintln!("look {} median dE: {value:.3}", family.0);
        assert!(value <= 0.5, "look {} median {value}", family.0);
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
    let first = body(&input, "body-0").id.clone();
    let last = body(&input, "body-2").id.clone();
    let middle = body(&input, "body-1").id.clone();

    assert_eq!(fit.refit_cascade(&[first]), vec![LookFamilyId(0)]);
    assert_eq!(fit.refit_cascade(&[last]), vec![LookFamilyId(1)]);
    assert_eq!(
        fit.refit_cascade(&[middle]),
        vec![LookFamilyId(0), LookFamilyId(1)]
    );
}

#[test]
fn published_uncertainty_covers_held_out_bodies() {
    let data = multi_body(&config());
    let input = data.body_data();
    let fit = fit_hierarchical(&input, &hier_options()).expect("hierarchical fit");
    let family = LookFamilyId(0);
    let parameters = fit.look(family).expect("family 0 is fitted");
    let published = published_uncertainty(
        &parameters.look,
        &parameters.pool,
        &parameters.groups,
        &parameters.probes,
        &parameters.density,
        &fit_options(),
        &BootstrapOptions {
            resamples: 32,
            seed: 0x0b00_7e57,
            leave_one_body_out: true,
        },
    )
    .expect("published uncertainty");
    assert_eq!(published.fit_method, UncertaintyMethod::Bootstrap);
    assert!(!published.disagreement.is_empty());

    let mut before = 0usize;
    let mut after = 0usize;
    let mut regions = 0usize;
    for bin in 0..published.model.variance.len() {
        let held = published.model.held_out_rms[bin];
        if held <= 0.0 {
            continue;
        }
        regions += 1;
        let reference = published.model.reference_rms[bin];
        if held <= 2.0 * reference {
            before += 1;
        }
        let total = (reference * reference + published.model.variance[bin]).sqrt();
        if held <= 2.0 * total {
            after += 1;
        }
    }
    let before_fraction = before as f64 / regions.max(1) as f64;
    let after_fraction = after as f64 / regions.max(1) as f64;
    eprintln!(
        "held-out region coverage before {before}/{regions} = {before_fraction:.3}, after {after}/{regions} = {after_fraction:.3}"
    );
    assert!(
        regions >= 20,
        "only {regions} regions with held-out samples"
    );
    assert!(after_fraction >= 0.85, "coverage {after_fraction}");
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
            firmware.push(tag.firmware);
            images.push(image);
        }
    }
    (images, nuisances, firmware)
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
    let base = data.truth(LookFamilyId(0)).expect("truth look");
    let (images, nuisances, firmware) = projected_body_images(&data);
    let projections = firmware_images(
        base,
        &images,
        &nuisances,
        &firmware,
        RgbSpace::Srgb,
        0.03,
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

fn deterministic_noise(state: &mut u64) -> f64 {
    *state ^= *state << 13;
    *state ^= *state >> 7;
    *state ^= *state << 17;
    ((*state >> 11) as f64 / (1u64 << 53) as f64) - 0.5
}

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
