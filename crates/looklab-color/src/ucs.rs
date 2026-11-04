// SPDX-License-Identifier: AGPL-3.0-or-later
//! CAM16-UCS, CAM16-LCD and CAM16-SCD uniform colour spaces.
//!
//! Port of `colour.JMh_CAM16_to_CAM16UCS` / `colour.CAM16UCS_to_JMh_CAM16` and the
//! LCD/SCD variants from colour-science 0.4.7 (BSD-3-Clause, Colour Developers).
//!
//! The uniform-space transform is Li et al. (2017) applied to CAM16; the fitting
//! coefficients are Luo, M. R., Cui, G., Li, C. (2006). *Uniform colour spaces based
//! on CIECAM02 colour appearance model.* Color Research & Application, 31(4), 320-330.
//! Reference: Li, C. et al. (2017). Color Research & Application, 42(6), 703-718.

use crate::cam16::Jmh;

const PI: f64 = core::f64::consts::PI;

/// `c1` is 0.007 for all three spaces, so `J'` is shared.
const C1: f64 = 0.007;

/// A point in one of the CAM16 uniform spaces, on the 0-100 reference scale.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Ucs {
    pub j: f64,
    pub a: f64,
    pub b: f64,
}

impl Ucs {
    pub const fn new(j: f64, a: f64, b: f64) -> Self {
        Self { j, a, b }
    }

    pub const fn to_array(self) -> [f64; 3] {
        [self.j, self.a, self.b]
    }

    pub const fn from_array(v: [f64; 3]) -> Self {
        Self::new(v[0], v[1], v[2])
    }
}

/// Which uniform space to use: `Ucs` (uniform), `Lcd` (large colour differences)
/// or `Scd` (small colour differences).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UcsSpace {
    Ucs,
    Lcd,
    Scd,
}

/// Fitting coefficients of Luo et al. (2006), `COEFFICIENTS_UCS_LUO2006`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct UcsCoefficients {
    pub k_l: f64,
    pub c1: f64,
    pub c2: f64,
}

impl UcsSpace {
    pub const fn coefficients(self) -> UcsCoefficients {
        match self {
            Self::Ucs => UcsCoefficients {
                k_l: 1.00,
                c1: 0.007,
                c2: 0.0228,
            },
            Self::Lcd => UcsCoefficients {
                k_l: 0.77,
                c1: 0.007,
                c2: 0.0053,
            },
            Self::Scd => UcsCoefficients {
                k_l: 1.24,
                c1: 0.007,
                c2: 0.0363,
            },
        }
    }
}

/// `JMh` to CAM16-UCS-family `J'a'b'`, `colour.JMh_CAM16_to_CAM16UCS`.
pub fn jmh_to_ucs(jmh: Jmh, space: UcsSpace) -> Ucs {
    let c2 = space.coefficients().c2;
    let m_p = libm::log1p(c2 * jmh.m) / c2;
    let hr = jmh.h * (PI / 180.0);
    Ucs {
        j: j_prime(jmh.j),
        a: m_p * libm::cos(hr),
        b: m_p * libm::sin(hr),
    }
}

/// `J'a'b'` to `JMh`, `colour.CAM16UCS_to_JMh_CAM16`.
pub fn ucs_to_jmh(ucs: Ucs, space: UcsSpace) -> Jmh {
    let c = space.coefficients();
    let j = -ucs.j / (c.c1 * ucs.j - 1.0 - 100.0 * c.c1);
    let m_p = (ucs.a * ucs.a + ucs.b * ucs.b).sqrt();
    let mut h = libm::atan2(ucs.b, ucs.a) * (180.0 / PI);
    if h < 0.0 {
        h += 360.0;
    }
    Jmh {
        j,
        m: libm::expm1(m_p * c.c2) / c.c2,
        h,
    }
}

pub(crate) fn j_prime(j: f64) -> f64 {
    ((1.0 + 100.0 * C1) * j) / (1.0 + C1 * j)
}

pub(crate) fn d_j_prime_d_j(j: f64) -> f64 {
    (1.0 + 100.0 * C1) / ((1.0 + C1 * j) * (1.0 + C1 * j))
}

/// CAM16-UCS colour difference, `K_L = 1`.
pub fn delta_e_ucs(a: Ucs, b: Ucs) -> f64 {
    delta_e(a, b, UcsSpace::Ucs)
}

/// CAM16-LCD colour difference, `K_L = 0.77`.
pub fn delta_e_lcd(a: Ucs, b: Ucs) -> f64 {
    delta_e(a, b, UcsSpace::Lcd)
}

/// CAM16-SCD colour difference, `K_L = 1.24`.
pub fn delta_e_scd(a: Ucs, b: Ucs) -> f64 {
    delta_e(a, b, UcsSpace::Scd)
}

fn delta_e(a: Ucs, b: Ucs, space: UcsSpace) -> f64 {
    let k_l = space.coefficients().k_l;
    let dj = (a.j - b.j) / k_l;
    let da = a.a - b.a;
    let db = a.b - b.b;
    (dj * dj + da * da + db * db).sqrt()
}
