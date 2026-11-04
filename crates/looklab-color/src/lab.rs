// SPDX-License-Identifier: AGPL-3.0-or-later
//! CIELAB and its cylindrical form, ported from colour-science (`XYZ_to_Lab`,
//! `Lab_to_XYZ`, `Jab_to_JCh`, `JCh_to_Jab`; BSD-3-Clause, Colour Developers;
//! see `NOTICE`).
//!
//! Whites are reference tristimulus values with Y = 1, such as
//! [`Illuminant::xyz`](crate::xyy::Illuminant::xyz).

use crate::types::Xyz;
use core::f64::consts::PI;
use serde::{Deserialize, Serialize};

/// CIE 1976 L*a*b*.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Lab {
    pub l: f64,
    pub a: f64,
    pub b: f64,
}

/// CIELAB in cylindrical form; hue is in degrees over `[0, 360)`.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Lchab {
    pub l: f64,
    pub c: f64,
    pub h: f64,
}

impl Lab {
    pub const fn new(l: f64, a: f64, b: f64) -> Self {
        Self { l, a, b }
    }

    pub const fn to_array(self) -> [f64; 3] {
        [self.l, self.a, self.b]
    }

    pub const fn from_array(v: [f64; 3]) -> Self {
        Self::new(v[0], v[1], v[2])
    }
}

impl Lchab {
    pub const fn new(l: f64, c: f64, h: f64) -> Self {
        Self { l, c, h }
    }

    pub const fn to_array(self) -> [f64; 3] {
        [self.l, self.c, self.h]
    }

    pub const fn from_array(v: [f64; 3]) -> Self {
        Self::new(v[0], v[1], v[2])
    }
}

/// `(24 / 116)^3`, the CIE 1976 lightness segment boundary.
const CIE_EPSILON: f64 = (24.0 / 116.0) * (24.0 / 116.0) * (24.0 / 116.0);
const CIE_DELTA: f64 = 16.0 / 116.0;

/// colour-science `intermediate_lightness_function_CIE1976`.
fn cie_f(t: f64) -> f64 {
    if t > CIE_EPSILON {
        libm::cbrt(t)
    } else {
        (841.0 / 108.0) * t + CIE_DELTA
    }
}

/// colour-science `intermediate_luminance_function_CIE1976`.
fn cie_f_inverse(f: f64) -> f64 {
    if f > 24.0 / 116.0 {
        f * f * f
    } else {
        (f - CIE_DELTA) * (108.0 / 841.0)
    }
}

/// colour-science `XYZ_to_Lab`.
pub fn xyz_to_lab(xyz: Xyz, white: Xyz) -> Lab {
    let f_x = cie_f(xyz.x / white.x);
    let f_y = cie_f(xyz.y / white.y);
    let f_z = cie_f(xyz.z / white.z);
    Lab::new(116.0 * f_y - 16.0, 500.0 * (f_x - f_y), 200.0 * (f_y - f_z))
}

/// colour-science `Lab_to_XYZ`.
pub fn lab_to_xyz(lab: Lab, white: Xyz) -> Xyz {
    let f_y = (lab.l + 16.0) / 116.0;
    let f_x = lab.a / 500.0 + f_y;
    let f_z = f_y - lab.b / 200.0;
    Xyz::new(
        cie_f_inverse(f_x) * white.x,
        cie_f_inverse(f_y) * white.y,
        cie_f_inverse(f_z) * white.z,
    )
}

/// colour-science `Lab_to_LCHab`.
pub fn lab_to_lchab(lab: Lab) -> Lchab {
    Lchab::new(
        lab.l,
        libm::hypot(lab.a, lab.b),
        mod_360(libm::atan2(lab.b, lab.a) * (180.0 / PI)),
    )
}

/// colour-science `LCHab_to_Lab`.
pub fn lchab_to_lab(lchab: Lchab) -> Lab {
    let h = lchab.h * (PI / 180.0);
    Lab::new(lchab.l, lchab.c * libm::cos(h), lchab.c * libm::sin(h))
}

/// Python's `%` for a positive divisor.
fn mod_360(degrees: f64) -> f64 {
    let remainder = degrees % 360.0;
    if remainder < 0.0 {
        remainder + 360.0
    } else {
        remainder
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::xyy::Illuminant;

    #[test]
    fn white_maps_to_l_star_100() {
        let white = Illuminant::D65.xyz();
        let lab = xyz_to_lab(white, white);
        assert!((lab.l - 100.0).abs() < 1e-12);
        assert!(lab.a.abs() < 1e-12);
        assert!(lab.b.abs() < 1e-12);
    }

    #[test]
    fn lchab_hue_is_in_degrees() {
        let lchab = lab_to_lchab(Lab::new(50.0, 0.0, 20.0));
        assert!((lchab.c - 20.0).abs() < 1e-12);
        assert!((lchab.h - 90.0).abs() < 1e-12);
    }
}
