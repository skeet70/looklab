// SPDX-License-Identifier: AGPL-3.0-or-later

use looklab_prep::{
    CoveragePool, Image3, MaskOptions, PreselectOptions, SampleParams, build_mask, preselect,
    select_pixels,
};
use rand_chacha::ChaCha20Rng;
use rand_chacha::rand_core::SeedableRng;

fn image(width: u32, height: u32, pixel: impl Fn(u32, u32) -> [f32; 3]) -> Image3 {
    let mut data = Vec::with_capacity((width * height) as usize);
    for y in 0..height {
        for x in 0..width {
            data.push(pixel(x, y));
        }
    }
    Image3::new(width, height, data).expect("image length matches dimensions")
}

fn rng(seed: u8) -> ChaCha20Rng {
    ChaCha20Rng::from_seed([seed; 32])
}

#[test]
fn mask_excludes_borders_clipping_and_non_finite_pixels() {
    let mut pixels: Vec<[f32; 3]> = vec![[0.5, 0.5, 0.5]; 100 * 100];
    pixels[40 * 100 + 40] = [0.99, 0.4, 0.4];
    pixels[60 * 100 + 60] = [f32::NAN, 0.4, 0.4];
    let image = Image3::new(100, 100, pixels).expect("dimensions match");

    let mut mask = build_mask(&image, &MaskOptions::default());
    assert_eq!(mask.counts().border, 100 * 100 - 94 * 94);
    assert_eq!(mask.counts().invalid, 1);
    assert!(!mask.is_kept(60, 60));

    let mut clipped = vec![false; 100 * 100];
    clipped[40 * 100 + 40] = true;
    mask.exclude_clipped(&clipped);
    assert_eq!(mask.counts().clipped, 1);
    assert!(!mask.is_kept(40, 40));
    assert!(mask.is_kept(30, 30));
}

#[test]
fn mask_excludes_high_gradient_pixels() {
    let image = image(64, 64, |x, _| {
        if x < 32 {
            [0.2, 0.2, 0.2]
        } else {
            [0.8, 0.8, 0.8]
        }
    });
    let mask = build_mask(&image, &MaskOptions::default());
    assert!(mask.counts().gradient > 0);
    assert!(mask.kept() < 60 * 60);
}

#[test]
fn a_non_finite_neighbourhood_does_not_poison_the_percentile() {
    let mut pixels: Vec<[f32; 3]> = vec![[0.5, 0.5, 0.5]; 32 * 32];
    pixels[16 * 32 + 16] = [f32::NAN, f32::NAN, f32::NAN];
    let image = Image3::new(32, 32, pixels).expect("dimensions match");
    let mask = build_mask(&image, &MaskOptions::default());

    assert_eq!(mask.counts().invalid, 1);
    assert_eq!(mask.counts().gradient, 4);
    assert!(mask.kept() > 800);
}

#[test]
fn sampling_is_deterministic_per_seed() {
    let image = image(64, 64, |x, y| {
        let ramp = (x + y) as f32 / 126.0;
        [0.9 - ramp, 0.2 + ramp, 0.4]
    });
    let mask = build_mask(&image, &MaskOptions::default());
    let matrix = looklab_color::spaces::RgbSpace::Srgb.to_xyz_matrix();

    let first = select_pixels(
        &image,
        &mask,
        &matrix,
        &SampleParams::default(),
        &mut rng(1),
    )
    .expect("samples");
    let repeat = select_pixels(
        &image,
        &mask,
        &matrix,
        &SampleParams::default(),
        &mut rng(1),
    )
    .expect("samples");
    assert_eq!(first, repeat);

    let other = select_pixels(
        &image,
        &mask,
        &matrix,
        &SampleParams::default(),
        &mut rng(2),
    )
    .expect("samples");
    assert_eq!(first.len(), other.len());
    assert_ne!(first.indices(), other.indices());
}

#[test]
fn selecting_every_candidate_gives_uniform_weights() {
    let image = image(48, 48, |x, y| {
        [0.1 + x as f32 / 100.0, 0.2, 0.3 + y as f32 / 100.0]
    });
    let mask = build_mask(&image, &MaskOptions::default());
    let matrix = looklab_color::spaces::RgbSpace::Srgb.to_xyz_matrix();
    let selection = select_pixels(
        &image,
        &mask,
        &matrix,
        &SampleParams::default(),
        &mut rng(3),
    )
    .expect("samples");

    assert_eq!(selection.len(), mask.kept());
    for weight in selection.density_w() {
        assert_eq!(*weight, 1.0);
    }
}

#[test]
fn a_capped_selection_stays_within_budget_and_records_inclusion_weights() {
    let image = image(96, 96, |x, y| {
        let ramp = (x + y) as f32 / 190.0;
        [0.9 - ramp, 0.2 + ramp, 0.35]
    });
    let mask = build_mask(&image, &MaskOptions::default());
    let matrix = looklab_color::spaces::RgbSpace::Srgb.to_xyz_matrix();
    let params = SampleParams {
        max_samples: 200,
        ..SampleParams::default()
    };
    let selection = select_pixels(&image, &mask, &matrix, &params, &mut rng(4)).expect("samples");

    assert!(!selection.is_empty());
    assert!(selection.len() <= 200);
    assert!(selection.len() < mask.kept());
    let mut above_one = false;
    for weight in selection.density_w() {
        assert!(weight.is_finite() && *weight >= 1.0);
        assert!(*weight <= looklab_samples::MAX_DENSITY_WEIGHT);
        above_one |= *weight > 1.0;
    }
    assert!(above_one);
}

#[test]
fn sampling_with_every_pixel_clipped_has_no_candidates() {
    let image = image(32, 32, |_, _| [1.0, 1.0, 1.0]);
    let mut mask = build_mask(&image, &MaskOptions::default());
    mask.exclude_clipped(&[true; 32 * 32]);
    let matrix = looklab_color::spaces::RgbSpace::Srgb.to_xyz_matrix();
    let result = select_pixels(
        &image,
        &mask,
        &matrix,
        &SampleParams::default(),
        &mut rng(5),
    );
    assert!(matches!(result, Err(looklab_prep::PrepError::NoSamples)));
}

#[test]
fn coverage_gain_orders_distinct_images_before_duplicates() {
    let red = image(32, 32, |x, _| {
        if x < 16 {
            [0.8, 0.1, 0.1]
        } else {
            [0.1, 0.8, 0.1]
        }
    });
    let duplicate = red.clone();
    let blue = image(32, 32, |x, _| {
        if x < 16 {
            [0.1, 0.1, 0.9]
        } else {
            [0.2, 0.7, 0.2]
        }
    });

    let options = PreselectOptions::default();
    let mut pool = CoveragePool::new(options).expect("valid options");
    assert!(pool.gain(&red) > 0.0);
    assert_eq!(pool.gain(&red), pool.gain(&duplicate));
    pool.add(&red);
    assert_eq!(pool.gain(&duplicate), 0.0);
    assert!(pool.gain(&blue) > 0.0);

    let chosen = preselect(&[red, duplicate, blue], options).expect("selects");
    assert_eq!(chosen, vec![0, 2]);
}

#[test]
fn coverage_gain_respects_the_selection_cap() {
    let options = PreselectOptions {
        max_selected: 2,
        ..PreselectOptions::default()
    };
    let thumbnails: Vec<Image3> = (0..5)
        .map(|index| {
            image(16, 16, |x, y| {
                let base = index as f32 / 8.0;
                [base, 0.1 + x as f32 / 32.0, 0.1 + y as f32 / 32.0]
            })
        })
        .collect();
    let chosen = preselect(&thumbnails, options).expect("selects");
    assert_eq!(chosen.len(), 2);
}

#[test]
fn preselection_rejects_degenerate_grids() {
    let options = PreselectOptions {
        hue_bins: 0,
        ..PreselectOptions::default()
    };
    assert!(matches!(
        CoveragePool::new(options),
        Err(looklab_prep::PrepError::InvalidParams { field: "hue_bins" })
    ));
    let options = PreselectOptions {
        exposure_bins: 1024,
        ..PreselectOptions::default()
    };
    assert!(CoveragePool::new(options).is_err());
}

#[test]
fn sample_params_reject_out_of_range_values() {
    let params = SampleParams {
        max_samples: 0,
        ..SampleParams::default()
    };
    assert!(matches!(
        params.validate(),
        Err(looklab_prep::PrepError::InvalidParams {
            field: "max_samples"
        })
    ));
    let params = SampleParams {
        max_bin_weight: 0.0,
        ..SampleParams::default()
    };
    assert!(matches!(
        params.validate(),
        Err(looklab_prep::PrepError::InvalidParams {
            field: "max_bin_weight"
        })
    ));
}
