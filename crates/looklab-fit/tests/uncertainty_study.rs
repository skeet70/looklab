// SPDX-License-Identifier: AGPL-3.0-or-later
//! Recovery study for the identity prior, covariance floor and prior-bias support
//! (docs/02 §5).

#![cfg(feature = "synth")]
#![allow(clippy::disallowed_types)]

use looklab_color::ucs::delta_e_ucs;
use looklab_fit::synth::{LookSelector, SynthConfig, dataset_with, truth_look};
use looklab_fit::{FitOptions, FitResult, Look, fit};
use looklab_model::Tone;
use looklab_model::probes::v1 as probes_v1;

const IMAGE_COUNTS: [usize; 4] = [3, 5, 10, 20];

fn injected(seed: u64, gain: f64, hue: f64, shoulder: f64) -> Look {
    let mut look = truth_look(seed);
    for value in &mut look.color.g.plane {
        *value += gain;
    }
    for value in &mut look.color.g.volume {
        *value += gain;
    }
    for value in &mut look.color.r.plane {
        *value += hue;
    }
    for value in &mut look.color.r.volume {
        *value += hue;
    }
    if shoulder != 1.0
        && let Tone::Spline { beta, .. } = &mut look.tone
    {
        let start = beta.len() * 3 / 4;
        for value in beta.iter_mut().skip(start) {
            *value *= shoulder;
        }
    }
    look
}

fn options(before: bool) -> FitOptions {
    FitOptions {
        seed: 0x5eed_0f0f,
        max_samples_per_image: 800,
        hessian_samples: 3000,
        max_sweeps: 128,
        sweep_budget: 2_400_000.0,
        density_floor: 0.0,
        ..if before {
            FitOptions {
                prior_floor: false,
                bias_fraction: 1e9,
                support_sigma_exit: 1.5,
                ..FitOptions::default()
            }
        } else {
            FitOptions::default()
        }
    }
}

fn gauge(result: &FitResult, data: &looklab_fit::synth::SyntheticDataset) -> f64 {
    let mut shifts: Vec<f64> = result
        .images
        .iter()
        .enumerate()
        .map(|(index, image)| image.exposure - data.nuisances[index].exposure)
        .collect();
    shifts.sort_by(f64::total_cmp);
    shifts[shifts.len() / 2]
}

fn metrics(
    result: &FitResult,
    look: &Look,
    data: &looklab_fit::synth::SyntheticDataset,
) -> (f64, f64, f64) {
    let probe_set = probes_v1();
    let space = result.output_space;
    let clip = result.realization.clip_k;
    let shift = libm::pow(2.0, gauge(result, data));
    let mut bias = Vec::new();
    let mut covered = 0usize;
    let mut supported = 0usize;
    for (index, probe) in probe_set.probes.iter().enumerate() {
        let estimate = &result.probes.probes[index];
        if !estimate.supported {
            continue;
        }
        supported += 1;
        let truth = looklab_fit::post_clip(look, probe.xyz, space, look.clip.k);
        let predicted = looklab_fit::post_clip(&result.look, probe.xyz.scale(shift), space, clip);
        let error = delta_e_ucs(predicted, truth);
        bias.push(error);
        if error <= 2.0 * estimate.post_sigma_de {
            covered += 1;
        }
    }
    bias.sort_by(f64::total_cmp);
    let median = bias.get(bias.len() / 2).copied().unwrap_or(0.0);
    let coverage = covered as f64 / supported.max(1) as f64;
    let unsupported = 1.0 - supported as f64 / probe_set.probes.len() as f64;
    (median, coverage, unsupported)
}

fn post_errors_k(
    result: &FitResult,
    look: &Look,
    data: &looklab_fit::synth::SyntheticDataset,
    indices: &[usize],
    clip: f64,
) -> Vec<f64> {
    let probe_set = probes_v1();
    let space = result.output_space;
    let shift = libm::pow(2.0, gauge(result, data));
    indices
        .iter()
        .map(|index| {
            let truth =
                looklab_fit::post_clip(look, probe_set.probes[*index].xyz, space, look.clip.k);
            let predicted = looklab_fit::post_clip(
                &result.look,
                probe_set.probes[*index].xyz.scale(shift),
                space,
                clip,
            );
            delta_e_ucs(predicted, truth)
        })
        .collect()
}

fn post_errors(
    result: &FitResult,
    look: &Look,
    data: &looklab_fit::synth::SyntheticDataset,
    indices: &[usize],
) -> Vec<f64> {
    post_errors_k(result, look, data, indices, result.realization.clip_k)
}

fn median(values: &mut [f64]) -> f64 {
    values.sort_by(f64::total_cmp);
    values.get(values.len() / 2).copied().unwrap_or(0.0)
}

#[test]
#[ignore = "diagnostic study; run with --ignored --nocapture"]
fn velvia_bias_diagnostic() {
    let look = injected(0x51e2_0101, 0.47, 4.0, 1.0);
    let all: Vec<usize> = (0..probes_v1().probes.len()).collect();
    let mut fixed: Option<Vec<usize>> = None;
    for images in IMAGE_COUNTS {
        let config = SynthConfig {
            seed: 0x51e2_0200 + images as u64,
            images,
            samples_per_image: 800,
            noise_ucs: 0.35,
            ..SynthConfig::default()
        };
        let data = dataset_with(&config, &look, &LookSelector::Single(&look));
        let full = fit(&data.images, &options(false)).expect("full fit");
        let nosm = fit(
            &data.images,
            &FitOptions {
                run_gcv: false,
                ..options(false)
            },
        )
        .expect("nosm fit");
        let supported: Vec<usize> = full
            .probes
            .probes
            .iter()
            .enumerate()
            .filter(|(_, estimate)| estimate.supported)
            .map(|(index, _)| index)
            .collect();
        if fixed.is_none() {
            fixed = Some(supported.clone());
        }
        let fixed = fixed.clone().unwrap_or_default();
        let mut full_all = post_errors(&full, &look, &data, &all);
        let mut full_supported = post_errors(&full, &look, &data, &supported);
        let mut full_fixed = post_errors(&full, &look, &data, &fixed);
        let mut nosm_all = post_errors(&nosm, &look, &data, &all);
        let mut nosm_supported = post_errors(&nosm, &look, &data, &supported);
        let mut clipped = Vec::new();
        let mut unclipped = Vec::new();
        for index in &supported {
            let estimate = &full.probes.probes[*index];
            let error = post_errors(&full, &look, &data, &[*index])[0];
            if delta_e_ucs(estimate.ucs, estimate.post_ucs) > 0.01 {
                clipped.push(error);
            } else {
                unclipped.push(error);
            }
        }
        let mut identity_bias: Vec<f64> = full
            .probes
            .probes
            .iter()
            .filter(|estimate| estimate.supported)
            .map(|estimate| estimate.bias_de)
            .collect();
        eprintln!(
            "n={images:<2} full all {:.3} supported {:.3} fixed {:.3} | nosm all {:.3} supported {:.3} | clip-bias {:.3} ({} probes) unclipped {:.3} ({}), b_p {:.3}, k {:.4}, lam T{:.1} N{:.1} GRL{:.1} R{:.1}",
            median(&mut full_all),
            median(&mut full_supported),
            median(&mut full_fixed),
            median(&mut nosm_all),
            median(&mut nosm_supported),
            median(&mut clipped),
            clipped.len(),
            median(&mut unclipped),
            unclipped.len(),
            median(&mut identity_bias),
            full.realization.clip_k,
            full.lambdas.tone,
            full.lambdas.tint,
            full.lambdas.grl,
            full.lambdas.residual,
        );
    }
    let config = SynthConfig {
        seed: 0x51e2_0300,
        images: 20,
        samples_per_image: 1200,
        noise_ucs: 0.0,
        ..SynthConfig::default()
    };
    let data = dataset_with(&config, &look, &LookSelector::Single(&look));
    for (name, options) in [
        ("prior+smooth", options(false)),
        (
            "smooth-off",
            FitOptions {
                run_gcv: false,
                ..options(false)
            },
        ),
    ] {
        let result = fit(&data.images, &options).expect("noise-free fit");
        let mut errors = post_errors(&result, &look, &data, &all);
        eprintln!(
            "noise-free 20 images {name}: all-probe median {:.3}, k {:.4}, lam GRL {:.1}",
            median(&mut errors),
            result.realization.clip_k,
            result.lambdas.grl,
        );
    }
}

#[test]
#[ignore = "clip-k experiment; run with --ignored --nocapture"]
fn clip_prior_diagnostic() {
    let look = injected(0x51e2_0101, 0.47, 4.0, 1.0);
    let all: Vec<usize> = (0..probes_v1().probes.len()).collect();
    let config = SynthConfig {
        seed: 0x51e2_0214,
        images: 20,
        samples_per_image: 800,
        noise_ucs: 0.35,
        ..SynthConfig::default()
    };
    let data = dataset_with(&config, &look, &LookSelector::Single(&look));
    for (name, clip_prior) in [
        ("default", (0.03, 0.02)),
        ("tight", (0.03, 0.001)),
        ("offset", (0.09, 0.001)),
    ] {
        let result = fit(
            &data.images,
            &FitOptions {
                clip_prior,
                ..options(false)
            },
        )
        .expect("fit");
        let supported: Vec<usize> = result
            .probes
            .probes
            .iter()
            .enumerate()
            .filter(|(_, estimate)| estimate.supported)
            .map(|(index, _)| index)
            .collect();
        let mut all_errors = post_errors(&result, &look, &data, &all);
        let mut supported_errors = post_errors(&result, &look, &data, &supported);
        let mut gain_errors: Vec<f64> = all
            .iter()
            .map(|index| {
                let xyz = probes_v1().probes[*index].xyz;
                let truth = looklab_model::LookInput::new(xyz);
                let shift = libm::pow(2.0, gauge(&result, &data));
                let fitted = looklab_model::LookInput::new(xyz.scale(shift));
                (result.look.layer_values(&fitted).log_g - look.layer_values(&truth).log_g).abs()
            })
            .collect();
        eprintln!(
            "clip {name}: all {:.3}, supported {:.3} ({}), k {:.4}, |logG| {:.3}, lam GRL {:.1}",
            median(&mut all_errors),
            median(&mut supported_errors),
            supported.len(),
            result.realization.clip_k,
            median(&mut gain_errors),
            result.lambdas.grl
        );
    }
}

#[test]
#[ignore = "convergence experiment; run with --ignored --nocapture"]
fn convergence_diagnostic() {
    let look = injected(0x51e2_0101, 0.47, 4.0, 1.0);
    let all: Vec<usize> = (0..probes_v1().probes.len()).collect();
    for (name, noise) in [("noise-free", 0.0), ("noisy", 0.35)] {
        let config = SynthConfig {
            seed: 0x51e2_0400,
            images: 20,
            samples_per_image: 800,
            noise_ucs: noise,
            ..SynthConfig::default()
        };
        let data = dataset_with(&config, &look, &LookSelector::Single(&look));
        for (label, sweeps, budget) in [
            ("8 sweeps", 8, 1_000_000.0),
            ("40 sweeps", 40, 10_000_000.0),
        ] {
            let mut options = options(false);
            options.max_sweeps = sweeps;
            options.sweep_budget = budget;
            options.hessian_samples = 8000;
            let result = fit(&data.images, &options).expect("fit");
            let mut errors = post_errors(&result, &look, &data, &all);
            eprintln!(
                "{name} {label}: all {:.3}, k {:.4}, |logG| {:.3}, sweeps {}",
                median(&mut errors),
                result.realization.clip_k,
                {
                    let mut gains: Vec<f64> = all
                        .iter()
                        .map(|index| {
                            let xyz = probes_v1().probes[*index].xyz;
                            let shift = libm::pow(2.0, gauge(&result, &data));
                            (result
                                .look
                                .layer_values(&looklab_model::LookInput::new(xyz.scale(shift)))
                                .log_g
                                - look.layer_values(&looklab_model::LookInput::new(xyz)).log_g)
                                .abs()
                        })
                        .collect();
                    median(&mut gains)
                },
                result.diagnostics.sweeps,
            );
        }
    }
    let config = SynthConfig {
        seed: 0x51e2_0400,
        images: 20,
        samples_per_image: 800,
        noise_ucs: 0.0,
        ..SynthConfig::default()
    };
    let data = dataset_with(&config, &look, &LookSelector::Single(&look));
    for grl in [0.5_f64, 2.0, 6.5, 20.0, 60.0] {
        let result = fit(
            &data.images,
            &FitOptions {
                run_gcv: false,
                initial_lambdas: looklab_fit::StageLambdas {
                    tone: 0.1,
                    tint: 2.0,
                    grl,
                    ..looklab_fit::StageLambdas::default()
                },
                ..options(false)
            },
        )
        .expect("lambda fit");
        let mut errors = post_errors(&result, &look, &data, &all);
        let mut fixed = post_errors_k(&result, &look, &data, &all, 0.03);
        eprintln!(
            "noise-free lambda grl {grl}: all {:.3}, fixed-k {:.3}, k {:.4}, |logG| {:.3}",
            median(&mut errors),
            median(&mut fixed),
            result.realization.clip_k,
            {
                let mut gains: Vec<f64> = all
                    .iter()
                    .map(|index| {
                        let xyz = probes_v1().probes[*index].xyz;
                        let shift = libm::pow(2.0, gauge(&result, &data));
                        (result
                            .look
                            .layer_values(&looklab_model::LookInput::new(xyz.scale(shift)))
                            .log_g
                            - look.layer_values(&looklab_model::LookInput::new(xyz)).log_g)
                            .abs()
                    })
                    .collect();
                median(&mut gains)
            },
        );
    }
    let result = fit(
        &data.images,
        &FitOptions {
            run_gcv: false,
            fit_residual: false,
            initial_lambdas: looklab_fit::StageLambdas {
                tone: 0.1,
                tint: 2.0,
                grl: 6.5,
                ..looklab_fit::StageLambdas::default()
            },
            ..options(false)
        },
    )
    .expect("no-residual fit");
    let mut errors = post_errors(&result, &look, &data, &all);
    eprintln!(
        "noise-free no-residual: all {:.3}, k {:.4}, |logG| {:.3}",
        median(&mut errors),
        result.realization.clip_k,
        {
            let mut gains: Vec<f64> = all
                .iter()
                .map(|index| {
                    let xyz = probes_v1().probes[*index].xyz;
                    let shift = libm::pow(2.0, gauge(&result, &data));
                    (result
                        .look
                        .layer_values(&looklab_model::LookInput::new(xyz.scale(shift)))
                        .log_g
                        - look.layer_values(&looklab_model::LookInput::new(xyz)).log_g)
                        .abs()
                })
                .collect();
            median(&mut gains)
        },
    );
}

#[test]
#[ignore = "singular repro; run with --ignored --nocapture"]
fn singular_repro() {
    let data = looklab_fit::synth::dataset(&SynthConfig {
        seed: 21,
        images: 8,
        samples_per_image: 500,
        ..SynthConfig::default()
    });
    let result = fit(&data.images, &FitOptions::default()).expect("fit");
    eprintln!(
        "fit ok, supported {}",
        result.probes.probes.iter().filter(|p| p.supported).count()
    );
}

#[test]
#[ignore = "controlled singular repro; run with --ignored --nocapture"]
fn singular_controlled_repro() {
    let schema = looklab_schema::validate_control_schema(include_str!(
        "../../../data/bodies/fujifilm-x-t5.yaml"
    ))
    .expect("schema");
    let data = looklab_fit::synth::dataset(&SynthConfig {
        seed: 21,
        images: 8,
        samples_per_image: 500,
        ..SynthConfig::default()
    });
    let result = looklab_fit::fit_controlled(
        &data.images,
        &schema,
        1,
        &looklab_schema::SimKey::new("classic_chrome").expect("sim"),
        &FitOptions::default(),
    )
    .expect("controlled fit");
    eprintln!(
        "controlled fit ok, supported {}",
        result.probes.probes.iter().filter(|p| p.supported).count()
    );
}

#[test]
#[ignore = "convergence shapes; run with --ignored --nocapture"]
fn convergence_shapes() {
    for (images, samples) in [(2usize, 200usize), (4, 300), (6, 400), (8, 500)] {
        let data = looklab_fit::synth::dataset(&SynthConfig {
            seed: 21,
            images,
            samples_per_image: samples,
            ..SynthConfig::default()
        });
        let result = fit(
            &data.images,
            &FitOptions {
                seed: 0x5eed_1a5e,
                ..FitOptions::default()
            },
        )
        .expect("fit");
        eprintln!(
            "{images}x{samples}: converged {} evaluations {} cost {:.1}",
            result.diagnostics.converged,
            result.diagnostics.evaluations,
            result.diagnostics.final_cost
        );
    }
}

#[test]
fn convergence_removes_the_noise_free_bias_floor() {
    let look = injected(0x51e2_0101, 0.47, 4.0, 1.0);
    let config = SynthConfig {
        seed: 0x51e2_0700,
        images: 10,
        samples_per_image: 800,
        noise_ucs: 0.0,
        ..SynthConfig::default()
    };
    let data = dataset_with(&config, &look, &LookSelector::Single(&look));
    let result = fit(&data.images, &options(false)).expect("fit");
    let all: Vec<usize> = (0..probes_v1().probes.len()).collect();
    let mut errors = post_errors(&result, &look, &data, &all);
    let bias = median(&mut errors);
    eprintln!(
        "noise-free 10 images: all-probe bias {bias:.3}, converged {}",
        result.diagnostics.converged
    );
    // `converged` stays false when the sweep cap or sample budget binds; the
    // regression is the bias floor, not a stationary point on this fixture.
    assert!(bias <= 0.35, "bias {bias}");
}

#[test]
#[ignore = "thin coverage diagnostic; run with --ignored --nocapture"]
fn thin_coverage_diagnostic() {
    let look = injected(0x51e2_0101, 0.47, 4.0, 1.0);
    let config = SynthConfig {
        seed: 0x51e2_0800,
        images: 20,
        samples_per_image: 800,
        noise_ucs: 0.35,
        ..SynthConfig::default()
    };
    let data = dataset_with(&config, &look, &LookSelector::Single(&look));
    let all: Vec<usize> = (0..probes_v1().probes.len()).collect();
    let full = fit(&data.images, &options(false)).expect("full fit");
    let plain = fit(
        &data.images,
        &FitOptions {
            prior_floor: false,
            ..options(false)
        },
    )
    .expect("plain fit");
    let mut classes: [Vec<usize>; 3] = [Vec::new(), Vec::new(), Vec::new()];
    for index in &all {
        let probe = &full.probes.probes[*index];
        if probe.density < 0.05 {
            classes[0].push(*index);
        }
        if probe.clusters <= 3 {
            classes[1].push(*index);
        }
        if probe.supported_sigma && !probe.supported {
            classes[2].push(*index);
        }
    }
    let errors = |result: &FitResult| post_errors(result, &look, &data, &all);
    let full_errors = errors(&full);
    let plain_errors = errors(&plain);
    for (name, indices) in [
        ("low density", &classes[0]),
        ("<=3 clusters", &classes[1]),
        ("support boundary", &classes[2]),
    ] {
        for (label, result, errors) in [
            ("full", &full, &full_errors),
            ("plain", &plain, &plain_errors),
        ] {
            let mut inside = 0usize;
            for index in indices {
                if errors[*index] <= 2.0 * result.probes.probes[*index].post_sigma_de {
                    inside += 1;
                }
            }
            eprintln!(
                "thin {name} {label}: {} probes, coverage {:.3}",
                indices.len(),
                inside as f64 / indices.len().max(1) as f64
            );
        }
    }
}

#[test]
#[ignore = "bias floor diagnostic; run with --ignored --nocapture"]
fn bias_floor_diagnostic() {
    let look = injected(0x51e2_0101, 0.47, 4.0, 1.0);
    let all: Vec<usize> = (0..probes_v1().probes.len()).collect();
    for images in [5usize, 20] {
        let config = SynthConfig {
            seed: 0x51e2_0600 + images as u64,
            images,
            samples_per_image: 800,
            noise_ucs: 0.0,
            ..SynthConfig::default()
        };
        let data = dataset_with(&config, &look, &LookSelector::Single(&look));
        let result = fit(&data.images, &options(false)).expect("fit");
        let errors = post_errors(&result, &look, &data, &all);
        let probe_set = probes_v1();
        let mut groups: std::collections::BTreeMap<String, Vec<f64>> = Default::default();
        let mut data_bins = Vec::new();
        let mut empty_bins = Vec::new();
        for index in &all {
            let input = looklab_model::LookInput::new(probe_set.probes[*index].xyz);
            groups
                .entry(format!("{:?}", probe_set.probes[*index].group))
                .or_default()
                .push(errors[*index]);
            if result.density.at(input.s, input.h_in, input.c_in) >= 0.01 {
                data_bins.push(errors[*index]);
            } else {
                empty_bins.push(errors[*index]);
            }
        }
        let mut ratios = result.diagnostics.grl_precision_ratio.clone();
        ratios.sort_by(f64::total_cmp);
        let ratio_at = |fraction: f64| {
            ratios
                .get((ratios.len() as f64 * fraction) as usize)
                .copied()
                .unwrap_or(0.0)
        };
        let below =
            ratios.iter().filter(|value| **value < 1.0).count() as f64 / ratios.len().max(1) as f64;
        eprintln!(
            "n={images}: data-bin bias {:.3} ({}), empty-bin bias {:.3} ({}); precision ratio p10 {:.3} median {:.3} p90 {:.3} below-1 {:.3}",
            median(&mut data_bins),
            data_bins.len(),
            median(&mut empty_bins),
            empty_bins.len(),
            ratio_at(0.1),
            ratio_at(0.5),
            ratio_at(0.9),
            below,
        );
        for (group, mut values) in groups {
            eprintln!(
                "  group {group}: median {:.3} ({} probes)",
                median(&mut values),
                values.len()
            );
        }
    }
    let config = SynthConfig {
        seed: 0x51e2_0614,
        images: 20,
        samples_per_image: 800,
        noise_ucs: 0.0,
        ..SynthConfig::default()
    };
    let data = dataset_with(&config, &look, &LookSelector::Single(&look));
    for n_eff in [500.0_f64, 5000.0] {
        let result = fit(
            &data.images,
            &FitOptions {
                n_eff,
                ..options(false)
            },
        )
        .expect("fit");
        let mut errors = post_errors(&result, &look, &data, &all);
        let mut ratios = result.diagnostics.grl_precision_ratio.clone();
        ratios.sort_by(f64::total_cmp);
        eprintln!(
            "n=20 noise-free n_eff {n_eff}: all bias {:.3}, sweeps {}, converged {}, ratio median {:.3}, priors dominate {:.3}",
            median(&mut errors),
            result.diagnostics.sweeps,
            result.diagnostics.converged,
            ratios.get(ratios.len() / 2).copied().unwrap_or(0.0),
            ratios.iter().filter(|value| **value < 1.0).count() as f64 / ratios.len().max(1) as f64,
        );
    }
}

#[test]
#[ignore = "sigma range check; run with --ignored --nocapture"]
fn sigma_range_diagnostic() {
    use looklab_fit::small_cluster_factor;
    let look = injected(0x51e2_0101, 0.47, 4.0, 1.0);
    for images in [2usize, 3, 5, 20] {
        let config = SynthConfig {
            seed: 0x51e2_0500,
            images,
            samples_per_image: 800,
            noise_ucs: 0.35,
            ..SynthConfig::default()
        };
        let data = dataset_with(&config, &look, &LookSelector::Single(&look));
        let result = fit(&data.images, &options(false)).expect("fit");
        let mut sigmas: Vec<f64> = result
            .probes
            .probes
            .iter()
            .map(|probe| probe.post_sigma_de)
            .collect();
        sigmas.sort_by(f64::total_cmp);
        let mut pre: Vec<f64> = result
            .probes
            .probes
            .iter()
            .map(|probe| probe.sigma_de)
            .collect();
        pre.sort_by(f64::total_cmp);
        let supported = result.probes.probes.iter().filter(|p| p.supported).count();
        eprintln!(
            "n={images}: post sigma min {:.4} median {:.3} max {:.3}; pre sigma min {:.4} max {:.3}; c {:.1}; supported {}",
            sigmas[0],
            sigmas[sigmas.len() / 2],
            sigmas[sigmas.len() - 1],
            pre[0],
            pre[pre.len() - 1],
            small_cluster_factor(images),
            supported,
        );
    }
}

#[test]
#[ignore = "bias threshold calibration; run with --ignored --nocapture"]
fn bias_threshold_diagnostic() {
    let look = injected(0x51e2_0101, 0.47, 4.0, 1.0);
    for images in [5usize, 10, 20] {
        let config = SynthConfig {
            seed: 0x51e2_0200 + images as u64,
            images,
            samples_per_image: 800,
            noise_ucs: 0.35,
            ..SynthConfig::default()
        };
        let data = dataset_with(&config, &look, &LookSelector::Single(&look));
        let result = fit(&data.images, &options(false)).expect("fit");
        let all: Vec<usize> = (0..probes_v1().probes.len()).collect();
        let errors = post_errors(&result, &look, &data, &all);
        for threshold in [0.495_f64, 0.4, 0.3, 0.25, 0.2] {
            let mut selected = Vec::new();
            let mut inside = 0usize;
            for index in &all {
                let estimate = &result.probes.probes[*index];
                if !(estimate.supported_sigma
                    && estimate.supported_density
                    && estimate.supported_clusters
                    && estimate.bias_de <= threshold)
                {
                    continue;
                }
                selected.push(errors[*index]);
                if errors[*index] <= 2.0 * estimate.post_sigma_de {
                    inside += 1;
                }
            }
            let coverage = inside as f64 / selected.len().max(1) as f64;
            eprintln!(
                "n={images:<2} bias<={threshold:.3}: supported {} unsup {:.3} bias {:.3} cov {:.3}",
                selected.len(),
                1.0 - selected.len() as f64 / all.len() as f64,
                median(&mut selected),
                coverage
            );
        }
    }
}

#[test]
#[ignore = "recovery study; run with --ignored --nocapture. Supported bias 0.10-0.27 dE, fixed 0.09-0.23, all-probe 0.23-0.32"]
fn identity_prior_recovery_study() {
    let looks = [
        ("velvia", injected(0x51e2_0101, 0.47, 4.0, 1.0)),
        ("classic-chrome", injected(0x51e2_0102, -0.2, -6.0, 0.85)),
    ];
    let mut worst_after = 0.0_f64;
    let mut worst_sigma = 0.0_f64;
    for (name, look) in &looks {
        let mut fixed: Option<Vec<usize>> = None;
        for images in IMAGE_COUNTS {
            let config = SynthConfig {
                seed: 0x51e2_0200 + images as u64,
                images,
                samples_per_image: 800,
                noise_ucs: 0.35,
                ..SynthConfig::default()
            };
            let data = dataset_with(&config, look, &LookSelector::Single(look));
            let before = fit(&data.images, &options(true)).expect("before fit");
            let after = fit(&data.images, &options(false)).expect("after fit");
            let (bias_before, cov_before, unsup_before) = metrics(&before, look, &data);
            let (bias_after, cov_after, unsup_after) = metrics(&after, look, &data);
            let mean_sigma: f64 = after
                .probes
                .probes
                .iter()
                .filter(|probe| probe.supported)
                .map(|probe| probe.post_sigma_de)
                .sum::<f64>()
                / after
                    .probes
                    .probes
                    .iter()
                    .filter(|probe| probe.supported)
                    .count()
                    .max(1) as f64;
            let supported: Vec<usize> = after
                .probes
                .probes
                .iter()
                .enumerate()
                .filter(|(_, probe)| probe.supported)
                .map(|(index, _)| index)
                .collect();
            if fixed.is_none() {
                fixed = Some(supported.clone());
            }
            let all: Vec<usize> = (0..probes_v1().probes.len()).collect();
            let mut all_errors = post_errors(&after, look, &data, &all);
            let mut fixed_errors = post_errors(&after, look, &data, fixed.as_ref().unwrap_or(&all));
            eprintln!(
                "{name:<14} n={images:<2} before bias {bias_before:.3} cov {cov_before:.3} unsup {unsup_before:.3} | after supported {bias_after:.3} cov {cov_after:.3} unsup {unsup_after:.3} all {:.3} fixed {:.3} sigma {mean_sigma:.3} sweeps {} converged {}",
                median(&mut all_errors),
                median(&mut fixed_errors),
                after.diagnostics.evaluations,
                after.diagnostics.converged,
            );
            if images >= 5 {
                worst_after = worst_after.max(bias_after);
                worst_sigma = worst_sigma.max(mean_sigma);
            }
        }
    }
    eprintln!(
        "decision: worst bias from 5 images {worst_after:.3} ΔE, worst mean sigma {worst_sigma:.3} ΔE"
    );
}
