// SPDX-License-Identifier: AGPL-3.0-or-later
//! P1.7: a single-illuminant `camera_rgb_wb_linear` bake with a shaper at D65.

mod common;

use std::sync::LazyLock;

use looklab_export::{
    BakeIlluminant, BakeOutput, Baker, CctK, Format, Interpolation, cube_files, read_cube,
    read_cube_1d, written_bytes,
};
use looklab_schema::InterfaceProfile;

const ACCEPTS: &str = r#"  - type: lut3d
    sizes: [17, 33, 65]
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
    common::profile(&common::profile_yaml(insertion, ACCEPTS, "[cube]", 1 << 24))
}

fn baked() -> &'static (InterfaceProfile, BakeOutput) {
    static BAKED: LazyLock<(InterfaceProfile, BakeOutput)> = LazyLock::new(|| {
        let profile = profile();
        let body = common::body();
        let look = common::look();
        let output = Baker::new(&profile, &look, &body)
            .bake(BakeIlluminant::Single { cct: CctK::D65 })
            .expect("bake");
        (profile, output)
    });
    &BAKED
}

#[test]
fn camera_linear_bake_round_trips_at_the_bake_cct() {
    let (_, output) = baked();
    let body = common::body();
    let look = common::look();

    let lut = common::lut(output);
    assert!(lut.shaper.is_some(), "the accepted shaper is emitted");
    assert_eq!(output.artifacts.len(), 1);

    let probes = common::probes();
    let report = looklab_export::validate(output, &look, &body, &probes, None).expect("validation");
    let eligible = probes.probes.len() - report.primary.unsupported;
    assert!(
        report.primary.stats.count * 10 >= eligible * 9,
        "evaluated {} of {eligible} supported probes",
        report.primary.stats.count
    );
    assert!(
        report.primary.stats.p95 <= 1.0,
        "p95 {} at the bake CCT",
        report.primary.stats.p95
    );
    assert!(!report.flagged());
    assert!(report.primary.clipped > 0);

    for cct in [CctK::DAYLIGHT, CctK::TUNGSTEN] {
        let other = report.at(cct).expect("other-CCT report");
        assert_eq!(
            other.stats.count + other.unsupported,
            probes.probes.len(),
            "every supported probe is evaluated at {cct}"
        );
        assert!(other.stats.p95.is_finite());
    }
}

#[test]
fn bake_respects_the_byte_limit_and_writers_agree() {
    let (profile, output) = baked();
    assert_eq!(common::lut(output).size, 65);

    let bytes = written_bytes(output, Format::Cube).expect("bytes");
    assert!(
        bytes <= profile.limits.max_bytes,
        "{bytes} > {}",
        profile.limits.max_bytes
    );
}

#[test]
fn cube_pair_round_trips_through_the_writer() {
    let (_, output) = baked();
    let lut = common::lut(output);
    let (grid_text, shaper_text) = cube_files(lut, "test", output.insertion.range);
    let grid = read_cube(&grid_text).expect("cube");
    assert_eq!(grid.lut.data, lut.data);
    assert_eq!(grid.lut.size, lut.size);
    assert_eq!(grid.domain, [0.0, 1.0]);

    let shaper = read_cube_1d(&shaper_text.expect("shaper")).expect("shaper");
    assert_eq!(shaper.domain, output.insertion.range);
    assert_eq!(Some(&shaper.shaper), lut.shaper.as_ref());

    for code in [0.1, 0.35, 0.7] {
        let at = [code, code, code];
        let through = grid.lut.eval(
            [0, 1, 2].map(|channel| shaper.shaper.eval(at[channel])),
            Interpolation::Tetrahedral,
        );
        let direct = lut.eval(at, Interpolation::Tetrahedral);
        for channel in 0..3 {
            assert!(
                (through[channel] - direct[channel]).abs() < 1.0e-6,
                "{through:?} vs {direct:?}"
            );
        }
    }
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
