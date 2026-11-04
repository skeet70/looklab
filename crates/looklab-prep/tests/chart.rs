// SPDX-License-Identifier: AGPL-3.0-or-later

mod common;

use looklab_color::{RgbSpace, Xyz};
use looklab_prep::chart::{self, ChartLayout, ChartParams, ChartRegion};
use looklab_prep::geom::{Point, Quad};
use looklab_prep::{
    Image3, LocalImageKey, PrepError, PrepParams, PriorTransform, SamplingSeed, SeedFile,
    prepare_aligned_raw, prepare_decoded, prepare_decoded_with_chart,
};
use looklab_samples::{PatchId, PatchTable, PriorTransformVersion};
use rand_chacha::ChaCha20Rng;

const WIDTH: u32 = 480;
const HEIGHT: u32 = 360;
const WB: [f64; 3] = [2.0, 1.0, 1.5];

fn fixed_seed(fill: u8) -> SamplingSeed {
    let hex: String = format!("{fill:02x}").repeat(32);
    let json = format!(
        r#"{{"format":"looklab-seeds/1","prior_transform_version":1,"entries":[{{"key":"A.RAF","seed":"{hex}"}}]}}"#
    );
    SeedFile::from_json(&json)
        .expect("valid seed file")
        .seed_for(&LocalImageKey::new("A.RAF").expect("valid key"))
        .expect("entry exists")
}

fn prior() -> PriorTransform {
    PriorTransform::new(
        RgbSpace::Srgb.to_xyz_matrix(),
        5500.0,
        PriorTransformVersion::new(3),
    )
}

/// Patch colours in linear sRGB, clamped to a photographic range.
fn truth(layout: &ChartLayout) -> Vec<[f32; 3]> {
    let matrix = RgbSpace::Srgb.from_xyz_matrix();
    (0..layout.patch_count())
        .map(|index| {
            let xyz = layout
                .reference_xyz(PatchId::new(index as u16))
                .expect("reference values cover every patch");
            let rgb: Xyz = Xyz::from_array(matrix.mul_vec(xyz.to_array()));
            rgb.to_array().map(|value| value.clamp(0.01, 0.92) as f32)
        })
        .collect()
}

fn template(layout: &ChartLayout, truth: &[[f32; 3]]) -> Image3 {
    let width = 600;
    let height = 400;
    let mut image = common::blank(width, height, [0.22, 0.22, 0.22]);
    for (index, colour) in truth.iter().enumerate() {
        let quad = layout
            .patch_quad(index as u32)
            .expect("patch index is inside the grid");
        let x0 = quad[0].x * f64::from(width);
        let y0 = quad[0].y * f64::from(height);
        let x1 = quad[2].x * f64::from(width);
        let y1 = quad[2].y * f64::from(height);
        common::fill_rect(&mut image, x0, y0, x1, y1, *colour);
    }
    image
}

fn pose(rng: &mut ChaCha20Rng) -> Quad {
    let inset_x = 0.09 * f64::from(WIDTH);
    let inset_y = 0.08 * f64::from(HEIGHT);
    let base = [
        Point::new(inset_x, inset_y),
        Point::new(f64::from(WIDTH) - inset_x, inset_y),
        Point::new(f64::from(WIDTH) - inset_x, f64::from(HEIGHT) - inset_y),
        Point::new(inset_x, f64::from(HEIGHT) - inset_y),
    ];
    common::jitter_quad(&base, 4.0, rng)
}

fn photograph(layout: &ChartLayout, truth: &[[f32; 3]], pose: &Quad) -> Image3 {
    let mut rng = common::rng(0x5eed);
    let scene = common::warp_fine(
        &template(layout, truth),
        pose,
        WIDTH,
        HEIGHT,
        [0.02, 0.02, 0.025],
    );
    let scene = common::blur(&scene, 2);
    common::add_noise(&scene, 0.0002, &mut rng)
}

#[test]
fn chart_patch_means_match_the_truth_across_an_exposure_bracket() {
    let layout = chart::colorchecker_classic();
    let truth = truth(layout);
    let mut rng = common::rng(0x11);
    let pose = pose(&mut rng);
    let base = photograph(layout, &truth, &pose);
    let params = ChartParams::default();

    let mut tables: Vec<PatchTable> = Vec::new();
    for exposure in [1.0, 0.5, 0.25] {
        let scene = common::scale(&base, exposure);
        let jpeg = common::jpeg_from(scene.clone());
        let extraction = chart::extract(
            layout,
            &pose,
            &jpeg,
            &scene,
            &common::identity_alignment(),
            &params,
        )
        .expect("extracts the chart");

        assert_eq!(extraction.rejected.len(), 0);
        assert_eq!(extraction.table.rows().len(), 24);
        for row in extraction.table.rows() {
            let expected = truth[row.id.get() as usize].map(|value| f64::from(value) * exposure);
            for (channel, expected) in expected.iter().enumerate() {
                let measured = f64::from(row.in_mean[channel]);
                assert!(
                    (measured - expected).abs() <= 0.005 * expected,
                    "exposure {exposure} patch {} in channel {channel}: {measured} against {expected}",
                    row.id.get()
                );
                let rendered = f64::from(row.out_mean[channel]);
                assert!(
                    (rendered - expected).abs() <= 0.005 * expected,
                    "exposure {exposure} patch {} out channel {channel}: {rendered} against {expected}",
                    row.id.get()
                );
            }
        }
        tables.push(extraction.table);
    }

    for pair in tables.windows(2) {
        for (brighter, darker) in pair[0].rows().iter().zip(pair[1].rows()) {
            assert_eq!(brighter.id, darker.id);
            for (darker, brighter) in darker.in_mean.iter().zip(&brighter.in_mean) {
                let ratio = f64::from(*darker) / f64::from(*brighter);
                assert!(
                    (ratio - 0.5).abs() < 0.005,
                    "one stop changes {} to {ratio}",
                    0.5
                );
            }
        }
    }
}

#[test]
fn corner_refinement_lands_within_half_a_pixel() {
    let layout = chart::colorchecker_classic();
    let truth = truth(layout);
    let mut rng = common::rng(0x22);
    let params = ChartParams::default();
    for trial in 0..3 {
        let pose = pose(&mut rng);
        let scene = photograph(layout, &truth, &pose);
        let jpeg = common::jpeg_from(scene);
        let clicked = common::jitter_quad(&pose, 3.0, &mut rng);
        let refined = chart::refine_corners(&jpeg, &clicked, &params).expect("refines");
        for (index, (found, expected)) in refined.iter().zip(&pose).enumerate() {
            let error = found.distance(*expected);
            assert!(
                error < 0.5,
                "trial {trial} corner {index} is {error} px off"
            );
        }
    }
}

#[test]
fn an_unaligned_click_far_outside_the_chart_is_rejected() {
    let layout = chart::colorchecker_classic();
    let truth = truth(layout);
    let scene = photograph(layout, &truth, &pose(&mut common::rng(0x33)));
    let jpeg = common::jpeg_from(scene);
    let outside = [
        Point::new(-20.0, -20.0),
        Point::new(40.0, -20.0),
        Point::new(40.0, 40.0),
        Point::new(-20.0, 40.0),
    ];
    assert!(matches!(
        chart::refine_corners(&jpeg, &outside, &ChartParams::default()),
        Err(chart::ChartError::CornerOutside)
    ));
}

#[test]
fn a_chart_region_leaves_no_density_samples_inside_it() {
    let layout = chart::colorchecker_classic();
    let truth = truth(layout);
    let scene = photograph(layout, &truth, &pose(&mut common::rng(0x44)));
    let raw = common::raw_from(&scene, WB);
    let jpeg = common::jpeg_from(scene);
    let params = PrepParams::default();
    let seed = fixed_seed(0x77);
    let width = f64::from(WIDTH);
    let height = f64::from(HEIGHT);

    let without =
        prepare_decoded(&raw, &[&jpeg], &prior(), seed, &params).expect("prepares without a chart");
    assert!(without[0].n() > 10);

    let full: Quad = [
        Point::new(0.0, 0.0),
        Point::new(width, 0.0),
        Point::new(width, height),
        Point::new(0.0, height),
    ];
    let region = ChartRegion::new(full, (WIDTH, HEIGHT)).expect("the frame is a quad");
    assert!(matches!(
        prepare_decoded_with_chart(&raw, &[&jpeg], &prior(), seed, &params, Some(&region)),
        Err(PrepError::NoSamples)
    ));

    let half: Quad = [
        Point::new(0.0, 0.0),
        Point::new(width * 0.5, 0.0),
        Point::new(width * 0.5, height),
        Point::new(0.0, height),
    ];
    let region = ChartRegion::new(half, (WIDTH, HEIGHT)).expect("half the frame is a quad");
    let with = prepare_decoded_with_chart(&raw, &[&jpeg], &prior(), seed, &params, Some(&region))
        .expect("prepares with a chart");
    assert!(with[0].n() > 0);
    assert!(with[0].n() < without[0].n());
    assert_ne!(
        with[0].header().prep_params_hash,
        without[0].header().prep_params_hash
    );
    let repeated =
        prepare_decoded_with_chart(&raw, &[&jpeg], &prior(), seed, &params, Some(&region))
            .expect("prepares with a chart");
    assert_eq!(
        repeated[0].header().prep_params_hash,
        with[0].header().prep_params_hash
    );
}

#[test]
fn prepare_aligned_raw_returns_the_oriented_white_balanced_image() {
    let layout = chart::colorchecker_classic();
    let truth = truth(layout);
    let scene = photograph(layout, &truth, &pose(&mut common::rng(0x55)));
    let raw = common::raw_from(&scene, WB);
    let jpeg = common::jpeg_from(scene);
    let aligned = prepare_aligned_raw(&raw, &jpeg, &PrepParams::default()).expect("aligns");

    assert_eq!(aligned.size(), (WIDTH, HEIGHT));
    assert!(aligned.alignment().central_correlation > 0.9);
    let centre_x = f64::from(WIDTH) * 0.5;
    let centre_y = f64::from(HEIGHT) * 0.5;
    let (mapped_x, mapped_y) = aligned
        .alignment()
        .jpeg_to_raw(centre_x, centre_y, WIDTH, HEIGHT, WIDTH, HEIGHT);
    assert!((mapped_x - centre_x).abs() < 2.0);
    assert!((mapped_y - centre_y).abs() < 2.0);
}

#[test]
fn the_chart_layout_geometry_is_consistent() {
    let layout = chart::colorchecker_classic();
    let mut previous_y = f64::NEG_INFINITY;
    for index in 0..layout.patch_count() {
        let quad = layout
            .patch_quad(index)
            .expect("patch index is inside the grid");
        assert!(quad[0].x < quad[2].x && quad[0].y < quad[2].y);
        assert!(quad[0].x >= 0.0 && quad[2].x <= 1.0);
        let centre_y = quad[0].y;
        if index % layout.cols() == 0 && index > 0 {
            assert!(centre_y > previous_y);
        }
        previous_y = centre_y;
    }
    assert!(matches!(
        ChartRegion::new(
            [
                Point::new(0.0, 0.0),
                Point::new(0.0, 10.0),
                Point::new(0.0, 20.0),
                Point::new(0.0, 30.0)
            ],
            (100, 100)
        ),
        Err(chart::ChartError::DegenerateCorners)
    ));
}
