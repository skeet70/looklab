// SPDX-License-Identifier: AGPL-3.0-or-later
//! P3.3: writer round-trips, the example profile, and the fitted artifact kinds.

mod common;

use looklab_export::{
    Artifact, BakeIlluminant, BakeOutput, Baker, CctK, ExportError, Format, Interpolation,
    NativeDocument, cube_files, read_cube, read_cube_1d, read_haldclut, read_native_json, validate,
    write_haldclut, write_native_json, written_bytes,
};

const LUT_ACCEPTS: &str = r#"  - type: lut3d
    sizes: [17, 33]
    interpolation: tetrahedral
    precision: f16
    with_shaper: true
  - type: matrix3x3
  - type: curve1d
    points_max: 256
    per_channel: true"#;

fn bake_fixture() -> (looklab_schema::InterfaceProfile, BakeOutput) {
    let insertion = r#"  point: camera_rgb_wb_linear
  encoding:
    transfer: linear
    primaries: camera_native
    range: [0.0, 8.0]
  illuminant_handling: single"#;
    let profile = common::profile(&common::profile_yaml(
        insertion,
        LUT_ACCEPTS,
        "[cube, haldclut, native_json]",
        1 << 22,
    ));
    let body = common::body();
    let look = common::look();
    let output = Baker::new(&profile, &look, &body)
        .bake(BakeIlluminant::Single { cct: CctK::D65 })
        .expect("bake");
    (profile, output)
}

#[test]
fn cube_and_shaper_round_trip_exactly() {
    let (_, output) = bake_fixture();
    let lut = common::lut(&output);
    let (grid_text, shaper_text) =
        cube_files(lut, &output.profile_id.to_string(), output.insertion.range);
    let grid = read_cube(&grid_text).expect("cube");
    assert_eq!(grid.lut.data, lut.data);
    assert_eq!(grid.domain, [0.0, 1.0]);

    let shaper = lut.shaper.as_ref().expect("shaper");
    let parsed_shaper = read_cube_1d(&shaper_text.expect("shaper file")).expect("shaper");
    assert_eq!(parsed_shaper.shaper, *shaper);
    assert_eq!(parsed_shaper.domain, output.insertion.range);

    for code in [0.2, 0.5, 0.8] {
        let at = [code, code, code];
        let through = grid.lut.eval(
            [0, 1, 2].map(|channel| parsed_shaper.shaper.eval(at[channel])),
            Interpolation::Tetrahedral,
        );
        let direct = lut.eval(at, Interpolation::Tetrahedral);
        for channel in 0..3 {
            assert!((through[channel] - direct[channel]).abs() < 1.0e-6);
        }
    }
}

#[test]
fn native_json_rejects_malformed_artifacts() {
    let (_, output) = bake_fixture();
    let document = NativeDocument::from_output(&output);

    let mut short = document.clone();
    if let Artifact::Lut3d(lut) = &mut short.artifacts[0].artifact {
        lut.data.clear();
    }
    assert!(matches!(
        read_native_json(&serde_json::to_string(&short).expect("json")),
        Err(ExportError::InvalidArtifact(_))
    ));

    let mut small = document.clone();
    if let Artifact::Lut3d(lut) = &mut small.artifacts[0].artifact {
        lut.size = 2;
        lut.data.clear();
    }
    assert!(matches!(
        read_native_json(&serde_json::to_string(&small).expect("json")),
        Err(ExportError::InvalidArtifact(_))
    ));

    let mut broken_shaper = document.clone();
    if let Artifact::Lut3d(lut) = &mut broken_shaper.artifacts[0].artifact
        && let Some(shaper) = &mut lut.shaper
    {
        shaper.values.truncate(3);
    }
    assert!(matches!(
        read_native_json(&serde_json::to_string(&broken_shaper).expect("json")),
        Err(ExportError::InvalidArtifact(_))
    ));

    let mut empty = document.clone();
    empty.artifacts.clear();
    assert!(matches!(
        read_native_json(&serde_json::to_string(&empty).expect("json")),
        Err(ExportError::InvalidArtifact(_))
    ));

    let mut bad_range = document.clone();
    bad_range.insertion.range = [1.0, 0.0];
    assert!(matches!(
        read_native_json(&serde_json::to_string(&bad_range).expect("json")),
        Err(ExportError::Profile(_))
    ));
}

#[test]
fn haldclut_round_trips_within_16_bit() {
    let (_, output) = bake_fixture();
    let mut lut = common::lut(&output).clone();
    lut.shaper = None;
    let resampled = lut.resample(64, looklab_schema::Precision::F32);
    let png = write_haldclut(&resampled).expect("haldclut");
    let back = read_haldclut(&png).expect("haldclut");
    assert_eq!(back.size, 64);
    for (written, read) in resampled.data.iter().zip(back.data.iter()) {
        for channel in 0..3 {
            assert!(
                (written[channel] - read[channel]).abs() <= 0.5 / 65_535.0 + 1.0e-7,
                "{written:?} vs {read:?}"
            );
        }
    }
}

#[test]
fn native_json_round_trips_the_bake() {
    let (_, output) = bake_fixture();
    let text = write_native_json(&output).expect("json");
    let back = read_native_json(&text).expect("json");
    assert_eq!(back, output);
}

#[test]
fn every_declared_format_fits_the_byte_limit() {
    let (profile, output) = bake_fixture();
    for format in [Format::Cube, Format::Haldclut, Format::NativeJson] {
        let bytes = written_bytes(&output, format).expect("bytes");
        assert!(bytes <= profile.limits.max_bytes, "{format:?}: {bytes}");
    }
}

#[test]
fn the_example_interface_profile_bakes_and_validates() {
    let yaml = include_str!("../../../data/interfaces/example-firmware-sim-v1.yaml");
    let profile = common::profile(yaml);
    let body = common::body();
    let look = common::look();
    let output = Baker::new(&profile, &look, &body)
        .bake(BakeIlluminant::Single { cct: CctK::D65 })
        .expect("bake");
    assert_eq!(output.kind(), "lut3d");
    let report = validate(&output, &look, &body, &common::probes(), None).expect("validation");
    assert_eq!(report.primary.cct, CctK::D65);
    assert!(report.primary.stats.count > 100, "{:?}", report.primary);
    // The profile's 256 KB budget buys a 17^3 grid, whose p95 over this look is
    // above the 1.0 flag threshold; the report carries that finding.
    assert!(report.flagged(), "{:?}", report.primary);

    for format in [Format::Cube, Format::NativeJson] {
        let bytes = written_bytes(&output, format).expect("bytes");
        assert!(bytes <= profile.limits.max_bytes, "{format:?}: {bytes}");
    }
}

#[test]
fn a_matrix_only_profile_fits_a_matrix() {
    let insertion = common::log2_insertion("camera_rgb_wb_linear", "camera_native", "single");
    let profile = common::profile(&common::profile_yaml(
        &insertion,
        "  - type: matrix3x3",
        "[native_json]",
        1 << 22,
    ));
    let body = common::body();
    let look = common::look();
    let output = Baker::new(&profile, &look, &body)
        .bake(BakeIlluminant::Single { cct: CctK::D65 })
        .expect("bake");
    assert!(matches!(
        output.primary_artifact(),
        Some(Artifact::Matrix3x3(_))
    ));

    let text = write_native_json(&output).expect("json");
    assert_eq!(read_native_json(&text).expect("json"), output);
}

#[test]
fn a_curve_only_profile_fits_a_curve() {
    let insertion = common::log2_insertion("scene_linear_xyz_d65", "xyz_d65", "single");
    let profile = common::profile(&common::profile_yaml(
        &insertion,
        "  - type: curve1d\n    points_max: 64\n    per_channel: true",
        "[native_json]",
        1 << 22,
    ));
    let body = common::body();
    let look = common::look();
    let output = Baker::new(&profile, &look, &body)
        .bake(BakeIlluminant::Single { cct: CctK::D65 })
        .expect("bake");
    let Some(Artifact::Curve1d(curve)) = output.primary_artifact() else {
        panic!("expected a curve artifact");
    };
    assert_eq!(curve.points.len(), 64);
}

#[test]
fn baked_output_is_deterministic() {
    let (_, first) = bake_fixture();
    let (_, second) = bake_fixture();
    assert_eq!(first.artifacts, second.artifacts);
}
