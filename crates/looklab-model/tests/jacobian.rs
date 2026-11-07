// SPDX-License-Identifier: AGPL-3.0-or-later
//! Layer Jacobian against central finite differences.

use looklab_color::Xyz;
use looklab_model::look::{LayerValues, LookInput};
use looklab_model::{Look, Tone};

fn sample_look() -> Look {
    let mut look = Look::colorimetric();
    let mut beta = [0.0_f64; looklab_model::basis::TONE_COEFS];
    beta[3] = 0.1;
    beta[11] = 0.25;
    look.tone = Tone::spline(0.35, beta).expect("non-negative betas");
    look.tint.a[2] = 0.4;
    look.tint.a[7] = -0.2;
    look.tint.b[5] = -0.3;
    look.color.g.plane[3] = 0.2;
    look.color.g.volume[10] = -0.1;
    look.color.r.plane[4] = 5.0;
    look.color.r.volume[37] = 4.0;
    look.color.lambda.plane[2] = 1.0;
    look.color.lambda.volume[101] = -0.7;
    look.residual.values[100] = 0.5;
    look.residual.values[5000] = -0.4;
    look.residual.values[14000] = 0.2;
    look
}

fn shifted(values: &LayerValues, column: usize, delta: f64) -> LayerValues {
    let mut out = *values;
    match column {
        0 => out.t += delta,
        1 => out.n_a += delta,
        2 => out.n_b += delta,
        3 => out.log_g += delta,
        4 => out.r += delta,
        5 => out.lambda += delta,
        other => out.residual[other - 6] += delta,
    }
    out
}

struct SplitMix64(u64);

impl SplitMix64 {
    fn next(&mut self) -> f64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        ((z ^ (z >> 31)) >> 11) as f64 / (1u64 << 53) as f64
    }

    fn range(&mut self, lo: f64, hi: f64) -> f64 {
        lo + (hi - lo) * self.next()
    }
}

#[test]
fn jacobian_matches_finite_differences() {
    let look = sample_look();
    let mut rng = SplitMix64(0x5eed_9876_5432_1000);
    for _ in 0..40 {
        let y = rng.range(0.01, 3.0);
        let xyz = Xyz::new(y * rng.range(0.3, 1.8), y, y * rng.range(0.2, 1.9));
        let input = LookInput::new(xyz);
        let values = look.layer_values(&input);
        let jacobian = look.d_output_d_layer_values(&input, &values);
        let steps = [1e-5, 1e-3, 1e-3, 1e-5, 1e-3, 1e-3, 1e-3, 1e-3, 1e-3];
        for column in 0..9 {
            let h = steps[column];
            let plus = look.eval_from_layer_values(&input, &shifted(&values, column, h));
            let minus = look.eval_from_layer_values(&input, &shifted(&values, column, -h));
            let numerical = [
                (plus.j - minus.j) / (2.0 * h),
                (plus.a - minus.a) / (2.0 * h),
                (plus.b - minus.b) / (2.0 * h),
            ];
            for row in 0..3 {
                let analytical = jacobian[row][column];
                let scale = numerical[row].abs().max(analytical.abs()).max(1.0);
                let error = (analytical - numerical[row]).abs() / scale;
                assert!(
                    error < 5e-5,
                    "input {xyz:?} column {column} row {row}: analytical {analytical} numerical {}",
                    numerical[row]
                );
            }
        }
    }
}

#[test]
fn layer_jacobian_matches_saturated_solver() {
    use looklab_model::neutral_j_prime_max;

    let look = Look::colorimetric();
    let input = LookInput::new(Xyz::new(0.4, 0.3, 0.2));
    let max = neutral_j_prime_max();
    let h = 1e-5;
    for t in [max + 1.0, -1.0, 150.0] {
        let mut values = look.layer_values(&input);
        values.t = t;
        let jacobian = look.d_output_d_layer_values(&input, &values);
        let plus = look.eval_from_layer_values(&input, &shifted(&values, 0, h));
        let minus = look.eval_from_layer_values(&input, &shifted(&values, 0, -h));
        let numerical = [
            (plus.j - minus.j) / (2.0 * h),
            (plus.a - minus.a) / (2.0 * h),
            (plus.b - minus.b) / (2.0 * h),
        ];
        for (row, column) in jacobian.iter().enumerate() {
            let scale = numerical[row].abs().max(column[0].abs()).max(1.0);
            assert!(
                (column[0] - numerical[row]).abs() / scale < 1e-4,
                "t {t} row {row}: {} vs {}",
                column[0],
                numerical[row]
            );
        }
        if t > max {
            for column in &jacobian {
                assert_eq!(column[0], 0.0);
            }
        }
    }
}

fn full_look() -> Look {
    let mut beta = [0.0_f64; looklab_model::basis::TONE_COEFS];
    for (index, value) in beta.iter_mut().enumerate() {
        *value = 0.3 + 0.01 * index as f64;
    }
    let tint = looklab_model::Tint {
        a: (0..12).map(|index| 0.2 * libm::sin(index as f64)).collect(),
        b: (0..12)
            .map(|index| 0.15 * libm::cos(index as f64))
            .collect(),
    };
    let mut color = looklab_model::ColorLayer::zeros();
    for component in [&mut color.g, &mut color.r, &mut color.lambda] {
        for (index, coefficient) in component
            .plane
            .iter_mut()
            .chain(component.volume.iter_mut())
            .enumerate()
        {
            *coefficient = 0.05 * libm::sin(index as f64 * 0.01);
        }
    }
    let residual = looklab_model::Residual {
        values: (0..looklab_model::basis::RESIDUAL_COEFS)
            .map(|index| 0.02 * libm::sin(index as f64 * 0.003))
            .collect(),
    };
    Look::new(
        Tone::spline(1.0, beta).expect("non-negative betas"),
        tint,
        color,
        residual,
        looklab_model::ClipSettings::default_v1(),
    )
    .expect("valid look")
}

#[test]
fn coefficient_jacobian_matches_perturbations() {
    let look = full_look();
    let input = LookInput::new(Xyz::new(0.5, 0.4, 0.3));
    let coefficients = look.coef_vector();
    let terms = look.coef_jacobian(&input);
    assert!(terms.len() > 200, "terms {}", terms.len());
    let h = 1e-6;
    for term in terms {
        let mut plus = coefficients.clone();
        let mut minus = coefficients.clone();
        plus[term.index] += h;
        minus[term.index] -= h;
        let mut look_plus = look.clone();
        let mut look_minus = look.clone();
        look_plus.apply_coef(&plus).expect("plus");
        look_minus.apply_coef(&minus).expect("minus");
        let p = look_plus.eval_input(&input);
        let m = look_minus.eval_input(&input);
        let numerical = [
            (p.j - m.j) / (2.0 * h),
            (p.a - m.a) / (2.0 * h),
            (p.b - m.b) / (2.0 * h),
        ];
        for (row, value) in numerical.iter().enumerate() {
            let scale = value.abs().max(term.derivative[row].abs()).max(1.0);
            let error = (term.derivative[row] - value).abs() / scale;
            assert!(
                error < 5e-4,
                "index {} row {row}: {} vs {}",
                term.index,
                term.derivative[row],
                value
            );
        }
    }
}

#[test]
fn tint_derivative_follows_the_c2_continuation() {
    let look = full_look();
    let evaluate = |t: f64| {
        let (indices, weights, _) = looklab_model::basis::tint_weights(t);
        let mut a = 0.0;
        let mut b = 0.0;
        for (position, index) in indices.iter().enumerate() {
            a += weights[position] * look.tint.a[*index];
            b += weights[position] * look.tint.b[*index];
        }
        (a, b)
    };
    for t in [50.0, 120.0] {
        let (a, b) = look.d_tint_d_t(t);
        let h = 1e-6;
        let plus = evaluate(t + h);
        let minus = evaluate(t - h);
        let numerical_a = (plus.0 - minus.0) / (2.0 * h);
        let numerical_b = (plus.1 - minus.1) / (2.0 * h);
        assert!(
            (a - numerical_a).abs() < 1e-5,
            "t {t} a {a} vs {numerical_a}"
        );
        assert!(
            (b - numerical_b).abs() < 1e-5,
            "t {t} b {b} vs {numerical_b}"
        );
    }
    for t in [-1.0, 150.0] {
        let (a, b) = look.d_tint_d_t(t);
        assert!(a.is_finite() && b.is_finite());
    }
}

#[test]
fn neutral_jacobian_has_unit_tone_response() {
    let look = Look::colorimetric();
    let input = LookInput::new(looklab_color::xyy::Illuminant::D65.xyz().scale(0.5));
    let values = look.layer_values(&input);
    let jacobian = look.d_output_d_layer_values(&input, &values);
    assert!((jacobian[0][0] - 1.0).abs() < 1e-9);
    assert_eq!(jacobian[0][5], 1.0);
    assert_eq!(jacobian[0][6], 1.0);
    assert_eq!(jacobian[1][1], 1.0);
    assert_eq!(jacobian[2][2], 1.0);
}
