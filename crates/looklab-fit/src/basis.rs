// SPDX-License-Identifier: AGPL-3.0-or-later
//! Spline values and derivatives over the model's knot vectors. Cubic B-splines per Piegl & Tiller (1997) A2.2.

use looklab_model::basis::{C_MAX, HUE_COEFS, S_KNOTS};

static TONE_BASIS: std::sync::LazyLock<looklab_model::basis::ToneBasis> =
    std::sync::LazyLock::new(looklab_model::basis::ToneBasis::new);

/// `dI_k/ds` of the tone I-spline at `s` (`ToneBasis::derivative`).
pub(crate) fn tone_basis_derivative(s: f64) -> [f64; 20] {
    TONE_BASIS.derivative(s)
}

pub(crate) fn find_span(knots: &[f64], degree: usize, x: f64) -> usize {
    let n = knots.len() - degree - 1;
    if x >= knots[n] {
        let mut index = n - 1;
        while index > degree && knots[index] == knots[index + 1] {
            index -= 1;
        }
        return index;
    }
    if x <= knots[degree] {
        let mut index = degree;
        while index < n - 1 && knots[index] == knots[index + 1] {
            index += 1;
        }
        return index;
    }
    let mut lo = degree;
    let mut hi = n;
    while hi - lo > 1 {
        let mid = lo + (hi - lo) / 2;
        if knots[mid] <= x {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    lo
}

fn basis(knots: &[f64], degree: usize, x: f64, out: &mut [f64]) -> usize {
    let span = find_span(knots, degree, x);
    out.fill(0.0);
    out[0] = 1.0;
    let mut left = [0.0_f64; 8];
    let mut right = [0.0_f64; 8];
    for j in 1..=degree {
        left[j] = x - knots[span + 1 - j];
        right[j] = knots[span + j] - x;
        let mut saved = 0.0;
        for r in 0..j {
            let denom = right[r + 1] + left[j - r];
            let temp = if denom == 0.0 { 0.0 } else { out[r] / denom };
            out[r] = saved + right[r + 1] * temp;
            saved = left[j - r] * temp;
        }
        out[j] = saved;
    }
    span - degree
}

/// Open knot vector; the periodic hue axis uses [`hue_values`].
pub(crate) fn cubic_with_derivative(knots: &[f64], x: f64) -> (usize, [f64; 4], [f64; 4]) {
    let mut values = [0.0_f64; 4];
    let start = basis(knots, 3, x, &mut values);
    let mut quadratic = [0.0_f64; 3];
    let start_q = basis(knots, 2, x, &mut quadratic);
    let q_at = |i: usize| {
        if i >= start_q && i < start_q + 3 {
            quadratic[i - start_q]
        } else {
            0.0
        }
    };
    let mut derivative = [0.0_f64; 4];
    for (m, i) in (start..start + 4).enumerate() {
        let d0 = knots[i + 3] - knots[i];
        let d1 = knots[i + 4] - knots[i + 1];
        let t0 = if d0 == 0.0 { 0.0 } else { q_at(i) / d0 };
        let t1 = if d1 == 0.0 { 0.0 } else { q_at(i + 1) / d1 };
        derivative[m] = 3.0 * (t0 - t1);
    }
    (start, values, derivative)
}

/// `s`-axis weights (`S_KNOTS`) and `d/ds`; extrapolates outside the knot range.
pub(crate) fn s_values(s: f64) -> ([usize; 4], [f64; 4], [f64; 4]) {
    let (start, values, derivative) = cubic_with_derivative(&S_KNOTS, s);
    ([start, start + 1, start + 2, start + 3], values, derivative)
}

/// Tint weights over `J'_base` and `d/dJ'`; the model's C² continuation above the join.
pub(crate) fn tint_values(j_prime: f64) -> ([usize; 4], [f64; 4], [f64; 4]) {
    looklab_model::basis::tint_weights(j_prime)
}

/// Periodic uniform cubic hue weights and `d/dh` (degrees).
pub(crate) fn hue_values(h_degrees: f64) -> ([usize; 4], [f64; 4], [f64; 4]) {
    let u = h_degrees / 360.0 * HUE_COEFS as f64;
    let base = u.floor();
    let f = u - base;
    let i = base as i64;
    let indices = [
        (i - 1).rem_euclid(HUE_COEFS as i64) as usize,
        i.rem_euclid(HUE_COEFS as i64) as usize,
        (i + 1).rem_euclid(HUE_COEFS as i64) as usize,
        (i + 2).rem_euclid(HUE_COEFS as i64) as usize,
    ];
    let one_minus = 1.0 - f;
    let weights = [
        one_minus * one_minus * one_minus / 6.0,
        (3.0 * f * f * f - 6.0 * f * f + 4.0) / 6.0,
        (-3.0 * f * f * f + 3.0 * f * f + 3.0 * f + 1.0) / 6.0,
        f * f * f / 6.0,
    ];
    let slope = HUE_COEFS as f64 / 360.0;
    let derivatives = [
        -(one_minus * one_minus) / 2.0 * slope,
        (9.0 * f * f - 12.0 * f) / 6.0 * slope,
        (-9.0 * f * f + 6.0 * f + 3.0) / 6.0 * slope,
        f * f / 2.0 * slope,
    ];
    (indices, weights, derivatives)
}

/// Bernstein weights in `√(C/C_MAX)` and `d/dC`.
pub(crate) fn c_values(c: f64) -> ([usize; 4], [f64; 4], [f64; 4]) {
    let u = (c / C_MAX).sqrt().clamp(0.0, 1.0);
    let v = 1.0 - u;
    let weights = [v * v * v, 3.0 * u * v * v, 3.0 * u * u * v, u * u * u];
    let du_dc = if c <= 0.0 {
        0.0
    } else {
        1.0 / (2.0 * (c * C_MAX).sqrt())
    };
    let du = [
        -3.0 * v * v,
        3.0 * (v * v - 2.0 * u * v),
        3.0 * (2.0 * u * v - u * u),
        3.0 * u * u,
    ];
    let mut derivatives = [0.0_f64; 4];
    for m in 0..4 {
        derivatives[m] = du[m] * du_dc;
    }
    ([0, 1, 2, 3], weights, derivatives)
}

/// Cubic B-spline values, first and second derivatives at `x`.
pub(crate) fn cubic_with_second(knots: &[f64], x: f64) -> (usize, [f64; 4], [f64; 4], [f64; 4]) {
    let (start, values, derivative) = cubic_with_derivative(knots, x);
    let mut linear = [0.0_f64; 2];
    let start_linear = basis(knots, 1, x, &mut linear);
    let mut second = [0.0_f64; 4];
    for (position, k) in (start_linear..start_linear + 2).enumerate() {
        let weight = linear[position];
        let denominator = knots[k + 2] - knots[k];
        if weight == 0.0 || denominator == 0.0 {
            continue;
        }
        let left_span = knots[k + 3] - knots[k];
        let right_span = if k >= 1 {
            knots[k + 2] - knots[k - 1]
        } else {
            0.0
        };
        let left = if left_span == 0.0 {
            0.0
        } else {
            1.0 / left_span
        };
        let right = if right_span == 0.0 {
            0.0
        } else {
            1.0 / right_span
        };
        let scale = 6.0 * weight / denominator;
        if k >= start && k < start + 4 {
            second[k - start] += scale * left;
        }
        if k >= 1 {
            let coefficient = k - 1;
            if coefficient >= start && coefficient < start + 4 {
                second[coefficient - start] -= scale * (left + right);
            }
        }
        if k >= 2 {
            let coefficient = k - 2;
            if coefficient >= start && coefficient < start + 4 {
                second[coefficient - start] += scale * right;
            }
        }
    }
    (start, values, derivative, second)
}

/// `s`-axis weights and first two derivatives w.r.t. `s`.
pub(crate) fn s_values_second(s: f64) -> ([usize; 4], [f64; 4], [f64; 4], [f64; 4]) {
    let (start, values, derivative, second) = cubic_with_second(&S_KNOTS, s);
    (
        [start, start + 1, start + 2, start + 3],
        values,
        derivative,
        second,
    )
}

/// Periodic hue weights and first two derivatives w.r.t. degrees.
pub(crate) fn hue_values_second(h_degrees: f64) -> ([usize; 4], [f64; 4], [f64; 4], [f64; 4]) {
    let u = h_degrees / 360.0 * HUE_COEFS as f64;
    let base = u.floor();
    let f = u - base;
    let i = base as i64;
    let indices = [
        (i - 1).rem_euclid(HUE_COEFS as i64) as usize,
        i.rem_euclid(HUE_COEFS as i64) as usize,
        (i + 1).rem_euclid(HUE_COEFS as i64) as usize,
        (i + 2).rem_euclid(HUE_COEFS as i64) as usize,
    ];
    let one_minus = 1.0 - f;
    let weights = [
        one_minus * one_minus * one_minus / 6.0,
        (3.0 * f * f * f - 6.0 * f * f + 4.0) / 6.0,
        (-3.0 * f * f * f + 3.0 * f * f + 3.0 * f + 1.0) / 6.0,
        f * f * f / 6.0,
    ];
    let first = [
        -(one_minus * one_minus) / 2.0,
        (9.0 * f * f - 12.0 * f) / 6.0,
        (-9.0 * f * f + 6.0 * f + 3.0) / 6.0,
        f * f / 2.0,
    ];
    let second = [one_minus, 3.0 * f - 2.0, 1.0 - 3.0 * f, f];
    let slope = HUE_COEFS as f64 / 360.0;
    let mut derivatives = [0.0_f64; 4];
    let mut curvature = [0.0_f64; 4];
    for m in 0..4 {
        derivatives[m] = first[m] * slope;
        curvature[m] = second[m] * slope * slope;
    }
    (indices, weights, derivatives, curvature)
}

/// Bernstein weights in `sqrt(C/C_MAX)` and first two derivatives w.r.t. `C`.
pub(crate) fn c_values_second(c: f64) -> ([usize; 4], [f64; 4], [f64; 4], [f64; 4]) {
    let (indices, weights, _) = c_values(c);
    let mut first = [0.0_f64; 4];
    let mut second = [0.0_f64; 4];
    if c > 0.0 && c < C_MAX {
        let u = (c / C_MAX).sqrt();
        let v = 1.0 - u;
        let du = [
            -3.0 * v * v,
            3.0 * (v * v - 2.0 * u * v),
            3.0 * (2.0 * u * v - u * u),
            3.0 * u * u,
        ];
        let ddu = [
            6.0 * v,
            6.0 * (3.0 * u - 2.0),
            6.0 * (1.0 - 3.0 * u),
            6.0 * u,
        ];
        let du_dc = 1.0 / (2.0 * (c * C_MAX).sqrt());
        let ddu_dc = -1.0 / (4.0 * c * (c * C_MAX).sqrt());
        for m in 0..4 {
            first[m] = du[m] * du_dc;
            second[m] = ddu[m] * du_dc * du_dc + du[m] * ddu_dc;
        }
    }
    (indices, weights, first, second)
}

/// Second derivative of the tone I-spline basis: `d²I_k/ds² = dB_k/ds`.
pub(crate) fn tone_basis_second_derivative(s: f64) -> [f64; 20] {
    let (start, _, derivative, _) = cubic_with_second(&looklab_model::basis::TONE_KNOTS, s);
    let mut out = [0.0_f64; 20];
    out[start..start + 4].copy_from_slice(&derivative);
    out
}

/// Residual control-point weights and first two derivatives w.r.t. `(s, a, b)`.
#[allow(clippy::type_complexity)]
pub(crate) fn residual_values_second(
    s: f64,
    a: f64,
    b: f64,
) -> ([usize; 64], [f64; 64], [[f64; 64]; 3], [[[f64; 64]; 3]; 3]) {
    let domain = looklab_model::look::RESIDUAL_DOMAIN;
    let (a_coord, a_scale) = residual_coord_and_scale(a, domain[1]);
    let (b_coord, b_scale) = residual_coord_and_scale(b, domain[2]);
    let s_coord = (s - domain[0][0]) / (domain[0][1] - domain[0][0]);
    let (si, sw, dsw, ddsw) = residual_axis_second(s_coord);
    let (aj, aw, daw, ddaw) = residual_axis_second(a_coord);
    let (bk, bw, dbw, ddbw) = residual_axis_second(b_coord);
    let span = [
        domain[0][1] - domain[0][0],
        domain[1][1] - domain[1][0],
        domain[2][1] - domain[2][0],
    ];
    let mut indices = [0_usize; 64];
    let mut weights = [0.0_f64; 64];
    let mut first = [[0.0_f64; 64]; 3];
    let mut second = [[[0.0_f64; 64]; 3]; 3];
    let mut position = 0;
    for i in 0..4 {
        for j in 0..4 {
            for k in 0..4 {
                let grid = looklab_model::basis::RESIDUAL_GRID;
                indices[position] = (si[i] * grid + aj[j]) * grid + bk[k];
                weights[position] = sw[i] * aw[j] * bw[k];
                first[0][position] = dsw[i] * aw[j] * bw[k] / span[0];
                first[1][position] = sw[i] * daw[j] * bw[k] * a_scale / span[1];
                first[2][position] = sw[i] * aw[j] * dbw[k] * b_scale / span[2];
                second[0][0][position] = ddsw[i] * aw[j] * bw[k] / (span[0] * span[0]);
                second[1][1][position] =
                    sw[i] * ddaw[j] * bw[k] * a_scale * a_scale / (span[1] * span[1]);
                second[2][2][position] =
                    sw[i] * aw[j] * ddbw[k] * b_scale * b_scale / (span[2] * span[2]);
                second[0][1][position] = dsw[i] * daw[j] * bw[k] * a_scale / (span[0] * span[1]);
                second[1][0][position] = second[0][1][position];
                second[0][2][position] = dsw[i] * aw[j] * dbw[k] * b_scale / (span[0] * span[2]);
                second[2][0][position] = second[0][2][position];
                second[1][2][position] =
                    sw[i] * daw[j] * dbw[k] * a_scale * b_scale / (span[1] * span[2]);
                second[2][1][position] = second[1][2][position];
                position += 1;
            }
        }
    }
    (indices, weights, first, second)
}

fn residual_coord_and_scale(value: f64, bounds: [f64; 2]) -> (f64, f64) {
    let raw = (value - bounds[0]) / (bounds[1] - bounds[0]);
    if raw > 0.0 && raw < 1.0 {
        (raw, 1.0)
    } else {
        (raw.clamp(0.0, 1.0), 0.0)
    }
}

fn residual_axis_second(x: f64) -> ([usize; 4], [f64; 4], [f64; 4], [f64; 4]) {
    let (start, values, derivative, second) =
        cubic_with_second(&looklab_model::basis::RESIDUAL_KNOTS, x);
    (
        [start, start + 1, start + 2, start + 3],
        values,
        derivative,
        second,
    )
}

/// Near-neutral core basis and derivatives w.r.t. `(a, b)`.
#[allow(clippy::type_complexity)]
pub(crate) fn core_weights_derivatives(
    a: f64,
    b: f64,
) -> ([f64; 5], [[f64; 5]; 2], [[[f64; 5]; 2]; 2]) {
    let join = looklab_model::basis::NEUTRAL_JOIN_C;
    let x = a / join;
    let y = b / join;
    let value = [x, y, x * x, x * y, y * y];
    let inv = 1.0 / join;
    let first = [
        [inv, 0.0, 2.0 * x * inv, y * inv, 0.0],
        [0.0, inv, 0.0, x * inv, 2.0 * y * inv],
    ];
    let inv2 = inv * inv;
    let second = [
        [[0.0, 0.0, 2.0 * inv2, 0.0, 0.0], [0.0; 5]],
        [[0.0; 5], [0.0, 0.0, 0.0, 0.0, 2.0 * inv2]],
    ];
    let mut mixed = [[0.0_f64; 5]; 2];
    mixed[0][3] = inv2;
    let mut second = second;
    second[0][1] = mixed[0];
    second[1][0] = mixed[0];
    (value, first, second)
}

pub(crate) fn exposure_lookup_second_derivative(s: f64) -> f64 {
    let low = looklab_model::basis::S_KNOTS[3];
    let high = looklab_model::basis::S_KNOTS[looklab_model::basis::S_COEFS];
    if s >= low && s <= high {
        return 0.0;
    }
    if s < low {
        let start = low - looklab_model::basis::S_SHADOW_TRANSITION;
        if s <= start {
            return 0.0;
        }
        let width = looklab_model::basis::S_SHADOW_TRANSITION;
        let y = (s - start) / width;
        return (60.0 * y * y * y - 96.0 * y * y + 36.0 * y) / width;
    }
    let end = high + looklab_model::basis::S_HIGHLIGHT_TRANSITION;
    if s >= end {
        return 0.0;
    }
    let width = looklab_model::basis::S_HIGHLIGHT_TRANSITION;
    let y = (s - high) / width;
    (60.0 * y * y * y - 84.0 * y * y + 24.0 * y) / width
}

/// Second-difference rows over `n` coefficients.
pub(crate) fn second_difference(n: usize, periodic: bool) -> Vec<Vec<f64>> {
    let rows = if periodic { n } else { n.saturating_sub(2) };
    let mut out = Vec::with_capacity(rows);
    for row in 0..rows {
        let mut entries = vec![0.0; n];
        if periodic {
            entries[row] = 1.0;
            entries[(row + n - 1) % n] -= 2.0;
            entries[(row + n - 2) % n] += 1.0;
        } else {
            entries[row] = 1.0;
            entries[row + 1] -= 2.0;
            entries[row + 2] += 1.0;
        }
        out.push(entries);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use looklab_color::Xyz;
    use looklab_model::LookInput;

    #[test]
    fn s_and_hue_weights_match_the_model() {
        let mut state = 0x9e37_79b9_7f4a_7c15_u64;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state >> 11) as f64 / (1u64 << 53) as f64
        };
        for _ in 0..200 {
            let xyz = Xyz::new(0.05 + next(), 0.05 + next(), 0.05 + next());
            let input = LookInput::new(xyz);
            let (indices, values, _) = s_values(input.s);
            for m in 0..4 {
                assert_eq!(indices[m], input.grl.s_idx[m]);
                assert!((values[m] - input.grl.s_w[m]).abs() < 1e-12);
            }
            let (hue_indices, hue_values, _) = hue_values(input.h_in);
            for m in 0..4 {
                assert_eq!(hue_indices[m], input.grl.hue_idx[m]);
                assert!((hue_values[m] - input.grl.hue_w[m]).abs() < 1e-12);
            }
        }
    }

    #[test]
    fn derivatives_match_finite_differences() {
        let (_, _, ds) = {
            let (i, v, d) = s_values(0.7);
            (i, v, d)
        };
        let h = 1e-6;
        let (_, plus, _) = s_values(0.7 + h);
        let (_, minus, _) = s_values(0.7 - h);
        for m in 0..4 {
            let numerical = (plus[m] - minus[m]) / (2.0 * h);
            assert!((ds[m] - numerical).abs() < 1e-6);
        }

        let (_, _, dh) = hue_values(47.0);
        let (_, hp, _) = hue_values(47.0 + h);
        let (_, hm, _) = hue_values(47.0 - h);
        for m in 0..4 {
            let numerical = (hp[m] - hm[m]) / (2.0 * h);
            assert!((dh[m] - numerical).abs() < 1e-6, "m {m} {dh:?}");
        }

        let (_, _, dc) = c_values(30.0);
        let (_, cp, _) = c_values(30.0 + h);
        let (_, cm, _) = c_values(30.0 - h);
        for m in 0..4 {
            let numerical = (cp[m] - cm[m]) / (2.0 * h);
            assert!((dc[m] - numerical).abs() < 1e-6);
        }
    }
}
