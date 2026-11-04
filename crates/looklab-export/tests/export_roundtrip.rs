// SPDX-License-Identifier: AGPL-3.0-or-later
//! P1.7: a single-illuminant `camera_rgb_wb_linear` bake with a shaper at D65.

mod common;

use looklab_export::{BakeIlluminant, Baker, CctK, Format, read_cube, write_cube, written_bytes};
use looklab_schema::InterfaceProfile;

const ACCEPTS: &str = r#"  - type: lut3d
    sizes: [17, 33]
    interpolation: tetrahedral
    precision: f16
    with_shaper: true
  - type: matrix3x3
  - type: curve1d
    points_max: 256
    per_channel: true"#;

fn profile() -> InterfaceProfile {
    let insertion = r#"  point: camera_rgb_wb_linear
  encoding:
    transfer: linear
    primaries: camera_native
    range: [0.0, 8.0]
  illuminant_handling: single"#;
    common::profile(&common::profile_yaml(insertion, ACCEPTS, "[cube]", 1 << 22))
}

#[test]
fn camera_linear_bake_round_trips_at_the_bake_cct() {
    let profile = profile();
    let body = common::body();
    let look = common::look();
    let output = Baker::new(&profile, &look, &body)
        .bake(BakeIlluminant::Single { cct: CctK::D65 })
        .expect("bake");

    let lut = common::lut(&output);
    assert!(lut.shaper.is_some(), "the accepted shaper is emitted");
    assert_eq!(output.artifacts.len(), 1);

    let report = looklab_export::validate(&output, &look, &body, &common::probes(), None)
        .expect("validation");
    assert!(
        report.primary.stats.p95 <= 1.0,
        "p95 {} at the bake CCT",
        report.primary.stats.p95
    );
    assert!(!report.flagged());
    assert!(report.primary.stats.count > 100, "{:?}", report.primary);

    for cct in [CctK::DAYLIGHT, CctK::TUNGSTEN] {
        let other = report.at(cct).expect("other-CCT report");
        assert_eq!(
            other.stats.count + other.unsupported + other.clipped,
            common::probes().probes.len()
        );
        assert!(other.stats.p95.is_finite());
    }
}

#[test]
fn bake_respects_the_byte_limit_and_writers_agree() {
    let profile = profile();
    let body = common::body();
    let look = common::look();
    let output = Baker::new(&profile, &look, &body)
        .bake(BakeIlluminant::Single { cct: CctK::D65 })
        .expect("bake");
    assert_eq!(common::lut(&output).size, 33);

    let bytes = written_bytes(&output, Format::Cube).expect("bytes");
    assert!(
        bytes <= profile.limits.max_bytes,
        "{bytes} > {}",
        profile.limits.max_bytes
    );
}

#[test]
fn cube_artifact_round_trips_through_the_writer() {
    let profile = profile();
    let body = common::body();
    let look = common::look();
    let output = Baker::new(&profile, &look, &body)
        .bake(BakeIlluminant::Single { cct: CctK::D65 })
        .expect("bake");
    let lut = common::lut(&output);
    let text = write_cube(lut, "test", output.insertion.range);
    let parsed = read_cube(&text).expect("cube");
    assert_eq!(parsed.lut.data, lut.data);
    assert_eq!(parsed.lut.size, lut.size);
}

#[test]
fn a_tighter_byte_limit_selects_a_smaller_grid() {
    let small = common::profile(&common::profile_yaml(
        &common::log2_insertion("camera_rgb_wb_linear", "camera_native", "single"),
        "  - type: lut3d\n    sizes: [17, 33]\n    interpolation: trilinear\n    precision: f16\n    with_shaper: false",
        "[cube]",
        200_000,
    ));
    let body = common::body();
    let look = common::look();
    let output = Baker::new(&small, &look, &body)
        .bake(BakeIlluminant::Single { cct: CctK::D65 })
        .expect("bake");
    let lut = common::lut(&output);
    assert_eq!(lut.size, 17);
    assert!(lut.shaper.is_none());
}
