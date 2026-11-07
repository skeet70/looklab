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

/// Tint axis join: the last tint knot, where the continuation starts.
pub const TINT_JOIN: f64 = TINT_KNOTS[TINT_COEFS];

/// Number of residual grid nodes per axis.
pub const RESIDUAL_GRID: usize = 17;

/// Open uniform cubic knots over `[0, 1]` for the 17 residual control points.
pub const RESIDUAL_KNOTS: [f64; RESIDUAL_GRID + 4] = {
    let mut knots = [0.0_f64; RESIDUAL_GRID + 4];
    let interior = RESIDUAL_GRID - 3;
    let mut index = 1;
    while index < interior {
        knots[3 + index] = index as f64 / interior as f64;
        index += 1;
    }
    let mut tail = RESIDUAL_GRID;
    while tail < RESIDUAL_GRID + 4 {
        knots[tail] = 1.0;
        tail += 1;
    }
    knots
};

/// Three channels per node, channel-minor.
pub const RESIDUAL_COEFS: usize = RESIDUAL_GRID * RESIDUAL_GRID * RESIDUAL_GRID * 3;

/// Span index `i` with `knots[i] <= x < knots[i + 1]`, clamped to the domain.
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

/// `C'' = Σ_k e_k B_{k,1}`, `e_k = 6(d_k - d_{k-1})/(t_{k+2} - t_k)`,
/// `d_k = 3(c_k - c_{k-1})/(t_{k+3} - t_k)` (Piegl & Tiller 1997, eq. 3.7 twice).
fn cubic_basis_derivatives(knots: &[f64], x: f64) -> (usize, [f64; 4], [f64; 4], [f64; 4]) {
    let (start, values, derivative) = cubic_basis_with_derivative(knots, x);
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
        let first_knot = TONE_KNOTS[3];
        let last_knot = TONE_KNOTS[TONE_COEFS];
        if x > last_knot {
            let mut out = self.values(last_knot);
            let mid = 0.5 * (last_knot + x);
            let half = 0.5 * (x - last_knot);
            let delta = half / 3.0_f64.sqrt();
            for node in [mid - delta, mid + delta] {
                let mut weights = [0.0_f64; 4];
                let first = basis(&TONE_KNOTS, 3, node, &mut weights);
                for (m, weight) in weights.iter().enumerate() {
                    out[first + m] += half * weight;
                }
            }
            return out;
        }
        if x < first_knot {
            let mid = 0.5 * (first_knot + x);
            let half = 0.5 * (first_knot - x);
            let delta = half / 3.0_f64.sqrt();
            let mut out = [0.0_f64; TONE_COEFS];
            for node in [mid - delta, mid + delta] {
                let mut weights = [0.0_f64; 4];
                let first = basis(&TONE_KNOTS, 3, node, &mut weights);
                for (m, weight) in weights.iter().enumerate() {
                    out[first + m] -= half * weight;
                }
            }
            return out;
        }
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

/// `w = 1 - (1 - t)³` with `t = C² / C0²`, `w = 1` for `C ≥ C0`; returns `(w, dw/dC)`.
pub fn outer_weight(c: f64) -> (f64, f64) {
    if c.is_nan() || c <= 0.0 {
        return (0.0, 0.0);
    }
    let t = c * c / (NEUTRAL_JOIN_C * NEUTRAL_JOIN_C);
    if t >= 1.0 {
        return (1.0, 0.0);
    }
    let remainder = 1.0 - t;
    let w = 1.0 - remainder * remainder * remainder;
    let slope = 3.0 * remainder * remainder * 2.0 * c / (NEUTRAL_JOIN_C * NEUTRAL_JOIN_C);
    (w, slope)
}

/// Cubic B-spline weights on the `s` axis; the knot span is held at the ends,
/// so a coordinate outside the domain extrapolates the end cubic piece.
pub(crate) fn s_weights(s: f64) -> ([usize; 4], [f64; 4]) {
    let mut values = [0.0_f64; 4];
    let start = basis(&S_KNOTS, 3, s, &mut values);
    ([start, start + 1, start + 2, start + 3], values)
}

/// Width in stops of the shadow exposure transition, outside `-12`.
pub const S_SHADOW_TRANSITION: f64 = 2.0;

/// Width in stops of the highlight exposure transition, outside `6`.
pub const S_HIGHLIGHT_TRANSITION: f64 = 1.0;

/// Bounded spline coordinate for physical `s = log2(Y/0.18)`: identity on the
/// tone range `[-12, 6]`, C² quintic Hermite transitions outside it.
pub fn exposure_lookup(s: f64) -> f64 {
    let low = S_KNOTS[3];
    let high = S_KNOTS[S_COEFS];
    if s >= low && s <= high {
        return s;
    }
    if s < low {
        let start = low - S_SHADOW_TRANSITION;
        if s <= start {
            return start;
        }
        let y = (s - start) / S_SHADOW_TRANSITION;
        let y2 = y * y;
        let y3 = y2 * y;
        return start + S_SHADOW_TRANSITION * (3.0 * y3 * y2 - 8.0 * y2 * y2 + 6.0 * y3);
    }
    let end = high + S_HIGHLIGHT_TRANSITION;
    if s >= end {
        return end;
    }
    let y = (s - high) / S_HIGHLIGHT_TRANSITION;
    let y2 = y * y;
    let y3 = y2 * y;
    high + S_HIGHLIGHT_TRANSITION * (3.0 * y3 * y2 - 7.0 * y2 * y2 + 4.0 * y3 + y)
}

pub fn exposure_lookup_derivative(s: f64) -> f64 {
    let low = S_KNOTS[3];
    let high = S_KNOTS[S_COEFS];
    if s >= low && s <= high {
        return 1.0;
    }
    if s < low {
        let start = low - S_SHADOW_TRANSITION;
        if s <= start {
            return 0.0;
        }
        let y = (s - start) / S_SHADOW_TRANSITION;
        let y2 = y * y;
        return 15.0 * y2 * y2 - 32.0 * y2 * y + 18.0 * y2;
    }
    let end = high + S_HIGHLIGHT_TRANSITION;
    if s >= end {
        return 0.0;
    }
    let y = (s - high) / S_HIGHLIGHT_TRANSITION;
    let y2 = y * y;
    15.0 * y2 * y2 - 28.0 * y2 * y + 12.0 * y2 + 1.0
}

/// Largest tint argument the reference inverse can represent (the neutral `J'`
/// limit of the viewing conditions).
pub fn tint_continuation_end() -> f64 {
    crate::solve::neutral_j_prime_max()
}

/// Cubic tint weights and `d/dJ'`, continued above [`TINT_JOIN`] by matching value, slope and second derivative and reaching zero slope and curvature at [`tint_continuation_end`].
pub fn tint_weights(j_prime: f64) -> ([usize; 4], [f64; 4], [f64; 4]) {
    let (start, values, derivative, second) =
        cubic_basis_derivatives(&TINT_KNOTS, j_prime.clamp(TINT_KNOTS[3], TINT_JOIN));
    let indices = [start, start + 1, start + 2, start + 3];
    if j_prime <= TINT_JOIN {
        return (indices, values, derivative);
    }
    let length = (tint_continuation_end() - TINT_JOIN).max(f64::MIN_POSITIVE);
    let x = ((j_prime - TINT_JOIN) / length).clamp(0.0, 1.0);
    let x2 = x * x;
    let x3 = x2 * x;
    let x4 = x3 * x;
    let mut weights = [0.0_f64; 4];
    let mut slope = [0.0_f64; 4];
    for m in 0..4 {
        let s0 = derivative[m];
        let c0 = second[m];
        let a = c0 * length + 2.0 * s0;
        let b = -2.0 * c0 * length - 3.0 * s0;
        let c = c0 * length;
        weights[m] = values[m] + length * (0.25 * a * x4 + b * x3 / 3.0 + 0.5 * c * x2 + s0 * x);
        slope[m] = a * x3 + b * x2 + c * x + s0;
    }
    (indices, weights, slope)
}

/// Cubic tint weights with first and second derivatives w.r.t. `J'`.
pub fn tint_weights_second(j_prime: f64) -> ([usize; 4], [f64; 4], [f64; 4], [f64; 4]) {
    let (start, values, derivative, second) =
        cubic_basis_derivatives(&TINT_KNOTS, j_prime.clamp(TINT_KNOTS[3], TINT_JOIN));
    let indices = [start, start + 1, start + 2, start + 3];
    if j_prime <= TINT_JOIN {
        return (indices, values, derivative, second);
    }
    let length = (tint_continuation_end() - TINT_JOIN).max(f64::MIN_POSITIVE);
    let x = ((j_prime - TINT_JOIN) / length).clamp(0.0, 1.0);
    let x2 = x * x;
    let x3 = x2 * x;
    let x4 = x3 * x;
    let mut weights = [0.0_f64; 4];
    let mut slope = [0.0_f64; 4];
    let mut curvature = [0.0_f64; 4];
    for m in 0..4 {
        let s0 = derivative[m];
        let c0 = second[m];
        let a = c0 * length + 2.0 * s0;
        let b = -2.0 * c0 * length - 3.0 * s0;
        let c = c0 * length;
        weights[m] = values[m] + length * (0.25 * a * x4 + b * x3 / 3.0 + 0.5 * c * x2 + s0 * x);
        slope[m] = a * x3 + b * x2 + c * x + s0;
        curvature[m] = (3.0 * a * x2 + 2.0 * b * x + c) / length;
    }
    (indices, weights, slope, curvature)
}

/// Cubic residual control-point indices and weights at `(s, a, b)`.
pub fn residual_weights(s: f64, a: f64, b: f64) -> ([usize; 64], [f64; 64]) {
    let (si, sw) = residual_axis(residual_s_coord(s));
    let (aj, aw) = residual_axis(residual_coord(a, crate::look::RESIDUAL_DOMAIN[1]));
    let (bk, bw) = residual_axis(residual_coord(b, crate::look::RESIDUAL_DOMAIN[2]));
    let mut indices = [0_usize; 64];
    let mut weights = [0.0_f64; 64];
    let mut position = 0;
    for i in 0..4 {
        for j in 0..4 {
            for k in 0..4 {
                indices[position] = (si[i] * RESIDUAL_GRID + aj[j]) * RESIDUAL_GRID + bk[k];
                weights[position] = sw[i] * aw[j] * bw[k];
                position += 1;
            }
        }
    }
    (indices, weights)
}

/// Residual control-point indices, weights and derivatives w.r.t. `(s, a, b)`.
#[allow(clippy::type_complexity)]
pub fn residual_values(s: f64, a: f64, b: f64) -> ([usize; 64], [f64; 64], [[f64; 64]; 3]) {
    let (a_coord, a_scale) = residual_coord_and_scale(a, crate::look::RESIDUAL_DOMAIN[1]);
    let (b_coord, b_scale) = residual_coord_and_scale(b, crate::look::RESIDUAL_DOMAIN[2]);
    let (si, sw, dsw) = residual_axis_with_derivative(residual_s_coord(s));
    let (aj, aw, daw) = residual_axis_with_derivative(a_coord);
    let (bk, bw, dbw) = residual_axis_with_derivative(b_coord);
    let span = [
        crate::look::RESIDUAL_DOMAIN[0][1] - crate::look::RESIDUAL_DOMAIN[0][0],
        crate::look::RESIDUAL_DOMAIN[1][1] - crate::look::RESIDUAL_DOMAIN[1][0],
        crate::look::RESIDUAL_DOMAIN[2][1] - crate::look::RESIDUAL_DOMAIN[2][0],
    ];
    let mut indices = [0_usize; 64];
    let mut weights = [0.0_f64; 64];
    let mut derivatives = [[0.0_f64; 64]; 3];
    let mut position = 0;
    for i in 0..4 {
        for j in 0..4 {
            for k in 0..4 {
                indices[position] = (si[i] * RESIDUAL_GRID + aj[j]) * RESIDUAL_GRID + bk[k];
                weights[position] = sw[i] * aw[j] * bw[k];
                derivatives[0][position] = dsw[i] * aw[j] * bw[k] / span[0];
                derivatives[1][position] = sw[i] * daw[j] * bw[k] * a_scale / span[1];
                derivatives[2][position] = sw[i] * aw[j] * dbw[k] * b_scale / span[2];
                position += 1;
            }
        }
    }
    (indices, weights, derivatives)
}

fn residual_s_coord(s: f64) -> f64 {
    (s - crate::look::RESIDUAL_DOMAIN[0][0])
        / (crate::look::RESIDUAL_DOMAIN[0][1] - crate::look::RESIDUAL_DOMAIN[0][0])
}

fn residual_coord(value: f64, bounds: [f64; 2]) -> f64 {
    ((value - bounds[0]) / (bounds[1] - bounds[0])).clamp(0.0, 1.0)
}

fn residual_coord_and_scale(value: f64, bounds: [f64; 2]) -> (f64, f64) {
    let raw = (value - bounds[0]) / (bounds[1] - bounds[0]);
    if raw > 0.0 && raw < 1.0 {
        (raw, 1.0)
    } else {
        (raw.clamp(0.0, 1.0), 0.0)
    }
}

fn residual_axis(x: f64) -> ([usize; 4], [f64; 4]) {
    let mut values = [0.0_f64; 4];
    let start = basis(&RESIDUAL_KNOTS, 3, x, &mut values);
    ([start, start + 1, start + 2, start + 3], values)
}

fn residual_axis_with_derivative(x: f64) -> ([usize; 4], [f64; 4], [f64; 4]) {
    let (start, values, derivative) = cubic_basis_with_derivative(&RESIDUAL_KNOTS, x);
    ([start, start + 1, start + 2, start + 3], values, derivative)
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
