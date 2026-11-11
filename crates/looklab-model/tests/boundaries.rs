// SPDX-License-Identifier: AGPL-3.0-or-later
//! Boundary regularity at the tint join and the exposure lookup joins.

use looklab_color::Xyz;
use looklab_model::basis::{
    S_HIGHLIGHT_TRANSITION, S_SHADOW_TRANSITION, TINT_COEFS, TINT_JOIN, TINT_KNOTS,
    exposure_lookup, exposure_lookup_derivative, tint_continuation_end, tint_weights,
};
use looklab_model::solve::{neutral_j_prime_max, reference_white, try_solve_y_ref};
use looklab_model::{Look, LookInput};

const PI: f64 = core::f64::consts::PI;
const LN_2: f64 = core::f64::consts::LN_2;

fn neutral_at(s: f64) -> Xyz {
    let white = reference_white();
    let y = 0.18 * libm::pow(2.0, s);
    Xyz::new(white.x * y, white.y * y, white.z * y)
}

fn sample(look: &Look, s: f64) -> [f64; 3] {
    let ucs = look.eval(neutral_at(s));
    [ucs.j, ucs.a, ucs.b]
}

fn fd2(look: &Look, s: f64, h: f64) -> [f64; 3] {
    let plus = sample(look, s + h);
    let center = sample(look, s);
    let minus = sample(look, s - h);
    [
        (plus[0] - 2.0 * center[0] + minus[0]) / (h * h),
        (plus[1] - 2.0 * center[1] + minus[1]) / (h * h),
        (plus[2] - 2.0 * center[2] + minus[2]) / (h * h),
    ]
}

/// `-d log|fd2| / d log h` between the largest and smallest step.
fn exponent(coarse: [f64; 3], fine: [f64; 3], ratio: f64) -> [f64; 3] {
    let mut out = [0.0; 3];
    for (slot, (coarse, fine)) in out.iter_mut().zip(coarse.iter().zip(fine.iter())) {
        let coarse = coarse.abs();
        let fine = fine.abs();
        if coarse > 1e-12 && fine > 1e-12 {
            *slot = -libm::log(fine / coarse) / libm::log(ratio);
        }
    }
    out
}

fn tint_coefficients() -> ([f64; TINT_COEFS], [f64; TINT_COEFS]) {
    let mut a = [0.0_f64; TINT_COEFS];
    let mut b = [0.0_f64; TINT_COEFS];
    for m in 0..TINT_COEFS {
        let j = (TINT_KNOTS[m + 1] + TINT_KNOTS[m + 2] + TINT_KNOTS[m + 3]) / 3.0;
        a[m] = 0.8 * libm::sin(PI * j / 100.0);
        b[m] = -0.6 * libm::cos(PI * j / 100.0 + 0.4);
    }
    (a, b)
}

fn tinted_look() -> Look {
    let mut look = Look::colorimetric();
    let (a, b) = tint_coefficients();
    look.tint.a.copy_from_slice(&a);
    look.tint.b.copy_from_slice(&b);
    look
}

#[test]
fn tint_join_is_c2() {
    let look = tinted_look();
    let y = try_solve_y_ref(TINT_JOIN, 0.18).expect("J'=100 is attainable");
    let s = libm::log(y / 0.18) / LN_2;

    let steps = [1e-3, 1e-4, 1e-5];
    let values = steps.map(|h| fd2(&look, s, h));
    let measured = exponent(values[0], values[2], steps[0] / steps[2]);
    println!("tint join fd2: {values:?} exponent {measured:?}");

    for (channel, value) in measured.iter().enumerate() {
        assert!(
            value.abs() < 0.25,
            "channel {channel} exponent {value} at the tint join"
        );
    }

    let eps = 1e-6;
    let left = sample(&look, s - eps);
    let right = sample(&look, s + eps);
    let center = sample(&look, s);
    for channel in 0..3 {
        assert!((left[channel] - right[channel]).abs() < 1e-4);
        let left_slope = (center[channel] - left[channel]) / eps;
        let right_slope = (right[channel] - center[channel]) / eps;
        assert!(
            (left_slope - right_slope).abs() < 1e-3,
            "channel {channel} slopes {left_slope} {right_slope}"
        );
    }
}

#[test]
fn tint_continuation_is_bounded() {
    let (a, b) = tint_coefficients();
    let end = tint_continuation_end();
    let value = |j: f64| {
        let (indices, weights, derivative) = tint_weights(j);
        let mut a_value = 0.0;
        let mut a_slope = 0.0;
        let mut b_value = 0.0;
        let mut b_slope = 0.0;
        for m in 0..4 {
            a_value += a[indices[m]] * weights[m];
            a_slope += a[indices[m]] * derivative[m];
            b_value += b[indices[m]] * weights[m];
            b_slope += b[indices[m]] * derivative[m];
        }
        (a_value, b_value, a_slope, b_slope)
    };
    let at_join = value(TINT_JOIN);
    let beyond = value(end);
    let past = value(end + 50.0);
    assert!(at_join.0.is_finite() && at_join.1.is_finite());
    assert!(beyond.0.is_finite() && beyond.1.is_finite());
    assert!((beyond.0 - past.0).abs() < 1e-12);
    assert!((beyond.1 - past.1).abs() < 1e-12);
    assert!(past.2.abs() < 1e-12 && past.3.abs() < 1e-12);
    assert!(end > TINT_JOIN && end < neutral_j_prime_max() + 1.0);
}

#[test]
fn exposure_lookup_is_identity_inside_and_saturates() {
    assert_eq!(exposure_lookup(-12.0), -12.0);
    assert_eq!(exposure_lookup(6.0), 6.0);
    assert_eq!(exposure_lookup(-20.0), -12.0 - S_SHADOW_TRANSITION);
    assert_eq!(exposure_lookup(20.0), 6.0 + S_HIGHLIGHT_TRANSITION);
    assert_eq!(exposure_lookup_derivative(-20.0), 0.0);
    assert_eq!(exposure_lookup_derivative(20.0), 0.0);
    for step in 0..=180 {
        let s = -12.0 + 18.0 * step as f64 / 180.0;
        let mapped = exposure_lookup(s);
        assert!((mapped - s).abs() < 1e-12, "s={s} mapped={mapped}");
        assert!((exposure_lookup_derivative(s) - 1.0).abs() < 1e-12);
    }
    for step in 0..=40 {
        let offset = step as f64 / 10.0;
        let low = exposure_lookup(-12.0 - offset);
        let high = exposure_lookup(6.0 + offset);
        assert!((-12.0 - S_SHADOW_TRANSITION..=-12.0).contains(&low));
        assert!((6.0..=6.0 + S_HIGHLIGHT_TRANSITION).contains(&high));
    }
}

fn exposure_look() -> Look {
    let mut look = Look::colorimetric_spline().expect("colorimetric spline");
    let (a, b) = tint_coefficients();
    look.tint.a.copy_from_slice(&a);
    look.tint.b.copy_from_slice(&b);
    look.color.lambda.plane[0] = 6.0;
    look.color.lambda.plane[1] = -2.0;
    look.color.lambda.plane[2] = 1.0;
    look.color.lambda.plane[3] = -0.5;
    look.color.g.plane[0] = 0.2;
    look.color.g.plane[1] = -0.1;
    look
}

#[test]
fn exposure_transition_joins_are_c2() {
    let look = exposure_look();
    let steps = [1e-3, 1e-4, 1e-5];
    for join in [-12.0, 6.0] {
        let values = steps.map(|h| fd2(&look, join, h));
        let measured = exponent(values[0], values[2], steps[0] / steps[2]);
        println!("exposure join {join} fd2: {values:?} exponent {measured:?}");
        for (channel, value) in measured.iter().enumerate() {
            assert!(
                value.abs() < 0.25,
                "join {join} channel {channel} exponent {value}"
            );
        }
    }
}

fn one_sided_second(look: &Look, s: f64, h: f64, forward: bool) -> [f64; 3] {
    let center = sample(look, s);
    let (near, far) = if forward {
        (sample(look, s + h), sample(look, s + 2.0 * h))
    } else {
        (sample(look, s - h), sample(look, s - 2.0 * h))
    };
    [
        (far[0] - 2.0 * near[0] + center[0]) / (h * h),
        (far[1] - 2.0 * near[1] + center[1]) / (h * h),
        (far[2] - 2.0 * near[2] + center[2]) / (h * h),
    ]
}

#[test]
fn exposure_saturation_joins_are_smooth() {
    let look = exposure_look();
    for join in [-12.0 - S_SHADOW_TRANSITION, 6.0 + S_HIGHLIGHT_TRANSITION] {
        let steps = [1e-3, 1e-4, 1e-5];
        let values = steps.map(|h| fd2(&look, join, h));
        let measured = exponent(values[0], values[2], steps[0] / steps[2]);
        println!("exposure saturation {join} fd2: {values:?} exponent {measured:?}");
        let eps = 1e-6;
        let center = sample(&look, join);
        let left = sample(&look, join - eps);
        let right = sample(&look, join + eps);
        for channel in 0..3 {
            assert!((left[channel] - right[channel]).abs() < 1e-4);
            let left_slope = (center[channel] - left[channel]) / eps;
            let right_slope = (right[channel] - center[channel]) / eps;
            assert!(
                (left_slope - right_slope).abs() < 1e-3,
                "saturation {join} channel {channel} slopes {left_slope} {right_slope}"
            );
        }
        let h = 1e-6;
        let backward = one_sided_second(&look, join, h, false);
        let forward = one_sided_second(&look, join, h, true);
        for channel in 0..3 {
            assert!(
                backward[channel].abs() < 1e-2 && forward[channel].abs() < 1e-2,
                "saturation {join} channel {channel} second {backward:?} {forward:?}"
            );
        }
    }
}

#[test]
fn identity_look_is_unchanged_inside_the_old_domain() {
    let look = Look::colorimetric();
    for step in 0..=180 {
        let s = -12.0 + 18.0 * step as f64 / 180.0;
        let input = LookInput::new(neutral_at(s));
        let output = look.eval_input(&input);
        let expected = looklab_model::solve::ucs_of_xyz(neutral_at(s));
        assert!((output.j - expected.j).abs() < 1e-9, "s={s}");
        assert!((output.a - expected.a).abs() < 1e-9, "s={s}");
        assert!((output.b - expected.b).abs() < 1e-9, "s={s}");
    }
}
