// SPDX-License-Identifier: AGPL-3.0-or-later
//! Forward-mode `d(J'a'b')/dY` at fixed chromaticity for the Look Jacobian;
//! `deriv_matches_looklab_color` pins it to the crate's forward model.

use core::f64::consts::PI;
use core::ops::{Add, Div, Mul, Neg, Sub};
use looklab_color::cam16::ViewingConditions;
use looklab_color::ucs::Ucs;
use looklab_color::{Mat3, Xyz};
use std::sync::LazyLock;

static REFERENCE: LazyLock<ReferenceCam16> = LazyLock::new(ReferenceCam16::new);

pub(crate) fn reference_cam16() -> &'static ReferenceCam16 {
    &REFERENCE
}

const M16: Mat3 = Mat3([
    [0.401_288, 0.650_173, -0.051_461],
    [-0.250_268, 1.204_414, 0.045_854],
    [-0.002_079, 0.048_952, 0.953_127],
]);

/// CAM16-UCS `c2` (Luo et al. 2006).
const C2: f64 = 0.0228;

#[derive(Clone, Copy, Debug)]
struct Dual {
    v: f64,
    d: f64,
}

impl Dual {
    fn new(v: f64, d: f64) -> Self {
        Self { v, d }
    }

    fn constant(v: f64) -> Self {
        Self { v, d: 0.0 }
    }

    fn abs(self) -> Self {
        if self.v > 0.0 {
            self
        } else if self.v < 0.0 {
            Self::new(-self.v, -self.d)
        } else {
            Self::new(0.0, 0.0)
        }
    }

    fn sqrt(self) -> Self {
        if self.v <= 0.0 {
            Self::new(self.v.sqrt(), 0.0)
        } else {
            let root = self.v.sqrt();
            Self::new(root, self.d / (2.0 * root))
        }
    }

    fn pow_signed(self, p: f64) -> Self {
        let magnitude = self.v.abs();
        let value = self.v.signum() * libm::pow(magnitude, p);
        let derivative = if magnitude > 0.0 {
            p * libm::pow(magnitude, p - 1.0) * self.d
        } else {
            0.0
        };
        Self::new(value, derivative)
    }

    fn cos(self) -> Self {
        Self::new(libm::cos(self.v), -libm::sin(self.v) * self.d)
    }

    fn sin(self) -> Self {
        Self::new(libm::sin(self.v), libm::cos(self.v) * self.d)
    }
}

impl Add for Dual {
    type Output = Self;
    fn add(self, other: Self) -> Self {
        Self::new(self.v + other.v, self.d + other.d)
    }
}

impl Sub for Dual {
    type Output = Self;
    fn sub(self, other: Self) -> Self {
        Self::new(self.v - other.v, self.d - other.d)
    }
}

impl Mul for Dual {
    type Output = Self;
    fn mul(self, other: Self) -> Self {
        Self::new(self.v * other.v, self.d * other.v + self.v * other.d)
    }
}

impl Div for Dual {
    type Output = Self;
    fn div(self, other: Self) -> Self {
        Self::new(
            self.v / other.v,
            (self.d * other.v - self.v * other.d) / (other.v * other.v),
        )
    }
}

impl Neg for Dual {
    type Output = Self;
    fn neg(self) -> Self {
        Self::new(-self.v, -self.d)
    }
}

fn log1p(x: Dual) -> Dual {
    let denominator = 1.0 + x.v;
    let derivative = if denominator == 0.0 {
        0.0
    } else {
        x.d / denominator
    };
    Dual::new(libm::log1p(x.v), derivative)
}

fn atan2_degrees(b: Dual, a: Dual) -> Dual {
    let scale = 180.0 / PI;
    let denominator = a.v * a.v + b.v * b.v;
    let derivative = if denominator == 0.0 {
        0.0
    } else {
        (a.v * b.d - b.v * a.d) / denominator * scale
    };
    Dual::new(libm::atan2(b.v, a.v) * scale, derivative)
}

fn post_adaptation(response: Dual, f_l: f64) -> Dual {
    let u = response.abs() * Dual::constant(f_l / 100.0);
    let f = u.pow_signed(0.42);
    let sign = response.v.signum();
    Dual::constant(400.0 * sign) * f / (f + Dual::constant(27.13)) + Dual::constant(0.1)
}

fn post_adaptation_scalar(response: f64, f_l: f64) -> f64 {
    let f = libm::pow(f_l * response.abs() / 100.0, 0.42);
    400.0 * response.signum() * f / (27.13 + f) + 0.1
}

fn signed_pow(x: f64, p: f64) -> f64 {
    x.signum() * libm::pow(x.abs(), p)
}

fn signed_sqrt(x: f64) -> f64 {
    x.signum() * x.abs().sqrt()
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct ReferenceCam16 {
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

impl ReferenceCam16 {
    pub(crate) fn new() -> Self {
        let vc = ViewingConditions::reference();
        let y_w = vc.white.y * 100.0;
        let rgb_w = M16.mul_vec([vc.white.x * 100.0, y_w, vc.white.z * 100.0]);
        let k = 1.0 / (5.0 * vc.l_a + 1.0);
        let k4 = k * k * k * k;
        let f_l = k4 * vc.l_a + 0.1 * (1.0 - k4) * (1.0 - k4) * libm::cbrt(5.0 * vc.l_a);
        let n = vc.y_b / y_w;
        let z = 1.48 + n.sqrt();
        let n_bb = 0.725 * libm::pow(1.0 / n, 0.2);
        let d_rgb = [y_w / rgb_w[0], y_w / rgb_w[1], y_w / rgb_w[2]];
        let rgb_aw = [
            post_adaptation_scalar(d_rgb[0] * rgb_w[0], f_l),
            post_adaptation_scalar(d_rgb[1] * rgb_w[1], f_l),
            post_adaptation_scalar(d_rgb[2] * rgb_w[2], f_l),
        ];
        let a_w = (2.0 * rgb_aw[0] + rgb_aw[1] + 0.05 * rgb_aw[2] - 0.305) * n_bb;
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

    /// CAM16-UCS of `xyz` (crate scale, white Y = 1).
    pub(crate) fn ucs(&self, xyz: Xyz) -> Ucs {
        let t = xyz.y * self.inv_white_y;
        let residual = [
            xyz.x - t * self.white.x,
            xyz.y - t * self.white.y,
            xyz.z - t * self.white.z,
        ];
        let mut rgb_c = [0.0_f64; 3];
        for (i, channel) in rgb_c.iter_mut().enumerate() {
            let response =
                M16.0[i][0] * residual[0] + M16.0[i][1] * residual[1] + M16.0[i][2] * residual[2];
            *channel = t * self.white_scale[i] + self.d_rgb[i] * response * 100.0;
        }
        let rgb_a = rgb_c.map(|response| post_adaptation_scalar(response, self.f_l));
        let a = (11.0 * (rgb_a[0] - rgb_a[1]) + (rgb_a[2] - rgb_a[1])) / 11.0;
        let b = ((rgb_a[0] - rgb_a[2]) + (rgb_a[1] - rgb_a[2])) / 9.0;
        let h = libm::atan2(b, a) * (180.0 / PI);
        let e_t = (libm::cos(2.0 + h * (PI / 180.0)) + 3.8) / 4.0;
        let achromatic = (2.0 * rgb_a[0] + rgb_a[1] + 0.05 * rgb_a[2] - 0.305) * self.n_bb;
        let j = 100.0 * signed_pow(achromatic / self.a_w, self.j_exp);
        let temporary =
            self.p_scale * e_t * (a * a + b * b).sqrt() / (rgb_a[0] + rgb_a[1] + 1.05 * rgb_a[2]);
        let chroma = signed_pow(temporary, 0.9) * signed_sqrt(j / 100.0) * self.ta;
        let m_prime = libm::log1p(C2 * chroma * self.fl_root4) / C2;
        let radians = h * (PI / 180.0);
        Ucs::new(
            j * (1.0 + 100.0 * 0.007) / (1.0 + 0.007 * j),
            m_prime * libm::cos(radians),
            m_prime * libm::sin(radians),
        )
    }

    /// `J'` of a D65 neutral at luminance `y` and its derivative.
    pub(crate) fn neutral_j_and_dj_dy(&self, y: f64) -> (f64, f64) {
        if y <= 0.0 {
            return (0.0, 0.0);
        }
        let response = 100.0 * y;
        let rgb_a = post_adaptation_scalar(response, self.f_l);
        let achromatic = (3.05 * rgb_a - 0.305) * self.n_bb;
        let ratio = achromatic / self.a_w;
        let j = 100.0 * signed_pow(ratio, self.j_exp);
        let j_prime = j * (1.0 + 100.0 * 0.007) / (1.0 + 0.007 * j);
        if ratio <= 0.0 {
            return (j_prime, 0.0);
        }
        let u = self.f_l * response / 100.0;
        let f = libm::pow(u, 0.42);
        let d_response =
            400.0 * 27.13 / ((27.13 + f) * (27.13 + f)) * 0.42 * (f / u) * (self.f_l / 100.0);
        let d_achromatic = 3.05 * self.n_bb * d_response * 100.0;
        let d_j_d_achromatic = 100.0 * self.j_exp * (j / 100.0) / ratio / self.a_w;
        let d_j_prime_d_j = 1.7 / ((1.0 + 0.007 * j) * (1.0 + 0.007 * j));
        (j_prime, d_j_prime_d_j * d_j_d_achromatic * d_achromatic)
    }

    pub(crate) fn neutral_y_for_j_prime(&self, target: f64) -> Option<f64> {
        let j = target / (1.0 + 100.0 * 0.007 - 0.007 * target);
        if !(j > 0.0 && j.is_finite()) {
            return None;
        }
        let ratio = libm::pow(j / 100.0, 1.0 / self.j_exp);
        let achromatic = ratio * self.a_w;
        let rgb_a = (achromatic / self.n_bb + 0.305) / 3.05;
        let x = rgb_a - 0.1;
        if !(x > 0.0 && x < 400.0) {
            return None;
        }
        let f = 27.13 * x / (400.0 - x);
        let u = libm::pow(f, 1.0 / 0.42);
        let y = u / self.f_l;
        (y > 0.0 && y.is_finite()).then_some(y)
    }

    /// Neutral `J'` as the stimulus luminance goes to infinity.
    pub(crate) fn neutral_j_prime_max(&self) -> f64 {
        let response = 400.1;
        let achromatic = (3.05 * response - 0.305) * self.n_bb;
        let ratio = achromatic / self.a_w;
        let j = 100.0 * signed_pow(ratio, self.j_exp);
        j * (1.0 + 100.0 * 0.007) / (1.0 + 0.007 * j)
    }

    /// CAM16-UCS of `unit_xyz * y`, with derivatives with respect to `y`.
    pub(crate) fn ucs_and_derivative(&self, unit_xyz: Xyz, y: f64) -> (Ucs, [f64; 3]) {
        let x = Dual::new(unit_xyz.x * y, unit_xyz.x);
        let yy = Dual::new(unit_xyz.y * y, unit_xyz.y);
        let z = Dual::new(unit_xyz.z * y, unit_xyz.z);

        let t = yy * Dual::constant(self.inv_white_y);
        let residual = [
            x - t * Dual::constant(self.white.x),
            yy - t * Dual::constant(self.white.y),
            z - t * Dual::constant(self.white.z),
        ];
        let mut rgb_c = [Dual::constant(0.0); 3];
        for (i, channel) in rgb_c.iter_mut().enumerate() {
            let response = Dual::constant(M16.0[i][0]) * residual[0]
                + Dual::constant(M16.0[i][1]) * residual[1]
                + Dual::constant(M16.0[i][2]) * residual[2];
            *channel = t * Dual::constant(self.white_scale[i])
                + Dual::constant(self.d_rgb[i]) * response * Dual::constant(100.0);
        }

        let rgb_a = rgb_c.map(|response| post_adaptation(response, self.f_l));
        let a = (Dual::constant(11.0) * (rgb_a[0] - rgb_a[1]) + (rgb_a[2] - rgb_a[1]))
            / Dual::constant(11.0);
        let b = ((rgb_a[0] - rgb_a[2]) + (rgb_a[1] - rgb_a[2])) / Dual::constant(9.0);
        let h = atan2_degrees(b, a);
        let e_t = (Dual::constant(2.0) + h * Dual::constant(PI / 180.0)).cos()
            * Dual::constant(0.25)
            + Dual::constant(0.95);

        let achromatic =
            (Dual::constant(2.0) * rgb_a[0] + rgb_a[1] + Dual::constant(0.05) * rgb_a[2]
                - Dual::constant(0.305))
                * Dual::constant(self.n_bb);
        let j =
            (achromatic / Dual::constant(self.a_w)).pow_signed(self.j_exp) * Dual::constant(100.0);
        let temporary = Dual::constant(self.p_scale) * e_t * (a * a + b * b).sqrt()
            / (rgb_a[0] + rgb_a[1] + Dual::constant(1.05) * rgb_a[2]);
        let chroma = temporary.pow_signed(0.9)
            * (j * Dual::constant(0.01)).pow_signed(0.5)
            * Dual::constant(self.ta);
        let colourfulness = chroma * Dual::constant(self.fl_root4);

        let j_prime = j * Dual::constant(1.0 + 100.0 * 0.007)
            / (Dual::constant(1.0) + j * Dual::constant(0.007));
        let m_prime = log1p(colourfulness * Dual::constant(C2)) * Dual::constant(1.0 / C2);
        let h_radians = h * Dual::constant(PI / 180.0);
        let a_prime = m_prime * h_radians.cos();
        let b_prime = m_prime * h_radians.sin();

        (
            Ucs::new(j_prime.v, a_prime.v, b_prime.v),
            [j_prime.d, a_prime.d, b_prime.d],
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use looklab_color::ucs::{UcsSpace, jmh_to_ucs};

    fn crate_ucs(unit: Xyz, y: f64) -> Ucs {
        let model = looklab_color::cam16::Cam16Model::new(&ViewingConditions::reference());
        let cam = model.xyz_to_cam16(Xyz::new(unit.x * y, unit.y * y, unit.z * y));
        jmh_to_ucs(
            looklab_color::cam16::Jmh {
                j: cam.j,
                m: cam.m,
                h: cam.h,
            },
            UcsSpace::Ucs,
        )
    }

    #[test]
    fn deriv_matches_looklab_color() {
        let reference = ReferenceCam16::new();
        let mut state = 0x1234_5678_9abc_def0_u64;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state >> 11) as f64 / (1u64 << 53) as f64
        };
        for _ in 0..200 {
            let unit = Xyz::new(0.3 + 1.2 * next(), 0.3 + 1.2 * next(), 0.3 + 1.2 * next());
            let y = 0.01 + 1.5 * next();
            let ours = reference.ucs_and_derivative(unit, y).0;
            let theirs = crate_ucs(unit, y);
            assert!((ours.j - theirs.j).abs() < 1e-10, "{ours:?} vs {theirs:?}");
            assert!((ours.a - theirs.a).abs() < 1e-10);
            assert!((ours.b - theirs.b).abs() < 1e-10);
        }
    }

    #[test]
    fn fast_forward_matches_looklab_color() {
        let reference = ReferenceCam16::new();
        let mut state = 0xfeed_face_0000_0001_u64;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state >> 11) as f64 / (1u64 << 53) as f64
        };
        for _ in 0..200 {
            let xyz = Xyz::new(0.001 + next(), 0.001 + next(), 0.001 + next());
            let ours = reference.ucs(xyz);
            let theirs = crate_ucs(Xyz::new(xyz.x / xyz.y, 1.0, xyz.z / xyz.y), xyz.y);
            assert!((ours.j - theirs.j).abs() < 1e-10, "{ours:?} vs {theirs:?}");
            assert!((ours.a - theirs.a).abs() < 1e-10);
            assert!((ours.b - theirs.b).abs() < 1e-10);
        }
    }

    #[test]
    fn fast_neutral_matches_looklab_color() {
        let reference = ReferenceCam16::new();
        let model = looklab_color::cam16::Cam16Model::new(&ViewingConditions::reference());
        let white = ViewingConditions::reference().white;
        for y in [1e-4_f64, 0.001, 0.01, 0.18, 0.5, 1.5, 5.0] {
            let (j_prime, derivative) = reference.neutral_j_and_dj_dy(y);
            let (theirs_j, theirs_derivative) = model.ucs_j_and_dj_dy(white, y);
            assert!((j_prime - theirs_j).abs() < 1e-11, "y {y}");
            let scale = theirs_derivative.abs().max(1.0);
            assert!(
                (derivative - theirs_derivative).abs() / scale < 1e-9,
                "y {y}: {derivative} vs {theirs_derivative}"
            );
            let inverted = reference.neutral_y_for_j_prime(j_prime).expect("inverse");
            assert!((inverted - y).abs() / y < 1e-9, "y {y} inverted {inverted}");
        }
    }

    #[test]
    fn derivative_matches_finite_difference() {
        let reference = ReferenceCam16::new();
        let units = [Xyz::new(0.65, 1.0, 0.45), Xyz::new(0.912, 1.0, 1.033)];
        for unit in units {
            for y in [0.001_f64, 0.005, 0.02, 0.1, 0.4, 1.5] {
                let h = 1e-7 * y.max(0.01);
                let plus = crate_ucs(unit, y + h);
                let minus = crate_ucs(unit, y - h);
                let (_, derivative) = reference.ucs_and_derivative(unit, y);
                let numerical = [
                    (plus.j - minus.j) / (2.0 * h),
                    (plus.a - minus.a) / (2.0 * h),
                    (plus.b - minus.b) / (2.0 * h),
                ];
                for channel in 0..3 {
                    let scale = numerical[channel].abs().max(1.0);
                    assert!(
                        (derivative[channel] - numerical[channel]).abs() / scale < 1e-4,
                        "unit {unit:?} y {y} channel {channel}: {} vs {}",
                        derivative[channel],
                        numerical[channel]
                    );
                }
            }
        }
    }
}
