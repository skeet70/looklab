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

/// Opponent coordinates taken before the chroma power (Li et al. 2017,
/// `opponent_colour_dimensions_forward`). Linear in the responses.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ConeOpponent {
    /// Pre-compression adapted cone responses.
    pub rgb_c: [f64; 3],
    /// Post-adaptation responses `R'a, G'a, B'a`.
    pub rgb_a: [f64; 3],
    pub a: f64,
    pub b: f64,
    /// Temporary-magnitude divisor `R'a + G'a + 21·B'a/20`.
    pub denominator: f64,
    /// CAM16 lightness `J`.
    pub lightness: f64,
}

/// `d/dy` of [`ConeOpponent`] at fixed chromaticity, `y` the luminance scale.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ConeOpponentSlope {
    pub a: f64,
    pub b: f64,
    pub denominator: f64,
    pub lightness: f64,
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

        let (a, b) = opponent_dimensions(rgb_a);

        let h = hue_degrees(a, b);
        let e_t = eccentricity(h);

        let achromatic = achromatic_response(rgb_a, self.n_bb);
        let j = 100.0 * spow(achromatic / self.a_w, self.j_exp);
        let q = self.q_scale * (j / 100.0).sqrt();

        // `temporary_magnitude_quantity_forward`.
        let t = sdiv(
            self.p_scale * (e_t * (a * a + b * b).sqrt()),
            temporary_denominator(rgb_a),
        );
        let c = spow(t, 0.9) * spow(j / 100.0, 0.5) * self.ta;
        let m = c * self.fl_root4;
        let s = 100.0 * spow(sdiv(m, q), 0.5);

        Cam16 { j, c, h, m, s, q }
    }

    /// Post-adaptation responses and the pre-power opponent coordinates of `xyz`.
    pub fn cone_opponent(&self, xyz: Xyz) -> ConeOpponent {
        let rgb_c = self.adapted_rgb(xyz);
        let rgb_a = [
            post_adaptation(rgb_c[0], self.f_l),
            post_adaptation(rgb_c[1], self.f_l),
            post_adaptation(rgb_c[2], self.f_l),
        ];
        let (a, b) = opponent_dimensions(rgb_a);
        let achromatic = achromatic_response(rgb_a, self.n_bb);
        ConeOpponent {
            rgb_c,
            rgb_a,
            a,
            b,
            denominator: temporary_denominator(rgb_a),
            lightness: 100.0 * spow(achromatic / self.a_w, self.j_exp),
        }
    }

    pub fn cone_opponent_and_slope(
        &self,
        unit_xyz: Xyz,
        y: f64,
    ) -> (ConeOpponent, ConeOpponentSlope) {
        let opponent = self.cone_opponent(unit_xyz.scale(y));
        let mut slope = ConeOpponentSlope {
            a: 0.0,
            b: 0.0,
            denominator: 0.0,
            lightness: 0.0,
        };
        let mut d_response = [0.0_f64; 3];
        let mut d_a = 0.0;
        for i in 0..3 {
            let channel = self.white_scale[i]
                + self.d_rgb[i]
                    * 100.0
                    * (M16.0[i][0] * (unit_xyz.x - self.white.x)
                        + M16.0[i][1] * (unit_xyz.y - self.white.y)
                        + M16.0[i][2] * (unit_xyz.z - self.white.z));
            d_response[i] = post_adaptation_slope(opponent.rgb_c[i], self.f_l) * channel;
            slope.denominator += TEMPORARY_DENOMINATOR[i] * d_response[i];
            slope.a += OPPONENT_A[i] * d_response[i];
            slope.b += OPPONENT_B[i] * d_response[i];
            d_a += [2.0, 1.0, 0.05][i] * d_response[i];
        }
        let d_achromatic = self.n_bb * d_a;
        let ratio = opponent.lightness / 100.0;
        if ratio != 0.0 {
            slope.lightness = 100.0 * self.j_exp * libm::pow(ratio.abs(), self.j_exp - 1.0)
                / self.a_w
                * d_achromatic;
        }
        (opponent, slope)
    }

    /// [`ConeOpponent`] of `xyz` and `d(a, b, denominator)/dXYZ`.
    pub fn cone_opponent_jacobian(&self, xyz: Xyz) -> (ConeOpponent, [[f64; 3]; 3]) {
        let opponent = self.cone_opponent(xyz);
        let mut out = [[0.0_f64; 3]; 3];
        for (channel, row) in M16.0.iter().enumerate() {
            let slope = post_adaptation_slope(opponent.rgb_c[channel], self.f_l);
            for (axis, coefficient) in row.iter().enumerate() {
                let d_response = slope * self.d_rgb[channel] * coefficient * 100.0;
                out[0][axis] += OPPONENT_A[channel] * d_response;
                out[1][axis] += OPPONENT_B[channel] * d_response;
                out[2][axis] += TEMPORARY_DENOMINATOR[channel] * d_response;
            }
        }
        (opponent, out)
    }

    /// Adapted cone responses the inverse model produces for `jmh`, before the
    /// non-linear decompression.
    pub fn inverse_responses(&self, jmh: Jmh) -> [f64; 3] {
        self.inverse_rgb_a(jmh.j, jmh.m / self.fl_root4, jmh.h)
    }

    /// [`Self::inverse_responses`] and `d(rgb_a)/d(j, M, h)`.
    pub fn inverse_responses_and_jacobian(&self, jmh: Jmh) -> ([f64; 3], [[f64; 3]; 3]) {
        let j = Jet3::variable(jmh.j, 0);
        let c = Jet3::variable(jmh.m, 1) * Jet3::constant(1.0 / self.fl_root4);
        let h = Jet3::variable(jmh.h, 2);
        let rgb_a = self.inverse_rgb_a_jet(j, c, h);
        let mut value = [0.0_f64; 3];
        let mut jacobian = [[0.0_f64; 3]; 3];
        for channel in 0..3 {
            value[channel] = rgb_a[channel].v;
            jacobian[channel] = rgb_a[channel].d;
        }
        (value, jacobian)
    }

    /// `xyz` from [`Self::jmh_to_xyz`] and `dXYZ/d(j, M, h)`.
    pub fn inverse_xyz_and_jacobian(&self, jmh: Jmh) -> (Xyz, [[f64; 3]; 3]) {
        let (responses, response_jacobian) = self.inverse_responses_and_jacobian(jmh);
        let mut rgb = [0.0_f64; 3];
        let mut d_rgb = [[0.0_f64; 3]; 3];
        for channel in 0..3 {
            rgb[channel] =
                post_adaptation_inverse(responses[channel], self.f_l) / self.d_rgb[channel];
            let slope =
                post_adaptation_inverse_slope(responses[channel], self.f_l) / self.d_rgb[channel];
            for axis in 0..3 {
                d_rgb[channel][axis] = slope * response_jacobian[channel][axis];
            }
        }
        let xyz = self.m16_inverse.mul_vec(rgb);
        let mut jacobian = [[0.0_f64; 3]; 3];
        for axis in 0..3 {
            let column = self
                .m16_inverse
                .mul_vec([d_rgb[0][axis], d_rgb[1][axis], d_rgb[2][axis]]);
            for (row, value) in column.iter().enumerate() {
                jacobian[row][axis] = value / 100.0;
            }
        }
        (
            Xyz::new(xyz[0] / 100.0, xyz[1] / 100.0, xyz[2] / 100.0),
            jacobian,
        )
    }

    /// Largest `|rgb_a − 0.1|` the inverse compression is defined on.
    pub const RESPONSE_LIMIT: f64 = 400.0;

    /// Largest `J` whose neutral achromatic response stays inside [`Self::RESPONSE_LIMIT`].
    pub fn inverse_lightness_limit(&self) -> f64 {
        let response = 0.1 + Self::RESPONSE_LIMIT;
        let achromatic = (2.0 * response + response + 0.05 * response - 0.305) * self.n_bb;
        100.0 * spow(achromatic / self.a_w, self.j_exp)
    }

    /// Largest neutral `J'` the inverse model reaches, from [`Self::inverse_lightness_limit`].
    pub fn inverse_j_prime_limit(&self) -> f64 {
        let j = self.inverse_lightness_limit();
        crate::ucs::j_prime(j)
    }

    /// Scale `K` of the near-neutral normalized coordinates: `K·|(a, b)|/denominator`
    /// equals `join_chroma` at the opponent radius whose colorimetric chroma at
    /// `neutral_xyz` is `join_chroma`.
    pub fn neutral_chroma_scale(&self, neutral_xyz: Xyz, join_chroma: f64) -> f64 {
        let opponent = self.cone_opponent(neutral_xyz);
        let c2 = crate::ucs::UcsSpace::Ucs.coefficients().c2;
        let lightness = opponent.lightness / 100.0;
        let target = libm::expm1(c2 * join_chroma) / c2;
        let t = libm::pow(
            target / (self.fl_root4 * self.ta * lightness.sqrt()),
            1.0 / 0.9,
        );
        let radius = t * opponent.denominator / (self.p_scale * ECCENTRICITY_MEAN);
        join_chroma * opponent.denominator / radius
    }

    /// The exact CAM16-UCS chroma of the stimulus [`Self::opponent_chroma_to_xyz`]
    /// reconstructs; `|n| = 0` gives exactly `(0, 0)`.
    pub fn ucs_chroma_from_opponent(&self, n: [f64; 2], j_prime: f64, scale: f64) -> [f64; 2] {
        let magnitude = (n[0] * n[0] + n[1] * n[1]).sqrt();
        let h = hue_degrees(n[0], n[1]);
        let j = j_from_j_prime(j_prime);
        let t = self.p_scale * eccentricity(h) * magnitude / scale;
        let c = spow(t, 0.9) * spow(j / 100.0, 0.5) * self.ta;
        let ucs = crate::ucs::jmh_to_ucs(
            Jmh {
                j,
                m: c * self.fl_root4,
                h,
            },
            crate::ucs::UcsSpace::Ucs,
        );
        [ucs.a, ucs.b]
    }

    /// Inverse of [`Self::ucs_chroma_from_opponent`]; a zero UCS chroma maps to
    /// exactly `(0, 0)`.
    pub fn opponent_chroma_from_ucs(&self, chroma: [f64; 2], j_prime: f64, scale: f64) -> [f64; 2] {
        let m_prime = (chroma[0] * chroma[0] + chroma[1] * chroma[1]).sqrt();
        if m_prime == 0.0 {
            return [0.0, 0.0];
        }
        let j = j_from_j_prime(j_prime);
        if j.is_nan() || j <= 0.0 {
            return [0.0, 0.0];
        }
        let c2 = crate::ucs::UcsSpace::Ucs.coefficients().c2;
        let h = hue_degrees(chroma[0], chroma[1]);
        let c = libm::expm1(c2 * m_prime) / c2 / self.fl_root4;
        let t = libm::pow(c / ((j / 100.0).sqrt() * self.ta), 1.0 / 0.9);
        let magnitude = t * scale / (self.p_scale * eccentricity(h));
        let hr = h * (PI / 180.0);
        [magnitude * libm::cos(hr), magnitude * libm::sin(hr)]
    }

    #[allow(dead_code)]
    pub fn ucs_chroma_from_opponent_jacobian(
        &self,
        n: [f64; 2],
        j_prime: f64,
        scale: f64,
    ) -> ([[f64; 2]; 2], [f64; 2]) {
        let rho = (n[0] * n[0] + n[1] * n[1]).sqrt();
        if rho == 0.0 {
            return ([[f64::NAN; 2]; 2], [f64::NAN; 2]);
        }
        let h = hue_degrees(n[0], n[1]);
        let hr = h * (PI / 180.0);
        let e_t = eccentricity(h);
        let d_e_t = -0.25 * libm::sin(2.0 + hr) * (PI / 180.0);
        let j = j_from_j_prime(j_prime);
        let t = self.p_scale * e_t * rho / scale;
        let root = (j / 100.0).sqrt();
        let c = spow(t, 0.9) * root * self.ta;
        let m = c * self.fl_root4;
        let d_m_prime = 1.0 / (1.0 + crate::ucs::UcsSpace::Ucs.coefficients().c2 * m);
        let d_c_d_t = 0.9 * libm::pow(t, -0.1) * root * self.ta;
        let d_c_d_j = spow(t, 0.9) * self.ta / (20.0 * j.sqrt());
        let d_j_d_j_prime = 1.0 / (1.7 / ((1.0 + 0.007 * j) * (1.0 + 0.007 * j)));
        let d_m_prime_d_rho = d_m_prime * self.fl_root4 * d_c_d_t * self.p_scale * e_t / scale;
        let _d_m_prime_d_h =
            d_m_prime * self.fl_root4 * d_c_d_t * self.p_scale * d_e_t * rho / scale;
        let d_m_prime_d_j_prime = d_m_prime * self.fl_root4 * d_c_d_j * d_j_d_j_prime;
        let (sin_hr, cos_hr) = (libm::sin(hr), libm::cos(hr));
        let d_rho = [n[0] / rho, n[1] / rho];
        let d_h = [-n[1] / (rho * rho), n[0] / (rho * rho)];
        let mut jacobian = [[0.0_f64; 2]; 2];
        for axis in 0..2 {
            jacobian[0][axis] =
                d_m_prime_d_rho * d_rho[axis] * cos_hr - m * sin_hr * (PI / 180.0) * d_h[axis];
            jacobian[1][axis] =
                d_m_prime_d_rho * d_rho[axis] * sin_hr + m * cos_hr * (PI / 180.0) * d_h[axis];
        }
        let gradient = [d_m_prime_d_j_prime * cos_hr, d_m_prime_d_j_prime * sin_hr];
        (jacobian, gradient)
    }

    /// Smooth inverse of [`Self::ucs_chroma_from_opponent`], linear at the neutral
    /// origin.
    pub fn opponent_chroma_to_xyz(&self, n: [f64; 2], j_prime: f64, scale: f64) -> Xyz {
        let magnitude = (n[0] * n[0] + n[1] * n[1]).sqrt();
        let h = hue_degrees(n[0], n[1]);
        let j = j_from_j_prime(j_prime);
        let achromatic = self.a_w * spow(j / 100.0, self.inv_j_exp);
        let p_1 = if magnitude == 0.0 {
            f64::INFINITY
        } else {
            scale / magnitude
        };
        let p_2 = achromatic / self.n_bb + 0.305;
        let p_3 = 21.0 / 20.0;
        let (a, b) = opponent_colour_dimensions_inverse(p_1, p_2, p_3, h);
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

    /// Inverse model, `colour.CAM16_to_XYZ`.
    ///
    /// `j` is lightness and `h` the hue angle in degrees; `chroma` carries either
    /// `C` or `M`. Returns XYZ on the crate scale (Y of white = 1).
    pub fn cam16_to_xyz(&self, j: f64, chroma: Chroma, h: f64) -> Xyz {
        let c = match chroma {
            Chroma::C(c) => c,
            Chroma::M(m) => m / self.fl_root4,
        };
        let rgb_a = self.inverse_rgb_a(j, c, h);
        let rgb = [
            post_adaptation_inverse(rgb_a[0], self.f_l) / self.d_rgb[0],
            post_adaptation_inverse(rgb_a[1], self.f_l) / self.d_rgb[1],
            post_adaptation_inverse(rgb_a[2], self.f_l) / self.d_rgb[2],
        ];
        let xyz = self.m16_inverse.mul_vec(rgb);
        Xyz::new(xyz[0] / 100.0, xyz[1] / 100.0, xyz[2] / 100.0)
    }

    fn inverse_rgb_a(&self, j: f64, c: f64, h: f64) -> [f64; 3] {
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
        [
            (460.0 * p_2 + 451.0 * a + 288.0 * b) * inv_1403,
            (460.0 * p_2 - 891.0 * a - 261.0 * b) * inv_1403,
            (460.0 * p_2 - 220.0 * a - 6300.0 * b) * inv_1403,
        ]
    }

    fn inverse_rgb_a_jet(&self, j: Jet3, c: Jet3, h: Jet3) -> [Jet3; 3] {
        let j_safe = if j.v > f64::EPSILON {
            j
        } else {
            Jet3::constant(f64::EPSILON)
        };
        let root = (j_safe * Jet3::constant(0.01)).sqrt();
        let t = (c / (root * Jet3::constant(self.ta))).pow_signed(1.0 / 0.9);
        let e_t = (h * Jet3::constant(PI / 180.0) + Jet3::constant(2.0)).cos()
            * Jet3::constant(0.25)
            + Jet3::constant(0.95);
        let achromatic =
            (j * Jet3::constant(0.01)).pow_signed(self.inv_j_exp) * Jet3::constant(self.a_w);
        let p_1 = Jet3::constant(self.p_scale) * e_t / t;
        let p_2 = achromatic / Jet3::constant(self.n_bb) + Jet3::constant(0.305);
        let p_3 = Jet3::constant(21.0 / 20.0);
        let (a, b) = opponent_inverse_jet(p_1, p_2, p_3, h);
        let (a, b) = if t.v == 0.0 {
            (Jet3::constant(0.0), Jet3::constant(0.0))
        } else {
            (a, b)
        };
        let inv_1403 = Jet3::constant(1.0 / 1403.0);
        [
            (Jet3::constant(460.0) * p_2 + Jet3::constant(451.0) * a + Jet3::constant(288.0) * b)
                * inv_1403,
            (Jet3::constant(460.0) * p_2 - Jet3::constant(891.0) * a - Jet3::constant(261.0) * b)
                * inv_1403,
            (Jet3::constant(460.0) * p_2 - Jet3::constant(220.0) * a - Jet3::constant(6300.0) * b)
                * inv_1403,
        ]
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

fn j_from_j_prime(j_prime: f64) -> f64 {
    let c1 = 0.007;
    j_prime / (1.0 + 100.0 * c1 - c1 * j_prime)
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

/// Infinite at a zero response.
fn post_adaptation_slope(response: f64, f_l: f64) -> f64 {
    let u = f_l * response.abs() / 100.0;
    if u == 0.0 {
        return f64::INFINITY;
    }
    let f = libm::pow(u, 0.42);
    let denom = 27.13 + f;
    400.0 * 27.13 / (denom * denom) * 0.42 * (f / u) * (f_l / 100.0)
}

/// `opponent_colour_dimensions_forward`, factored so `R'a = G'a = B'a` gives exact zero.
fn opponent_dimensions(rgb_a: [f64; 3]) -> (f64, f64) {
    let a = (11.0 * (rgb_a[0] - rgb_a[1]) + (rgb_a[2] - rgb_a[1])) / 11.0;
    let b = ((rgb_a[0] - rgb_a[2]) + (rgb_a[1] - rgb_a[2])) / 9.0;
    (a, b)
}

/// `R'a + G'a + 21·B'a/20`, the temporary-magnitude divisor.
fn temporary_denominator(rgb_a: [f64; 3]) -> f64 {
    rgb_a[0] + rgb_a[1] + 21.0 * rgb_a[2] / 20.0
}

/// Coefficients of `a`, `b` and the denominator in the post-adaptation responses.
const OPPONENT_A: [f64; 3] = [1.0, -12.0 / 11.0, 1.0 / 11.0];
const OPPONENT_B: [f64; 3] = [1.0 / 9.0, 1.0 / 9.0, -2.0 / 9.0];
const TEMPORARY_DENOMINATOR: [f64; 3] = [1.0, 1.0, 21.0 / 20.0];

/// Constant term of the eccentricity factor `(3.8 + cos(2+h))/4`; its hue mean.
pub const ECCENTRICITY_MEAN: f64 = 0.95;

/// Forward-mode value with three partials, for the inverse model's Jacobian.
#[derive(Clone, Copy, Debug)]
struct Jet3 {
    v: f64,
    d: [f64; 3],
}

impl Jet3 {
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
}

impl core::ops::Add for Jet3 {
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

impl core::ops::Sub for Jet3 {
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

impl core::ops::Mul for Jet3 {
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

impl core::ops::Div for Jet3 {
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

fn sdiv_jet(a: Jet3, b: Jet3) -> Jet3 {
    if b.v == 0.0 {
        Jet3::constant(0.0)
    } else {
        a / b
    }
}

/// Same branch rule as [`opponent_colour_dimensions_inverse`].
fn opponent_inverse_jet(p_1: Jet3, p_2: Jet3, p_3: Jet3, h: Jet3) -> (Jet3, Jet3) {
    let hr = h * Jet3::constant(PI / 180.0);
    let sin_hr = hr.sin();
    let cos_hr = hr.cos();
    let cos_sin = sdiv_jet(cos_hr, sin_hr);
    let sin_cos = sdiv_jet(sin_hr, cos_hr);
    let n = p_2 * (Jet3::constant(2.0) + p_3) * Jet3::constant(460.0 / 1403.0);
    if sin_hr.v.abs() >= cos_hr.v.abs() {
        let p_4 = sdiv_jet(p_1, sin_hr);
        let b = n
            / (p_4 + (Jet3::constant(2.0) + p_3) * Jet3::constant(220.0 / 1403.0) * cos_sin
                - Jet3::constant(27.0 / 1403.0)
                + p_3 * Jet3::constant(6300.0 / 1403.0));
        (b * cos_sin, b)
    } else {
        let p_5 = sdiv_jet(p_1, cos_hr);
        let a = n
            / (p_5 + (Jet3::constant(2.0) + p_3) * Jet3::constant(220.0 / 1403.0)
                - (Jet3::constant(27.0 / 1403.0) - p_3 * Jet3::constant(6300.0 / 1403.0))
                    * sin_cos);
        (a, a * sin_cos)
    }
}

fn post_adaptation_inverse(rgb_a: f64, f_l: f64) -> f64 {
    let x = rgb_a - 0.1;
    x.signum() * 100.0 / f_l * libm::pow(27.13 * x.abs() / (400.0 - x.abs()), 1.0 / 0.42)
}

/// Zero at a zero response; infinite at the response limit.
fn post_adaptation_inverse_slope(rgb_a: f64, f_l: f64) -> f64 {
    let magnitude = (rgb_a - 0.1).abs();
    if magnitude == 0.0 {
        return 0.0;
    }
    let base = 27.13 * magnitude / (400.0 - magnitude);
    100.0 / f_l * (1.0 / 0.42) * libm::pow(base, 1.0 / 0.42 - 1.0) * 27.13 * 400.0
        / ((400.0 - magnitude) * (400.0 - magnitude))
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
