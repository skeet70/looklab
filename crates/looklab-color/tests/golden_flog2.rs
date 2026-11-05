// SPDX-License-Identifier: AGPL-3.0-or-later
//! Golden vectors from colour-science 0.4.7; regenerate with `tools/golden/gen_flog2.py`.

use std::sync::LazyLock;

use serde::Deserialize;

use looklab_color::{flog2, spaces};

#[derive(Deserialize)]
struct Golden {
    flog2_encode: Vectors,
    flog2_decode: Vectors,
    fgamut: FgamutGolden,
}

#[derive(Deserialize)]
struct Vectors {
    inputs: Vec<Vec<f64>>,
    outputs: Vec<Vec<f64>>,
}

#[derive(Deserialize)]
struct FgamutGolden {
    rgb_to_xyz: Vec<f64>,
    xyz_to_rgb: Vec<f64>,
}

static GOLDEN: LazyLock<Golden> = LazyLock::new(|| {
    serde_json::from_str(include_str!("golden/flog2.json")).expect("golden vectors parse")
});

fn assert_close(actual: f64, expected: f64) {
    let tolerance = (expected.abs() * 1e-6).max(1e-12);
    assert!(
        (actual - expected).abs() <= tolerance,
        "actual {actual:?}, expected {expected:?}"
    );
}

fn check_vectors(set: &Vectors, convert: fn(f64) -> f64) {
    assert_eq!(set.inputs.len(), set.outputs.len());
    for (input, expected) in set.inputs.iter().zip(&set.outputs) {
        assert_eq!(input.len(), 1);
        assert_eq!(expected.len(), 1);
        assert_close(convert(input[0]), expected[0]);
    }
}

fn flatten(matrix: looklab_color::Mat3) -> Vec<f64> {
    let m = matrix.0;
    vec![
        m[0][0], m[0][1], m[0][2], m[1][0], m[1][1], m[1][2], m[2][0], m[2][1], m[2][2],
    ]
}

#[test]
fn flog2_encode_matches_colour_science() {
    check_vectors(&GOLDEN.flog2_encode, flog2::encode);
}

#[test]
fn flog2_decode_matches_colour_science() {
    check_vectors(&GOLDEN.flog2_decode, flog2::decode);
}

#[test]
fn fgamut_matrices_match_colour_science() {
    for (actual, expected) in flatten(spaces::fgamut_to_xyz_matrix())
        .into_iter()
        .zip(&GOLDEN.fgamut.rgb_to_xyz)
    {
        assert_close(actual, *expected);
    }
    for (actual, expected) in flatten(spaces::fgamut_from_xyz_matrix())
        .into_iter()
        .zip(&GOLDEN.fgamut.xyz_to_rgb)
    {
        assert_close(actual, *expected);
    }
}
