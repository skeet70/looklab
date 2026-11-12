// SPDX-License-Identifier: AGPL-3.0-or-later
//! Per-sample trust weights reach the likelihood (docs/05 §6 region caps).

#![cfg(feature = "synth")]
#![allow(clippy::disallowed_types)]
// The long recovery cases are compiled only for the pre-release tier.
#![cfg_attr(not(feature = "long-tests"), allow(dead_code, unused_imports))]

use looklab_fit::synth::{self, LookSelector, SynthConfig};
use looklab_fit::{FitImage, FitOptions, fit};

fn pool(seed: u64, images: usize) -> Vec<FitImage> {
    let config = SynthConfig {
        seed,
        images,
        samples_per_image: 500,
        noise_ucs: 0.6,
        ..SynthConfig::default()
    };
    let truth = synth::truth_look(seed);
    synth::dataset_with(&config, &truth, &LookSelector::Single(&truth)).images
}

#[cfg(feature = "long-tests")]
#[test]
fn zero_sample_trust_removes_an_image_from_the_likelihood() {
    let options = FitOptions {
        seed: 3,
        ..FitOptions::default()
    };
    let mut images = pool(21, 3);
    let hint = images[2].exposure_hint;
    let n = images[2].samples.n();
    images[2].sample_trust = Some(vec![0.0; n]);
    let fitted = fit(&images, &options).expect("muted");

    // No likelihood contribution means no gradient on the image's exposure nuisance, so it
    // stays at its prior centre.
    assert!(
        (fitted.images[2].exposure - hint).abs() < 1e-9,
        "muted image moved its exposure from {hint} to {}",
        fitted.images[2].exposure
    );
    let moved = fitted
        .images
        .iter()
        .take(2)
        .zip(images.iter().take(2))
        .map(|(fitted, image)| (fitted.exposure - image.exposure_hint).abs())
        .fold(0.0_f64, f64::max);
    assert!(moved > 1e-3, "the weighted images did not move at all");
}

#[test]
fn sample_trust_must_match_the_sample_count() {
    let mut images = pool(22, 2);
    images[0].sample_trust = Some(vec![1.0; 3]);
    let error = fit(&images, &FitOptions::default()).expect_err("length check");
    assert!(matches!(
        error,
        looklab_fit::FitError::SampleTrustLength {
            image: 0,
            found: 3,
            expected: 500
        }
    ));
}

#[cfg(feature = "long-tests")]
#[test]
fn down_weighted_samples_pull_less() {
    let options = FitOptions {
        seed: 4,
        run_gcv: false,
        ..FitOptions::default()
    };
    let mut images = pool(23, 3);
    let plain = fit(&images, &options).expect("plain");
    // Mute every sample whose scene is not near neutral, so only the neutral ones inform it.
    for image in &mut images {
        let n = image.samples.n();
        let mut trust = vec![1.0_f32; n];
        for (index, value) in trust.iter_mut().enumerate() {
            if index % 4 != 0 {
                *value = 0.01;
            }
        }
        image.sample_trust = Some(trust);
    }
    let shaded = fit(&images, &options).expect("shaded");
    let left = plain.look.coef_vector();
    let right = shaded.look.coef_vector();
    let changed = left
        .iter()
        .zip(right.iter())
        .map(|(a, b)| (a - b).abs())
        .fold(0.0_f64, f64::max);
    assert!(
        changed > 1e-3,
        "per-sample trust had no effect on the fit ({changed})"
    );
}
