// SPDX-License-Identifier: AGPL-3.0-or-later
//! Forward-mode `d(J'a'b')/dY` at fixed chromaticity for the Look Jacobian;
//! `deriv_matches_looklab_color` pins it to the crate's forward model.

use core::f64::consts::PI;
use core::ops::{Add, Div, Mul, Neg, Sub};
use looklab_color::cam16::{ConeOpponent, ViewingConditions};
use looklab_color::ucs::Ucs;
use looklab_color::{Mat3, WhiteRelativeXyz, Xyz};
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

/// Coefficients of `a`, `b` and the denominator in the post-adaptation responses.
const OPPONENT_A: [f64; 3] = [1.0, -12.0 / 11.0, 1.0 / 11.0];
const OPPONENT_B: [f64; 3] = [1.0 / 9.0, 1.0 / 9.0, -2.0 / 9.0];
const TEMPORARY_DENOMINATOR: [f64; 3] = [1.0, 1.0, 21.0 / 20.0];

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

    fn sqrt(self) -> Self {
        if self.v < 0.0 {
            return Self::new(f64::NAN, 0.0);
        }
        if self.v == 0.0 {
            return Self::new(0.0, infinite_scale(1.0, self.d));
        }
        let root = self.v.sqrt();
        Self::new(root, self.d / (2.0 * root))
    }

    fn pow_signed(self, p: f64) -> Self {
        let magnitude = self.v.abs();
        let value = self.v.signum() * libm::pow(magnitude, p);
        let derivative = if magnitude > 0.0 {
            p * libm::pow(magnitude, p - 1.0) * self.d
        } else if p == 1.0 {
            self.d
        } else if p < 1.0 {
            infinite_scale(1.0, self.d)
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
        infinite_scale(1.0, x.d)
    } else {
        x.d / denominator
    };
    Dual::new(libm::log1p(x.v), derivative)
}

fn atan2_degrees(b: Dual, a: Dual) -> Dual {
    let scale = 180.0 / PI;
    let denominator = a.v * a.v + b.v * b.v;
    let derivative = if denominator == 0.0 {
        if a.d == 0.0 && b.d == 0.0 {
            0.0
        } else {
            f64::NAN
        }
    } else {
        (a.v * b.d - b.v * a.d) / denominator * scale
    };
    Dual::new(libm::atan2(b.v, a.v) * scale, derivative)
}

/// Zero when the direction does not vary.
fn infinite_scale(slope: f64, d: f64) -> f64 {
    if d == 0.0 { 0.0 } else { slope * d }
}

fn post_adaptation(response: Dual, f_l: f64) -> Dual {
    let slope = if response.v == 0.0 {
        f64::INFINITY
    } else {
        let u = f_l * response.v.abs() / 100.0;
        let f = libm::pow(u, 0.42);
        400.0 * 27.13 / ((27.13 + f) * (27.13 + f)) * 0.42 * (f / u) * (f_l / 100.0)
    };
    Dual::new(
        post_adaptation_scalar(response.v, f_l),
        infinite_scale(slope, response.d),
    )
}

fn post_adaptation_departure_dual(neutral: f64, delta: Dual, f_l: f64) -> Dual {
    Dual::new(
        post_adaptation_departure(neutral, delta.v, f_l),
        post_adaptation_slope(neutral + delta.v, f_l) * delta.d,
    )
}

fn post_adaptation_scalar(response: f64, f_l: f64) -> f64 {
    let f = libm::pow(f_l * response.abs() / 100.0, 0.42);
    400.0 * response.signum() * f / (27.13 + f) + 0.1
}

/// `post_adaptation_scalar(neutral + delta) - post_adaptation_scalar(neutral)`,
/// evaluated without cancelling a `delta` below the resolution of `neutral`.
// A NaN operand compares false and takes the direct difference.
#[allow(clippy::neg_cmp_op_on_partial_ord)]
fn post_adaptation_departure(neutral: f64, delta: f64, f_l: f64) -> f64 {
    let response = neutral + delta;
    if !(response > 0.0) || !(neutral > 0.0) || delta.abs() > neutral {
        return post_adaptation_scalar(response, f_l) - post_adaptation_scalar(neutral, f_l);
    }
    let f0 = libm::pow(f_l * neutral / 100.0, 0.42);
    let difference = f0 * libm::expm1(0.42 * libm::log1p(delta / neutral));
    let base = 27.13 + f0;
    400.0 * 27.13 * difference / (base * (base + difference))
}

/// Infinite at a zero response.
fn post_adaptation_slope(response: f64, f_l: f64) -> f64 {
    let u = f_l * response.abs() / 100.0;
    if u == 0.0 {
        return f64::INFINITY;
    }
    let f = libm::pow(u, 0.42);
    400.0 * 27.13 / ((27.13 + f) * (27.13 + f)) * 0.42 * (f / u) * (f_l / 100.0)
}

/// `d²/dresponse²` of [`post_adaptation_scalar`]; odd in `response`.
fn post_adaptation_curvature(response: f64, f_l: f64) -> f64 {
    if response == 0.0 {
        return f64::NAN;
    }
    let u = f_l * response.abs() / 100.0;
    let f = libm::pow(u, 0.42);
    let p = 0.42;
    let f_prime = p * f / response;
    let k = 400.0 * 27.13 / ((27.13 + f) * (27.13 + f));
    let k_prime = -2.0 * 400.0 * 27.13 / ((27.13 + f) * (27.13 + f) * (27.13 + f)) * f_prime;
    let f_second = p * (f_prime * response - f) / (response * response);
    response.signum() * (k_prime * f_prime + k * f_second)
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
    neutral_a: f64,
    neutral_b: f64,
    neutral_denominator: f64,
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
        let neutral_response = [
            post_adaptation_scalar(y_w, f_l),
            post_adaptation_scalar(y_w, f_l),
            post_adaptation_scalar(y_w, f_l),
        ];
        let neutral_a = (11.0 * (neutral_response[0] - neutral_response[1])
            + (neutral_response[2] - neutral_response[1]))
            / 11.0;
        let neutral_b = ((neutral_response[0] - neutral_response[2])
            + (neutral_response[1] - neutral_response[2]))
            / 9.0;
        let neutral_denominator =
            neutral_response[0] + neutral_response[1] + 1.05 * neutral_response[2];
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
            neutral_a,
            neutral_b,
            neutral_denominator,
        }
    }

    pub(crate) fn ucs_and_opponent(&self, xyz: Xyz) -> (Ucs, ConeOpponent) {
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
        let denominator = rgb_a[0] + rgb_a[1] + 1.05 * rgb_a[2];
        let temporary = self.p_scale * e_t * (a * a + b * b).sqrt() / denominator;
        let chroma = signed_pow(temporary, 0.9) * signed_sqrt(j / 100.0) * self.ta;
        let m_prime = libm::log1p(C2 * chroma * self.fl_root4) / C2;
        let radians = h * (PI / 180.0);
        (
            Ucs::new(
                j * (1.0 + 100.0 * 0.007) / (1.0 + 0.007 * j),
                m_prime * libm::cos(radians),
                m_prime * libm::sin(radians),
            ),
            ConeOpponent {
                rgb_c,
                rgb_a,
                a,
                b,
                denominator,
                lightness: j,
            },
        )
    }

    /// CAM16-UCS of `xyz` (crate scale, white Y = 1).
    pub(crate) fn ucs(&self, xyz: Xyz) -> Ucs {
        self.ucs_and_opponent(xyz).0
    }

    /// `dJ'/dXYZ`; `J'` depends only on the achromatic response.
    #[allow(dead_code)]
    pub(crate) fn j_prime_xyz_gradient(&self, xyz: Xyz) -> [f64; 3] {
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
        let achromatic = (2.0 * rgb_a[0] + rgb_a[1] + 0.05 * rgb_a[2] - 0.305) * self.n_bb;
        let ratio = achromatic / self.a_w;
        let j = 100.0 * signed_pow(ratio, self.j_exp);
        let d_j_d_achromatic =
            100.0 * self.j_exp * libm::pow(ratio.abs(), self.j_exp - 1.0) / self.a_w;
        let d_j_prime_d_j = 1.7 / ((1.0 + 0.007 * j) * (1.0 + 0.007 * j));
        let scale = d_j_prime_d_j * d_j_d_achromatic;
        let mut gradient = [0.0_f64; 3];
        for (i, coefficient) in [2.0, 1.0, 0.05].into_iter().enumerate() {
            let slope = post_adaptation_slope(rgb_c[i], self.f_l);
            let response = scale * coefficient * self.n_bb * slope * self.d_rgb[i] * 100.0;
            for (axis, value) in gradient.iter_mut().enumerate() {
                *value += response * M16.0[i][axis];
            }
        }
        gradient
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
        self.neutral_y_and_dy_for_j_prime(target).map(|(y, _)| y)
    }

    /// Neutral `Y` for `target = J'`, and `dY/dJ'` of the closed form.
    pub(crate) fn neutral_y_and_dy_for_j_prime(&self, target: f64) -> Option<(f64, f64)> {
        let denominator = 1.0 + 100.0 * 0.007 - 0.007 * target;
        if !denominator.is_finite() || denominator <= 0.0 {
            return None;
        }
        let j = target / denominator;
        if !(j > 0.0 && j.is_finite()) {
            return None;
        }
        let base = j / 100.0;
        let ratio = libm::pow(base, 1.0 / self.j_exp);
        let achromatic = ratio * self.a_w;
        let rgb_a = (achromatic / self.n_bb + 0.305) / 3.05;
        let x = rgb_a - 0.1;
        if !(x > 0.0 && x < 400.0) {
            return None;
        }
        let f = 27.13 * x / (400.0 - x);
        if !(f > 0.0 && f.is_finite()) {
            return None;
        }
        let u = libm::pow(f, 1.0 / 0.42);
        let y = u / self.f_l;
        if !(y > 0.0 && y.is_finite()) {
            return None;
        }
        let d_j_d_j_prime = 1.7 / (denominator * denominator);
        let d_ratio_d_j = ratio / (self.j_exp * j);
        let d_achromatic_d_j = self.a_w * d_ratio_d_j;
        let d_rgb_a_d_achromatic = 1.0 / (3.05 * self.n_bb);
        let d_f_d_x = 27.13 * 400.0 / ((400.0 - x) * (400.0 - x));
        let d_u_d_f = u / (0.42 * f);
        let d_y_d_j_prime =
            d_j_d_j_prime * d_achromatic_d_j * d_rgb_a_d_achromatic * d_f_d_x * d_u_d_f / self.f_l;
        Some((y, d_y_d_j_prime))
    }

    /// Neutral `J'` as the stimulus luminance goes to infinity.
    pub(crate) fn neutral_j_prime_max(&self) -> f64 {
        let response = 400.1;
        let achromatic = (3.05 * response - 0.305) * self.n_bb;
        let ratio = achromatic / self.a_w;
        let j = 100.0 * signed_pow(ratio, self.j_exp);
        j * (1.0 + 100.0 * 0.007) / (1.0 + 0.007 * j)
    }

    /// Adapted cone responses at `xyz`, `∂response_c/∂XYZ`, and `∂²response_c/∂XYZ²`
    /// packed as `(0,0) (1,1) (2,2) (0,1) (0,2) (1,2)`.
    pub(crate) fn response_xyz_derivatives(
        &self,
        xyz: Xyz,
    ) -> ([f64; 3], [[f64; 3]; 3], [[f64; 3]; 6]) {
        let t = xyz.y * self.inv_white_y;
        let residual = [
            xyz.x - t * self.white.x,
            xyz.y - t * self.white.y,
            xyz.z - t * self.white.z,
        ];
        let residual_jacobian = [
            [1.0, -self.white.x * self.inv_white_y, 0.0],
            [0.0, 1.0 - self.white.y * self.inv_white_y, 0.0],
            [0.0, -self.white.z * self.inv_white_y, 1.0],
        ];
        let t_gradient = [0.0, self.inv_white_y, 0.0];
        let mut value = [0.0_f64; 3];
        let mut jacobian = [[0.0_f64; 3]; 3];
        let mut second = [[0.0_f64; 3]; 6];
        for (channel, row) in M16.0.iter().enumerate() {
            let response = row[0] * residual[0] + row[1] * residual[1] + row[2] * residual[2];
            let linear = t * self.white_scale[channel] + self.d_rgb[channel] * response * 100.0;
            let mut gradient = [0.0_f64; 3];
            for (axis, value) in gradient.iter_mut().enumerate() {
                let d_response = row[0] * residual_jacobian[0][axis]
                    + row[1] * residual_jacobian[1][axis]
                    + row[2] * residual_jacobian[2][axis];
                *value = self.white_scale[channel] * t_gradient[axis]
                    + self.d_rgb[channel] * 100.0 * d_response;
            }
            value[channel] = post_adaptation_scalar(linear, self.f_l);
            let slope = post_adaptation_slope(linear, self.f_l);
            let curvature = post_adaptation_curvature(linear, self.f_l);
            for axis in 0..3 {
                jacobian[channel][axis] = slope * gradient[axis];
            }
            for (slot, (a, b)) in [(0usize, 0usize), (1, 1), (2, 2), (0, 1), (0, 2), (1, 2)]
                .into_iter()
                .enumerate()
            {
                second[slot][channel] = curvature * gradient[a] * gradient[b];
            }
        }
        (value, jacobian, second)
    }

    /// CAM16-UCS of `unit_xyz * y`, with derivatives with respect to `y`.
    pub(crate) fn ucs_and_derivative(&self, unit_xyz: Xyz, y: f64) -> (Ucs, [f64; 3]) {
        self.ucs_dual(
            Xyz::new(unit_xyz.x * y, unit_xyz.y * y, unit_xyz.z * y),
            [unit_xyz.x, unit_xyz.y, unit_xyz.z],
        )
    }

    /// CAM16-UCS of `xyz` and `d(J'a'b')/dXYZ`.
    pub(crate) fn ucs_xyz_jacobian(&self, xyz: Xyz) -> (Ucs, [[f64; 3]; 3]) {
        let mut value = Ucs::new(0.0, 0.0, 0.0);
        let mut jacobian = [[0.0_f64; 3]; 3];
        for axis in 0..3 {
            let mut direction = [0.0_f64; 3];
            direction[axis] = 1.0;
            let (ucs, derivative) = self.ucs_dual(xyz, direction);
            value = ucs;
            for channel in 0..3 {
                jacobian[channel][axis] = derivative[channel];
            }
        }
        (value, jacobian)
    }

    /// CAM16-UCS of `white + departure` and `d(J'a'b')/dXYZ`.
    pub(crate) fn ucs_departure_and_jacobian(
        &self,
        departure: WhiteRelativeXyz,
    ) -> (Ucs, [[f64; 3]; 3]) {
        let mut value = Ucs::new(0.0, 0.0, 0.0);
        let mut jacobian = [[0.0_f64; 3]; 3];
        for axis in 0..3 {
            let mut direction = [0.0_f64; 3];
            direction[axis] = 1.0;
            let (ucs, derivative) = self.ucs_dual_departure(departure, direction);
            value = ucs;
            for channel in 0..3 {
                jacobian[channel][axis] = derivative[channel];
            }
        }
        (value, jacobian)
    }

    fn ucs_dual(&self, xyz: Xyz, direction: [f64; 3]) -> (Ucs, [f64; 3]) {
        let x = Dual::new(xyz.x, direction[0]);
        let yy = Dual::new(xyz.y, direction[1]);
        let z = Dual::new(xyz.z, direction[2]);

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
        let denominator = rgb_a[0] + rgb_a[1] + Dual::constant(1.05) * rgb_a[2];
        let achromatic =
            (Dual::constant(2.0) * rgb_a[0] + rgb_a[1] + Dual::constant(0.05) * rgb_a[2]
                - Dual::constant(0.305))
                * Dual::constant(self.n_bb);
        self.ucs_dual_from_opponent(a, b, denominator, achromatic)
    }

    fn ucs_dual_departure(
        &self,
        departure: WhiteRelativeXyz,
        direction: [f64; 3],
    ) -> (Ucs, [f64; 3]) {
        let x = Dual::new(departure.x(), direction[0]);
        let yy = Dual::new(departure.y(), direction[1]);
        let z = Dual::new(departure.z(), direction[2]);

        let mut departures = [Dual::constant(0.0); 3];
        for (channel, row) in M16.0.iter().enumerate() {
            let response = Dual::constant(row[0]) * x
                + Dual::constant(row[1]) * yy
                + Dual::constant(row[2]) * z;
            let delta = Dual::constant(self.d_rgb[channel] * 100.0) * response;
            departures[channel] =
                post_adaptation_departure_dual(self.white_scale[channel], delta, self.f_l);
        }

        let a = departures.iter().enumerate().fold(
            Dual::constant(self.neutral_a),
            |value, (channel, departure)| value + Dual::constant(OPPONENT_A[channel]) * *departure,
        );
        let b = departures.iter().enumerate().fold(
            Dual::constant(self.neutral_b),
            |value, (channel, departure)| value + Dual::constant(OPPONENT_B[channel]) * *departure,
        );
        let denominator = departures.iter().enumerate().fold(
            Dual::constant(self.neutral_denominator),
            |value, (channel, departure)| {
                value + Dual::constant(TEMPORARY_DENOMINATOR[channel]) * *departure
            },
        );
        let achromatic = Dual::constant(self.a_w)
            + (Dual::constant(2.0) * departures[0]
                + departures[1]
                + Dual::constant(0.05) * departures[2])
                * Dual::constant(self.n_bb);
        self.ucs_dual_from_opponent(a, b, denominator, achromatic)
    }

    fn ucs_dual_from_opponent(
        &self,
        a: Dual,
        b: Dual,
        denominator: Dual,
        achromatic: Dual,
    ) -> (Ucs, [f64; 3]) {
        let h = atan2_degrees(b, a);
        let e_t = (Dual::constant(2.0) + h * Dual::constant(PI / 180.0)).cos()
            * Dual::constant(0.25)
            + Dual::constant(0.95);

        let j =
            (achromatic / Dual::constant(self.a_w)).pow_signed(self.j_exp) * Dual::constant(100.0);
        let temporary = Dual::constant(self.p_scale) * e_t * (a * a + b * b).sqrt() / denominator;
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

/// Second-order directional dual: value, first and second derivative along one direction.
#[derive(Clone, Copy, Debug)]
struct Dual2 {
    v: f64,
    d: f64,
    dd: f64,
}

impl Dual2 {
    fn new(v: f64, d: f64, dd: f64) -> Self {
        Self { v, d, dd }
    }

    fn constant(v: f64) -> Self {
        Self { v, d: 0.0, dd: 0.0 }
    }

    fn sqrt(self) -> Self {
        if self.v < 0.0 {
            return Self::new(f64::NAN, 0.0, 0.0);
        }
        if self.v == 0.0 {
            return Self::new(0.0, infinite_scale(1.0, self.d), 0.0);
        }
        let root = self.v.sqrt();
        let first = self.d / (2.0 * root);
        let second = self.dd / (2.0 * root) - self.d * self.d / (4.0 * root * root * root);
        Self::new(root, first, second)
    }

    fn pow_signed(self, p: f64) -> Self {
        let magnitude = self.v.abs();
        let value = self.v.signum() * libm::pow(magnitude, p);
        if magnitude == 0.0 {
            let first = if p == 1.0 {
                self.d
            } else if p < 1.0 {
                infinite_scale(1.0, self.d)
            } else {
                0.0
            };
            return Self::new(value, first, 0.0);
        }
        let first = p * libm::pow(magnitude, p - 1.0) * self.d;
        let second = p * (p - 1.0) * libm::pow(magnitude, p - 2.0) * self.d * self.d
            + p * libm::pow(magnitude, p - 1.0) * self.dd;
        Self::new(value, first, second)
    }

    fn cos(self) -> Self {
        let (sin, cos) = (libm::sin(self.v), libm::cos(self.v));
        Self::new(cos, -sin * self.d, -cos * self.d * self.d - sin * self.dd)
    }

    fn sin(self) -> Self {
        let (sin, cos) = (libm::sin(self.v), libm::cos(self.v));
        Self::new(sin, cos * self.d, -sin * self.d * self.d + cos * self.dd)
    }
}

impl Add for Dual2 {
    type Output = Self;
    fn add(self, other: Self) -> Self {
        Self::new(self.v + other.v, self.d + other.d, self.dd + other.dd)
    }
}

impl Sub for Dual2 {
    type Output = Self;
    fn sub(self, other: Self) -> Self {
        Self::new(self.v - other.v, self.d - other.d, self.dd - other.dd)
    }
}

impl Mul for Dual2 {
    type Output = Self;
    fn mul(self, other: Self) -> Self {
        Self::new(
            self.v * other.v,
            self.d * other.v + self.v * other.d,
            self.dd * other.v + 2.0 * self.d * other.d + self.v * other.dd,
        )
    }
}

impl Div for Dual2 {
    type Output = Self;
    fn div(self, other: Self) -> Self {
        let denominator = other.v * other.v;
        let numerator = self.d * other.v - self.v * other.d;
        Self::new(
            self.v / other.v,
            numerator / denominator,
            (self.dd * other.v - self.v * other.dd) / denominator
                - 2.0 * other.d * numerator / (denominator * other.v),
        )
    }
}

impl Neg for Dual2 {
    type Output = Self;
    fn neg(self) -> Self {
        Self::new(-self.v, -self.d, -self.dd)
    }
}

fn log1p2(x: Dual2) -> Dual2 {
    let denominator = 1.0 + x.v;
    if denominator == 0.0 {
        return Dual2::new(libm::log1p(x.v), infinite_scale(1.0, x.d), 0.0);
    }
    Dual2::new(
        libm::log1p(x.v),
        x.d / denominator,
        x.dd / denominator - x.d * x.d / (denominator * denominator),
    )
}

fn atan2_degrees2(b: Dual2, a: Dual2) -> Dual2 {
    let scale = 180.0 / PI;
    let denominator = a.v * a.v + b.v * b.v;
    if denominator == 0.0 {
        let first = if a.d == 0.0 && b.d == 0.0 {
            0.0
        } else {
            f64::NAN
        };
        let second = if a.d == 0.0 && b.d == 0.0 && a.dd == 0.0 && b.dd == 0.0 {
            0.0
        } else {
            f64::NAN
        };
        return Dual2::new(libm::atan2(b.v, a.v) * scale, first, second);
    }
    let numerator = a.v * b.d - b.v * a.d;
    let numerator2 = a.v * b.dd - b.v * a.dd;
    Dual2::new(
        libm::atan2(b.v, a.v) * scale,
        numerator / denominator * scale,
        (numerator2 * denominator - numerator * 2.0 * (a.v * a.d + b.v * b.d))
            / (denominator * denominator)
            * scale,
    )
}

fn post_adaptation2(response: Dual2, f_l: f64) -> Dual2 {
    if response.v == 0.0 {
        let slope = f64::INFINITY;
        let first = infinite_scale(slope, response.d);
        return Dual2::new(
            post_adaptation_scalar(0.0, f_l),
            first,
            if response.d == 0.0 { 0.0 } else { f64::NAN },
        );
    }
    let u = f_l * response.v.abs() / 100.0;
    let f = libm::pow(u, 0.42);
    let p = 0.42;
    let slope = 400.0 * 27.13 / ((27.13 + f) * (27.13 + f)) * 0.42 * (f / u) * (f_l / 100.0);
    let f_prime = p * f / response.v;
    let k = 400.0 * 27.13 / ((27.13 + f) * (27.13 + f));
    let k_prime = -2.0 * 400.0 * 27.13 / ((27.13 + f) * (27.13 + f) * (27.13 + f)) * f_prime;
    let f_second = p * (f_prime * response.v - f) / (response.v * response.v);
    let second = response.v.signum() * (k_prime * f_prime + k * f_second);
    Dual2::new(
        post_adaptation_scalar(response.v, f_l),
        slope * response.d,
        second * response.d * response.d + slope * response.dd,
    )
}

fn post_adaptation_departure2(neutral: f64, delta: Dual2, f_l: f64) -> Dual2 {
    let response = neutral + delta.v;
    let slope = post_adaptation_slope(response, f_l);
    Dual2::new(
        post_adaptation_departure(neutral, delta.v, f_l),
        slope * delta.d,
        post_adaptation_curvature(response, f_l) * delta.d * delta.d + slope * delta.dd,
    )
}

impl ReferenceCam16 {
    /// CAM16-UCS of `unit_xyz * y`; both derivative vectors are along `y`.
    pub(crate) fn ucs_and_derivative_d2(&self, unit_xyz: Xyz, y: f64) -> (Ucs, [f64; 3], [f64; 3]) {
        let xyz = Xyz::new(unit_xyz.x * y, unit_xyz.y * y, unit_xyz.z * y);
        self.ucs_dual2(xyz, [unit_xyz.x, unit_xyz.y, unit_xyz.z])
    }

    /// Neutral `Y` for `target = J'` with first and second derivatives.
    pub(crate) fn neutral_y_and_dy2_for_j_prime(&self, target: f64) -> Option<(f64, f64, f64)> {
        let model = self;
        let target = Dual2::new(target, 1.0, 0.0);
        let denominator = Dual2::constant(1.0 + 100.0 * 0.007) - Dual2::constant(0.007) * target;
        if denominator.v.is_nan() || denominator.v <= 0.0 {
            return None;
        }
        let j = target / denominator;
        if !(j.v > 0.0 && j.v.is_finite()) {
            return None;
        }
        let base = j * Dual2::constant(0.01);
        let ratio = base.pow_signed(1.0 / model.j_exp);
        let achromatic = ratio * Dual2::constant(model.a_w);
        let rgb_a = (achromatic / Dual2::constant(model.n_bb) + Dual2::constant(0.305))
            / Dual2::constant(3.05);
        let x = rgb_a - Dual2::constant(0.1);
        if !(x.v > 0.0 && x.v < 400.0) {
            return None;
        }
        let f = Dual2::constant(27.13) * x / (Dual2::constant(400.0) - x);
        if !(f.v > 0.0 && f.v.is_finite()) {
            return None;
        }
        let u = f.pow_signed(1.0 / 0.42);
        let y = u / Dual2::constant(model.f_l);
        if !(y.v > 0.0 && y.v.is_finite()) {
            return None;
        }
        Some((y.v, y.d, y.dd))
    }

    /// `∂²UCS/∂XYZ_a∂XYZ_b`, packed as `(0,0) (1,1) (2,2) (0,1) (0,2) (1,2)`.
    pub(crate) fn ucs_xyz_second(&self, xyz: Xyz) -> [[f64; 3]; 6] {
        let mut out = [[0.0_f64; 3]; 6];
        let diagonal = [0usize, 1, 2];
        for (slot, &axis) in diagonal.iter().enumerate() {
            let mut direction = [0.0_f64; 3];
            direction[axis] = 1.0;
            out[slot] = self.ucs_dual2(xyz, direction).2;
        }
        let mixed = [(0usize, 1usize, 3usize), (0, 2, 4), (1, 2, 5)];
        for (a, b, slot) in mixed {
            let mut direction = [0.0_f64; 3];
            direction[a] = 1.0;
            direction[b] = 1.0;
            let second = self.ucs_dual2(xyz, direction).2;
            for channel in 0..3 {
                out[slot][channel] = 0.5 * (second[channel] - out[a][channel] - out[b][channel]);
            }
        }
        out
    }

    pub(crate) fn ucs_xyz_second_departure(&self, departure: WhiteRelativeXyz) -> [[f64; 3]; 6] {
        let mut out = [[0.0_f64; 3]; 6];
        let diagonal = [0usize, 1, 2];
        for (slot, &axis) in diagonal.iter().enumerate() {
            let mut direction = [0.0_f64; 3];
            direction[axis] = 1.0;
            out[slot] = self.ucs_dual2_departure(departure, direction).2;
        }
        let mixed = [(0usize, 1usize, 3usize), (0, 2, 4), (1, 2, 5)];
        for (a, b, slot) in mixed {
            let mut direction = [0.0_f64; 3];
            direction[a] = 1.0;
            direction[b] = 1.0;
            let second = self.ucs_dual2_departure(departure, direction).2;
            for channel in 0..3 {
                out[slot][channel] = 0.5 * (second[channel] - out[a][channel] - out[b][channel]);
            }
        }
        out
    }

    /// `n = K·(a, b)/denominator`, packed as `(0,0) (1,1) (2,2) (0,1) (0,2) (1,2)`.
    pub(crate) fn neutral_xyz_second(&self, xyz: Xyz, scale: f64) -> [[f64; 2]; 6] {
        let mut out = [[0.0_f64; 2]; 6];
        for (slot, axis) in [0usize, 1, 2].into_iter().enumerate() {
            let mut direction = [0.0_f64; 3];
            direction[axis] = 1.0;
            out[slot] = self.neutral_second(xyz, direction, scale).2;
        }
        for (a, b, slot) in [(0usize, 1usize, 3usize), (0, 2, 4), (1, 2, 5)] {
            let mut direction = [0.0_f64; 3];
            direction[a] = 1.0;
            direction[b] = 1.0;
            let second = self.neutral_second(xyz, direction, scale).2;
            for channel in 0..2 {
                out[slot][channel] = 0.5 * (second[channel] - out[a][channel] - out[b][channel]);
            }
        }
        out
    }

    pub(crate) fn neutral_xyz_second_departure(
        &self,
        departure: WhiteRelativeXyz,
        scale: f64,
    ) -> [[f64; 2]; 6] {
        let mut out = [[0.0_f64; 2]; 6];
        for (slot, axis) in [0usize, 1, 2].into_iter().enumerate() {
            let mut direction = [0.0_f64; 3];
            direction[axis] = 1.0;
            out[slot] = self.neutral_second_departure(departure, direction, scale).2;
        }
        for (a, b, slot) in [(0usize, 1usize, 3usize), (0, 2, 4), (1, 2, 5)] {
            let mut direction = [0.0_f64; 3];
            direction[a] = 1.0;
            direction[b] = 1.0;
            let second = self.neutral_second_departure(departure, direction, scale).2;
            for channel in 0..2 {
                out[slot][channel] = 0.5 * (second[channel] - out[a][channel] - out[b][channel]);
            }
        }
        out
    }

    /// Normalized neutral chroma `scale·(a, b)/denominator` at `xyz` with first and
    /// second directional derivatives along `direction`.
    pub(crate) fn neutral_second(
        &self,
        xyz: Xyz,
        direction: [f64; 3],
        scale: f64,
    ) -> ([f64; 2], [f64; 2], [f64; 2]) {
        let x = Dual2::new(xyz.x, direction[0], 0.0);
        let yy = Dual2::new(xyz.y, direction[1], 0.0);
        let z = Dual2::new(xyz.z, direction[2], 0.0);
        let t = yy * Dual2::constant(self.inv_white_y);
        let residual = [
            x - t * Dual2::constant(self.white.x),
            yy - t * Dual2::constant(self.white.y),
            z - t * Dual2::constant(self.white.z),
        ];
        let mut rgb_c = [Dual2::constant(0.0); 3];
        for (i, channel) in rgb_c.iter_mut().enumerate() {
            let response = Dual2::constant(M16.0[i][0]) * residual[0]
                + Dual2::constant(M16.0[i][1]) * residual[1]
                + Dual2::constant(M16.0[i][2]) * residual[2];
            *channel = t * Dual2::constant(self.white_scale[i])
                + Dual2::constant(self.d_rgb[i]) * response * Dual2::constant(100.0);
        }
        let rgb_a = rgb_c.map(|response| post_adaptation2(response, self.f_l));
        let a = (Dual2::constant(11.0) * (rgb_a[0] - rgb_a[1]) + (rgb_a[2] - rgb_a[1]))
            / Dual2::constant(11.0);
        let b = ((rgb_a[0] - rgb_a[2]) + (rgb_a[1] - rgb_a[2])) / Dual2::constant(9.0);
        let denominator = rgb_a[0] + rgb_a[1] + Dual2::constant(1.05) * rgb_a[2];
        let scale = Dual2::constant(scale);
        let na = scale * a / denominator;
        let nb = scale * b / denominator;
        ([na.v, nb.v], [na.d, nb.d], [na.dd, nb.dd])
    }

    fn neutral_second_departure(
        &self,
        departure: WhiteRelativeXyz,
        direction: [f64; 3],
        scale: f64,
    ) -> ([f64; 2], [f64; 2], [f64; 2]) {
        let x = Dual2::new(departure.x(), direction[0], 0.0);
        let yy = Dual2::new(departure.y(), direction[1], 0.0);
        let z = Dual2::new(departure.z(), direction[2], 0.0);
        let mut departures = [Dual2::constant(0.0); 3];
        for (channel, row) in M16.0.iter().enumerate() {
            let response = Dual2::constant(row[0]) * x
                + Dual2::constant(row[1]) * yy
                + Dual2::constant(row[2]) * z;
            let delta = Dual2::constant(self.d_rgb[channel] * 100.0) * response;
            departures[channel] =
                post_adaptation_departure2(self.white_scale[channel], delta, self.f_l);
        }
        let a = departures.iter().enumerate().fold(
            Dual2::constant(self.neutral_a),
            |value, (channel, departure)| value + Dual2::constant(OPPONENT_A[channel]) * *departure,
        );
        let b = departures.iter().enumerate().fold(
            Dual2::constant(self.neutral_b),
            |value, (channel, departure)| value + Dual2::constant(OPPONENT_B[channel]) * *departure,
        );
        let denominator = departures.iter().enumerate().fold(
            Dual2::constant(self.neutral_denominator),
            |value, (channel, departure)| {
                value + Dual2::constant(TEMPORARY_DENOMINATOR[channel]) * *departure
            },
        );
        let scale = Dual2::constant(scale);
        let na = scale * a / denominator;
        let nb = scale * b / denominator;
        ([na.v, nb.v], [na.d, nb.d], [na.dd, nb.dd])
    }

    fn ucs_dual2(&self, xyz: Xyz, direction: [f64; 3]) -> (Ucs, [f64; 3], [f64; 3]) {
        let x = Dual2::new(xyz.x, direction[0], 0.0);
        let yy = Dual2::new(xyz.y, direction[1], 0.0);
        let z = Dual2::new(xyz.z, direction[2], 0.0);

        let t = yy * Dual2::constant(self.inv_white_y);
        let residual = [
            x - t * Dual2::constant(self.white.x),
            yy - t * Dual2::constant(self.white.y),
            z - t * Dual2::constant(self.white.z),
        ];
        let mut rgb_c = [Dual2::constant(0.0); 3];
        for (i, channel) in rgb_c.iter_mut().enumerate() {
            let response = Dual2::constant(M16.0[i][0]) * residual[0]
                + Dual2::constant(M16.0[i][1]) * residual[1]
                + Dual2::constant(M16.0[i][2]) * residual[2];
            *channel = t * Dual2::constant(self.white_scale[i])
                + Dual2::constant(self.d_rgb[i]) * response * Dual2::constant(100.0);
        }

        let rgb_a = rgb_c.map(|response| post_adaptation2(response, self.f_l));
        let a = (Dual2::constant(11.0) * (rgb_a[0] - rgb_a[1]) + (rgb_a[2] - rgb_a[1]))
            / Dual2::constant(11.0);
        let b = ((rgb_a[0] - rgb_a[2]) + (rgb_a[1] - rgb_a[2])) / Dual2::constant(9.0);
        let denominator = rgb_a[0] + rgb_a[1] + Dual2::constant(1.05) * rgb_a[2];
        let achromatic =
            (Dual2::constant(2.0) * rgb_a[0] + rgb_a[1] + Dual2::constant(0.05) * rgb_a[2]
                - Dual2::constant(0.305))
                * Dual2::constant(self.n_bb);
        self.ucs_dual2_from_opponent(a, b, denominator, achromatic)
    }

    fn ucs_dual2_departure(
        &self,
        departure: WhiteRelativeXyz,
        direction: [f64; 3],
    ) -> (Ucs, [f64; 3], [f64; 3]) {
        let x = Dual2::new(departure.x(), direction[0], 0.0);
        let yy = Dual2::new(departure.y(), direction[1], 0.0);
        let z = Dual2::new(departure.z(), direction[2], 0.0);

        let mut departures = [Dual2::constant(0.0); 3];
        for (channel, row) in M16.0.iter().enumerate() {
            let response = Dual2::constant(row[0]) * x
                + Dual2::constant(row[1]) * yy
                + Dual2::constant(row[2]) * z;
            let delta = Dual2::constant(self.d_rgb[channel] * 100.0) * response;
            departures[channel] =
                post_adaptation_departure2(self.white_scale[channel], delta, self.f_l);
        }

        let a = departures.iter().enumerate().fold(
            Dual2::constant(self.neutral_a),
            |value, (channel, departure)| value + Dual2::constant(OPPONENT_A[channel]) * *departure,
        );
        let b = departures.iter().enumerate().fold(
            Dual2::constant(self.neutral_b),
            |value, (channel, departure)| value + Dual2::constant(OPPONENT_B[channel]) * *departure,
        );
        let denominator = departures.iter().enumerate().fold(
            Dual2::constant(self.neutral_denominator),
            |value, (channel, departure)| {
                value + Dual2::constant(TEMPORARY_DENOMINATOR[channel]) * *departure
            },
        );
        let achromatic = Dual2::constant(self.a_w)
            + (Dual2::constant(2.0) * departures[0]
                + departures[1]
                + Dual2::constant(0.05) * departures[2])
                * Dual2::constant(self.n_bb);
        self.ucs_dual2_from_opponent(a, b, denominator, achromatic)
    }

    fn ucs_dual2_from_opponent(
        &self,
        a: Dual2,
        b: Dual2,
        denominator: Dual2,
        achromatic: Dual2,
    ) -> (Ucs, [f64; 3], [f64; 3]) {
        let h = atan2_degrees2(b, a);
        let e_t = (Dual2::constant(2.0) + h * Dual2::constant(PI / 180.0)).cos()
            * Dual2::constant(0.25)
            + Dual2::constant(0.95);

        let j = (achromatic / Dual2::constant(self.a_w)).pow_signed(self.j_exp)
            * Dual2::constant(100.0);
        let temporary = Dual2::constant(self.p_scale) * e_t * (a * a + b * b).sqrt() / denominator;
        let chroma = temporary.pow_signed(0.9)
            * (j * Dual2::constant(0.01)).pow_signed(0.5)
            * Dual2::constant(self.ta);
        let colourfulness = chroma * Dual2::constant(self.fl_root4);

        let j_prime = j * Dual2::constant(1.0 + 100.0 * 0.007)
            / (Dual2::constant(1.0) + j * Dual2::constant(0.007));
        let m_prime = log1p2(colourfulness * Dual2::constant(C2)) * Dual2::constant(1.0 / C2);
        let h_radians = h * Dual2::constant(PI / 180.0);
        let a_prime = m_prime * h_radians.cos();
        let b_prime = m_prime * h_radians.sin();

        (
            Ucs::new(j_prime.v, a_prime.v, b_prime.v),
            [j_prime.d, a_prime.d, b_prime.d],
            [j_prime.dd, a_prime.dd, b_prime.dd],
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
            let (inverted, inverted_derivative) = reference
                .neutral_y_and_dy_for_j_prime(j_prime)
                .expect("inverse with derivative");
            assert!((inverted - y).abs() / y < 1e-9);
            let expected = 1.0 / derivative;
            assert!(
                (inverted_derivative - expected).abs() / expected.abs() < 1e-6,
                "y {y}: {} vs {expected}",
                inverted_derivative
            );
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

    #[test]
    fn second_derivative_matches_difference_of_first() {
        let reference = ReferenceCam16::new();
        let units = [Xyz::new(0.65, 1.0, 0.45), Xyz::new(0.912, 1.0, 1.033)];
        for unit in units {
            for y in [0.005_f64, 0.02, 0.1, 0.4, 1.5] {
                let h = 1e-6 * y.max(0.01);
                let (_, plus, _) = reference.ucs_and_derivative_d2(unit, y + h);
                let (_, minus, _) = reference.ucs_and_derivative_d2(unit, y - h);
                let (_, _, second) = reference.ucs_and_derivative_d2(unit, y);
                for channel in 0..3 {
                    let numerical = (plus[channel] - minus[channel]) / (2.0 * h);
                    let scale = numerical.abs().max(1.0);
                    assert!(
                        (second[channel] - numerical).abs() / scale < 1e-3,
                        "unit {unit:?} y {y} channel {channel}: {} vs {}",
                        second[channel],
                        numerical
                    );
                }
            }
        }
    }

    #[test]
    fn neutral_inverse_second_derivative_matches_difference_of_first() {
        let reference = ReferenceCam16::new();
        for y in [0.001_f64, 0.01, 0.18, 0.5, 1.5, 5.0] {
            let (j_prime, _) = reference.neutral_j_and_dj_dy(y);
            let (_, first, second) = reference
                .neutral_y_and_dy2_for_j_prime(j_prime)
                .expect("inverse with derivatives");
            let h = 1e-6 * j_prime.max(1.0);
            let (_, plus, _) = reference
                .neutral_y_and_dy2_for_j_prime(j_prime + h)
                .expect("plus");
            let (_, minus, _) = reference
                .neutral_y_and_dy2_for_j_prime(j_prime - h)
                .expect("minus");
            let numerical = (plus - minus) / (2.0 * h);
            let scale = numerical.abs().max(first.abs()).max(1.0);
            assert!(
                (second - numerical).abs() / scale < 1e-4,
                "y {y}: {} vs {}",
                second,
                numerical
            );
        }
    }
}
