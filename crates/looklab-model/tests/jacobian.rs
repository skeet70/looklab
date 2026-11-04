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
