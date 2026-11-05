// SPDX-License-Identifier: AGPL-3.0-or-later
//! Every finite XYZ evaluates and encodes to finite coordinates (docs/02 §3).

use looklab_color::RgbSpace;
use looklab_color::Xyz;
use looklab_model::basis::{RESIDUAL_COEFS, TINT_COEFS};
use looklab_model::solve::ucs_of_xyz;
use looklab_model::{Look, Residual, Tint, output_encode_clip};
use proptest::prelude::*;

fn full_look() -> Look {
    let mut look = Look::colorimetric_spline().expect("a colorimetric spline tone");
    look.tint = Tint {
        a: vec![0.5; TINT_COEFS],
        b: vec![-0.4; TINT_COEFS],
    };
    look.residual = Residual {
        values: vec![0.03; RESIDUAL_COEFS],
    };
    look
}

fn finite(values: [f64; 3]) -> bool {
    values.iter().all(|value| value.is_finite())
}

#[test]
fn an_out_of_gamut_node_is_finite() {
    let look = full_look();
    let xyz = Xyz::new(-9.758, 0.066, 9.562);
    let target = look.eval(xyz);
    assert!(finite(target.to_array()), "{target:?}");
    let encoded = look.output_encode(target, RgbSpace::Srgb);
    assert!(finite(encoded.to_array()), "{encoded:?}");
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    #[test]
    fn eval_and_output_encode_are_finite_for_any_finite_xyz(
        x in -1e6_f64..1e6,
        y in -1e6_f64..1e6,
        z in -1e6_f64..1e6,
    ) {
        let look = full_look();
        let xyz = Xyz::new(x, y, z);
        let target = look.eval(xyz);
        prop_assert!(finite(target.to_array()), "eval({xyz:?}) = {target:?}");

        let encoded = look.output_encode(target, RgbSpace::Srgb);
        prop_assert!(finite(encoded.to_array()), "output_encode({target:?}) = {encoded:?}");

        let clipped = output_encode_clip(ucs_of_xyz(xyz), RgbSpace::Rec2020, 0.03);
        prop_assert!(finite(clipped.to_array()), "output_encode_clip({xyz:?}) = {clipped:?}");
    }
}
