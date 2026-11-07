// SPDX-License-Identifier: AGPL-3.0-or-later
//! Synthetic acceptance tests for the single-body fit (docs/09 P1.5).

#![cfg(feature = "synth")]
#![allow(clippy::disallowed_types)]

#[cfg(feature = "full-scale")]
use std::time::{Duration, Instant};

use looklab_color::ucs::delta_e_ucs;
use looklab_fit::synth::{self, LookSelector, SynthConfig};
use looklab_fit::{FitOptions, Verdict, fit};
#[cfg(feature = "full-scale")]
use looklab_model::LookInput;
use looklab_model::probes::v1 as probes_v1;

fn options(seed: u64) -> FitOptions {
    FitOptions {
        seed,
        ..FitOptions::default()
    }
}

#[cfg(feature = "full-scale")]
fn layered_options(seed: u64) -> FitOptions {
    FitOptions {
        seed,
        fit_residual: false,
        // 20 x 10k input: the default 2M sample budget would stop before the
        // G/R/Λ stage, making the layer-separation comparison vacuous.
        sweep_budget: 20_000_000.0,
        ..FitOptions::default()
    }
}

fn percentiles(mut values: Vec<f64>) -> (f64, f64) {
    values.sort_by(f64::total_cmp);
    let median = values[values.len() / 2];
    let p95 = values[(values.len() * 95 / 100).min(values.len() - 1)];
    (median, p95)
}

fn gauge(result: &looklab_fit::FitResult, data: &synth::SyntheticDataset) -> f64 {
    let mut shifts: Vec<f64> = result
        .images
        .iter()
        .enumerate()
        .map(|(index, image)| image.exposure - data.nuisances[index].exposure)
        .collect();
    shifts.sort_by(f64::total_cmp);
    shifts[shifts.len() / 2]
}

/// Held-out ΔE between the fitted and true latent Look at noise-free scene colours.
fn held_out_latent_errors(
    result: &looklab_fit::FitResult,
    training: &synth::SyntheticDataset,
    held: &synth::SyntheticDataset,
) -> Vec<f64> {
    // The absolute exposure gauge is a definition.
    let shift = libm::pow(2.0, gauge(result, training));
    held.points
        .iter()
        .flatten()
        .map(|xyz| delta_e_ucs(result.look.eval(xyz.scale(shift)), held.truth.eval(*xyz)))
        .collect()
}

/// Supported probes whose latent error lies inside twice the published post-clip σ.
fn supported_coverage(
    result: &looklab_fit::FitResult,
    data: &synth::SyntheticDataset,
) -> (usize, f64) {
    let shift = libm::pow(2.0, gauge(result, data));
    let mut count = 0usize;
    let mut inside = 0usize;
    for (probe, estimate) in probes_v1().probes.iter().zip(result.probes.probes.iter()) {
        if !estimate.supported {
            continue;
        }
        count += 1;
        let truth = looklab_fit::post_clip(
            &data.truth,
            probe.xyz,
            result.output_space,
            data.truth.clip.k,
        );
        let predicted = looklab_fit::post_clip(
            &result.look,
            probe.xyz.scale(shift),
            result.output_space,
            result.realization.clip_k,
        );
        if delta_e_ucs(predicted, truth) <= 2.0 * estimate.post_sigma_de {
            inside += 1;
        }
    }
    (count, inside as f64 / count.max(1) as f64)
}

fn training(seed: u64) -> SynthConfig {
    SynthConfig {
        seed,
        images: 20,
        samples_per_image: 1000,
        ..SynthConfig::default()
    }
}

#[test]
fn held_out_scenes_are_close() {
    let config = training(11);
    let data = synth::dataset(&config);
    let result = fit(&data.images, &options(3)).expect("fit");
    let held = synth::dataset_with(
        &SynthConfig {
            seed: 99,
            images: 3,
            samples_per_image: 900,
            ..config
        },
        &data.truth,
        &LookSelector::Single(&data.truth),
    );
    let (median, p95) = percentiles(held_out_latent_errors(&result, &data, &held));
    println!("held-out median {median:.3} p95 {p95:.3}");
    assert!(median <= 1.5, "median {median}, p95 {p95}");
    assert!(p95 <= 4.0, "median {median}, p95 {p95}");
}

#[test]
fn mislabelled_image_is_quarantined() {
    let config = SynthConfig {
        seed: 21,
        images: 8,
        samples_per_image: 500,
        ..SynthConfig::default()
    };
    let truth = synth::truth_look(config.seed);
    let other = synth::perturbed_look(&truth);
    let data = synth::dataset_with(
        &config,
        &truth,
        &LookSelector::Replace {
            image: 3,
            look: &other,
        },
    );
    let result = fit(&data.images, &options(5)).expect("fit");
    let stats = &result.image_stats[3];
    assert!(
        stats.z > 3.5,
        "z {} stats {:?}",
        stats.z,
        result.image_stats
    );
    assert_eq!(stats.verdict, Verdict::Quarantine);
}

#[cfg(feature = "full-scale")]
#[test]
#[ignore = "tone leak identity ablation; run with --ignored --nocapture"]
fn tone_leak_identity_ablation() {
    let config = SynthConfig {
        seed: 31,
        images: 20,
        samples_per_image: 10_000,
        ..SynthConfig::default()
    };
    let truth = synth::truth_look(config.seed);
    let variant = synth::tone_only_variant(&truth);
    let baseline = synth::dataset_with(&config, &truth, &LookSelector::Single(&truth));
    let shifted = synth::dataset_with(&config, &variant, &LookSelector::Single(&variant));
    for scale in [1.0_f64, 0.0] {
        let options = FitOptions {
            identity_scale: scale,
            ..layered_options(7)
        };
        let a = fit(&baseline.images, &options).expect("fit a");
        let b = fit(&shifted.images, &options).expect("fit b");
        let left_shift = libm::pow(2.0, gauge(&a, &baseline));
        let right_shift = libm::pow(2.0, gauge(&b, &shifted));
        let mut worst_gain: f64 = 0.0;
        let mut worst_rotation: f64 = 0.0;
        let mut worst_lightness: f64 = 0.0;
        for probe in probes_v1().probes {
            let left = a
                .look
                .layer_values(&LookInput::new(probe.xyz.scale(left_shift)));
            let right = b
                .look
                .layer_values(&LookInput::new(probe.xyz.scale(right_shift)));
            worst_gain = worst_gain.max((left.log_g - right.log_g).abs());
            worst_rotation = worst_rotation.max((left.r - right.r).abs());
            worst_lightness = worst_lightness.max((left.lambda - right.lambda).abs());
        }
        eprintln!(
            "identity scale {scale}: worst log G {worst_gain:.4}, rotation {worst_rotation:.3}, lightness {worst_lightness:.3}, lambdas {:.2}/{:.2}",
            a.lambdas.grl, b.lambdas.grl
        );
    }
}

#[cfg(feature = "full-scale")]
#[test]
fn tone_only_difference_leaves_the_colour_layers_alone() {
    let config = SynthConfig {
        seed: 31,
        images: 20,
        samples_per_image: 10_000,
        noise_ucs: 0.0,
        ..SynthConfig::default()
    };
    let truth = synth::truth_look(config.seed);
    let variant = synth::tone_only_variant(&truth);
    let baseline = synth::dataset_with(&config, &truth, &LookSelector::Single(&truth));
    let shifted = synth::dataset_with(&config, &variant, &LookSelector::Single(&variant));
    let a = fit(&baseline.images, &layered_options(7)).expect("fit a");
    let b = fit(&shifted.images, &layered_options(7)).expect("fit b");
    assert!(
        a.diagnostics.converged,
        "tone-free baseline did not converge"
    );
    assert!(b.diagnostics.converged, "tone-shifted fit did not converge");
    let mut worst_gain: f64 = 0.0;
    let mut worst_rotation: f64 = 0.0;
    let mut worst_lightness: f64 = 0.0;
    let mut reliable_gain: f64 = 0.0;
    let mut reliable_rotation: f64 = 0.0;
    let mut reliable_lightness: f64 = 0.0;
    let mut reliable = 0usize;
    let left_shift = libm::pow(2.0, gauge(&a, &baseline));
    let right_shift = libm::pow(2.0, gauge(&b, &shifted));
    // Compare where the training data reaches: at or above the median probe density.
    let mut densities: Vec<f64> = a.probes.probes.iter().map(|p| p.density).collect();
    densities.sort_by(f64::total_cmp);
    let threshold = densities[densities.len() / 2];
    let mut checked = 0usize;
    for (probe, estimate) in probes_v1().probes.iter().zip(a.probes.probes.iter()) {
        if estimate.density < threshold {
            continue;
        }
        let left = a
            .look
            .layer_values(&LookInput::new(probe.xyz.scale(left_shift)));
        let right = b
            .look
            .layer_values(&LookInput::new(probe.xyz.scale(right_shift)));
        checked += 1;
        let gain = (left.log_g - right.log_g).abs();
        let rotation = (left.r - right.r).abs();
        let lightness = (left.lambda - right.lambda).abs();
        worst_gain = worst_gain.max(gain);
        worst_rotation = worst_rotation.max(rotation);
        worst_lightness = worst_lightness.max(lightness);
        if estimate.pre_clip_reliable {
            reliable += 1;
            reliable_gain = reliable_gain.max(gain);
            reliable_rotation = reliable_rotation.max(rotation);
            reliable_lightness = reliable_lightness.max(lightness);
        }
    }
    assert!(checked >= 100, "only {checked} covered probes");
    println!("covered probes {checked}");
    // Each layer's tolerance is 2% of its range: log G, 0.02 × 90 deg, 0.02 × 100 J'.
    let peak = |result: &looklab_fit::FitResult| {
        let mut gain: f64 = 0.0;
        for probe in probes_v1().probes {
            let input = LookInput::new(probe.xyz);
            gain = gain.max(result.look.layer_values(&input).log_g.abs());
        }
        gain
    };
    println!(
        "worst layer differences {worst_gain:.6} {worst_rotation:.6} {worst_lightness:.6}; reliable {reliable} probes {reliable_gain:.6} {reliable_rotation:.6} {reliable_lightness:.6}; peak |logG| {:.3} vs {:.3}; conv {} {}; lambdas a {:?} b {:?}",
        peak(&a),
        peak(&b),
        a.diagnostics.converged,
        b.diagnostics.converged,
        a.lambdas,
        b.lambdas
    );
    assert!(worst_gain < 0.02, "log G difference {worst_gain}");
    assert!(
        worst_rotation < 1.8,
        "rotation difference {worst_rotation} deg"
    );
    assert!(
        worst_lightness < 2.0,
        "lightness difference {worst_lightness}"
    );
}

#[test]
fn noisy_tone_fixtures_recover_their_latent_looks() {
    let config = SynthConfig {
        seed: 31,
        images: 20,
        samples_per_image: 10_000,
        noise_ucs: 1.0,
        nu: 4.0,
        ..SynthConfig::default()
    };
    let truth = synth::truth_look(config.seed);
    let variant = synth::tone_only_variant(&truth);
    let baseline = synth::dataset_with(&config, &truth, &LookSelector::Single(&truth));
    let shifted = synth::dataset_with(&config, &variant, &LookSelector::Single(&variant));
    let a = fit(&baseline.images, &layered_options(7)).expect("fit a");
    let b = fit(&shifted.images, &layered_options(7)).expect("fit b");
    let held_config = SynthConfig {
        seed: 99,
        images: 3,
        samples_per_image: 900,
        ..config
    };
    let held_baseline = synth::dataset_with(&held_config, &truth, &LookSelector::Single(&truth));
    let held_shifted = synth::dataset_with(&held_config, &variant, &LookSelector::Single(&variant));
    for (label, result, training, held) in [
        ("baseline", &a, &baseline, &held_baseline),
        ("tone-shifted", &b, &shifted, &held_shifted),
    ] {
        assert!(result.diagnostics.converged, "{label} did not converge");
        let (median, p95) = percentiles(held_out_latent_errors(result, training, held));
        let (covered, coverage) = supported_coverage(result, training);
        let mut biases: Vec<f64> = result
            .probes
            .probes
            .iter()
            .filter(|probe| probe.supported)
            .map(|probe| probe.bias_de)
            .collect();
        biases.sort_by(f64::total_cmp);
        println!(
            "{label}: held latent median {median:.3} p95 {p95:.3}; supported {covered} coverage {coverage:.3}; bias median {:.3} p90 {:.3}",
            biases.get(biases.len() / 2).copied().unwrap_or(0.0),
            biases.get(biases.len() * 9 / 10).copied().unwrap_or(0.0),
        );
        assert!(median <= 1.5, "{label} latent median {median}, p95 {p95}");
        assert!(p95 <= 4.0, "{label} latent median {median}, p95 {p95}");
        assert!(covered >= 20, "{label} supported probes {covered}");
        assert!(
            coverage >= 0.90,
            "{label} coverage {coverage} over {covered} supported probes"
        );
    }
}

#[test]
fn sandwich_covers_the_true_probe_error() {
    let config = SynthConfig {
        seed: 51,
        images: 14,
        samples_per_image: 1200,
        ..SynthConfig::default()
    };
    let data = synth::dataset(&config);
    let result = fit(&data.images, &options(9)).expect("fit");
    let probes = probes_v1();
    let mut inside = 0usize;
    let mut missed = 0usize;
    let mut count = 0usize;
    let shift = libm::pow(2.0, gauge(&result, &data));
    for (probe, estimate) in probes.probes.iter().zip(result.probes.probes.iter()) {
        if !estimate.supported {
            continue;
        }
        count += 1;
        let space = result.output_space;
        let truth = looklab_fit::post_clip(&data.truth, probe.xyz, space, data.truth.clip.k);
        let predicted = looklab_fit::post_clip(
            &result.look,
            probe.xyz.scale(shift),
            space,
            result.realization.clip_k,
        );
        let error = delta_e_ucs(predicted, truth);
        if error > 2.0 * estimate.post_sigma_de && missed < 12 {
            println!(
                "  miss {} error {:.3} sigma {:.3} density {:.2} floor {:.2}",
                probe.id.as_str(),
                error,
                estimate.post_sigma_de,
                estimate.density,
                result.density.floor
            );
            missed += 1;
        }
        if error <= 2.0 * estimate.post_sigma_de {
            inside += 1;
        }
    }
    let fraction = inside as f64 / count.max(1) as f64;
    let support = result.probes.probes.iter().filter(|p| p.supported).count();
    let mut biases: Vec<f64> = result.probes.probes.iter().map(|p| p.bias_de).collect();
    biases.sort_by(f64::total_cmp);
    let causes = |take: fn(&looklab_fit::ProbeEstimate) -> bool| {
        result.probes.probes.iter().filter(|p| take(p)).count()
    };
    println!(
        "sandwich coverage {inside}/{count} = {fraction:.3}; total {support}/{}; bias median {:.3} p90 {:.3}; sigma {} density {} clusters {} bias {}",
        result.probes.probes.len(),
        biases[biases.len() / 2],
        biases[biases.len() * 9 / 10],
        causes(|p| p.supported_sigma),
        causes(|p| p.supported_density),
        causes(|p| p.supported_clusters),
        causes(|p| p.supported_bias),
    );
    assert!(count >= 20, "only {count} supported probes");
    // The post-clip covariance includes the clip-k uncertainty, which dominates near
    // the shoulders; measured 0.977 over 389 supported probes.
    assert!(fraction >= 0.90, "coverage {fraction}");
}

#[test]
fn repeated_fits_are_bit_identical() {
    let config = SynthConfig {
        seed: 61,
        images: 4,
        samples_per_image: 400,
        ..SynthConfig::default()
    };
    let data = synth::dataset(&config);
    let first = fit(&data.images, &options(13)).expect("fit a");
    let second = fit(&data.images, &options(13)).expect("fit b");
    let left = first.look.coef_vector();
    let right = second.look.coef_vector();
    assert_eq!(left.len(), right.len());
    for (a, b) in left.iter().zip(right.iter()) {
        assert_eq!(a.to_bits(), b.to_bits());
    }
}

#[cfg(feature = "full-scale")]
#[test]
#[ignore = "release-mode timing measurement"]
fn twenty_images_of_ten_thousand_samples_under_a_minute() {
    let config = SynthConfig {
        seed: 71,
        images: 20,
        samples_per_image: 10_000,
        ..SynthConfig::default()
    };
    let data = synth::dataset(&config);
    let start = Instant::now();
    let result = fit(&data.images, &options(17)).expect("fit");
    let elapsed = start.elapsed();
    println!(
        "20 x 10k fit: {:?} ({} samples, {} sweeps, cost {:.3})",
        elapsed,
        result.diagnostics.objective_samples,
        result.diagnostics.sweeps,
        result.diagnostics.final_cost
    );
    assert!(elapsed < Duration::from_secs(60), "{elapsed:?}");
}
