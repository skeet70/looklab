// SPDX-License-Identifier: AGPL-3.0-or-later

use looklab_color::cam16::Chroma;
use looklab_color::ucs::{Ucs, UcsSpace, ucs_to_jmh};
use looklab_color::{Mat3, RgbSpace, Xyz};
use serde::{Deserialize, Serialize};

use crate::solve;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClipFunction {
    TanhSoftV1,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct ClipSettings {
    pub function: ClipFunction,
    pub k: f64,
}

impl ClipSettings {
    pub fn default_v1() -> Self {
        Self {
            function: ClipFunction::TanhSoftV1,
            k: 0.03,
        }
    }

    pub fn validate(&self) -> bool {
        (0.005..=0.1).contains(&self.k)
    }
}

/// `c(x)` of docs/02 §3.4: identity on `[k, 1-k]`, `tanh` shoulders outside.
pub fn soft_clip(x: f64, k: f64) -> f64 {
    if x > 1.0 - k {
        (1.0 - k) + k * libm::tanh((x - (1.0 - k)) / k)
    } else if x < k {
        k - k * libm::tanh((k - x) / k)
    } else {
        x
    }
}

/// `J'a'b'` through linear output RGB, the per-channel soft clip, and back.
pub fn output_encode_clip(ucs: Ucs, output_space: RgbSpace, k: f64) -> Ucs {
    if let Some(encoded) = encode_once(ucs, output_space, k) {
        return encoded;
    }
    // Contract to the neutral at the same lightness, bounded to CAM16's inverse domain.
    let neutral = if ucs.j.is_finite() {
        Ucs::new(ucs.j.clamp(0.0, solve::neutral_j_prime_max()), 0.0, 0.0)
    } else {
        Ucs::new(0.0, 0.0, 0.0)
    };
    encode_once(neutral, output_space, k)
        .or_else(|| encode_once(Ucs::new(0.0, 0.0, 0.0), output_space, k))
        .unwrap_or(Ucs::new(0.0, 0.0, 0.0))
}

/// `xyz` of the pre-clip prediction and `dXYZ/d(J'a'b')`; chromatic columns vanish
/// at a neutral prediction.
pub fn preclip_xyz_jacobian(pre: Ucs) -> (Xyz, [[f64; 3]; 3]) {
    let model = solve::reference_model();
    let jmh = ucs_to_jmh(pre, UcsSpace::Ucs);
    let (xyz, d_xyz) = model.inverse_xyz_and_jacobian(jmh);
    let c1 = 0.007;
    let denominator = 1.0 + 100.0 * c1 - c1 * pre.j;
    let d_j = (1.0 + 100.0 * c1) / (denominator * denominator);
    let c2 = UcsSpace::Ucs.coefficients().c2;
    let m_prime = (pre.a * pre.a + pre.b * pre.b).sqrt();
    let (d_m_a, d_m_b, d_h_a, d_h_b) = if m_prime > 0.0 {
        let d_m = libm::exp(c2 * m_prime);
        let degrees = 180.0 / core::f64::consts::PI;
        (
            d_m * pre.a / m_prime,
            d_m * pre.b / m_prime,
            -pre.b / (m_prime * m_prime) * degrees,
            pre.a / (m_prime * m_prime) * degrees,
        )
    } else {
        (0.0, 0.0, 0.0, 0.0)
    };
    let chain = [[d_j, 0.0, 0.0], [0.0, d_m_a, d_m_b], [0.0, d_h_a, d_h_b]];
    let mut jacobian = [[0.0_f64; 3]; 3];
    for (row, values) in jacobian.iter_mut().enumerate() {
        for (axis, value) in values.iter_mut().enumerate() {
            for (k, row_values) in chain.iter().enumerate() {
                *value += d_xyz[row][k] * row_values[axis];
            }
        }
    }
    (xyz, jacobian)
}

/// `∂²XYZ/∂(J'a'b')²` of the pre-clip inverse, as `[c][d]` over the two pre-clip
/// channels; zero where the forward map is singular.
pub fn preclip_xyz_second(pre: Ucs) -> [[[f64; 3]; 3]; 3] {
    let (xyz, _) = preclip_xyz_jacobian(pre);
    let model = crate::deriv::reference_cam16();
    let (_, forward) = model.ucs_xyz_jacobian(xyz);
    let second = model.ucs_xyz_second(xyz);
    inverse_second(forward, &second)
}

/// `D²(f⁻¹)[e_c, e_d] = −Df⁻¹ · D²f[Df⁻¹e_c, Df⁻¹e_d]`.
fn inverse_second(forward: [[f64; 3]; 3], second: &[[f64; 3]; 6]) -> [[[f64; 3]; 3]; 3] {
    let mut out = [[[0.0_f64; 3]; 3]; 3];
    let Some(inverse) = Mat3(forward).inverse() else {
        return out;
    };
    if !inverse.0.iter().flatten().all(|value| value.is_finite()) {
        return out;
    }
    let packed = [(0usize, 0usize), (1, 1), (2, 2), (0, 1), (0, 2), (1, 2)];
    for (c, rows) in out.iter_mut().enumerate() {
        for (d, entry) in rows.iter_mut().enumerate() {
            let left = [inverse.0[0][c], inverse.0[1][c], inverse.0[2][c]];
            let right = [inverse.0[0][d], inverse.0[1][d], inverse.0[2][d]];
            let mut direction = [0.0_f64; 3];
            for (slot, (a, b)) in packed.into_iter().enumerate() {
                let weight = if a == b {
                    left[a] * right[b]
                } else {
                    left[a] * right[b] + left[b] * right[a]
                };
                for (channel, value) in direction.iter_mut().enumerate() {
                    *value += weight * second[slot][channel];
                }
            }
            *entry = inverse.mul_vec(direction).map(|value| -value);
        }
    }
    out
}

/// `∂²XYZ/∂(J', n_a, n_b)²` of the internal pre-clip inverse, as `[c][d]` over
/// those three channels.
pub fn preclip_internal_xyz_second(chroma: [f64; 2], j_prime: f64) -> [[[f64; 3]; 3]; 3] {
    let xyz = crate::look::internal_chroma_to_xyz(chroma, j_prime);
    let model = crate::deriv::reference_cam16();
    let scale = crate::look::neutral_scale();
    let mut forward = [[0.0_f64; 3]; 3];
    forward[0] = model.j_prime_xyz_gradient(xyz);
    for axis in 0..3 {
        let mut direction = [0.0_f64; 3];
        direction[axis] = 1.0;
        let (_, first, _) = model.neutral_second(xyz, direction, scale);
        forward[1][axis] = first[0];
        forward[2][axis] = first[1];
    }
    let ucs = model.ucs_xyz_second(xyz);
    let neutral = model.neutral_xyz_second(xyz, scale);
    let mut second = [[0.0_f64; 3]; 6];
    for (slot, value) in second.iter_mut().enumerate() {
        *value = [ucs[slot][0], neutral[slot][0], neutral[slot][1]];
    }
    inverse_second(forward, &second)
}

/// `xyz` of the internal chroma and `dXYZ/d(n_a, n_b, J')`.
pub fn preclip_internal_xyz_jacobian(chroma: [f64; 2], j_prime: f64) -> (Xyz, [[f64; 3]; 3]) {
    let xyz = crate::look::internal_chroma_to_xyz(chroma, j_prime);
    let (opponent, d_opponent) = solve::reference_model().cone_opponent_jacobian(xyz);
    let scale = crate::look::neutral_scale();
    let denominator = opponent.denominator;
    let mut forward = [[0.0_f64; 3]; 3];
    forward[0] = crate::look::j_prime_xyz_gradient(xyz);
    for axis in 0..3 {
        forward[1][axis] = scale
            * (d_opponent[0][axis] * denominator - opponent.a * d_opponent[2][axis])
            / (denominator * denominator);
        forward[2][axis] = scale
            * (d_opponent[1][axis] * denominator - opponent.b * d_opponent[2][axis])
            / (denominator * denominator);
    }
    let inverse = Mat3(forward).inverse();
    if let Some(inverse) = inverse
        && inverse.0.iter().flatten().all(|value| value.is_finite())
    {
        return (xyz, inverse.0);
    }
    // At a zero cone response the forward slope is infinite and the inverse is not
    // recoverable by inversion; the inverse map itself is finite there.
    let mut jacobian = [[0.0_f64; 3]; 3];
    let step = 1e-7;
    for axis in 0..3 {
        let mut plus = [chroma[0], chroma[1], j_prime];
        let mut minus = plus;
        plus[axis] += step;
        minus[axis] -= step;
        let a = crate::look::internal_chroma_to_xyz([plus[0], plus[1]], plus[2]);
        let b = crate::look::internal_chroma_to_xyz([minus[0], minus[1]], minus[2]);
        jacobian[0][axis] = (a.x - b.x) / (2.0 * step);
        jacobian[1][axis] = (a.y - b.y) / (2.0 * step);
        jacobian[2][axis] = (a.z - b.z) / (2.0 * step);
    }
    (xyz, jacobian)
}

/// [`output_encode_clip`] for a point CAM16's inverse model is defined on.
fn encode_once(ucs: Ucs, output_space: RgbSpace, k: f64) -> Option<Ucs> {
    let model = solve::reference_model();
    let jmh = ucs_to_jmh(ucs, UcsSpace::Ucs);
    if !(jmh.j.is_finite() && jmh.m.is_finite() && jmh.h.is_finite()) || jmh.j < 0.0 {
        return None;
    }
    let xyz = model.cam16_to_xyz(jmh.j, Chroma::M(jmh.m), jmh.h);
    let rgb = output_space.xyz_to_rgb_linear(xyz);
    let clipped = looklab_color::Rgb::new(
        soft_clip(rgb.r, k),
        soft_clip(rgb.g, k),
        soft_clip(rgb.b, k),
    );
    if !(clipped.r.is_finite() && clipped.g.is_finite() && clipped.b.is_finite()) {
        return None;
    }
    let encoded: Xyz = output_space.rgb_linear_to_xyz(clipped);
    let out = solve::ucs_of_xyz(encoded);
    out.to_array()
        .iter()
        .all(|value| value.is_finite())
        .then_some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn soft_clip_is_c1_at_the_knots() {
        for k in [0.005, 0.03, 0.1] {
            for knot in [k, 1.0 - k] {
                let h = 1e-7;
                let left = (soft_clip(knot, k) - soft_clip(knot - h, k)) / h;
                let right = (soft_clip(knot + h, k) - soft_clip(knot, k)) / h;
                assert!((left - 1.0).abs() < 1e-5, "k={k} left={left}");
                assert!((right - 1.0).abs() < 1e-5, "k={k} right={right}");
            }
        }
    }

    #[test]
    fn negative_lightness_contracts_to_the_neutral() {
        let neutral = output_encode_clip(Ucs::new(0.0, 0.0, 0.0), RgbSpace::Srgb, 0.03);
        let negative = output_encode_clip(Ucs::new(-0.16, 12.0, -8.0), RgbSpace::Srgb, 0.03);
        assert!(
            (negative.j - neutral.j).abs() < 1e-9,
            "{negative:?} vs {neutral:?}"
        );
        assert!((negative.a - neutral.a).abs() < 1e-9);
        assert!((negative.b - neutral.b).abs() < 1e-9);
    }

    #[test]
    fn preclip_second_matches_differences_of_the_jacobian() {
        for pre in [
            Ucs::new(55.0, 12.0, -7.0),
            Ucs::new(20.0, -4.0, 9.0),
            Ucs::new(85.0, 25.0, 18.0),
        ] {
            let second = preclip_xyz_second(pre);
            let h = 1e-6;
            let mut worst = 0.0_f64;
            for c in 0..3 {
                let mut plus = pre.to_array();
                let mut minus = pre.to_array();
                plus[c] += h;
                minus[c] -= h;
                let (_, jacobian_plus) = preclip_xyz_jacobian(Ucs::from_array(plus));
                let (_, jacobian_minus) = preclip_xyz_jacobian(Ucs::from_array(minus));
                for d in 0..3 {
                    for row in 0..3 {
                        let numerical =
                            (jacobian_plus[row][d] - jacobian_minus[row][d]) / (2.0 * h);
                        let analytic = second[c][d][row];
                        let scale = numerical.abs().max(1e-3);
                        let gap = (analytic - numerical).abs() / scale;
                        if gap > worst {
                            worst = gap;
                            println!(
                                "pre {pre:?} c {c} d {d} row {row}: analytic {analytic:.9e} numerical {numerical:.9e}"
                            );
                        }
                    }
                }
            }
            println!("preclip_xyz_second worst relative gap {worst:.3e} for {pre:?}");
            assert!(worst < 1e-5, "worst relative gap {worst:.3e}");
        }
    }

    #[test]
    fn soft_clip_limits_and_monotonicity() {
        let k = 0.03;
        assert!(soft_clip(1e6, k) <= 1.0);
        assert!(soft_clip(1e6, k) >= 1.0 - 1e-9);
        assert!(soft_clip(-1e6, k) >= 0.0);
        assert!(soft_clip(-1e6, k) <= 1e-9);
        let mut previous = f64::NEG_INFINITY;
        for step in 0..=2000 {
            let x = -2.0 + 4.0 * step as f64 / 2000.0;
            let y = soft_clip(x, k);
            assert!(y >= previous);
            previous = y;
        }
    }

    #[test]
    fn identity_on_the_interior() {
        let k = 0.03;
        for x in [0.03, 0.5, 0.97] {
            assert_eq!(soft_clip(x, k), x);
        }
    }
}
