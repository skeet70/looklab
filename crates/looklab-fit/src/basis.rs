// SPDX-License-Identifier: AGPL-3.0-or-later
//! Spline values and derivatives over the model's knot vectors. Cubic B-splines per Piegl & Tiller (1997) A2.2.

use looklab_model::basis::{
    C_MAX, HUE_COEFS, RESIDUAL_GRID, S_COEFS, S_KNOTS, TINT_COEFS, TINT_KNOTS,
};

static TONE_BASIS: std::sync::LazyLock<looklab_model::basis::ToneBasis> =
    std::sync::LazyLock::new(looklab_model::basis::ToneBasis::new);

/// `dI_k/ds` of the tone I-spline at `s` (`ToneBasis::derivative`).
pub(crate) fn tone_basis_derivative(s: f64) -> [f64; 20] {
    TONE_BASIS.derivative(s)
}

pub(crate) fn find_span(knots: &[f64], degree: usize, x: f64) -> usize {
    let n = knots.len() - degree - 1;
    if x >= knots[n] {
        return n - 1;
    }
    if x <= knots[degree] {
        return degree;
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

/// `s`-axis weights (`S_KNOTS`) and `d/ds`.
pub(crate) fn s_values(s: f64) -> ([usize; 4], [f64; 4], [f64; 4]) {
    let (start, values, derivative) =
        cubic_with_derivative(&S_KNOTS, s.clamp(S_KNOTS[3], S_KNOTS[S_COEFS]));
    ([start, start + 1, start + 2, start + 3], values, derivative)
}

/// Tint weights over `J'_base` (`TINT_KNOTS`) and `d/dJ'`.
pub(crate) fn tint_values(j_prime: f64) -> ([usize; 4], [f64; 4], [f64; 4]) {
    let x = j_prime.clamp(TINT_KNOTS[3], TINT_KNOTS[TINT_COEFS]);
    let (start, values, derivative) = cubic_with_derivative(&TINT_KNOTS, x);
    ([start, start + 1, start + 2, start + 3], values, derivative)
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

/// Trilinear corner indices and weights plus `d/du`, `d/dv`, `d/dw` (normalized coords).
#[allow(clippy::type_complexity)]
pub(crate) fn trilinear(u: f64, v: f64, w: f64) -> ([usize; 8], [f64; 8], [[f64; 8]; 3]) {
    let last = RESIDUAL_GRID - 1;
    let coord = |value: f64| {
        let g = value.clamp(0.0, 1.0) * last as f64;
        let i = g.floor();
        let f = g - i;
        let i = (i as usize).min(last);
        let i1 = (i + 1).min(last);
        (i, i1, f)
    };
    let (i0, i1, fu) = coord(u);
    let (j0, j1, fv) = coord(v);
    let (k0, k1, fw) = coord(w);
    let indices = [
        i0 * RESIDUAL_GRID * RESIDUAL_GRID + j0 * RESIDUAL_GRID + k0,
        i0 * RESIDUAL_GRID * RESIDUAL_GRID + j0 * RESIDUAL_GRID + k1,
        i0 * RESIDUAL_GRID * RESIDUAL_GRID + j1 * RESIDUAL_GRID + k0,
        i0 * RESIDUAL_GRID * RESIDUAL_GRID + j1 * RESIDUAL_GRID + k1,
        i1 * RESIDUAL_GRID * RESIDUAL_GRID + j0 * RESIDUAL_GRID + k0,
        i1 * RESIDUAL_GRID * RESIDUAL_GRID + j0 * RESIDUAL_GRID + k1,
        i1 * RESIDUAL_GRID * RESIDUAL_GRID + j1 * RESIDUAL_GRID + k0,
        i1 * RESIDUAL_GRID * RESIDUAL_GRID + j1 * RESIDUAL_GRID + k1,
    ];
    let weights = [
        (1.0 - fu) * (1.0 - fv) * (1.0 - fw),
        (1.0 - fu) * (1.0 - fv) * fw,
        (1.0 - fu) * fv * (1.0 - fw),
        (1.0 - fu) * fv * fw,
        fu * (1.0 - fv) * (1.0 - fw),
        fu * (1.0 - fv) * fw,
        fu * fv * (1.0 - fw),
        fu * fv * fw,
    ];
    let scale = last as f64;
    let mut du = [0.0_f64; 8];
    let mut dv = [0.0_f64; 8];
    let mut dw = [0.0_f64; 8];
    for index in 0..8 {
        let high_u = index >= 4;
        let high_v = (index & 2) != 0;
        let high_w = (index & 1) != 0;
        let sign = |high: bool| if high { 1.0 } else { -1.0 };
        let bu = if high_u { fu } else { 1.0 - fu };
        let bv = if high_v { fv } else { 1.0 - fv };
        let bw = if high_w { fw } else { 1.0 - fw };
        du[index] = sign(high_u) * scale * bv * bw;
        dv[index] = sign(high_v) * scale * bu * bw;
        dw[index] = sign(high_w) * scale * bu * bv;
    }
    (indices, weights, [du, dv, dw])
}

/// residual-grid coordinates for `(s, a_in, b_in)` in `[0, 1]^3`.
pub(crate) fn residual_coords(s: f64, a: f64, b: f64) -> [f64; 3] {
    let domain = looklab_model::look::RESIDUAL_DOMAIN;
    let normalize = |value: f64, bounds: [f64; 2]| {
        ((value - bounds[0]) / (bounds[1] - bounds[0])).clamp(0.0, 1.0)
    };
    [
        normalize(s, domain[0]),
        normalize(a, domain[1]),
        normalize(b, domain[2]),
    ]
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
