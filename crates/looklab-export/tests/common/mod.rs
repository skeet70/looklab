// SPDX-License-Identifier: AGPL-3.0-or-later
//! Synthetic body, look and profiles shared by the export tests.

#![allow(dead_code)]

use looklab_color::Mat3;
use looklab_color::dng::{CalibrationMatrix, DngColorSpec};
use looklab_export::{Artifact, BakeOutput, DngInputTransform, Lut3d};
use looklab_model::{Look, ProbeSet};

/// X-S10 ColorMatrix1/2 from the committed fixture, as a dual-illuminant body.
pub fn body() -> DngInputTransform {
    let spec = DngColorSpec::new(
        vec![
            CalibrationMatrix::new(
                2850.0,
                Mat3([
                    [1.6064, -0.9942, 0.112],
                    [-0.376, 1.2096, 0.1853],
                    [-0.0007, 0.0545, 0.6654],
                ]),
            )
            .expect("calibration temperature"),
            CalibrationMatrix::new(
                6500.0,
                Mat3([
                    [1.3426, -0.6334, -0.1177],
                    [-0.4244, 1.2136, 0.2371],
                    [-0.058, 0.1303, 0.598],
                ]),
            )
            .expect("calibration temperature"),
        ],
        Vec::new(),
    )
    .expect("colour profile");
    DngInputTransform::new(spec).expect("input transform")
}

/// A colourimetric tone with a 6% lift, a warm shift, 4% chroma gain and a 1.5°
/// hue rotation.
pub fn look() -> Look {
    let mut look = Look::colorimetric_spline().expect("tone spline");
    if let looklab_model::Tone::Spline { c0, beta } = &mut look.tone {
        *c0 *= 1.06;
        for coefficient in beta.iter_mut() {
            *coefficient *= 1.03;
        }
    }
    look.tint.a.fill(1.0);
    look.tint.b.fill(-0.6);
    look.color.g.plane.fill(0.04);
    look.color.g.volume.fill(0.01);
    look.color.r.plane.fill(1.5);
    look.color.lambda.plane.fill(0.3);
    look
}

pub fn probes() -> ProbeSet {
    looklab_model::probes::v1()
}

pub fn profile(yaml: &str) -> looklab_schema::InterfaceProfile {
    looklab_schema::validate_interface_profile(yaml).expect("interface profile")
}

pub fn profile_yaml(insertion: &str, accepts: &str, formats: &str, max_bytes: u64) -> String {
    format!(
        r#"id: test/firmware-slot
name: "Test firmware slot"
version: 3
insertion:
{insertion}
accepts:
{accepts}
downstream:
  - output_transform: {{ primaries: srgb, transfer: srgb }}
  - gamut_clip: hard
formats: {formats}
limits: {{ max_bytes: {max_bytes} }}
"#
    )
}

pub fn log2_insertion(point: &str, primaries: &str, handling: &str) -> String {
    format!(
        r#"  point: {point}
  encoding:
    transfer: log2_shaper
    shaper: {{ min_ev: -10, max_ev: 4, mid_gray: 0.18 }}
    primaries: {primaries}
    range: [0.0, 1.0]
  illuminant_handling: {handling}"#
    )
}

pub fn lut(output: &BakeOutput) -> &Lut3d {
    output
        .primary_artifact()
        .and_then(Artifact::as_lut)
        .expect("a 3D LUT artifact")
}
