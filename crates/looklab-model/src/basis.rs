// SPDX-License-Identifier: AGPL-3.0-or-later
//! Cubic B-splines per Piegl & Tiller, *The NURBS Book* (1997), Algorithm A2.2.
//! The tone basis is their integral (I-spline, Ramsay 1988, Statistical Science
//! 3(4)); two-point Gauss-Legendre integrates the cubic pieces exactly.

/// Coefficients of the tone I-spline: `(c0, beta_0..beta_19)`.
pub const TONE_COEFS: usize = 20;

/// Twenty coefficients over `[-12, 6]`, dense in `[-4, 3]` (docs/02 §3.1).
pub const TONE_KNOTS: [f64; 24] = [
    -12.0, -12.0, -12.0, -12.0, -10.0, -8.0, -6.5, -5.0, -4.5, -4.0, -3.0, -2.0, -1.0, 0.0, 1.0,
    2.0, 3.0, 3.5, 4.5, 5.0, 6.0, 6.0, 6.0, 6.0,
];

/// Ten coefficients; every interior knot is drawn from [`TONE_KNOTS`].
pub const S_KNOTS: [f64; 14] = [
    -12.0, -12.0, -12.0, -12.0, -8.0, -4.0, -2.0, 0.0, 2.0, 3.5, 6.0, 6.0, 6.0, 6.0,
];

/// Number of coefficients on the `s` axis of the chroma/hue/lightness layer.
pub const S_COEFS: usize = 10;

/// Hue knots: one coefficient per 15 degrees.
pub const HUE_COEFS: usize = 24;

/// Coefficients on the `sqrt(C)` axis.
pub const C_COEFS: usize = 4;

/// Maximum `M'` the `sqrt(C)` axis covers; larger inputs clamp to the last knot.
pub const C_MAX: f64 = 120.0;

/// Number of Cartesian monomials in the near-neutral core.
pub const CORE_TERMS: usize = 5;

/// Coefficients of the near-neutral core, indexed `[s][term]`.
pub const CORE_COEFS: usize = S_COEFS * CORE_TERMS;

/// Coefficients on the `C > 0` hue/s volume.
pub const VOLUME_COEFS: usize = HUE_COEFS * S_COEFS * 3;

/// Radius in CAM16-UCS `M'` at which the outer representation is rejoined.
pub const NEUTRAL_JOIN_C: f64 = 2.0;

/// Coefficients of each tint spline `(a', b')`, over `J'_base`.
pub const TINT_COEFS: usize = 12;

/// Twelve coefficients over `J' in [0, 100]`.
pub const TINT_KNOTS: [f64; 16] = [
    0.0, 0.0, 0.0, 0.0, 1.0, 4.0, 10.0, 20.0, 32.0, 45.0, 60.0, 80.0, 100.0, 100.0, 100.0, 100.0,
];

/// Number of residual grid nodes per axis.
pub const RESIDUAL_GRID: usize = 17;

/// Three channels per node, channel-minor.
pub const RESIDUAL_COEFS: usize = RESIDUAL_GRID * RESIDUAL_GRID * RESIDUAL_GRID * 3;

/// Span index `i` with `knots[i] <= x < knots[i + 1]`, clamped to the domain.
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

/// Writes `degree + 1` non-zero B-spline weights into `out` and returns the
/// index of the first coefficient. Zero-width support contributes zero.
pub(crate) fn basis(knots: &[f64], degree: usize, x: f64, out: &mut [f64]) -> usize {
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

/// Cubic B-spline weights and their derivative at `x`.
pub(crate) fn cubic_basis_with_derivative(knots: &[f64], x: f64) -> (usize, [f64; 4], [f64; 4]) {
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

/// I-spline basis over [`TONE_KNOTS`].
#[derive(Clone, Copy, Debug)]
pub struct ToneBasis {
    /// `prefix[k][j] = integral of B_k from t_k to t_j`.
    prefix: [[f64; 24]; TONE_COEFS],
}

impl ToneBasis {
    pub fn new() -> Self {
        let offset = 1.0 / 3.0_f64.sqrt();
        let mut prefix = [[0.0_f64; 24]; TONE_COEFS];
        for (k, row) in prefix.iter_mut().enumerate() {
            let mut accumulated = 0.0;
            for j in 0..24 {
                if j > k {
                    let a = TONE_KNOTS[j - 1];
                    let b = TONE_KNOTS[j];
                    if b > a {
                        let mid = 0.5 * (a + b);
                        let half = 0.5 * (b - a);
                        let delta = half * offset;
                        for x in [mid - delta, mid + delta] {
                            let mut values = [0.0_f64; 4];
                            let start = basis(&TONE_KNOTS, 3, x, &mut values);
                            if k >= start && k < start + 4 {
                                accumulated += half * values[k - start];
                            }
                        }
                    }
                }
                row[j] = accumulated;
            }
        }
        Self { prefix }
    }

    /// `I_k(s)` for `k = 0..20`.
    pub fn values(&self, x: f64) -> [f64; TONE_COEFS] {
        let span = find_span(&TONE_KNOTS, 3, x);
        let a = TONE_KNOTS[span];
        let b = TONE_KNOTS[span + 1].min(x);
        let mut partial = [0.0_f64; 4];
        let mut start = None;
        if b > a {
            let offset = 1.0 / 3.0_f64.sqrt();
            let mid = 0.5 * (a + b);
            let half = 0.5 * (b - a);
            let delta = half * offset;
            for node in [mid - delta, mid + delta] {
                let mut weights = [0.0_f64; 4];
                let first = basis(&TONE_KNOTS, 3, node, &mut weights);
                start = Some(first);
                for (m, weight) in weights.iter().enumerate() {
                    partial[m] += half * weight;
                }
            }
        }
        let mut out = [0.0_f64; TONE_COEFS];
        for (k, value) in out.iter_mut().enumerate() {
            if x >= TONE_KNOTS[k + 4] {
                *value = self.prefix[k][k + 4];
            } else if x <= TONE_KNOTS[k] {
                *value = 0.0;
            } else {
                let mut integral = self.prefix[k][span];
                if let Some(first) = start
                    && k >= first
                    && k < first + 4
                {
                    integral += partial[k - first];
                }
                *value = integral;
            }
        }
        out
    }

    /// `dI_k/ds = B_k(s)`, the cubic B-spline basis.
    pub fn derivative(&self, x: f64) -> [f64; TONE_COEFS] {
        let mut out = [0.0_f64; TONE_COEFS];
        let mut values = [0.0_f64; 4];
        let start = basis(&TONE_KNOTS, 3, x, &mut values);
        for (m, value) in values.iter().enumerate() {
            out[start + m] = *value;
        }
        out
    }
}

impl Default for ToneBasis {
    fn default() -> Self {
        Self::new()
    }
}

/// Periodic cubic weights for a hue in degrees, one coefficient per 15 degrees.
pub(crate) fn hue_weights(h_degrees: f64) -> ([usize; 4], [f64; 4]) {
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
    (indices, weights)
}

/// Bernstein weights over `[0, 1]`.
pub(crate) fn c_weights(c: f64) -> ([usize; 4], [f64; 4]) {
    let u = (c / C_MAX).sqrt().clamp(0.0, 1.0);
    let v = 1.0 - u;
    let weights = [v * v * v, 3.0 * u * v * v, 3.0 * u * u * v, u * u * u];
    ([0, 1, 2, 3], weights)
}

/// Normalized Cartesian monomials `[a, b, a², ab, b²]` over [`NEUTRAL_JOIN_C`].
pub fn core_weights(a: f64, b: f64) -> [f64; CORE_TERMS] {
    let x = a / NEUTRAL_JOIN_C;
    let y = b / NEUTRAL_JOIN_C;
    [x, y, x * x, x * y, y * y]
}

/// Outer weight `w(C)` and `dw/dC`; the C^∞ step
/// `e^(-1/x)/(e^(-1/x)+e^(-1/(1-x)))` (Lee, *Introduction to Smooth Manifolds*, §2).
pub fn outer_weight(c: f64) -> (f64, f64) {
    if !(c > 0.0) {
        return (0.0, 0.0);
    }
    if c >= NEUTRAL_JOIN_C {
        return (1.0, 0.0);
    }
    let p = 1.0 / c;
    let q = 1.0 / (NEUTRAL_JOIN_C - c);
    let a = libm::exp(-p);
    let b = libm::exp(-q);
    let sum = a + b;
    if sum == 0.0 {
        return (0.0, 0.0);
    }
    let w = a / sum;
    let slope = w * (1.0 - w) * (p * p + q * q);
    (w, slope)
}

/// Cubic B-spline weights on the `s` axis.
pub(crate) fn s_weights(s: f64) -> ([usize; 4], [f64; 4]) {
    let mut values = [0.0_f64; 4];
    let start = basis(
        &S_KNOTS,
        3,
        s.clamp(S_KNOTS[3], S_KNOTS[S_COEFS]),
        &mut values,
    );
    ([start, start + 1, start + 2, start + 3], values)
}

/// Cubic B-spline weights and derivative on the tint axis.
pub(crate) fn tint_weights(j_prime: f64) -> ([usize; 4], [f64; 4], [f64; 4]) {
    let x = j_prime.clamp(TINT_KNOTS[3], TINT_KNOTS[TINT_COEFS]);
    let (start, values, derivative) = cubic_basis_with_derivative(&TINT_KNOTS, x);
    ([start, start + 1, start + 2, start + 3], values, derivative)
}

/// Trilinear corner indices and weights for a point in `[0, 1]^3`.
pub(crate) fn trilinear(u: f64, v: f64, w: f64) -> ([usize; 8], [f64; 8]) {
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
    (indices, weights)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cubic_basis_partitions_unity() {
        for step in 0..=120 {
            let x = -12.0 + 18.0 * step as f64 / 120.0;
            let mut values = [0.0_f64; 4];
            basis(&TONE_KNOTS, 3, x, &mut values);
            let sum: f64 = values.iter().sum();
            assert!((sum - 1.0).abs() < 1e-12, "x={x} sum={sum}");
        }
    }

    #[test]
    fn ispline_is_monotone_and_finite() {
        let basis = ToneBasis::new();
        let mut previous = [0.0_f64; TONE_COEFS];
        for step in 0..=360 {
            let s = -12.0 + 18.0 * step as f64 / 360.0;
            let values = basis.values(s);
            for (k, value) in values.iter().enumerate() {
                assert!(value.is_finite());
                assert!(*value >= previous[k] - 1e-14, "k={k} s={s}");
            }
            previous = values;
        }
    }

    #[test]
    fn ispline_derivative_matches_finite_difference() {
        let basis = ToneBasis::new();
        for step in 1..60 {
            let s = -12.0 + 18.0 * step as f64 / 60.0;
            let analytical = basis.derivative(s);
            let h = 1e-6;
            let plus = basis.values(s + h);
            let minus = basis.values(s - h);
            for k in 0..TONE_COEFS {
                let numerical = (plus[k] - minus[k]) / (2.0 * h);
                assert!(
                    (analytical[k] - numerical).abs() < 1e-6,
                    "k={k} s={s} analytical={} numerical={numerical}",
                    analytical[k]
                );
            }
        }
    }

    #[test]
    fn tint_derivative_matches_finite_difference() {
        for step in 1..40 {
            let j = 100.0 * step as f64 / 40.0;
            let (indices, _, derivative) = tint_weights(j);
            let h = 1e-6;
            let (plus_indices, plus, _) = tint_weights(j + h);
            let (minus_indices, minus, _) = tint_weights(j - h);
            for (m, index) in indices.iter().enumerate() {
                let value = |indices: &[usize; 4], values: &[f64; 4]| {
                    indices
                        .iter()
                        .position(|candidate| candidate == index)
                        .map_or(0.0, |position| values[position])
                };
                let numerical =
                    (value(&plus_indices, &plus) - value(&minus_indices, &minus)) / (2.0 * h);
                assert!(
                    (derivative[m] - numerical).abs() < 1e-5,
                    "index={index} j={j} analytical={} numerical={numerical}",
                    derivative[m]
                );
            }
        }
    }

    #[test]
    fn hue_weights_are_periodic_and_partition_unity() {
        for step in 0..240 {
            let h = step as f64 * 1.5;
            let (_, weights) = hue_weights(h);
            let sum: f64 = weights.iter().sum();
            assert!((sum - 1.0).abs() < 1e-12);
            let (indices_a, weights_a) = hue_weights(h);
            let (indices_b, weights_b) = hue_weights(h + 360.0);
            assert_eq!(indices_a, indices_b);
            for (a, b) in weights_a.iter().zip(weights_b.iter()) {
                assert!((a - b).abs() < 1e-12);
            }
        }
    }

    #[test]
    fn neutral_chroma_uses_only_the_plane() {
        let (_, weights) = c_weights(0.0);
        assert_eq!(weights, [1.0, 0.0, 0.0, 0.0]);
    }

    #[test]
    fn outer_weight_derivative_matches_finite_difference() {
        for step in 1..=190 {
            let c = 2.0 * step as f64 / 200.0;
            let (_, derivative) = outer_weight(c);
            let h = 1e-6;
            let (plus, _) = outer_weight(c + h);
            let (minus, _) = outer_weight(c - h);
            let numerical = (plus - minus) / (2.0 * h);
            assert!(
                (derivative - numerical).abs() < 1e-5,
                "c={c} analytical={derivative} numerical={numerical}"
            );
        }
    }

    #[test]
    fn core_weights_are_bounded_on_the_join_disk() {
        for a in [-2.0, -1.0, 0.0, 1.0, 2.0] {
            for b in [-2.0, -1.0, 0.0, 1.0, 2.0] {
                for weight in core_weights(a, b) {
                    assert!(weight.is_finite());
                    assert!(weight.abs() <= 1.0, "a={a} b={b} weight={weight}");
                }
            }
        }
    }
}
