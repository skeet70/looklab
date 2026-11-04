// SPDX-License-Identifier: AGPL-3.0-or-later
//! CAM16 colour appearance model and its viewing conditions.
//!
//! Port of `colour.XYZ_to_CAM16` and `colour.CAM16_to_XYZ` from colour-science 0.4.7
//! (BSD-3-Clause, Colour Developers); see `NOTICE`.
//!
//! Reference: Li, C., Li, Z., Wang, Z., Xu, Y., Luo, M. R., Cui, G., Melgosa, M.,
//! Brill, M. H., Pointer, M. (2017). *Comprehensive color solutions: CAM16, CAT16,
//! and CAM16-UCS.* Color Research & Application, 42(6), 703-718.
//! doi:10.1002/col.22131
//!
//! Scale: [`Xyz`] keeps Y of the reference white at 1, while colour-science works at
//! 0-100. Inputs are multiplied by 100 internally; the correlates and the CAM16-UCS
//! coordinates stay on colour-science's 0-100 reference scale.

use serde::{Deserialize, Serialize};

use crate::linalg::Mat3;
use crate::types::Xyz;

/// Sharpened cone response matrix `MATRIX_16` of Li et al. (2017).
const M16: Mat3 = Mat3([
    [0.401_288, 0.650_173, -0.051_461],
    [-0.250_268, 1.204_414, 0.045_854],
    [-0.002_079, 0.048_952, 0.953_127],
]);

const PI: f64 = core::f64::consts::PI;

/// Surround induction factors `(F, c, N_c)` of CIECAM02 (CIE 159:2004), as used by
/// CAM16 and named in colour-science's `VIEWING_CONDITIONS_CAM16`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Surround {
    Average,
    Dim,
    Dark,
}

impl Surround {
    /// Maximum degree of adaptation `F`.
    pub const fn f(self) -> f64 {
        match self {
            Self::Average => 1.0,
            Self::Dim => 0.9,
            Self::Dark => 0.8,
        }
    }

    /// Exponential non-linearity `c`.
    pub const fn c(self) -> f64 {
        match self {
            Self::Average => 0.69,
            Self::Dim => 0.59,
            Self::Dark => 0.525,
        }
    }

    /// Chromatic induction factor `N_c`.
    pub const fn n_c(self) -> f64 {
        match self {
            Self::Average => 1.0,
            Self::Dim => 0.9,
            Self::Dark => 0.8,
        }
    }
}

/// Viewing conditions for a CAM16 conversion.
///
/// `white.y` must be 1 (the crate convention: Y of the reference white is 1);
/// `l_a` is the adapting field luminance in cd/m² and `y_b` the background luminance
/// factor on colour-science's 0-100 scale.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct ViewingConditions {
    pub white: Xyz,
    pub l_a: f64,
    pub y_b: f64,
    pub surround: Surround,
    pub discount_illuminant: bool,
}

impl ViewingConditions {
    /// The conditions every Look is fitted and compared under (docs/02 §1).
    ///
    /// D65 adopted white, `L_A = 64/π × 0.2 ≈ 4.07 cd/m²`, `Y_b = 20`, average
    /// surround, and the illuminant discounted (`D = 1`).
    pub const fn reference() -> Self {
        Self {
            // D65, CIE 1931 2° observer: xy = (0.3127, 0.3290), Y = 1.
            white: Xyz::new(0.950_455_927_051_671_6, 1.0, 1.089_057_750_759_878_4),
            l_a: 12.8 * core::f64::consts::FRAC_1_PI,
            y_b: 20.0,
            surround: Surround::Average,
            discount_illuminant: true,
        }
    }

    /// CIECAM02 formula, clipped to `[0, 1]` as in `colour.XYZ_to_CAM16`.
    pub fn degree_of_adaptation(&self) -> f64 {
        if self.discount_illuminant {
            1.0
        } else {
            let d = self.surround.f() * (1.0 - (1.0 / 3.6) * libm::exp((-self.l_a - 42.0) / 92.0));
            d.clamp(0.0, 1.0)
        }
    }
}

/// The CAM16 correlates for one stimulus, all on the 0-100 reference scale.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Cam16 {
    /// Lightness `J`.
    pub j: f64,
    /// Chroma `C`.
    pub c: f64,
    /// Hue angle `h` in degrees.
    pub h: f64,
    /// Colourfulness `M`.
    pub m: f64,
    /// Saturation `s`.
    pub s: f64,
    /// Brightness `Q`.
    pub q: f64,
}

/// Lightness, colourfulness and hue angle, the form CAM16-UCS consumes.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Jmh {
    pub j: f64,
    pub m: f64,
    /// Hue angle in degrees.
    pub h: f64,
}

/// Which chroma correlate a CAM16 specification carries.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Chroma {
    C(f64),
    M(f64),
}

/// Viewing-condition-dependent constants, computed once per condition set.
#[derive(Clone, Copy, Debug)]
pub struct Cam16Model {
    f_l: f64,
    /// `F_L^0.25`, the factor between chroma and colourfulness.
    fl_root4: f64,
    /// `c·z`, the exponent of the lightness correlate.
    j_exp: f64,
    inv_j_exp: f64,
    /// `(1.64 - 0.29^n)^0.73`, shared by chroma and its inverse.
    ta: f64,
    /// `(50000/13)·N_c·N_cb`, the scale of the temporary magnitude `t`.
    p_scale: f64,
    /// `(4/c)·(A_w + 4)·F_L^0.25`, the scale of brightness.
    q_scale: f64,
    a_w: f64,
    n_bb: f64,
    d_rgb: [f64; 3],
    white: Xyz,
    inv_white_y: f64,
    /// `D_RGB · RGB_w`; exactly `Y_w` when `D = 1`.
    white_scale: [f64; 3],
    m16_inverse: Mat3,
}

impl Cam16Model {
    pub fn new(vc: &ViewingConditions) -> Self {
        let y_w = vc.white.y * 100.0;
        let rgb_w = M16.mul_vec([vc.white.x * 100.0, y_w, vc.white.z * 100.0]);

        let d = vc.degree_of_adaptation();

        let k = 1.0 / (5.0 * vc.l_a + 1.0);
        let k4 = k * k * k * k;
        let f_l = k4 * vc.l_a + 0.1 * (1.0 - k4) * (1.0 - k4) * libm::cbrt(5.0 * vc.l_a);

        let n = vc.y_b / y_w;
        let z = 1.48 + n.sqrt();
        let n_bb = 0.725 * libm::pow(1.0 / n, 0.2);
        let n_cb = n_bb;

        let d_rgb = if d == 1.0 {
            // D = 1 makes the adaptation a plain rescale by the white's response.
            [y_w / rgb_w[0], y_w / rgb_w[1], y_w / rgb_w[2]]
        } else {
            [
                d * y_w / rgb_w[0] + 1.0 - d,
                d * y_w / rgb_w[1] + 1.0 - d,
                d * y_w / rgb_w[2] + 1.0 - d,
            ]
        };
        let rgb_aw = [
            post_adaptation(d_rgb[0] * rgb_w[0], f_l),
            post_adaptation(d_rgb[1] * rgb_w[1], f_l),
            post_adaptation(d_rgb[2] * rgb_w[2], f_l),
        ];
        let a_w = achromatic_response(rgb_aw, n_bb);

        let white_scale = if d == 1.0 {
            [y_w; 3]
        } else {
            [
                d_rgb[0] * rgb_w[0],
                d_rgb[1] * rgb_w[1],
                d_rgb[2] * rgb_w[2],
            ]
        };

        let c = vc.surround.c();
        let fl_root4 = libm::pow(f_l, 0.25);

        Self {
            f_l,
            fl_root4,
            j_exp: c * z,
            inv_j_exp: 1.0 / (c * z),
            ta: libm::pow(1.64 - libm::pow(0.29, n), 0.73),
            p_scale: (50000.0 / 13.0) * vc.surround.n_c() * n_cb,
            q_scale: (4.0 / c) * (a_w + 4.0) * fl_root4,
            a_w,
            n_bb,
            d_rgb,
            white: vc.white,
            inv_white_y: 1.0 / vc.white.y,
            white_scale,
            // M16 is a change of basis with determinant near 1.
            m16_inverse: M16.inverse().expect("MATRIX_16 is nonsingular"),
        }
    }

    /// Adapted cone responses, decomposed around the white so a neutral input gives
    /// three exactly equal channels.
    fn adapted_rgb(&self, xyz: Xyz) -> [f64; 3] {
        let t = xyz.y * self.inv_white_y;
        let residual = [
            xyz.x - t * self.white.x,
            xyz.y - t * self.white.y,
            xyz.z - t * self.white.z,
        ];
        let rgb_residual = M16.mul_vec([
            residual[0] * 100.0,
            residual[1] * 100.0,
            residual[2] * 100.0,
        ]);
        [
            t * self.white_scale[0] + self.d_rgb[0] * rgb_residual[0],
            t * self.white_scale[1] + self.d_rgb[1] * rgb_residual[1],
            t * self.white_scale[2] + self.d_rgb[2] * rgb_residual[2],
        ]
    }

    /// Forward model, `colour.XYZ_to_CAM16`.
    ///
    /// `xyz` follows the crate convention (Y of white = 1). Negative tristimulus
    /// values are supported through the signed power of the post-adaptation response.
    pub fn xyz_to_cam16(&self, xyz: Xyz) -> Cam16 {
        let rgb_c = self.adapted_rgb(xyz);
        let rgb_a = [
            post_adaptation(rgb_c[0], self.f_l),
            post_adaptation(rgb_c[1], self.f_l),
            post_adaptation(rgb_c[2], self.f_l),
        ];

        // `opponent_colour_dimensions_forward`, factored so R = G = B gives exact zero.
        let a = (11.0 * (rgb_a[0] - rgb_a[1]) + (rgb_a[2] - rgb_a[1])) / 11.0;
        let b = ((rgb_a[0] - rgb_a[2]) + (rgb_a[1] - rgb_a[2])) / 9.0;

        let h = hue_degrees(a, b);
        let e_t = eccentricity(h);

        let achromatic = achromatic_response(rgb_a, self.n_bb);
        let j = 100.0 * spow(achromatic / self.a_w, self.j_exp);
        let q = self.q_scale * (j / 100.0).sqrt();

        // `temporary_magnitude_quantity_forward`.
        let t = sdiv(
            self.p_scale * (e_t * (a * a + b * b).sqrt()),
            rgb_a[0] + rgb_a[1] + 21.0 * rgb_a[2] / 20.0,
        );
        let c = spow(t, 0.9) * spow(j / 100.0, 0.5) * self.ta;
        let m = c * self.fl_root4;
        let s = 100.0 * spow(sdiv(m, q), 0.5);

        Cam16 { j, c, h, m, s, q }
    }

    /// Inverse model, `colour.CAM16_to_XYZ`.
    ///
    /// `j` is lightness and `h` the hue angle in degrees; `chroma` carries either
    /// `C` or `M`. Returns XYZ on the crate scale (Y of white = 1).
    pub fn cam16_to_xyz(&self, j: f64, chroma: Chroma, h: f64) -> Xyz {
        let c = match chroma {
            Chroma::C(c) => c,
            Chroma::M(m) => m / self.fl_root4,
        };

        // `temporary_magnitude_quantity_inverse`.
        let j_safe = j.max(f64::EPSILON);
        let t = spow(c / ((j_safe / 100.0).sqrt() * self.ta), 1.0 / 0.9);

        let e_t = eccentricity(h);
        let achromatic = self.a_w * spow(j / 100.0, self.inv_j_exp);

        let p_1 = self.p_scale * e_t / t;
        let p_2 = achromatic / self.n_bb + 0.305;
        let p_3 = 21.0 / 20.0;

        let (a, b) = opponent_colour_dimensions_inverse(p_1, p_2, p_3, h);
        // A zero magnitude has no defined hue.
        let gate = if t == 0.0 { 0.0 } else { 1.0 };
        let a = a * gate;
        let b = b * gate;

        // `matrix_post_adaptation_non_linear_response_compression`, row-major.
        let inv_1403 = 1.0 / 1403.0;
        let rgb_a = [
            (460.0 * p_2 + 451.0 * a + 288.0 * b) * inv_1403,
            (460.0 * p_2 - 891.0 * a - 261.0 * b) * inv_1403,
            (460.0 * p_2 - 220.0 * a - 6300.0 * b) * inv_1403,
        ];
        let rgb = [
            post_adaptation_inverse(rgb_a[0], self.f_l) / self.d_rgb[0],
            post_adaptation_inverse(rgb_a[1], self.f_l) / self.d_rgb[1],
            post_adaptation_inverse(rgb_a[2], self.f_l) / self.d_rgb[2],
        ];
        let xyz = self.m16_inverse.mul_vec(rgb);
        Xyz::new(xyz[0] / 100.0, xyz[1] / 100.0, xyz[2] / 100.0)
    }

    /// Inverse model from [`Jmh`], the form the CAM16-UCS code produces.
    pub fn jmh_to_xyz(&self, jmh: Jmh) -> Xyz {
        self.cam16_to_xyz(jmh.j, Chroma::M(jmh.m), jmh.h)
    }

    /// CAM16-UCS lightness `J'` and its derivative with respect to the stimulus
    /// `Y`, at fixed chromaticity.
    ///
    /// `xyz_at_unit_y` is the chromaticity with `y = 1`; the stimulus is
    /// `xyz_at_unit_y · y`.
    ///
    /// `J'` depends only on the achromatic response. At `y = 0` the one-sided
    /// derivative is infinite; its sign follows the leading `y^0.42` term.
    pub fn ucs_j_and_dj_dy(&self, xyz_at_unit_y: Xyz, y: f64) -> (f64, f64) {
        let stimulus = Xyz::new(
            xyz_at_unit_y.x * y,
            xyz_at_unit_y.y * y,
            xyz_at_unit_y.z * y,
        );
        let rgb_c = self.adapted_rgb(stimulus);

        let mut rgb_a = [0.0; 3];
        let mut drgb_a = [0.0; 3];
        for ((ra, dra), response) in rgb_a.iter_mut().zip(drgb_a.iter_mut()).zip(rgb_c.iter()) {
            let u = self.f_l * response.abs() / 100.0;
            let f = libm::pow(u, 0.42);
            *ra = 400.0 * response.signum() * f / (27.13 + f) + 0.1;
            let df_du = 0.42 * libm::pow(u, -0.58);
            *dra = 400.0 * 27.13 / ((27.13 + f) * (27.13 + f)) * df_du * (self.f_l / 100.0);
        }

        let achromatic = achromatic_response(rgb_a, self.n_bb);
        let ratio = achromatic / self.a_w;
        let j = 100.0 * spow(ratio, self.j_exp);
        let j_prime = crate::ucs::j_prime(j);
        let dj_da = 100.0 * self.j_exp * libm::pow(ratio.abs(), self.j_exp - 1.0) / self.a_w;

        // dXYZ100/dy along the fixed chromaticity.
        let direction = [
            xyz_at_unit_y.x * 100.0,
            xyz_at_unit_y.y * 100.0,
            xyz_at_unit_y.z * 100.0,
        ];
        let d_rgb_c_dy = [
            self.d_rgb[0] * dot(&M16.0[0], &direction),
            self.d_rgb[1] * dot(&M16.0[1], &direction),
            self.d_rgb[2] * dot(&M16.0[2], &direction),
        ];
        let dy_dy = if xyz_at_unit_y.y == 0.0 {
            1.0
        } else {
            1.0 / xyz_at_unit_y.y
        };

        if y == 0.0 {
            // dR_a/dc diverges; the leading term of A(y) is the 0.42 power of the
            // directional response, so the derivative's sign is that sum's sign.
            let lead = self.n_bb
                * (400.0 / 27.13)
                * (2.0 * leading_response(self.f_l, d_rgb_c_dy[0])
                    + leading_response(self.f_l, d_rgb_c_dy[1])
                    + 0.05 * leading_response(self.f_l, d_rgb_c_dy[2]));
            let signed = lead * dy_dy;
            let slope = if signed == 0.0 {
                f64::INFINITY
            } else {
                f64::INFINITY.copysign(signed)
            };
            return (j_prime, slope);
        }

        let mut d_a_dy = 0.0;
        for (i, coefficient) in [2.0, 1.0, 0.05].into_iter().enumerate() {
            if d_rgb_c_dy[i] != 0.0 {
                d_a_dy += coefficient * drgb_a[i] * d_rgb_c_dy[i];
            }
        }
        let d_a_dy = self.n_bb * d_a_dy;

        (
            j_prime,
            crate::ucs::d_j_prime_d_j(j) * dj_da * d_a_dy * dy_dy,
        )
    }
}

/// Signed power, `colour.algebra.spow`.
fn spow(x: f64, p: f64) -> f64 {
    x.signum() * libm::pow(x.abs(), p)
}

fn dot(a: &[f64; 3], b: &[f64; 3]) -> f64 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

/// `sign(w)·(F_L·|w|/100)^0.42`, the leading term of a channel's response as the
/// stimulus luminance goes to zero.
fn leading_response(f_l: f64, w: f64) -> f64 {
    w.signum() * libm::pow(f_l * w.abs() / 100.0, 0.42)
}

/// Post-adaptation non-linear response compression, Luo et al. (2013) negative
/// value handling as in `post_adaptation_non_linear_response_compression_forward`.
fn post_adaptation(response: f64, f_l: f64) -> f64 {
    let f = libm::pow(f_l * response.abs() / 100.0, 0.42);
    400.0 * response.signum() * f / (27.13 + f) + 0.1
}

fn post_adaptation_inverse(rgb_a: f64, f_l: f64) -> f64 {
    let x = rgb_a - 0.1;
    x.signum() * 100.0 / f_l * libm::pow(27.13 * x.abs() / (400.0 - x.abs()), 1.0 / 0.42)
}

/// `achromatic_response_forward`: `(2R + G + B/20 - 0.305)·N_bb`.
fn achromatic_response(rgb_a: [f64; 3], n_bb: f64) -> f64 {
    (2.0 * rgb_a[0] + rgb_a[1] + 0.05 * rgb_a[2] - 0.305) * n_bb
}

/// `hue_angle`: `atan2(b, a)` in degrees on `[0, 360)`.
fn hue_degrees(a: f64, b: f64) -> f64 {
    let h = libm::atan2(b, a) * (180.0 / PI);
    if h < 0.0 { h + 360.0 } else { h }
}

/// `eccentricity_factor`: `(cos(2 + h) + 3.8)/4` with `h` in radians.
fn eccentricity(h: f64) -> f64 {
    0.25 * (libm::cos(2.0 + h * (PI / 180.0)) + 3.8)
}

/// Division that maps a zero denominator to zero, `colour.algebra.sdiv` in its
/// default ignore mode.
fn sdiv(a: f64, b: f64) -> f64 {
    if b == 0.0 { 0.0 } else { a / b }
}

/// `opponent_colour_dimensions_inverse`, picking the well-conditioned form from
/// `|sin h|` versus `|cos h|`.
fn opponent_colour_dimensions_inverse(p_1: f64, p_2: f64, p_3: f64, h: f64) -> (f64, f64) {
    let hr = h * (PI / 180.0);
    let sin_hr = libm::sin(hr);
    let cos_hr = libm::cos(hr);
    let cos_sin = sdiv(cos_hr, sin_hr);
    let sin_cos = sdiv(sin_hr, cos_hr);

    let n = p_2 * (2.0 + p_3) * (460.0 / 1403.0);
    if sin_hr.abs() >= cos_hr.abs() {
        let p_4 = sdiv(p_1, sin_hr);
        let b = n
            / (p_4 + (2.0 + p_3) * (220.0 / 1403.0) * cos_sin - 27.0 / 1403.0
                + p_3 * (6300.0 / 1403.0));
        (b * cos_sin, b)
    } else {
        let p_5 = sdiv(p_1, cos_hr);
        let a = n
            / (p_5 + (2.0 + p_3) * (220.0 / 1403.0)
                - ((27.0 / 1403.0) - p_3 * (6300.0 / 1403.0)) * sin_cos);
        (a, a * sin_cos)
    }
}
