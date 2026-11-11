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

#[test]
fn prediction_margin_is_positive_inside_and_negative_at_the_limit() {
    use looklab_color::ucs::Ucs;
    let neutral = looklab_model::neutral_j_prime(0.18);
    let (margin, gradient) = looklab_model::prediction_margin(Ucs::new(neutral, 0.0, 0.0));
    assert!(margin > 0.0, "margin {margin}");
    assert!(gradient.iter().all(|value| value.is_finite()));
    let limit = looklab_model::neutral_j_prime_max();
    let (margin, _) = looklab_model::prediction_margin(Ucs::new(limit + 1.0, 0.0, 0.0));
    assert!(margin <= 0.0, "margin {margin}");
}

#[test]
fn prediction_margin_gradient_matches_finite_differences() {
    use looklab_color::ucs::Ucs;
    let base = Ucs::new(60.0, 2.0, -1.0);
    let (margin, gradient) = looklab_model::prediction_margin(base);
    assert!(margin > 0.0, "margin {margin}");
    for (axis, analytical) in gradient.iter().enumerate() {
        let h = 1e-5;
        let mut plus = base;
        let mut minus = base;
        match axis {
            0 => {
                plus.j += h;
                minus.j -= h;
            }
            1 => {
                plus.a += h;
                minus.a -= h;
            }
            _ => {
                plus.b += h;
                minus.b -= h;
            }
        }
        let numerical = (looklab_model::prediction_margin(plus).0
            - looklab_model::prediction_margin(minus).0)
            / (2.0 * h);
        let scale = analytical.abs().max(numerical.abs()).max(1e-6);
        assert!(
            (analytical - numerical).abs() / scale < 1e-4,
            "axis {axis}: {analytical} vs {numerical}"
        );
    }
}

#[test]
fn barrier_is_finite_inside_and_infinite_outside() {
    assert!(looklab_model::barrier(0.5).is_finite());
    assert_eq!(looklab_model::barrier(0.0), f64::INFINITY);
    let (first, second) = looklab_model::barrier_derivatives(0.5);
    assert!((first + 2.0).abs() < 1e-12);
    assert!((second - 4.0).abs() < 1e-12);
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
