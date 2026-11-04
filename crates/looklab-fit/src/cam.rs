// SPDX-License-Identifier: AGPL-3.0-or-later
//! `d(J'a'b')/dXYZ` at the reference conditions (CAM16, Li et al. 2017; Luo et al. 2006), dual numbers.

use core::f64::consts::PI;
use looklab_color::Xyz;
use looklab_color::cam16::ViewingConditions;
use looklab_color::ucs::Ucs;
use std::sync::LazyLock;

const M16: [[f64; 3]; 3] = [
    [0.401_288, 0.650_173, -0.051_461],
    [-0.250_268, 1.204_414, 0.045_854],
    [-0.002_079, 0.048_952, 0.953_127],
];

const C2: f64 = 0.0228;

static CONSTANTS: LazyLock<Constants> = LazyLock::new(Constants::new);

/// `CAM16` viewing-condition constants at the reference conditions.
#[derive(Clone, Copy, Debug)]
struct Constants {
    f_l: f64,
    fl_root4: f64,
    j_exp: f64,
    ta: f64,
    p_scale: f64,
    a_w: f64,
    n_bb: f64,
    d_rgb: [f64; 3],
    white: Xyz,
    inv_white_y: f64,
    white_scale: [f64; 3],
}

impl Constants {
    fn new() -> Self {
        let vc = ViewingConditions::reference();
        let y_w = vc.white.y * 100.0;
        let rgb_w = mul(M16, [vc.white.x * 100.0, y_w, vc.white.z * 100.0]);
        let k = 1.0 / (5.0 * vc.l_a + 1.0);
        let k4 = k * k * k * k;
        let f_l = k4 * vc.l_a + 0.1 * (1.0 - k4) * (1.0 - k4) * libm::cbrt(5.0 * vc.l_a);
        let n = vc.y_b / y_w;
        let z = 1.48 + n.sqrt();
        let n_bb = 0.725 * libm::pow(1.0 / n, 0.2);
        let d_rgb = [y_w / rgb_w[0], y_w / rgb_w[1], y_w / rgb_w[2]];
        let a_w = (2.0 * post_adaptation_scalar(d_rgb[0] * rgb_w[0], f_l)
            + post_adaptation_scalar(d_rgb[1] * rgb_w[1], f_l)
            + 0.05 * post_adaptation_scalar(d_rgb[2] * rgb_w[2], f_l)
            - 0.305)
            * n_bb;
        let c = vc.surround.c();
        Self {
            f_l,
            fl_root4: libm::pow(f_l, 0.25),
            j_exp: c * z,
            ta: libm::pow(1.64 - libm::pow(0.29, n), 0.73),
            p_scale: (50000.0 / 13.0) * vc.surround.n_c() * n_bb,
            a_w,
            n_bb,
            d_rgb,
            white: vc.white,
            inv_white_y: 1.0 / vc.white.y,
            white_scale: [y_w; 3],
        }
    }
}

fn mul(m: [[f64; 3]; 3], v: [f64; 3]) -> [f64; 3] {
    [
        m[0][0] * v[0] + m[0][1] * v[1] + m[0][2] * v[2],
        m[1][0] * v[0] + m[1][1] * v[1] + m[1][2] * v[2],
        m[2][0] * v[0] + m[2][1] * v[1] + m[2][2] * v[2],
    ]
}

fn post_adaptation_scalar(response: f64, f_l: f64) -> f64 {
    let u = f_l * response.abs() / 100.0;
    let f = libm::pow(u, 0.42);
    400.0 * response.signum() * f / (27.13 + f) + 0.1
}

fn post_adaptation_derivative(response: f64, f_l: f64) -> f64 {
    if response == 0.0 {
        return 0.0;
    }
    let u = f_l * response.abs() / 100.0;
    let f = libm::pow(u, 0.42);
    let denom = 27.13 + f;
    400.0 * 27.13 / (denom * denom) * 0.42 * (f / u) * (f_l / 100.0)
}

/// Value and three partial derivatives.
#[derive(Clone, Copy, Debug)]
struct Jet {
    v: f64,
    d: [f64; 3],
}

impl Jet {
    fn constant(v: f64) -> Self {
        Self { v, d: [0.0; 3] }
    }

    fn variable(v: f64, axis: usize) -> Self {
        let mut d = [0.0; 3];
        d[axis] = 1.0;
        Self { v, d }
    }

    fn map(self, f: impl Fn(f64) -> (f64, f64)) -> Self {
        let (value, slope) = f(self.v);
        Self {
            v: value,
            d: [slope * self.d[0], slope * self.d[1], slope * self.d[2]],
        }
    }

    fn sqrt(self) -> Self {
        if self.v <= 0.0 {
            return Self::constant(self.v.max(0.0).sqrt());
        }
        self.map(|x| (x.sqrt(), 0.5 / x.sqrt()))
    }

    fn pow_signed(self, p: f64) -> Self {
        let magnitude = self.v.abs();
        let value = self.v.signum() * libm::pow(magnitude, p);
        let slope = if magnitude > 0.0 {
            p * libm::pow(magnitude, p - 1.0)
        } else {
            0.0
        };
        Self {
            v: value,
            d: [slope * self.d[0], slope * self.d[1], slope * self.d[2]],
        }
    }

    fn cos(self) -> Self {
        self.map(|x| (libm::cos(x), -libm::sin(x)))
    }

    fn sin(self) -> Self {
        self.map(|x| (libm::sin(x), libm::cos(x)))
    }

    fn log1p(self) -> Self {
        let denom = 1.0 + self.v;
        let slope = if denom == 0.0 { 0.0 } else { 1.0 / denom };
        self.map(|x| (libm::log1p(x), slope))
    }
}

impl core::ops::Add for Jet {
    type Output = Self;
    fn add(self, other: Self) -> Self {
        Self {
            v: self.v + other.v,
            d: [
                self.d[0] + other.d[0],
                self.d[1] + other.d[1],
                self.d[2] + other.d[2],
            ],
        }
    }
}

impl core::ops::Sub for Jet {
    type Output = Self;
    fn sub(self, other: Self) -> Self {
        Self {
            v: self.v - other.v,
            d: [
                self.d[0] - other.d[0],
                self.d[1] - other.d[1],
                self.d[2] - other.d[2],
            ],
        }
    }
}

impl core::ops::Mul for Jet {
    type Output = Self;
    fn mul(self, other: Self) -> Self {
        Self {
            v: self.v * other.v,
            d: [
                self.d[0] * other.v + self.v * other.d[0],
                self.d[1] * other.v + self.v * other.d[1],
                self.d[2] * other.v + self.v * other.d[2],
            ],
        }
    }
}

impl core::ops::Div for Jet {
    type Output = Self;
    fn div(self, other: Self) -> Self {
        let inv = 1.0 / other.v;
        Self {
            v: self.v * inv,
            d: [
                (self.d[0] - self.v * inv * other.d[0]) * inv,
                (self.d[1] - self.v * inv * other.d[1]) * inv,
                (self.d[2] - self.v * inv * other.d[2]) * inv,
            ],
        }
    }
}

fn post_adaptation(response: Jet, f_l: f64) -> Jet {
    let value = post_adaptation_scalar(response.v, f_l);
    let slope = post_adaptation_derivative(response.v, f_l);
    Jet {
        v: value,
        d: [
            slope * response.d[0],
            slope * response.d[1],
            slope * response.d[2],
        ],
    }
}

fn atan2_degrees(b: Jet, a: Jet) -> Jet {
    let denominator = a.v * a.v + b.v * b.v;
    let scale = 180.0 / PI;
    let value = libm::atan2(b.v, a.v) * scale;
    let (da, db) = if denominator == 0.0 {
        (0.0, 0.0)
    } else {
        (-b.v / denominator * scale, a.v / denominator * scale)
    };
    let mut d = [0.0; 3];
    for (axis, slot) in d.iter_mut().enumerate() {
        *slot = da * a.d[axis] + db * b.d[axis];
    }
    Jet { v: value, d }
}

/// CAM16-UCS of `xyz` at the reference conditions.
#[cfg(test)]
pub(crate) fn ucs(xyz: Xyz) -> Ucs {
    ucs_jacobian(xyz).0
}

/// CAM16-UCS of `xyz` and `d(J'a'b')/dXYZ`, rows are output channels.
pub(crate) fn ucs_jacobian(xyz: Xyz) -> (Ucs, [[f64; 3]; 3]) {
    let constants = &*CONSTANTS;
    let x = Jet::variable(xyz.x, 0);
    let y = Jet::variable(xyz.y, 1);
    let z = Jet::variable(xyz.z, 2);

    let t = y * Jet::constant(constants.inv_white_y);
    let residual = [
        x - t * Jet::constant(constants.white.x),
        y - t * Jet::constant(constants.white.y),
        z - t * Jet::constant(constants.white.z),
    ];
    let mut rgb_c = [Jet::constant(0.0); 3];
    for i in 0..3 {
        let response = Jet::constant(M16[i][0]) * residual[0]
            + Jet::constant(M16[i][1]) * residual[1]
            + Jet::constant(M16[i][2]) * residual[2];
        rgb_c[i] = t * Jet::constant(constants.white_scale[i])
            + Jet::constant(constants.d_rgb[i]) * response * Jet::constant(100.0);
    }

    let rgb_a = [
        post_adaptation(rgb_c[0], constants.f_l),
        post_adaptation(rgb_c[1], constants.f_l),
        post_adaptation(rgb_c[2], constants.f_l),
    ];
    let a =
        (Jet::constant(11.0) * (rgb_a[0] - rgb_a[1]) + (rgb_a[2] - rgb_a[1])) / Jet::constant(11.0);
    let b = ((rgb_a[0] - rgb_a[2]) + (rgb_a[1] - rgb_a[2])) / Jet::constant(9.0);
    let h = atan2_degrees(b, a);
    let e_t = (Jet::constant(2.0) + h * Jet::constant(PI / 180.0)).cos() * Jet::constant(0.25)
        + Jet::constant(0.95);
    let achromatic = (Jet::constant(2.0) * rgb_a[0] + rgb_a[1] + Jet::constant(0.05) * rgb_a[2]
        - Jet::constant(0.305))
        * Jet::constant(constants.n_bb);
    let j = (achromatic / Jet::constant(constants.a_w)).pow_signed(constants.j_exp)
        * Jet::constant(100.0);
    let temporary = Jet::constant(constants.p_scale) * e_t * (a * a + b * b).sqrt()
        / (rgb_a[0] + rgb_a[1] + Jet::constant(1.05) * rgb_a[2]);
    let chroma = temporary.pow_signed(0.9)
        * (j * Jet::constant(0.01)).pow_signed(0.5)
        * Jet::constant(constants.ta);
    let colourfulness = chroma * Jet::constant(constants.fl_root4);
    let j_prime =
        j * Jet::constant(1.0 + 100.0 * 0.007) / (Jet::constant(1.0) + j * Jet::constant(0.007));
    let m_prime = (colourfulness * Jet::constant(C2)).log1p() * Jet::constant(1.0 / C2);
    let radians = h * Jet::constant(PI / 180.0);
    let a_prime = m_prime * radians.cos();
    let b_prime = m_prime * radians.sin();

    (
        Ucs::new(j_prime.v, a_prime.v, b_prime.v),
        [j_prime.d, a_prime.d, b_prime.d],
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reference_ucs(xyz: Xyz) -> Ucs {
        looklab_model::solve::ucs_of_xyz(xyz)
    }

    #[test]
    fn value_matches_model_forward_model() {
        let mut state = 0x243f_6a88_85a3_08d3_u64;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state >> 11) as f64 / (1u64 << 53) as f64
        };
        for _ in 0..200 {
            let xyz = Xyz::new(0.002 + next(), 0.002 + next(), 0.002 + next());
            let mine = ucs(xyz);
            let theirs = reference_ucs(xyz);
            assert!((mine.j - theirs.j).abs() < 1e-9, "{mine:?} {theirs:?}");
            assert!((mine.a - theirs.a).abs() < 1e-9);
            assert!((mine.b - theirs.b).abs() < 1e-9);
        }
    }

    #[test]
    fn jacobian_matches_central_difference() {
        for xyz in [
            Xyz::new(0.2, 0.18, 0.1),
            Xyz::new(0.9, 0.4, 1.1),
            Xyz::new(0.03, 0.9, 0.02),
        ] {
            let (_, jacobian) = ucs_jacobian(xyz);
            for axis in 0..3 {
                let h = 1e-6 * xyz.to_array()[axis].abs().max(0.01);
                let mut plus = xyz;
                let mut minus = xyz;
                match axis {
                    0 => {
                        plus.x += h;
                        minus.x -= h;
                    }
                    1 => {
                        plus.y += h;
                        minus.y -= h;
                    }
                    _ => {
                        plus.z += h;
                        minus.z -= h;
                    }
                }
                let a = reference_ucs(plus);
                let b = reference_ucs(minus);
                let numerical = [
                    (a.j - b.j) / (2.0 * h),
                    (a.a - b.a) / (2.0 * h),
                    (a.b - b.b) / (2.0 * h),
                ];
                for channel in 0..3 {
                    let scale = numerical[channel].abs().max(1.0);
                    assert!(
                        (jacobian[channel][axis] - numerical[channel]).abs() / scale < 1e-5,
                        "{xyz:?} axis {axis} channel {channel}: {} vs {}",
                        jacobian[channel][axis],
                        numerical[channel]
                    );
                }
            }
        }
    }
}
