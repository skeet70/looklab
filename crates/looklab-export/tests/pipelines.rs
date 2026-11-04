// SPDX-License-Identifier: AGPL-3.0-or-later
//! P3.1: every insertion point, both illuminant handlings, perceptual clipping.

mod common;

use looklab_export::{BakeIlluminant, Baker, CctK, in_gamut, perceptual_clip, validate};
use looklab_schema::InterfaceProfile;

fn profile(point: &str, primaries: &str, handling: &str) -> InterfaceProfile {
    let accepts = r#"  - type: lut3d
    sizes: [45]
    interpolation: tetrahedral
    precision: f16
    with_shaper: false"#;
    common::profile(&common::profile_yaml(
        &common::log2_insertion(point, primaries, handling),
        accepts,
        "[cube]",
        1 << 24,
    ))
}

fn insertion_points() -> [(&'static str, &'static str); 4] {
    [
        ("camera_rgb_wb_linear", "camera_native"),
        ("scene_linear_rec2020", "rec2020"),
        ("scene_linear_xyz_d65", "xyz_d65"),
        ("display_encoded_post_tone", "srgb"),
    ]
}

#[test]
fn single_illuminant_bakes_round_trip_at_every_bake_cct() {
    let body = common::body();
    let base_look = common::look();
    for (point, primaries) in insertion_points() {
        let profile = profile(point, primaries, "single");
        for cct in [CctK::D65, CctK::DAYLIGHT, CctK::TUNGSTEN] {
            let output = Baker::new(&profile, &base_look, &body)
                .bake(BakeIlluminant::Single { cct })
                .expect("bake");
            let report =
                validate(&output, &base_look, &body, &common::probes(), None).expect("validation");
            assert_eq!(report.primary.cct, cct);
            assert_eq!(report.others.len(), 2, "{point}");
            assert!(
                report.primary.stats.p95 <= 1.0,
                "{point} at {cct}: p95 {} over {} probes",
                report.primary.stats.p95,
                report.primary.stats.count
            );
        }
    }
}

#[test]
fn dual_illuminant_bakes_round_trip_at_both_endpoints() {
    let body = common::body();
    let look = common::look();
    for (point, primaries) in insertion_points() {
        let profile = profile(point, primaries, "dual_interpolated");
        let output = Baker::new(&profile, &look, &body)
            .bake(BakeIlluminant::DualInterpolated)
            .expect("bake");
        assert_eq!(output.artifacts.len(), 2);
        let report = validate(&output, &look, &body, &common::probes(), None).expect("validation");
        for cct in [CctK::STD_A, CctK::D65] {
            let at = report.at(cct).expect("endpoint report");
            assert!(
                at.stats.p95 <= 1.0,
                "{point} at {cct}: p95 {}",
                at.stats.p95
            );
        }
        assert!(report.at(CctK::DAYLIGHT).is_some());
    }
}

#[test]
fn perceptual_clip_keeps_lightness_and_hue() {
    use looklab_color::ucs::{Ucs, UcsSpace, ucs_to_jmh};
    use looklab_export::OutputSpace;

    let far = Ucs::new(58.0, 85.0, -50.0);
    assert!(!in_gamut(far, OutputSpace::Srgb));
    let clipped = perceptual_clip(far, OutputSpace::Srgb);
    assert!(in_gamut(clipped, OutputSpace::Srgb));

    let before = ucs_to_jmh(far, UcsSpace::Ucs);
    let after = ucs_to_jmh(clipped, UcsSpace::Ucs);
    assert!((before.j - after.j).abs() < 1.0e-6);
    let hue = |jmh: looklab_color::cam16::Jmh| {
        let radians = jmh.h * core::f64::consts::PI / 180.0;
        let mut value = libm::atan2(libm::sin(radians), libm::cos(radians));
        if value < 0.0 {
            value += 2.0 * core::f64::consts::PI;
        }
        value
    };
    assert!((hue(before) - hue(after)).abs() < 1.0e-6);
    assert!(after.m < before.m);
}

#[test]
fn display_encoded_post_tone_reports_the_tone_it_loses() {
    let body = common::body();
    let look = common::look();
    let profile = profile("display_encoded_post_tone", "srgb", "single");
    let output = Baker::new(&profile, &look, &body)
        .bake(BakeIlluminant::Single { cct: CctK::D65 })
        .expect("bake");
    let report = validate(&output, &look, &body, &common::probes(), None).expect("validation");
    let mismatch = report.tone_mismatch.expect("tone mismatch is reported");
    assert!(mismatch.count > 100);
    assert!(mismatch.median > 0.0, "{mismatch:?}");
}

#[test]
fn a_colorimetric_look_loses_no_tone() {
    let body = common::body();
    let mut look = common::look();
    look.tone = looklab_model::Tone::Colorimetric;
    let profile = profile("display_encoded_post_tone", "srgb", "single");
    let output = Baker::new(&profile, &look, &body)
        .bake(BakeIlluminant::Single { cct: CctK::D65 })
        .expect("bake");
    let report = validate(&output, &look, &body, &common::probes(), None).expect("validation");
    assert!(report.tone_mismatch.is_none());
}

#[test]
fn support_mask_excludes_masked_probes() {
    let body = common::body();
    let look = common::look();
    let profile = profile("scene_linear_rec2020", "rec2020", "single");
    let output = Baker::new(&profile, &look, &body)
        .bake(BakeIlluminant::Single { cct: CctK::D65 })
        .expect("bake");
    let probes = common::probes();
    let mut mask = vec![true; probes.probes.len()];
    for (index, value) in mask.iter_mut().enumerate() {
        if index % 2 == 0 {
            *value = false;
        }
    }
    let report = validate(&output, &look, &body, &probes, Some(&mask)).expect("validation");
    let dropped = probes.probes.len().div_ceil(2);
    assert!(report.primary.unsupported >= dropped);
}
